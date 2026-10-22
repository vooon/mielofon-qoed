//! Policy classifier — decouples decision-making from measurement.
//!
//! Every probe report is appended to the replicated `Store` (measurement).
//! A background loop here re-derives each link's quality class and OSPF cost
//! from a window of recent samples (`Store::window`, per-dimension freshness
//! with last-known carry for the sparse throughput dimension), and writes the
//! resulting *policy record* into the LWW KV. The scheduler, register-seeding,
//! map, metrics and policy/quality endpoints all read that derived record, so
//! none of them depend on the lossy latest datapoint any more.
//!
//! Busy/conflict semantics: a link whose most recent state is `busy` or
//! `conflict` is never re-classified as degraded. It keeps its last derived
//! cost for up to `busy_hold_secs`; beyond that the sample is stale and the
//! classifier stops touching it (the underlay owns hard outages).
//!
//! Unknown semantics: a window with no current always-tier measurement (no
//! rtt/loss/jitter within `always_fresh_secs` — the agent stopped answering,
//! or its ping window was still empty) says nothing about the link, so the
//! last decision is kept as well. An unconstrained classification would
//! otherwise fall through to `good` and reset a degraded link's cost.
//!
//! Hysteresis: a link's class only changes once a new class has been observed
//! on `hysteresis_passes` consecutive *fresh samples* (a pass that sees the
//! same newest sample as the previous pass does not count). This stops a
//! single noisy sample — or a metric wobbling across a threshold — from
//! flipping the class and re-routing OSPF cost. The first classification of a
//! link applies immediately.
//!
//! Ownership: a node derives policy only for links of agents registered with
//! it — the links its scheduler can act on. Every member replicates the raw
//! samples, so the reverse half of a tunnel owned elsewhere is still available
//! for pairing, while the derived record for a link has exactly one writer
//! cluster-wide and the LWW gossip merge cannot flap between members.
//!
//! Pairing: the two directed halves of one tunnel (`A→B`, `B→A`) are
//! classified from a merged view (`classifier.pair_cost`, see
//! `config::PairCost`), so the round-trip dimensions both ends measure cannot
//! drift the two OSPF costs apart. The derived record still carries each
//! half's own measurements.

use crate::clock::now_secs;
use crate::config::PairCost;
use crate::model::{LinkKey, ProbeState, Quality, QualityRecord};
use crate::quality;
use crate::state::AppState;
use crate::store::{WindowSpec, WindowView};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

/// Per-link last-derived policy, used to hold a cost while a link is busy and
/// as the reference class for hysteresis.
#[derive(Clone, PartialEq)]
struct LastPolicy {
    quality: Quality,
    ospf_cost: u32,
}

/// A candidate class being confirmed across consecutive fresh samples.
#[derive(Clone, Copy)]
struct Pending {
    quality: Quality,
    /// Fresh samples (distinct newest-sample timestamps) that agreed so far.
    passes: u32,
    /// Timestamp of the newest sample counted last.
    last_ts: u64,
}

pub struct ClassifierState {
    last: RwLock<HashMap<LinkKey, LastPolicy>>,
    pending: RwLock<HashMap<LinkKey, Pending>>,
}

impl ClassifierState {
    pub fn new() -> Self {
        ClassifierState {
            last: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for ClassifierState {
    fn default() -> Self {
        Self::new()
    }
}

/// Map each link to its reverse half (`from`/`to` swapped). A link is paired
/// only when exactly one link exists in each direction between the two nodes;
/// parallel tunnels between the same pair cannot be matched by node names and
/// are left unpaired.
pub fn pair_links(links: &[LinkKey]) -> HashMap<LinkKey, LinkKey> {
    let mut by_dir: HashMap<(&str, &str), Vec<&LinkKey>> = HashMap::new();
    for l in links {
        by_dir
            .entry((l.from.as_str(), l.to.as_str()))
            .or_default()
            .push(l);
    }

    let mut pairs = HashMap::new();
    for l in links {
        let (Some(fwd), Some(rev)) = (
            by_dir.get(&(l.from.as_str(), l.to.as_str())),
            by_dir.get(&(l.to.as_str(), l.from.as_str())),
        ) else {
            continue;
        };
        if fwd.len() == 1 && rev.len() == 1 && l.from != l.to {
            pairs.insert(l.clone(), rev[0].clone());
        }
    }
    pairs
}

/// Worse of two upper-bound metrics (higher is worse); either may be unset.
fn worse_upper(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

/// Worse of two lower-bound metrics (lower is worse); either may be unset.
fn worse_lower(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// The view a paired link is classified from: the round-trip dims (rtt, loss,
/// jitter) are the worse of both halves; throughput is the link's own
/// (`Directional`) or the worse of both (`Symmetric`). State/ts/util stay the
/// link's own — busy/conflict hold is a per-direction matter.
pub fn merge_pair(own: &WindowView, rev: &WindowView, mode: PairCost) -> WindowView {
    let mut v = own.clone();
    if mode == PairCost::Off {
        return v;
    }
    v.rtt_ms = worse_upper(own.rtt_ms, rev.rtt_ms);
    v.loss_pct = worse_upper(own.loss_pct, rev.loss_pct);
    v.jitter_ms = worse_upper(own.jitter_ms, rev.jitter_ms);
    if mode == PairCost::Symmetric {
        v.tcp_mbps = worse_lower(own.tcp_mbps, rev.tcp_mbps);
        v.udp_mbps = worse_lower(own.udp_mbps, rev.udp_mbps);
        v.udp_loss_pct = worse_upper(own.udp_loss_pct, rev.udp_loss_pct);
        v.udp_jitter_ms = worse_upper(own.udp_jitter_ms, rev.udp_jitter_ms);
    }
    v
}

/// One classification pass over the links owned by this node's agents.
pub fn classify_once(state: &AppState) {
    let cfg = &state.cfg;
    let spec = WindowSpec {
        always_fresh_secs: cfg.classifier.always_fresh_secs,
        tcp_fresh_secs: cfg.classifier.tcp_fresh_secs,
        tcp_carry_secs: cfg.classifier.tcp_carry_secs,
    };
    let now = now_secs();

    let links = state.tsdb.links();
    let views: HashMap<LinkKey, WindowView> = links
        .iter()
        .filter_map(|l| state.tsdb.window(l, &spec, now).map(|v| (l.clone(), v)))
        .collect();
    let pairs = if cfg.classifier.pair_cost == PairCost::Off {
        HashMap::new()
    } else {
        pair_links(&links)
    };
    let owned: HashSet<LinkKey> = state
        .workers
        .owned_links()
        .into_iter()
        .map(|(_, l)| l)
        .collect();

    for link in links {
        if !owned.contains(&link) {
            continue;
        }
        let Some(view) = views.get(&link) else {
            continue;
        };

        // Busy/conflict: hold the last derived cost (never reclassify a link
        // in use as degraded). Expire the hold once the sample ages out.
        if (view.state == ProbeState::Busy || view.state == ProbeState::Conflict)
            && now.saturating_sub(view.ts) < cfg.classifier.busy_hold_secs
        {
            hold_last(state, &link, view);
            continue;
        }

        let class_view = match pairs.get(&link).and_then(|rev| views.get(rev)) {
            Some(rev) => merge_pair(view, rev, cfg.classifier.pair_cost),
            None => view.clone(),
        };

        // Unknown: no current ping data for the tunnel (own half, or either
        // half when paired). Nothing can be said about it now — keep the past
        // decision instead of classifying an empty window as good.
        if !class_view.measured() {
            hold_last(state, &link, view);
            continue;
        }

        // Fresh data: classify from the window, then apply hysteresis — a new
        // class only takes effect after `hysteresis_passes` consecutive fresh
        // samples agree. The first classification of a link applies immediately.
        let candidate = quality::classify_window(&cfg.quality, &class_view);

        let held = state.classifier.last.read().expect("classifier lock");
        let current = held.get(&link);

        let effective = match current {
            // First observation: apply immediately (no history to be stable).
            None => candidate,
            // Class unchanged: reset the confirmation counter.
            Some(last) if last.quality == candidate => {
                state
                    .classifier
                    .pending
                    .write()
                    .expect("classifier lock")
                    .remove(&link);
                candidate
            }
            // Candidate differs: require hysteresis_passes consecutive fresh
            // samples (a pass re-reading the same sample does not count).
            Some(last) => {
                let mut pending = state.classifier.pending.write().expect("classifier lock");
                let entry = pending.entry(link.clone()).or_insert(Pending {
                    quality: candidate,
                    passes: 0,
                    last_ts: 0,
                });
                if entry.quality != candidate {
                    // A different candidate interrupted the confirmation.
                    *entry = Pending {
                        quality: candidate,
                        passes: 0,
                        last_ts: 0,
                    };
                }
                if entry.last_ts != view.ts {
                    entry.passes += 1;
                    entry.last_ts = view.ts;
                }
                if entry.passes >= cfg.classifier.hysteresis_passes.max(1) {
                    pending.remove(&link);
                    candidate
                } else {
                    last.quality
                }
            }
        };
        drop(held);

        let effective_cost = policy_cost(cfg, &link, effective);
        state
            .classifier
            .last
            .write()
            .expect("classifier lock")
            .insert(
                link.clone(),
                LastPolicy {
                    quality: effective,
                    ospf_cost: effective_cost,
                },
            );
        write_derived(state, &link, view, Some(effective), Some(effective_cost));
    }
}

/// The cost a link advertises for a quality class: the class cost, plus the
/// hub→spoke offset when a member advertises towards a non-member.
pub fn policy_cost(cfg: &crate::config::Config, link: &LinkKey, q: Quality) -> u32 {
    let base = quality::cost_for_quality(&cfg.quality, q);
    let hub_to_spoke = cfg.members.contains_key(&link.from) && !cfg.members.contains_key(&link.to);
    if hub_to_spoke {
        base.saturating_add(cfg.classifier.hub_spoke_cost_offset)
    } else {
        base
    }
}

/// Keep a link's last derived quality/cost. The derived record is still
/// written (so the link stays on the map with its current state/ts, quality
/// unset if it was never classified), stamped with the held cost when one
/// exists.
fn hold_last(state: &AppState, link: &LinkKey, view: &WindowView) {
    let (held_quality, held_cost) = {
        let guard = state.classifier.last.read().expect("classifier lock");
        guard.get(link).map_or((None, None), |last| {
            (Some(last.quality), Some(last.ospf_cost))
        })
    };
    write_derived(state, link, view, held_quality, held_cost);
}

/// Stamp the collapsed window (rtt/loss/jitter/tcp/util/state) plus the derived
/// quality/cost into the KV as the link's live policy record. The KV keeps the
/// measurement dims so the map/metrics/quality endpoints keep showing them
/// (now window-collapsed and no longer wiped by every always probe).
fn write_derived(
    state: &AppState,
    link: &LinkKey,
    view: &crate::store::WindowView,
    quality: Option<Quality>,
    ospf_cost: Option<u32>,
) {
    let rec = QualityRecord {
        ts: view.ts,
        rtt_ms: view.rtt_ms,
        loss_pct: view.loss_pct,
        jitter_ms: view.jitter_ms,
        tcp_mbps: view.tcp_mbps,
        udp_mbps: view.udp_mbps,
        udp_jitter_ms: view.udp_jitter_ms,
        udp_loss_pct: view.udp_loss_pct,
        util_mbps: view.util_mbps,
        state: view.state,
        quality,
        ospf_cost,
    };
    state.kv.put(link.clone(), rec);
}

/// Periodic classifier loop.
pub async fn classifier_loop(state: AppState) {
    let interval = state.cfg.classifier.interval_secs.max(1);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
    loop {
        tick.tick().await;
        classify_once(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Classifier, Config};
    use crate::model::{LinkKey, ProbeState};
    use crate::state::AppState;
    use crate::tsdb::RawSample;

    fn state_with(fresh: u64, carry: u64) -> AppState {
        state_with_hyst(fresh, carry, 1)
    }

    /// Register every link's `from` node as an agent owning it, so the
    /// classifier (which only derives policy for owned links) handles it.
    fn own(state: &AppState, links: &[&LinkKey]) {
        let mut by_agent: HashMap<&str, Vec<LinkKey>> = HashMap::new();
        for l in links {
            by_agent.entry(&l.from).or_default().push((*l).clone());
        }
        for (agent, links) in by_agent {
            state.workers.register(agent, links, None);
        }
    }

    fn state_with_hyst(fresh: u64, carry: u64, hysteresis_passes: u32) -> AppState {
        let cfg = Config {
            classifier: Classifier {
                interval_secs: 5,
                always_fresh_secs: 60,
                tcp_fresh_secs: fresh,
                tcp_carry_secs: carry,
                busy_hold_secs: 120,
                hysteresis_passes,
                pair_cost: PairCost::Directional,
                hub_spoke_cost_offset: 0,
            },
            ..Config::default()
        };
        AppState::new(cfg)
    }

    fn append_always(state: &AppState, link: &LinkKey, ts: u64, rtt: f64) {
        state.tsdb.append(
            link,
            RawSample::from_dims(
                ProbeState::Quiet,
                ts,
                Some(rtt),
                Some(0.0),
                Some(2.0),
                None,
                0.0,
            ),
        );
    }

    fn append_busy(state: &AppState, link: &LinkKey, ts: u64) {
        state.tsdb.append(
            link,
            RawSample::from_dims(ProbeState::Busy, ts, None, None, None, None, 30.0),
        );
    }

    #[test]
    fn derives_policy_from_window_and_writes_kv() {
        let state = state_with(1200, 3600);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        let base = crate::clock::now_secs();
        // Old throughput sample (throttled) + fresh always probes.
        state.tsdb.append(
            &link,
            RawSample::from_dims(
                ProbeState::Quiet,
                base - 300,
                None,
                None,
                None,
                Some(1.5),
                0.0,
            ),
        );
        append_always(&state, &link, base, 15.0);

        classify_once(&state);
        let rec = state.kv.get(&link).expect("derived record");
        // The carried tcp (1.5 Mbps) makes it Bad, and that is written out.
        assert_eq!(rec.quality, Some(Quality::Bad));
        assert_eq!(rec.ospf_cost, Some(100));
        // The live record carries the collapsed measurement dims, not just cost.
        assert_eq!(rec.tcp_mbps, Some(1.5));
        assert_eq!(rec.rtt_ms, Some(15.0));
    }

    #[test]
    fn busy_link_holds_last_cost() {
        let state = state_with(1200, 3600);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        let base = crate::clock::now_secs();
        // First a good healthy link -> Good cost.
        state.tsdb.append(
            &link,
            RawSample::from_dims(
                ProbeState::Quiet,
                base,
                Some(10.0),
                Some(0.0),
                Some(2.0),
                Some(90.0),
                0.0,
            ),
        );
        classify_once(&state);
        assert_eq!(state.kv.get(&link).unwrap().quality, Some(Quality::Good));

        // Then a busy sample arrives: cost must NOT escalate, it stays good.
        append_busy(&state, &link, base + 30);
        classify_once(&state);
        assert_eq!(
            state.kv.get(&link).unwrap().quality,
            Some(Quality::Good),
            "busy must hold the last (good) cost"
        );
        assert_eq!(state.kv.get(&link).unwrap().ospf_cost, Some(10));
    }

    #[test]
    fn no_recent_data_means_no_derived_record() {
        let state = state_with(1200, 3600);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        // Beyond the carry horizon: window() yields None, KV stays empty.
        let old_ts = crate::clock::now_secs() - 7200;
        state.tsdb.append(
            &link,
            RawSample::from_dims(
                ProbeState::Quiet,
                old_ts,
                Some(10.0),
                Some(0.0),
                Some(100.0),
                Some(90.0),
                0.0,
            ),
        );
        classify_once(&state);
        assert!(state.kv.get(&link).is_none());
    }

    #[test]
    fn unmeasured_window_keeps_last_decision() {
        let state = state_with(1200, 3600);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        let base = crate::clock::now_secs();
        // A link last decided Bad (cost 100), whose pings then stopped: the
        // only sample inside the carry horizon is an old throughput one, so
        // the window has tcp but no rtt/loss/jitter.
        state
            .classifier
            .last
            .write()
            .expect("classifier lock")
            .insert(
                link.clone(),
                LastPolicy {
                    quality: Quality::Bad,
                    ospf_cost: 100,
                },
            );
        state.tsdb.append(
            &link,
            RawSample::from_dims(
                ProbeState::Quiet,
                base - 120,
                None,
                None,
                None,
                Some(90.0),
                0.0,
            ),
        );
        classify_once(&state);
        let rec = state.kv.get(&link).expect("record stays on the map");
        assert_eq!(rec.rtt_ms, None);
        assert_eq!(
            rec.quality,
            Some(Quality::Bad),
            "no ping data must not reset the link to good"
        );
        assert_eq!(rec.ospf_cost, Some(100));

        // Never decided + never measured: on the map, but no quality/cost.
        let fresh = LinkKey::new("spoke-1", "hub-b", "awg_hub_b");
        own(&state, &[&link, &fresh]);
        state.tsdb.append(
            &fresh,
            RawSample::from_dims(
                ProbeState::Quiet,
                base - 120,
                None,
                None,
                None,
                Some(90.0),
                0.0,
            ),
        );
        classify_once(&state);
        let rec = state.kv.get(&fresh).expect("record stays on the map");
        assert_eq!(rec.quality, None);
        assert_eq!(rec.ospf_cost, None);
    }

    #[test]
    fn busy_from_start_link_stays_on_map_unclassified() {
        let state = state_with(1200, 3600);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        let base = crate::clock::now_secs();
        // A link that was busy on its very first report: no held cost exists.
        append_busy(&state, &link, base);
        classify_once(&state);
        let rec = state.kv.get(&link).expect("busy link present on map");
        assert_eq!(rec.state, ProbeState::Busy);
        assert_eq!(rec.quality, None, "never classified while busy");
        assert_eq!(rec.ospf_cost, None);
    }

    #[test]
    fn class_change_requires_consecutive_passes() {
        // hysteresis_passes = 3: a candidate class must be observed 3
        // consecutive times before it is applied, so a single degraded sample
        // cannot flip the cost.
        let state = state_with_hyst(1200, 3600, 3);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        let mut ts = crate::clock::now_secs();

        // Establish a good link (low rtt, low jitter, healthy throughput).
        state.tsdb.append(
            &link,
            RawSample::from_dims(
                ProbeState::Quiet,
                ts,
                Some(10.0),
                Some(0.0),
                Some(2.0),
                Some(90.0),
                0.0,
            ),
        );
        classify_once(&state);
        assert_eq!(state.kv.get(&link).unwrap().quality, Some(Quality::Good));

        // Degrade the link (rtt > bad line) and classify several times with
        // fresh samples each pass. The class must NOT flip on the first one or
        // two observations — only after 3 consecutive degraded samples.
        let mut got_bad = false;
        for pass in 0..5 {
            ts += 5; // fresh sample within always_fresh_secs (60)
            state.tsdb.append(
                &link,
                RawSample::from_dims(
                    ProbeState::Quiet,
                    ts,
                    Some(600.0),
                    Some(0.0),
                    Some(2.0),
                    Some(90.0),
                    0.0,
                ),
            );
            classify_once(&state);
            let q = state.kv.get(&link).unwrap().quality.unwrap();
            if q == Quality::Bad {
                got_bad = true;
                // Bad is applied only on the 3rd consecutive degraded pass.
                assert!(
                    pass >= 2,
                    "class must not flip before hysteresis_passes (got bad on pass {pass})"
                );
            } else {
                assert_eq!(q, Quality::Good, "must hold good until hysteresis");
            }
        }
        assert!(got_bad, "sustained degradation must eventually apply");
    }

    #[test]
    fn hysteresis_counts_fresh_samples_not_passes() {
        let state = state_with_hyst(1200, 3600, 3);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        own(&state, &[&link]);
        let ts = crate::clock::now_secs();
        append_dims(&state, &link, ts, 10.0, Some(90.0));
        classify_once(&state);
        assert_eq!(state.kv.get(&link).unwrap().quality, Some(Quality::Good));

        // One degraded sample, re-read by many passes: must never flip.
        append_dims(&state, &link, ts + 5, 600.0, Some(90.0));
        for _ in 0..10 {
            classify_once(&state);
            assert_eq!(state.kv.get(&link).unwrap().quality, Some(Quality::Good));
        }
        // Two more fresh degraded samples complete the confirmation.
        append_dims(&state, &link, ts + 10, 600.0, Some(90.0));
        classify_once(&state);
        assert_eq!(state.kv.get(&link).unwrap().quality, Some(Quality::Good));
        append_dims(&state, &link, ts + 15, 600.0, Some(90.0));
        classify_once(&state);
        assert_eq!(state.kv.get(&link).unwrap().quality, Some(Quality::Bad));
    }

    #[test]
    fn hub_to_spoke_links_carry_the_transit_offset() {
        // hub-a and hub-b are members; node-0 is a spoke. Same quality on
        // every link: only the hub→spoke direction is pushed out of transit.
        let mut cfg = Config {
            classifier: Classifier {
                hysteresis_passes: 1,
                ..Classifier::default()
            },
            ..Config::default()
        };
        cfg.members
            .insert("hub-a".into(), "203.0.113.1".parse().unwrap());
        cfg.members
            .insert("hub-b".into(), "203.0.113.2".parse().unwrap());
        let state = AppState::new(cfg);
        let hub_spoke = LinkKey::new("hub-a", "node-0", "awg_sp_node_0");
        let spoke_hub = LinkKey::new("node-0", "hub-a", "awg_hub_a");
        let hub_hub = LinkKey::new("hub-a", "hub-b", "awg_int_hub_b");
        own(&state, &[&hub_spoke, &spoke_hub, &hub_hub]);
        let now = crate::clock::now_secs();
        for l in [&hub_spoke, &spoke_hub, &hub_hub] {
            append_dims(&state, l, now, 15.0, None);
        }
        classify_once(&state);
        assert_eq!(cost(&state, &spoke_hub), Some(10), "spoke→hub: class cost");
        assert_eq!(cost(&state, &hub_hub), Some(10), "hub↔hub: class cost");
        assert_eq!(
            cost(&state, &hub_spoke),
            Some(1010),
            "hub→spoke: offset added"
        );
        assert_eq!(
            state.kv.get(&hub_spoke).unwrap().quality,
            Some(Quality::Good),
            "quality itself is untouched"
        );
    }

    #[test]
    fn only_owned_links_get_a_derived_record() {
        // The reverse half is replicated from another member: it feeds the
        // pairing but is not written here (its owner derives it).
        let ab = LinkKey::new("node-0", "hub-a", "awg_hub_a");
        let ba = LinkKey::new("hub-a", "node-0", "awg_node_0");
        let now = crate::clock::now_secs();
        let state = state_with_pair(PairCost::Directional);
        own(&state, &[&ab]);
        append_dims(&state, &ab, now, 15.0, None);
        append_dims(&state, &ba, now, 120.0, None);
        classify_once(&state);
        assert_eq!(
            cost(&state, &ab),
            Some(50),
            "paired with the replicated half"
        );
        assert!(state.kv.get(&ba).is_none(), "not owned here");
    }

    fn state_with_pair(mode: PairCost) -> AppState {
        let cfg = Config {
            classifier: Classifier {
                hysteresis_passes: 1,
                pair_cost: mode,
                ..Classifier::default()
            },
            ..Config::default()
        };
        AppState::new(cfg)
    }

    fn append_dims(state: &AppState, link: &LinkKey, ts: u64, rtt: f64, tcp: Option<f64>) {
        state.tsdb.append(
            link,
            RawSample::from_dims(
                ProbeState::Quiet,
                ts,
                Some(rtt),
                Some(0.0),
                Some(2.0),
                tcp,
                0.0,
            ),
        );
    }

    fn cost(state: &AppState, link: &LinkKey) -> Option<u32> {
        state.kv.get(link).and_then(|r| r.ospf_cost)
    }

    #[test]
    fn pairs_only_unambiguous_reverse_links() {
        let ab = LinkKey::new("node-0", "hub-a", "awg_hub_a");
        let ba = LinkKey::new("hub-a", "node-0", "awg_node_0");
        // One-way only: no reverse half registered.
        let cb = LinkKey::new("node-1", "hub-a", "awg_hub_a");
        // Parallel tunnels node-2 -> hub-b: names cannot tell them apart.
        let p1 = LinkKey::new("node-2", "hub-b", "awg_hub_b");
        let p2 = LinkKey::new("node-2", "hub-b", "awg_hub_b2");
        let pr = LinkKey::new("hub-b", "node-2", "awg_node_2");

        let pairs = pair_links(&[
            ab.clone(),
            ba.clone(),
            cb.clone(),
            p1.clone(),
            p2,
            pr.clone(),
        ]);
        assert_eq!(pairs.get(&ab), Some(&ba));
        assert_eq!(pairs.get(&ba), Some(&ab));
        assert!(!pairs.contains_key(&cb));
        assert!(!pairs.contains_key(&p1));
        assert!(
            !pairs.contains_key(&pr),
            "reverse of parallel tunnels is ambiguous"
        );
    }

    #[test]
    fn paired_halves_share_round_trip_dims() {
        // Both ends ping the same round trip but measured differently (one
        // side saw congestion): without pairing the costs drift apart.
        let ab = LinkKey::new("node-0", "hub-a", "awg_hub_a");
        let ba = LinkKey::new("hub-a", "node-0", "awg_node_0");
        let now = crate::clock::now_secs();

        let state = state_with_pair(PairCost::Directional);
        own(&state, &[&ab, &ba]);
        append_dims(&state, &ab, now, 15.0, None);
        append_dims(&state, &ba, now, 120.0, None);
        classify_once(&state);
        assert_eq!(
            cost(&state, &ab),
            Some(50),
            "worse rtt (poor) applies to both"
        );
        assert_eq!(cost(&state, &ba), Some(50));
        // The derived record still shows each half's own measurement.
        assert_eq!(state.kv.get(&ab).unwrap().rtt_ms, Some(15.0));

        let off = state_with_pair(PairCost::Off);
        own(&off, &[&ab, &ba]);
        append_dims(&off, &ab, now, 15.0, None);
        append_dims(&off, &ba, now, 120.0, None);
        classify_once(&off);
        assert_eq!(cost(&off, &ab), Some(10));
        assert_eq!(cost(&off, &ba), Some(50));
    }

    #[test]
    fn throughput_is_directional_or_symmetric_by_mode() {
        // One-way shaping: node-0's upload to hub-a is throttled, the reverse
        // direction is healthy; rtt is good both ways.
        let ab = LinkKey::new("node-0", "hub-a", "awg_hub_a");
        let ba = LinkKey::new("hub-a", "node-0", "awg_node_0");
        let now = crate::clock::now_secs();

        let dir = state_with_pair(PairCost::Directional);
        own(&dir, &[&ab, &ba]);
        append_dims(&dir, &ab, now, 15.0, Some(1.5));
        append_dims(&dir, &ba, now, 15.0, Some(80.0));
        classify_once(&dir);
        assert_eq!(cost(&dir, &ab), Some(100), "throttled direction penalised");
        assert_eq!(
            cost(&dir, &ba),
            Some(10),
            "healthy direction keeps its cost"
        );

        let sym = state_with_pair(PairCost::Symmetric);
        own(&sym, &[&ab, &ba]);
        append_dims(&sym, &ab, now, 15.0, Some(1.5));
        append_dims(&sym, &ba, now, 15.0, Some(80.0));
        classify_once(&sym);
        assert_eq!(cost(&sym, &ab), Some(100));
        assert_eq!(cost(&sym, &ba), Some(100), "symmetric: both take the worse");
    }

    #[test]
    fn merge_pair_keeps_unset_dims_from_the_other_half() {
        let own = WindowView {
            rtt_ms: None,
            loss_pct: Some(0.5),
            ..Default::default()
        };
        let rev = WindowView {
            rtt_ms: Some(30.0),
            loss_pct: None,
            tcp_mbps: Some(5.0),
            ..Default::default()
        };
        let v = merge_pair(&own, &rev, PairCost::Directional);
        assert_eq!(v.rtt_ms, Some(30.0));
        assert_eq!(v.loss_pct, Some(0.5));
        assert_eq!(v.tcp_mbps, None, "directional never borrows reverse tcp");
        let v = merge_pair(&own, &rev, PairCost::Symmetric);
        assert_eq!(v.tcp_mbps, Some(5.0));
    }
}
