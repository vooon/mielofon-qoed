//! OSPF cost application hook. The agent only ever executes the cost it is
//! told to apply (from an `apply_cost` command). If the link has no
//! `cost_command`, the default is to drive BIRD directly via `birdc`.

use crate::config::LinkConfig;
use anyhow::Context;
use tokio::process::Command;

/// Apply `cost` to `link`'s interface. Returns Ok when applied (or explicitly
/// a no-op because no command is configured).
pub async fn apply_cost(l: &LinkConfig, cost: u32) -> anyhow::Result<()> {
    match &l.cost_command {
        Some(tmpl) => {
            let script = tmpl
                .replace("{interface}", &l.interface)
                .replace("{cost}", &cost.to_string());
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .output()
                .await
                .context("run cost command")?;
            if !out.status.success() {
                anyhow::bail!(
                    "cost command failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }
        // Default: drive BIRD through its control socket (birdc). The router
        // integration layer may override with cost_command; without birdc this
        // is a no-op so the agent stays functional on plain hosts.
        None => birdc_set_cost(&l.interface, cost).await,
    }
}

/// Best-effort `birdc configure` via a generated per-interface cost stanza.
/// This is the integration seam the private router config wires up.
async fn birdc_set_cost(interface: &str, cost: u32) -> anyhow::Result<()> {
    let cfg = format!("protocol ospf mesh {{ interface \"{interface}\" {{ cost {cost}; }}; }}");
    let out = Command::new("birdc")
        .args(["configure", &cfg])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(_) => {
            // birdc not present or BIRD not configured — no-op.
            tracing::warn!(interface, cost, "birdc unavailable; cost not applied");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(interface, cost, "birdc failed: {e}");
            Ok(())
        }
    }
}
