//! Controller scheduler: decides what each agent should do on each tick.
//!
//! The controller is the responsible party for whole-mesh coordination. On
//! every tick it issues, per owned link:
//!   - always-on probe work on a fixed cadence;
//!   - gated throughput probe work only when both endpoints of the link are
//!     free of other tests (acquiring the fence lease here, before dispatch):
//!     a node runs one `iperf3 -c` and serves one `iperf3 -s` at a time, so
//!     tests on disjoint node pairs run concurrently and the rest wait;
//!   - `apply_cost` when the derived cost differs from what the agent last was
//!     told to apply (or a failed apply is due for a retry).
//!
//! Every command is a stream opened on the agent's session
//! (`session::dispatch`); an agent whose session closed is gone from the
//! registry at once, so its links stop being scheduled.

use crate::clock::now_secs;
use crate::session::dispatch;
use crate::state::AppState;
use crate::worker::{Tier, WorkCmd};

/// Sanitized default cadences (seconds).
const ALWAYS_INTERVAL: u64 = 15;
const THROUGHPUT_INTERVAL: u64 = 300;
const FENCE_TTL_SECS: u64 = 120;

pub async fn scheduler_loop(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        plan_tick(&state);
    }
}

fn plan_tick(state: &AppState) {
    let now = now_secs();

    // Seeding for fresh agents is handled in register; here we only issue
    // work for links owned by registered agents.
    for (agent, link) in state.workers.owned_links() {
        let link_id = link.id();

        // Always-on tier (cheap, congestion-immune) — never gated.
        if state
            .workers
            .probe_due(&agent, &link, Tier::Always, now, ALWAYS_INTERVAL)
        {
            let span =
                tracing::info_span!("agent.command", agent=%agent, tier="always", link=%link_id);
            let traceparent = span.in_scope(mielofon_otel::tracectx::current_traceparent);
            let cmd = WorkCmd::Probe {
                id: cmd_id(),
                tier: Tier::Always,
                token: None,
                link: link.clone(),
                traceparent,
            };
            // dispatch() refuses while one is outstanding; stamp only when
            // the command actually went out.
            if dispatch(state, &agent, cmd) {
                state.workers.mark_issued(&agent, &link, Tier::Always, now);
            }
        }

        // Throughput tier (intrusive) — only when neither endpoint is in
        // another test (the fence is node-level). A link that cannot get the
        // lease is not stamped, so it stays due and is picked up on the tick
        // after the blocking lease is released.
        if state
            .workers
            .probe_due(&agent, &link, Tier::Throughput, now, THROUGHPUT_INTERVAL)
        {
            if let Ok(lease) = state.fence.acquire(&agent, &link, FENCE_TTL_SECS) {
                let span = tracing::info_span!("agent.command", agent=%agent, tier="throughput", link=%link_id);
                let traceparent = span.in_scope(mielofon_otel::tracectx::current_traceparent);
                let cmd = WorkCmd::Probe {
                    id: cmd_id(),
                    tier: Tier::Throughput,
                    token: Some(lease.token.clone()),
                    link: link.clone(),
                    traceparent,
                };
                if dispatch(state, &agent, cmd) {
                    state
                        .workers
                        .mark_issued(&agent, &link, Tier::Throughput, now);
                } else {
                    // A throughput command is still outstanding: give the
                    // lease back instead of letting it sit until its TTL.
                    state.fence.release(&link_id, &lease.token);
                }
            }
        }

        // Policy: apply the derived cost when it differs from what was sent.
        let Some(cost) = state.kv.get(&link).and_then(|rec| rec.ospf_cost) else {
            continue;
        };
        if state.workers.apply_due(&agent, &link, cost, now) {
            let span =
                tracing::info_span!("agent.command", agent=%agent, kind="apply", link=%link_id);
            let traceparent = span.in_scope(mielofon_otel::tracectx::current_traceparent);
            let cmd = WorkCmd::ApplyCost {
                id: cmd_id(),
                link: link.clone(),
                cost,
                traceparent,
            };
            if dispatch(state, &agent, cmd) {
                state.workers.set_applied_sent(&agent, &link, cost);
            }
        }
    }
}

/// Fresh, globally-unique command id per dispatch (like an `X-Request-Id`),
/// so command/reply correlation is unambiguous even across re-dispatch.
pub fn cmd_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::session::testing::{connect, settle, Answer};
    use mielofon_proto::LinkKey;

    fn throughput_links(cmds: &[WorkCmd]) -> Vec<String> {
        cmds.iter()
            .filter_map(|c| match c {
                WorkCmd::Probe {
                    tier: Tier::Throughput,
                    link,
                    ..
                } => Some(link.interface.clone()),
                _ => None,
            })
            .collect()
    }

    /// Always probes are answered silently (dropped), throughput tests hang
    /// so their lease stays held until the test releases it.
    fn hold_throughput(cmd: &WorkCmd) -> Answer {
        match cmd {
            WorkCmd::Probe {
                tier: Tier::Throughput,
                ..
            } => Answer::Hang,
            _ => Answer::Silent,
        }
    }

    #[tokio::test]
    async fn one_throughput_probe_per_agent_at_a_time() {
        let state = AppState::new(Config::default());
        let links: Vec<LinkKey> = ["awg_a", "awg_b", "awg_c"]
            .iter()
            .map(|i| LinkKey::new("hub-a", "hub-b", *i))
            .collect();
        let fake = connect(&state, "hub-a", links.clone(), None, hold_throughput);

        // First tick: every link gets its always probe, one gets the test.
        plan_tick(&state);
        settle().await;
        let cmds = fake.seen();
        let always = cmds
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    WorkCmd::Probe {
                        tier: Tier::Always,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(always, 3);
        let first = throughput_links(&cmds);
        assert_eq!(first.len(), 1, "{cmds:?}");

        // While that lease is held nothing else is issued for the agent.
        plan_tick(&state);
        settle().await;
        assert_eq!(throughput_links(&fake.seen()).len(), 1);

        // The reply releases the lease: the next link is picked up.
        let lease = state.fence.leases().pop().unwrap();
        assert!(state.fence.release(&lease.link, &lease.token));
        plan_tick(&state);
        settle().await;
        let all = throughput_links(&fake.seen());
        assert_eq!(all.len(), 2);
        assert_ne!(all[1], all[0]);
    }

    #[tokio::test]
    async fn tests_run_concurrently_only_on_disjoint_node_pairs() {
        let state = AppState::new(Config::default());
        // Four hubs each own one link; two pairs are disjoint, the spoke's two
        // links both end on hubs already in a test.
        for (agent, to) in [("hub-a", "hub-b"), ("hub-c", "hub-d")] {
            connect(
                &state,
                agent,
                vec![LinkKey::new(agent, to, format!("awg_int_{to}"))],
                None,
                hold_throughput,
            );
        }
        connect(
            &state,
            "node-0",
            vec![
                LinkKey::new("node-0", "hub-b", "awg_hub_b"),
                LinkKey::new("node-0", "hub-d", "awg_hub_d"),
            ],
            None,
            hold_throughput,
        );
        // Order of issue depends on registry iteration; what matters is the
        // invariant: no node in two tests, and every free pair is used.
        plan_tick(&state);
        settle().await;
        let leases = state.fence.leases();
        let mut nodes: Vec<String> = leases
            .iter()
            .flat_map(|l| [l.from.clone(), l.to.clone()])
            .collect();
        let n = nodes.len();
        nodes.sort();
        nodes.dedup();
        assert_eq!(nodes.len(), n, "a node is in two tests: {leases:?}");
        assert_eq!(leases.len(), 2, "{leases:?}");
        // Whatever was issued, nothing else fits until something is released.
        plan_tick(&state);
        settle().await;
        assert_eq!(state.fence.leases().len(), 2);
    }

    #[tokio::test]
    async fn applies_derived_cost_once_until_it_changes() {
        let state = AppState::new(Config::default());
        let link = LinkKey::new("node-0", "hub-a", "awg_hub_a");
        let fake = connect(
            &state,
            "node-0",
            vec![link.clone()],
            None,
            |cmd| match cmd {
                WorkCmd::ApplyCost { id, link, cost, .. } => {
                    Answer::Reply(mielofon_proto::AgentReply::Applied {
                        agent: "node-0".into(),
                        id: id.clone(),
                        link: link.clone(),
                        cost: *cost,
                        note: None,
                        traceparent: None,
                    })
                }
                _ => Answer::Silent,
            },
        );
        let mut rec = crate::model::QualityRecord::new(
            Some(10.0),
            Some(0.0),
            Some(1.0),
            None,
            None,
            0.0,
            crate::model::ProbeState::Quiet,
        );
        rec.ospf_cost = Some(20);
        state.kv.put(link.clone(), rec.clone());
        plan_tick(&state);
        settle().await;
        plan_tick(&state);
        settle().await;
        let applies = fake
            .seen()
            .into_iter()
            .filter(|c| matches!(c, WorkCmd::ApplyCost { cost: 20, .. }))
            .count();
        assert_eq!(applies, 1, "sent once, acked, not re-sent");
        rec.ospf_cost = Some(50);
        rec.ts += 1;
        state.kv.put(link.clone(), rec);
        plan_tick(&state);
        settle().await;
        assert!(fake
            .seen()
            .iter()
            .any(|c| matches!(c, WorkCmd::ApplyCost { cost: 50, .. })));
    }
}
