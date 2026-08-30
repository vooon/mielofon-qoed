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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut argv = std::env::args();
    let first = argv.nth(1);

    // `mielofon-controller cert <ca|node|agent> ...` — certificate generation
    // (nebula-cert style), runs on the operator's control plane only.
    if first.as_deref() == Some("cert") {
        mielofon_controller::cert::run(&argv.collect::<Vec<_>>())?;
        return Ok(());
    }

    let path = first.unwrap_or_else(|| "/etc/mielofon/mielofon-controller.toml".into());
    let cfg = Config::load(&path)?;

    // Select the ring CryptoProvider as the process default so rustls doesn't
    // fail on provider ambiguity (multiple deps pull different rustls features).
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    let _guard =
        mielofon_otel::install(&cfg.otel, "mielofon-controller", env!("CARGO_PKG_VERSION"))?;

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
    let admin_srv = axum_server::from_tcp(admin_listener).serve(
        api::admin_router()
            .with_state(state.clone())
            .into_make_service(),
    );
    let admin_handle = tokio::spawn(async move {
        let _ = admin_srv.await;
    });

    // Bind members + clients (mTLS).
    let members_handle = tokio::spawn(serve_mtls(
        state.clone(),
        api::members_router(),
        server_tls.clone(),
        state.cfg.listeners.members(),
    ));
    let clients_handle = tokio::spawn(serve_mtls(
        state.clone(),
        api::clients_router(),
        server_tls.clone(),
        state.cfg.listeners.clients(),
    ));

    state.set_ready(true);
    info!(
        node = %state.cfg.node.name,
        admin = %admin_bind,
        "mielofon-controller up and ready"
    );

    tokio::select! {
        _ = admin_handle => {}
        _ = members_handle => {}
        _ = clients_handle => {}
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
    let cfg = RustlsConfig::from_config(tls);
    let server = axum_server::from_tcp_rustls(listener, cfg)
        .serve(router.with_state(state).into_make_service());
    let _ = server.await;
}
