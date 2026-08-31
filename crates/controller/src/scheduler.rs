//! Controller scheduler: decides what each agent should do on each tick.
//!
//! The controller is the responsible party for whole-mesh coordination. On
//! every tick it issues, per owned link:
//!   - always-on probe work on a fixed cadence;
//!   - gated throughput probe work only when the link's fence lease is free
//!     (acquiring the lease here, before dispatch);
//!   - `apply_cost` when the derived cost differs from what the agent last was
//!     told to apply.

use crate::state::AppState;
use crate::worker::{Tier, WorkCmd};
use std::time::{SystemTime, UNIX_EPOCH};

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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn plan_tick(state: &AppState) {
    let now = now_secs();

    // 1. Seeding for fresh agents is handled in register; here we only queue
    //    work for links owned by registered agents.
    let owned = state.workers.owned_links();

    for (agent, link) in owned {
        let link_id = link.id();

        // Always-on tier (cheap, congestion-immune) — never gated.
        if state
            .workers
            .always_due(&agent, &link, now, ALWAYS_INTERVAL)
        {
            let cmd = WorkCmd::Probe {
                id: cmd_id(),
                tier: Tier::Always,
                token: None,
                link: link.clone(),
            };
            // push() dedups per (link, tier); stamp issued only when queued.
            if state.workers.push(&agent, cmd) {
                state.workers.mark_issued(&agent, &link, Tier::Always, now);
            }
        }

        // Throughput tier (intrusive) — only when the fence is free.
        if state
            .workers
            .throughput_due(&agent, &link, now, THROUGHPUT_INTERVAL)
        {
            if let Ok(lease) = state.fence.acquire(&agent, &link_id, FENCE_TTL_SECS) {
                let cmd = WorkCmd::Probe {
                    id: cmd_id(),
                    tier: Tier::Throughput,
                    token: Some(lease.token),
                    link: link.clone(),
                };
                if state.workers.push(&agent, cmd) {
                    state
                        .workers
                        .mark_issued(&agent, &link, Tier::Throughput, now);
                }
            }
        }

        // Policy: apply the derived cost when it differs from what was sent.
        if let Some(rec) = state.kv.get(&link) {
            if let Some(cost) = rec.ospf_cost {
                if state.workers.applied_sent(&agent, &link) != Some(cost) {
                    let cmd = WorkCmd::ApplyCost {
                        id: cmd_id(),
                        link: link.clone(),
                        cost,
                    };
                    state.workers.push(&agent, cmd);
                    state.workers.set_applied_sent(&agent, &link, cost);
                }
            }
        }
    }
}

/// Fresh, globally-unique command id per dispatch (like an `X-Request-Id`),
/// so command/reply correlation is unambiguous even across re-dispatch.
fn cmd_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
