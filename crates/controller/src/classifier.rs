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
//! Hysteresis: a link's class only changes once a new class has been observed
//! for `hysteresis_passes` consecutive classifier passes. This stops a single
//! noisy sample — or a metric wobbling across a threshold — from flipping the
//! class and re-routing OSPF cost. The first classification of a link applies
//! immediately.

use crate::model::{LinkKey, ProbeState, Quality, QualityRecord};
use crate::quality;
use crate::state::AppState;
use crate::store::WindowSpec;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-link last-derived policy, used to hold a cost while a link is busy and
/// as the reference class for hysteresis.
#[derive(Clone, PartialEq)]
struct LastPolicy {
    quality: Quality,
    ospf_cost: u32,
}

/// A candidate class being confirmed across consecutive passes.
#[derive(Clone, Copy)]
struct Pending {
    quality: Quality,
    passes: u32,
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// One classification pass over all links with recent samples.
pub fn classify_once(state: &AppState) {
    let cfg = &state.cfg;
    let spec = WindowSpec {
        always_fresh_secs: cfg.classifier.always_fresh_secs,
        tcp_fresh_secs: cfg.classifier.tcp_fresh_secs,
        tcp_carry_secs: cfg.classifier.tcp_carry_secs,
    };
    let now = now_secs();

    for link in state.tsdb.links() {
        let Some(view) = state.tsdb.window(&link, &spec, now) else {
            continue;
        };

        // Busy/conflict: hold the last derived cost (never reclassify a link
        // in use as degraded). Expire the hold once the sample ages out.
        if (view.state == ProbeState::Busy || view.state == ProbeState::Conflict)
            && now.saturating_sub(view.ts) < cfg.classifier.busy_hold_secs
        {
            // Always write the derived record (so the link stays on the map,
            // state=busy, quality unset if never classified) and stamp it with
            // the held cost when one exists.
            let (held_quality, held_cost) = {
                let guard = state.classifier.last.read().expect("classifier lock");
                guard.get(&link).map_or((None, None), |last| {
                    (Some(last.quality), Some(last.ospf_cost))
                })
            };
            write_derived(state, &link, &view, held_quality, held_cost);
            continue;
        }

        // Fresh data: classify from the window, then apply hysteresis — a new
        // class only takes effect after `hysteresis_passes` consecutive passes
        // agree. The first classification of a link applies immediately.
        let candidate = quality::classify_window(&cfg.quality, &view);

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
            // Candidate differs: require hysteresis_passes consecutive passes.
            Some(_) => {
                let mut pending = state.classifier.pending.write().expect("classifier lock");
                let entry = pending.entry(link.clone()).or_insert(Pending {
                    quality: candidate,
                    passes: 0,
                });
                if entry.quality == candidate {
                    entry.passes += 1;
                    if entry.passes >= cfg.classifier.hysteresis_passes.max(1) {
                        pending.remove(&link);
                        candidate
                    } else {
                        current.unwrap().quality
                    }
                } else {
                    // A different candidate interrupted the confirmation.
                    *entry = Pending {
                        quality: candidate,
                        passes: 1,
                    };
                    current.unwrap().quality
                }
            }
        };
        drop(held);

        let effective_cost = quality::cost_for_quality(&cfg.quality, effective);
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
        write_derived(state, &link, &view, Some(effective), Some(effective_cost));
    }
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
        udp_mbps: None,
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

    fn state_with_hyst(fresh: u64, carry: u64, hysteresis_passes: u32) -> AppState {
        let cfg = Config {
            classifier: Classifier {
                interval_secs: 5,
                always_fresh_secs: 60,
                tcp_fresh_secs: fresh,
                tcp_carry_secs: carry,
                busy_hold_secs: 120,
                hysteresis_passes,
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
        let base = crate::tsdb::now_secs();
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
        let base = crate::tsdb::now_secs();
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
        // Beyond the carry horizon: window() yields None, KV stays empty.
        let old_ts = crate::tsdb::now_secs() - 7200;
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
    fn busy_from_start_link_stays_on_map_unclassified() {
        let state = state_with(1200, 3600);
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        let base = crate::tsdb::now_secs();
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
        let mut ts = crate::tsdb::now_secs();

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
}
