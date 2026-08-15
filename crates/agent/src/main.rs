//! Mielofon agent entrypoint. Placeholder until Stage 2 wiring completes.

use mielofon_otel::OTelConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = OTelConfig::default();
    let guard = mielofon_otel::install(&cfg, "mielofon-agent", env!("CARGO_PKG_VERSION"))?;
    tracing::info!("mielofon-agent starting (skeleton)");
    tokio::signal::ctrl_c().await?;
    guard.shutdown();
    Ok(())
}
