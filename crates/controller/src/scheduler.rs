//! Controller scheduler: decides what each agent should do on each tick.
//!
//! The controller is the responsible party for whole-mesh coordination. On
//! every tick it issues, per owned link:
//!   - always-on probe work on a fixed cadence;
//!   - gated throughput probe work only when the link's fence lease is free
//!     (acquiring the lease here, before dispatch) and the agent holds no
//!     other throughput lease — it runs one iperf3 test at a time, so a
//!     second command in the same tick would only come back `busy`;
//!   - `apply_cost` when the derived cost differs from what the agent last was
//!     told to apply (or a failed apply is due for a retry).
//!
//! Agents that stopped polling are evicted after `AGENT_IDLE_SECS`, so their
//! links stop being scheduled and traces no longer wait on them.

use crate::clock::now_secs;
use crate::state::AppState;
use crate::worker::{Tier, WorkCmd};
use tracing::info;

/// Sanitized default cadences (seconds).
const ALWAYS_INTERVAL: u64 = 15;
const THROUGHPUT_INTERVAL: u64 = 300;
const FENCE_TTL_SECS: u64 = 120;
/// An agent long-polls every ~30 s; ten missed polls means it is gone.
const AGENT_IDLE_SECS: u64 = 300;

pub async fn scheduler_loop(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tick.tick().await;
        plan_tick(&state);
    }
}

fn plan_tick(state: &AppState) {
    let now = now_secs();

    for agent in state.workers.evict_idle(now, AGENT_IDLE_SECS) {
        info!(agent, "agent evicted: no poll for {AGENT_IDLE_SECS}s");
    }

    // Seeding for fresh agents is handled in register; here we only queue
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
            let traceparent = span.in_scope(crate::tracectx::current_traceparent);
            let cmd = WorkCmd::Probe {
                id: cmd_id(),
                tier: Tier::Always,
                token: None,
                link: link.clone(),
                traceparent,
            };
            // push() dedups per (link, tier); stamp issued only when queued.
            if state.workers.push(&agent, cmd) {
                state.workers.mark_issued(&agent, &link, Tier::Always, now);
            }
        }

        // Throughput tier (intrusive) — only when the fence is free and the
        // agent is not already running a test on another of its links. A
        // skipped link is not stamped, so it stays due and is picked up on
        // the tick after that lease is released.
        if state
            .workers
            .probe_due(&agent, &link, Tier::Throughput, now, THROUGHPUT_INTERVAL)
            && !state.fence.held_by(&agent, now)
        {
            if let Ok(lease) = state.fence.acquire(&agent, &link_id, FENCE_TTL_SECS) {
                let span = tracing::info_span!("agent.command", agent=%agent, tier="throughput", link=%link_id);
                let traceparent = span.in_scope(crate::tracectx::current_traceparent);
                let cmd = WorkCmd::Probe {
                    id: cmd_id(),
                    tier: Tier::Throughput,
                    token: Some(lease.token.clone()),
                    link: link.clone(),
                    traceparent,
                };
                if state.workers.push(&agent, cmd) {
                    state
                        .workers
                        .mark_issued(&agent, &link, Tier::Throughput, now);
                } else {
                    // A throughput command is still queued (agent away): give
                    // the lease back instead of letting it sit until its TTL.
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
            let traceparent = span.in_scope(crate::tracectx::current_traceparent);
            let cmd = WorkCmd::ApplyCost {
                id: cmd_id(),
                link: link.clone(),
                cost,
                traceparent,
            };
            if state.workers.push(&agent, cmd) {
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

    #[test]
    fn one_throughput_probe_per_agent_at_a_time() {
        let state = AppState::new(Config::default());
        let links: Vec<LinkKey> = ["awg_a", "awg_b", "awg_c"]
            .iter()
            .map(|i| LinkKey::new("hub-a", "hub-b", *i))
            .collect();
        state.workers.register("hub-a", links.clone(), None);

        // First tick: every link gets its always probe, one gets the test.
        plan_tick(&state);
        let cmds = state.workers.drain("hub-a").unwrap_or_default();
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
        let cmds = state.workers.drain("hub-a").unwrap_or_default();
        assert!(throughput_links(&cmds).is_empty(), "{cmds:?}");

        // The reply releases the lease: the next link is picked up.
        let lease = state.fence.leases().pop().unwrap();
        assert!(state.fence.release(&lease.link, &lease.token));
        plan_tick(&state);
        let second = throughput_links(&state.workers.drain("hub-a").unwrap_or_default());
        assert_eq!(second.len(), 1);
        assert_ne!(second, first);
    }
}
