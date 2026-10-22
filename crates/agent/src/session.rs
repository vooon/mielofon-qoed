//! The controller session: dial, say hello, serve command streams, redial.
//!
//! The agent resolves every configured endpoint, tries each address in turn
//! (the last one that worked first), and keeps one mTLS QUIC session. Its
//! first stream carries `register`; the controller then opens a stream for
//! every command, and the agent answers each on the same stream. A changed
//! link set is a new `register` on the live session. When the session drops
//! the agent redials with back-off; nothing is queued meanwhile, the
//! controller re-seeds costs on the next hello.

use crate::Runtime;
use anyhow::{bail, Context, Result};
use mielofon_proto::{AgentRequest, RegisterResp, WorkCmd};
use mielofon_transport::{request, resolve_all, respond, BiStream, QuicSession, Session};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Per-address dial budget: a black-holed hub costs this, not a hang.
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Keep a session with some controller, forever.
pub async fn run(rt: Arc<Runtime>) {
    let mut backoff = BACKOFF_MIN;
    let mut last_good: Option<SocketAddr> = None;
    loop {
        let Some(session) = connect(&rt, last_good).await else {
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        };
        backoff = BACKOFF_MIN;
        last_good = Some(session.remote());
        serve(&rt, session).await;
    }
}

/// Resolve the endpoint list and dial each address until one session is up.
async fn connect(rt: &Runtime, last_good: Option<SocketAddr>) -> Option<Arc<QuicSession>> {
    let mut candidates = resolve_all(&rt.agent.cfg.endpoints).await;
    if candidates.is_empty() {
        warn!("no controller endpoint resolved");
        return None;
    }
    if let Some(pos) = last_good.and_then(|a| candidates.iter().position(|(c, _)| *c == a)) {
        candidates.rotate_left(pos);
    }
    for (addr, server_name) in candidates {
        match rt.dialer.dial(addr, &server_name, DIAL_TIMEOUT).await {
            Ok(session) => {
                info!(%addr, "controller session up");
                rt.agent.metrics.counters.session_connects_inc();
                return Some(Arc::new(session));
            }
            Err(e) => {
                warn!("dial controller {addr}: {e:#}");
                rt.agent.metrics.counters.session_dial_errors_inc();
            }
        }
    }
    None
}

/// Say hello, then answer command streams until the session ends.
async fn serve(rt: &Arc<Runtime>, session: Arc<QuicSession>) {
    if let Err(e) = hello(rt, &session, true).await {
        warn!("register with {}: {e:#}", session.remote());
        session.close("register failed");
        return;
    }
    rt.agent.metrics.counters.set_connected(true);
    let reason = loop {
        tokio::select! {
            accepted = session.accept() => match accepted {
                Ok(stream) => {
                    tokio::spawn(handle_stream(rt.clone(), stream));
                }
                Err(e) => break e.to_string(),
            },
            _ = rt.links_changed.notified() => {
                if let Err(e) = hello(rt, &session, false).await {
                    warn!("re-register: {e:#}");
                    session.close("register failed");
                    break "register failed".to_string();
                }
            }
            reason = session.closed() => break reason,
        }
    };
    rt.agent.metrics.counters.set_connected(false);
    info!("controller session {} closed: {reason}", session.remote());
}

/// `register` on a fresh stream. `discover` re-runs link discovery first,
/// which the first hello after a dial needs (the refresher may not have run
/// yet); a link-change hello comes from the refresher with fresh links.
async fn hello(rt: &Runtime, session: &QuicSession, discover: bool) -> Result<()> {
    if discover {
        rt.agent.refresh().await;
    }
    let req = rt.agent.register_req();
    info!(
        "registering {} with {} links at {}",
        req.agent,
        req.links.len(),
        session.remote()
    );
    let links = req.links.clone();
    let resp: RegisterResp = request(session, &AgentRequest::Register(req), HELLO_TIMEOUT)
        .await
        .context("register")?;
    if !resp.ok {
        bail!("controller refused: {}", resp.error.unwrap_or_default());
    }
    *rt.registered.lock().expect("registered lock") = links;
    Ok(())
}

/// One controller-opened stream: a command and, unless the agent drops it
/// (an always probe for an interface it no longer has), its reply.
async fn handle_stream(rt: Arc<Runtime>, stream: BiStream) {
    let res = respond(stream, |cmd: WorkCmd| async move {
        debug!("command {}", kind(&cmd));
        rt.agent.execute(cmd).await
    })
    .await;
    if let Err(e) = res {
        warn!("command stream: {e}");
    }
}

fn kind(cmd: &WorkCmd) -> String {
    match cmd {
        WorkCmd::Probe { tier, link, .. } => format!("probe/{tier:?} {}", link.interface),
        WorkCmd::ApplyCost { link, cost, .. } => format!("apply_cost {} {cost}", link.interface),
        WorkCmd::Trace { target, .. } => format!("trace {target}"),
    }
}
