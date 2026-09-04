//! Mielofon controller entrypoint. Runs three listeners:
//!   members (9551, mTLS) — cluster gossip
//!   clients (9552, mTLS) — agents (quality reports + fence)
//!   admin  (9553, loopback) — dashboard, /metrics, /healthz, /readyz, reads

use axum_server::tls_rustls::RustlsConfig;
use std::net::TcpListener;
use std::sync::Arc;
use tracing::info;

use mielofon_controller::api;
use mielofon_controller::config::Config;
use mielofon_controller::state::AppState;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "mielofon-controller",
    about = "Distributed QoE link-quality coordination controller",
    version
)]
#[command(subcommand_required = true, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the controller daemon (the long-lived service).
    Daemon(DaemonArgs),
    /// mTLS certificate generation (openssl-based; runs on the operator host).
    #[command(subcommand)]
    Cert(mielofon_controller::cert::CertCli),
}

#[derive(clap::Args)]
struct DaemonArgs {
    /// Path to the controller TOML config.
    #[arg(default_value = "/etc/mielofon/mielofon-controller.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon(args) => run_daemon(&args.config).await,
        Command::Cert(args) => mielofon_controller::cert::run(args),
    }
}

async fn run_daemon(path: &str) -> anyhow::Result<()> {
    let cfg = Config::load(path)?;

    // Select the ring CryptoProvider as the process default so rustls doesn't
    // fail on provider ambiguity (multiple deps pull different rustls features).
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let _guard = mielofon_otel::install(
        &cfg.otel,
        &cfg.log.level,
        cfg.log.format.parse().map_err(anyhow::Error::from)?,
        "mielofon-controller",
        env!("CARGO_PKG_VERSION"),
    )?;

    // TLS. Members+clients demand mTLS pinned to the same CA; gossip reuses the
    // node client identity for outgoing anti-entropy pushes.
    let server_tls = mielofon_controller::tls::server_config(&cfg.tls, &[&cfg.tls.ca])?;
    let client_tls = mielofon_controller::tls::client_config(&cfg.tls, &[&cfg.tls.ca])?;

    let state = AppState::new(cfg.clone());
    state.set_ready(false);

    // Background: gossip anti-entropy pushes + KV expiry pruning.
    let gossip_state = state.clone();
    let gossip_client = client_tls.clone();
    tokio::spawn(mielofon_controller::gossip::gossip_loop(
        gossip_state,
        gossip_client,
    ));

    // Probe/policy scheduler — the controller is the mesh decision-maker and
    // drives registered agents through their work queues.
    let sched_state = state.clone();
    tokio::spawn(mielofon_controller::scheduler::scheduler_loop(sched_state));

    let prune_state = state.clone();
    let grace = state.cfg.cluster.grace_ttl_secs.max(10);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(grace / 2));
        loop {
            tick.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            prune_state.kv.prune(now, grace);
        }
    });

    // Bind admin (plain HTTP on loopback) up front so a port clash is caught.
    let admin_bind = state.cfg.listeners.admin();
    let admin_listener = TcpListener::bind(admin_bind)
        .map_err(|e| anyhow::anyhow!("bind admin {admin_bind}: {e}"))?;
    admin_listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("set admin nonblocking: {e}"))?;
    let admin_srv = axum_server::from_tcp(admin_listener)
        .map_err(|e| anyhow::anyhow!("admin server init: {e}"))?
        .serve(
            api::admin_router()
                .with_state(state.clone())
                .into_make_service(),
        );
    let admin_handle = tokio::spawn(async move {
        let _ = admin_srv.await;
    });

    // Bind cluster + client (mTLS).
    let cluster_handle = tokio::spawn(serve_mtls(
        state.clone(),
        api::cluster_router(),
        server_tls.clone(),
        state.cfg.listeners.cluster(),
    ));
    let client_handle = tokio::spawn(serve_mtls(
        state.clone(),
        api::client_router(),
        server_tls.clone(),
        state.cfg.listeners.client(),
    ));

    state.set_ready(true);
    info!(
        node = %state.cfg.node.name,
        admin = %admin_bind,
        "mielofon-controller up and ready"
    );

    tokio::select! {
        _ = admin_handle => {}
        _ = cluster_handle => {}
        _ = client_handle => {}
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal received");
        }
    }

    state.set_ready(false);
    _guard.shutdown();
    Ok(())
}

async fn serve_mtls(
    state: AppState,
    router: axum::Router<AppState>,
    tls: Arc<rustls::ServerConfig>,
    addr: std::net::SocketAddr,
) {
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            info!(%addr, "mtls listener bind failed: {e}");
            return;
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        info!(%addr, "mtls listener nonblocking failed: {e}");
        return;
    }
    let cfg = RustlsConfig::from_config(tls);
    let server = match axum_server::from_tcp_rustls(listener, cfg) {
        Ok(s) => s,
        Err(e) => {
            info!(%addr, "mtls server init failed: {e}");
            return;
        }
    };
    let server = server.serve(router.with_state(state).into_make_service());
    let _ = server.await;
}
