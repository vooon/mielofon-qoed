//! mielofon-agent — node agent of the mielofon mesh.
//!
//! Holds one mTLS QUIC session with a controller (`session.rs`): registers
//! the node's auto-discovered links, then answers every command stream the
//! controller opens (probe harvest, gated throughput test, OSPF cost apply,
//! trace lookup) on that same stream. Spokes sit behind NAT, so the agent
//! always dials out; the controller never does.

mod agent;
mod bird;
mod bus;
mod config;
mod discover;
mod icmp;
mod metrics;
mod session;
mod throughput;
mod tls;
mod window;

use agent::Agent;
use anyhow::{Context, Result};
use bus::UbusCli;
use config::Config;
use mielofon_proto::LinkKey;
use mielofon_transport::Dialer;
use std::sync::{Arc, Mutex};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// Map the UCI syslog-style level onto a tracing filter. `debug` applies to
/// the agent's own code; dependencies stay at `info` so the log is not
/// flooded with connection-pool chatter.
fn log_filter(level: &str) -> String {
    match level {
        "debug" => "info,mielofon_agent=debug,mielofon_transport=debug".into(),
        "info" | "notice" => "info".into(),
        "warning" | "warn" => "warn".into(),
        "err" | "error" | "crit" => "error".into(),
        _ => "info".into(),
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

pub struct Runtime {
    pub agent: Agent<UbusCli>,
    pub dialer: Dialer,
    /// Signalled by the refresher when the link set changed: the session
    /// re-registers.
    pub links_changed: Notify,
    /// The link set the controller last acknowledged.
    pub registered: Mutex<Vec<LinkKey>>,
}

impl Runtime {
    /// Periodic re-discovery; re-register when the link set changed.
    async fn refresher(self: Arc<Self>) {
        let mut tick = tokio::time::interval(self.agent.cfg.refresh_interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            self.agent.refresh().await;
            let now = self.agent.link_keys();
            if *self.registered.lock().expect("registered lock") != now {
                self.links_changed.notify_one();
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
        "mielofon-agent {} starting as {} (controller endpoints: {})",
        env!("CARGO_PKG_VERSION"),
        cfg.agent,
        cfg.endpoints
            .iter()
            .map(|e| format!("{}:{}", e.host, e.port))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let tls = tls::client_config(&cfg.cacert, &cfg.cert, &cfg.key)?;
    let dialer = Dialer::new(tls).context("QUIC dialer")?;
    let rt = Arc::new(Runtime {
        agent: Agent::new(cfg, bus),
        dialer,
        links_changed: Notify::new(),
        registered: Mutex::new(Vec::new()),
    });

    tokio::spawn(rt.clone().refresher());
    if rt.agent.cfg.prometheus_textfile.is_some() {
        tokio::spawn(rt.clone().metrics_writer());
    }

    let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    tokio::select! {
        _ = session::run(rt.clone()) => {}
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
