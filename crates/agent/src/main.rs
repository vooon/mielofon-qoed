//! mielofon-agent — node agent of the mielofon mesh.
//!
//! Registers the node's auto-discovered links with the controller, long-polls
//! `POST /v1/agent/command`, executes each command (probe harvest, gated
//! throughput test, OSPF cost apply, trace lookup) and answers on
//! `POST /v1/agent/reply`, echoing the command's job id. Spokes sit behind
//! NAT, so the agent always connects out; the controller never does.

mod agent;
mod bird;
mod bus;
mod client;
mod config;
mod discover;
mod icmp;
mod metrics;
mod throughput;
mod window;

use agent::Agent;
use anyhow::{Context, Result};
use bus::UbusCli;
use client::Controller;
use config::Config;
use mielofon_proto::{CommandReq, LinkKey, RegisterResp, WorkCmd, WorkResp};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{debug, error, info, warn};

/// Re-discovery cadence: tunnels and BGP sessions come up seconds apart at
/// boot, so the first register may see a partial link set.
const REFRESH: Duration = Duration::from_secs(30);
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Consecutive failed long-polls after which the agent registers again: the
/// controller may have restarted (and forgotten us) without ever answering.
const POLL_FAILURES_BEFORE_REREGISTER: u32 = 3;

/// Map the UCI syslog-style level onto a tracing filter.
fn log_filter(level: &str) -> &'static str {
    match level {
        "debug" => "debug",
        "info" | "notice" => "info",
        "warning" | "warn" => "warn",
        "err" | "error" | "crit" => "error",
        _ => "info",
    }
}

fn init_logging(level: &str) {
    // procd forwards stderr to syslog, which adds its own timestamps.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_filter(level)));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .without_time()
        .with_target(false)
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();
}

struct Runtime {
    agent: Agent<UbusCli>,
    ctl: Controller,
    need_register: AtomicBool,
    registered: Mutex<Vec<LinkKey>>,
}

impl Runtime {
    fn dispatch(self: &Arc<Self>, cmds: Vec<WorkCmd>) {
        for cmd in cmds {
            let rt = self.clone();
            tokio::spawn(async move {
                let Some(reply) = rt.agent.execute(cmd).await else {
                    return;
                };
                if let Err(e) = rt
                    .ctl
                    .post::<_, serde_json::Value>("/v1/agent/reply", &reply, REPLY_TIMEOUT)
                    .await
                {
                    warn!("reply: {e:#}");
                }
            });
        }
    }

    async fn register(self: &Arc<Self>) -> Result<()> {
        self.agent.refresh().await;
        let req = self.agent.register_req();
        info!("registering {} with {} links", req.agent, req.links.len());
        let resp: RegisterResp = self
            .ctl
            .post("/v1/agent/register", &req, REPLY_TIMEOUT)
            .await?;
        *self.registered.lock().expect("registered lock") = req.links;
        self.dispatch(resp.commands);
        Ok(())
    }

    /// Register when needed, then long-poll; forever. A `404` from the
    /// command endpoint means the controller does not know us (it restarted
    /// or evicted us): register again before the next poll. Repeated
    /// transport failures also trigger a re-register, in case the controller
    /// came back without ever answering a poll.
    async fn pump(self: Arc<Self>) {
        let cfg = &self.agent.cfg;
        let poll = CommandReq {
            agent: cfg.agent.clone(),
            timeout_ms: cfg.command_timeout.as_millis() as u64,
        };
        let mut failures = 0u32;
        loop {
            if self.need_register.swap(false, Ordering::AcqRel) {
                if let Err(e) = self.register().await {
                    warn!("register failed: {e:#}; retrying");
                    self.need_register.store(true, Ordering::Release);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            }
            match self
                .ctl
                .post::<_, WorkResp>(
                    "/v1/agent/command",
                    &poll,
                    cfg.command_timeout + Duration::from_secs(10),
                )
                .await
            {
                Ok(r) => {
                    failures = 0;
                    debug!("{} command(s)", r.commands.len());
                    self.dispatch(r.commands);
                }
                Err(e)
                    if e.downcast_ref::<client::HttpError>()
                        .is_some_and(|h| h.status == 404) =>
                {
                    info!("controller does not know us; registering again");
                    self.need_register.store(true, Ordering::Release);
                }
                Err(e) => {
                    failures += 1;
                    warn!("command long-poll failed: {e:#}; retrying");
                    if failures >= POLL_FAILURES_BEFORE_REREGISTER {
                        failures = 0;
                        self.need_register.store(true, Ordering::Release);
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    /// Periodic re-discovery; re-register when the link set changed.
    async fn refresher(self: Arc<Self>) {
        let mut tick = tokio::time::interval(REFRESH);
        tick.tick().await;
        loop {
            tick.tick().await;
            self.agent.refresh().await;
            let now = self.agent.link_keys();
            if *self.registered.lock().expect("registered lock") != now {
                self.need_register.store(true, Ordering::Release);
            }
        }
    }

    fn write_metrics(&self) {
        let Some(path) = &self.agent.cfg.prometheus_textfile else {
            return;
        };
        if let Err(e) = metrics::write_textfile(path, &self.agent.render_metrics()) {
            warn!("metrics textfile {path}: {e}");
        }
    }

    async fn metrics_writer(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.agent.cfg.prometheus_interval);
        loop {
            tick.tick().await;
            self.write_metrics();
        }
    }
}

async fn run() -> Result<()> {
    let bus = UbusCli::default();
    let cfg = Config::load(&bus).await?;
    init_logging(&cfg.log_level);
    info!(
        "mielofon-agent {} starting as {} (controller {})",
        env!("CARGO_PKG_VERSION"),
        cfg.agent,
        cfg.controller_url
    );

    let tls = client::tls_config(&cfg.cacert, &cfg.cert, &cfg.key)?;
    let ctl = Controller::new(&cfg.controller_url, tls)?;
    let rt = Arc::new(Runtime {
        agent: Agent::new(cfg, bus),
        ctl,
        need_register: AtomicBool::new(true),
        registered: Mutex::new(Vec::new()),
    });

    tokio::spawn(rt.clone().refresher());
    if rt.agent.cfg.prometheus_textfile.is_some() {
        tokio::spawn(rt.clone().metrics_writer());
    }

    let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    tokio::select! {
        _ = rt.clone().pump() => {}
        _ = term.recv() => info!("SIGTERM, exiting"),
        _ = int.recv() => info!("SIGINT, exiting"),
    }
    rt.write_metrics();
    Ok(())
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("mielofon-agent {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = runtime.block_on(run()) {
        // Logging may not be up yet (config load failed): stderr reaches
        // syslog through procd either way.
        error!("{e:#}");
        eprintln!("mielofon-agent: {e:#}");
        std::process::exit(1);
    }
}
