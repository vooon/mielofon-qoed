//! Mielofon controller entrypoint. Runs three listeners:
//!   cluster (9551/tcp, mTLS HTTP) — gossip between members
//!   client  (9552/udp, mTLS QUIC) — agent sessions (commands and replies)
//!   admin   (9553/tcp, loopback)  — dashboard, /metrics, /healthz, /readyz, reads

use anyhow::Context;
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
    /// Tooling that speaks the agent protocol (e2e harness, diagnostics).
    #[command(subcommand)]
    Tool(mielofon_controller::tool::ToolCli),
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
        Command::Tool(args) => mielofon_controller::tool::run(args).await,
    }
}

async fn run_daemon(path: &str) -> anyhow::Result<()> {
    let cfg = Config::load(path)?;

    // Select the ring CryptoProvider as the process default so rustls doesn't
    // fail on provider ambiguity (multiple deps pull different rustls features).
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let guard = mielofon_otel::install(
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

    // Bind every listener before the background loops start, so a port clash
    // or a bad bind address fails the start instead of a half-running daemon.
    let admin_bind = state.cfg.listeners.admin();
    let admin_listener = bind(admin_bind, "admin")?;
    let cluster_listener = bind(state.cfg.listeners.cluster(), "cluster")?;
    let client_bind = state.cfg.listeners.client();
    let client_listener = mielofon_transport::Listener::bind(server_tls.clone(), client_bind)
        .context("client listener")?;

    // Background: gossip anti-entropy pushes + KV expiry pruning.
    tokio::spawn(mielofon_controller::gossip::gossip_loop(
        state.clone(),
        client_tls.clone(),
    ));

    // Probe/policy scheduler — the controller is the mesh decision-maker and
    // drives registered agents through their work queues.
    tokio::spawn(mielofon_controller::scheduler::scheduler_loop(
        state.clone(),
    ));

    // Policy classifier — derives quality/cost from a window of stored samples
    // (decoupled from measurement ingest).
    tokio::spawn(mielofon_controller::classifier::classifier_loop(
        state.clone(),
    ));

    let prune_state = state.clone();
    let grace = state.cfg.cluster.grace_ttl_secs.max(10);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(grace / 2));
        loop {
            tick.tick().await;
            prune_state
                .kv
                .prune(mielofon_controller::clock::now_secs(), grace);
        }
    });

    // Time-series durability + retention (drains in-memory samples/buckets into
    // the redb file when persisting, and bounds the hot rings).
    tokio::spawn(mielofon_controller::tsdb::flush_loop(state.tsdb.clone()));

    let admin_srv = axum_server::from_tcp(admin_listener)
        .context("admin server init")?
        .serve(
            api::admin_router()
                .with_state(state.clone())
                .into_make_service(),
        );
    let admin_handle = tokio::spawn(async move { admin_srv.await.context("admin listener") });
    let cluster_handle = tokio::spawn(serve_mtls(
        state.clone(),
        api::cluster_router(),
        server_tls.clone(),
        cluster_listener,
        "cluster",
    ));
    let client_handle = tokio::spawn(mielofon_controller::session::serve(
        state.clone(),
        client_listener,
    ));

    state.set_ready(true);
    info!(
        node = %state.cfg.node.name,
        admin = %admin_bind,
        client = %format!("{client_bind}/udp"),
        "mielofon-controller up and ready"
    );

    // A listener task ending is a failure (they never return on their own);
    // a stop signal is a clean shutdown.
    let result = tokio::select! {
        r = admin_handle => listener_exit("admin", r),
        r = cluster_handle => listener_exit("cluster", r),
        r = client_handle => listener_exit("client", r),
        _ = shutdown_signal() => {
            info!("shutdown signal received");
            Ok(())
        }
    };

    state.set_ready(false);
    // procd stops us with SIGTERM: persist what the 30 s flush loop has not
    // written yet and drain the OTEL exporters before exiting.
    state.tsdb.flush();
    guard.shutdown();
    result
}

/// Resolves on SIGTERM (procd/systemd stop) or SIGINT (console).
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("SIGTERM handler: {e}; only SIGINT stops the daemon");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

fn listener_exit(
    name: &str,
    joined: Result<anyhow::Result<()>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match joined {
        Ok(Ok(())) => anyhow::bail!("{name} listener stopped unexpectedly"),
        Ok(Err(e)) => Err(e.context(format!("{name} listener"))),
        Err(e) => anyhow::bail!("{name} listener task: {e}"),
    }
}

fn bind(addr: std::net::SocketAddr, name: &str) -> anyhow::Result<TcpListener> {
    let listener = TcpListener::bind(addr).with_context(|| format!("bind {name} {addr}"))?;
    listener
        .set_nonblocking(true)
        .with_context(|| format!("set {name} listener nonblocking"))?;
    Ok(listener)
}

async fn serve_mtls(
    state: AppState,
    router: axum::Router<AppState>,
    tls: Arc<rustls::ServerConfig>,
    listener: TcpListener,
    name: &'static str,
) -> anyhow::Result<()> {
    let cfg = RustlsConfig::from_config(tls);
    let server = axum_server::from_tcp_rustls(listener, cfg)
        .with_context(|| format!("{name} mTLS server init"))?;
    server
        .serve(router.with_state(state).into_make_service())
        .await
        .with_context(|| format!("{name} mTLS listener"))
}
