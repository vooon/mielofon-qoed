//! Mielofon agent entrypoint.
//!
//! The agent is a thin executor: it registers its managed links with the
//! controller, polls the work queue, runs whatever probe it is told to run,
//! reports raw measurements, and applies the OSPF cost it is told to apply.
//! It holds no scheduling, policy, or classification logic.

use mielofon_agent::config::Config;
use mielofon_agent::cost;
use mielofon_agent::model::{
    ApplyAck, LinkKey, QualityReport, RegisterReq, WorkCmd, WorkReq, WorkResp,
};
use mielofon_agent::probes::{self, AlwaysProbe};
use serde::de::DeserializeOwned;
use std::time::Duration;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/etc/mielofon/mielofon-agent.toml".into());
    let cfg = Config::load(&path)?;

    let _guard = mielofon_otel::install(&cfg.otel, "mielofon-agent", env!("CARGO_PKG_VERSION"))?;
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let client =
        mielofon_agent::client::Client::new(&cfg.controller, cfg.controller_port, &cfg.tls)?;

    // Register managed links; the controller may seed the current cost policy.
    let links: Vec<LinkKey> = cfg
        .links
        .iter()
        .map(|l| LinkKey {
            from: l.from.clone(),
            to: l.to.clone(),
            interface: l.interface.clone(),
        })
        .collect();

    let reg: serde_json::Value = post(
        &client,
        "/v1/agent/register",
        &RegisterReq {
            agent: cfg.agent.clone(),
            links: links.clone(),
        },
    )
    .await?;
    // Handle the policy snapshot (apply_cost commands) returned on register.
    if let Some(cmds) = reg["commands"].as_array() {
        for c in cmds {
            let cmd: WorkCmd = serde_json::from_value(c.clone())?;
            handle_command(&cfg, &client, &cmd).await;
        }
    }

    info!(agent = %cfg.agent, links = links.len(), "registered");

    let mut tick = tokio::time::interval(Duration::from_secs(cfg.loop_.poll_secs.max(1)));
    loop {
        tick.tick().await;
        let resp: WorkResp = match post(
            &client,
            "/v1/agent/work",
            &WorkReq {
                agent: cfg.agent.clone(),
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("work poll failed: {e}");
                continue;
            }
        };
        for cmd in resp.commands {
            handle_command(&cfg, &client, &cmd).await;
        }
    }
}

async fn handle_command(cfg: &Config, client: &mielofon_agent::client::Client, cmd: &WorkCmd) {
    let result = match cmd {
        WorkCmd::Probe {
            tier, link, token, ..
        } => match tier.as_str() {
            "always" => run_always_and_report(cfg, client, link, token).await,
            "throughput" => run_throughput_and_report(cfg, client, link, token).await,
            other => {
                warn!(tier = other, "unknown probe tier");
                return;
            }
        },
        WorkCmd::ApplyCost { link, cost, .. } => {
            let lc = match cfg.link_by_interface(&link.interface) {
                Some(l) => l,
                None => {
                    warn!(iface = %link.interface, "apply_cost for unknown link");
                    return;
                }
            };
            Ok(match cost::apply_cost(lc, *cost).await {
                Ok(()) => {
                    ack_cost(client, &cfg.agent, link, *cost).await;
                    info!(iface = %link.interface, cost, "applied OSPF cost");
                }
                Err(e) => error!(iface = %link.interface, cost, "apply_cost failed: {e}"),
            })
        }
    };
    if let Err(e) = result {
        error!("command failed: {e}");
    }
}

async fn run_always_and_report(
    cfg: &Config,
    client: &mielofon_agent::client::Client,
    link: &LinkKey,
    token: &Option<String>,
) -> anyhow::Result<()> {
    let lc = cfg
        .link_by_interface(&link.interface)
        .ok_or_else(|| anyhow::anyhow!("unknown link {}", link.interface))?;
    let ap: AlwaysProbe = probes::run_always(&cfg.probes, lc).await?;
    let report = QualityReport {
        link: link.clone(),
        rtt_ms: ap.rtt_ms,
        loss_pct: ap.loss_pct,
        rr_tps: ap.rr_tps,
        tcp_mbps: None,
        udp_mbps: None,
        util_mbps: 0.0,
        state: mielofon_agent::model::ProbeState::Quiet,
        token: token.clone(),
    };
    let _: serde_json::Value = post(client, "/v1/quality", &report).await?;
    Ok(())
}

async fn run_throughput_and_report(
    cfg: &Config,
    client: &mielofon_agent::client::Client,
    link: &LinkKey,
    token: &Option<String>,
) -> anyhow::Result<()> {
    let lc = cfg
        .link_by_interface(&link.interface)
        .ok_or_else(|| anyhow::anyhow!("unknown link {}", link.interface))?;
    let tp = probes::run_throughput(&cfg.probes, lc).await?;
    let state = if probes::is_busy(&tp) {
        warn!(iface = %link.interface, util = tp.util_mbps, "link busy, throughput skipped");
        mielofon_agent::model::ProbeState::Busy
    } else {
        mielofon_agent::model::ProbeState::Quiet
    };
    let report = QualityReport {
        link: link.clone(),
        rtt_ms: f64::NAN,
        loss_pct: f64::NAN,
        rr_tps: f64::NAN,
        tcp_mbps: tp.tcp_mbps,
        udp_mbps: None,
        util_mbps: tp.util_mbps,
        state,
        token: token.clone(),
    };
    let _: serde_json::Value = post(client, "/v1/quality", &report).await?;
    Ok(())
}

async fn ack_cost(client: &mielofon_agent::client::Client, agent: &str, link: &LinkKey, cost: u32) {
    let _: anyhow::Result<serde_json::Value> = post(
        client,
        "/v1/apply/ack",
        &ApplyAck {
            agent: agent.to_string(),
            link: link.clone(),
            cost,
        },
    )
    .await;
}

async fn post<T: serde::Serialize, R: DeserializeOwned>(
    client: &mielofon_agent::client::Client,
    path: &str,
    body: &T,
) -> anyhow::Result<R> {
    let raw = serde_json::to_vec(body)?;
    let text = client.post_json(path, &raw).await?;
    tracing::trace!(path, %text, "response");
    Ok(serde_json::from_str(&text)?)
}
