//! Agent sessions on the client listener.
//!
//! An agent dials the controller over mTLS QUIC and opens a stream carrying
//! `register`: that is its hello, and the session is its registration. From
//! then on the controller opens a stream for every command it wants the
//! agent to run (`dispatch`) and reads the reply on the same stream. The
//! session closing, for any reason, is the eviction: no idle timers, no
//! re-register heuristics. A second session for the same agent name
//! supersedes the first, which is closed.

use crate::state::AppState;
use crate::trace::TraceHopReply;
use crate::worker::{track_key, WorkCmd};
use mielofon_proto::{AgentReply, AgentRequest, RegisterResp, Tier};
use mielofon_transport::{request, respond, Listener, Session};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn, Instrument};

/// A session that has not said hello within this long is closed.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long each command may take, stream open to reply read.
pub fn deadline(cmd: &WorkCmd) -> Duration {
    match cmd {
        WorkCmd::Probe {
            tier: Tier::Always, ..
        } => Duration::from_secs(30),
        // TCP leg + UDP leg + iperf3 connect timeouts.
        WorkCmd::Probe {
            tier: Tier::Throughput,
            ..
        } => Duration::from_secs(120),
        // BIRD reconfigure retries while another reconfiguration runs.
        WorkCmd::ApplyCost { .. } => Duration::from_secs(60),
        WorkCmd::Trace { .. } => Duration::from_secs(30),
    }
}

/// Accept agent sessions forever. Returns only when the listener dies.
pub async fn serve(state: AppState, listener: Listener) -> anyhow::Result<()> {
    loop {
        match listener.accept().await {
            None => anyhow::bail!("client listener closed"),
            Some(Err(e)) => warn!("client session refused: {e:#}"),
            Some(Ok(session)) => {
                tokio::spawn(handle(state.clone(), Arc::new(session)));
            }
        }
    }
}

/// Drive one agent session until it closes.
async fn handle(state: AppState, session: Arc<dyn Session>) {
    let remote = session.remote();
    debug!(%remote, "client session open");
    // Identity: (agent name, generation) once registered.
    let mut identity: Option<(String, u64)> = None;

    // The first stream must be the hello.
    let first = match tokio::time::timeout(HELLO_TIMEOUT, session.accept()).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            debug!(%remote, "client session ended before hello: {e}");
            return;
        }
        Err(_) => {
            warn!(%remote, "client session sent no hello in {HELLO_TIMEOUT:?}; closing");
            session.close("hello expected");
            return;
        }
    };
    serve_stream(&state, &session, &mut identity, first).await;
    if identity.is_none() {
        session.close("register expected");
        return;
    }

    let reason = loop {
        tokio::select! {
            accepted = session.accept() => match accepted {
                Ok(stream) => serve_stream(&state, &session, &mut identity, stream).await,
                Err(e) => break e.to_string(),
            },
            reason = session.closed() => break reason,
        }
    };

    if let Some((agent, generation)) = identity {
        if state.workers.detach(&agent, generation) {
            info!(agent, %remote, "agent disconnected: {reason}");
        } else {
            debug!(agent, %remote, "superseded session closed: {reason}");
        }
    }
}

/// One agent-opened stream: a request and its response.
async fn serve_stream(
    state: &AppState,
    session: &Arc<dyn Session>,
    identity: &mut Option<(String, u64)>,
    stream: mielofon_transport::BiStream,
) {
    let mut outcome: Option<(String, u64)> = None;
    let res = respond(stream, |req: AgentRequest| {
        let resp = match req {
            AgentRequest::Register(req) => {
                let (resp, id) = register(state, session, identity.as_ref(), req);
                outcome = id;
                resp
            }
        };
        async move { Some(resp) }
    })
    .await;
    if let Err(e) = res {
        debug!(remote = %session.remote(), "client request failed: {e}");
    }
    if outcome.is_some() {
        *identity = outcome;
    }
}

/// Register the session's agent: replace its link set, attach the session
/// (closing a superseded one) and re-seed every known cost.
fn register(
    state: &AppState,
    session: &Arc<dyn Session>,
    identity: Option<&(String, u64)>,
    req: mielofon_proto::RegisterReq,
) -> (RegisterResp, Option<(String, u64)>) {
    if req.agent.is_empty() {
        return (
            RegisterResp {
                ok: false,
                error: Some("empty agent name".into()),
            },
            None,
        );
    }
    if let Some((name, _)) = identity {
        if *name != req.agent {
            return (
                RegisterResp {
                    ok: false,
                    error: Some(format!("session belongs to {name}")),
                },
                None,
            );
        }
    }
    let own = identity.map(|(_, g)| *g);
    state
        .workers
        .register(&req.agent, req.links.clone(), req.loopback);
    let (generation, old) = state.workers.attach(&req.agent, session.clone(), own);
    if let Some(old) = old {
        info!(agent = req.agent, "new session supersedes the previous one");
        old.close("superseded by a new session");
    }
    info!(
        agent = req.agent,
        links = req.links.len(),
        remote = %session.remote(),
        "agent registered"
    );

    // Seed current policy: `register` forgot every cost previously sent, so
    // each known cost is re-issued on its own stream.
    for link in state.workers.links_of(&req.agent) {
        if let Some(cost) = state.kv.get(&link).and_then(|r| r.ospf_cost) {
            let cmd = WorkCmd::ApplyCost {
                id: crate::scheduler::cmd_id(),
                link: link.clone(),
                cost,
                traceparent: None,
            };
            if dispatch(state, &req.agent, cmd) {
                state.workers.set_applied_sent(&req.agent, &link, cost);
            }
        }
    }
    (
        RegisterResp {
            ok: true,
            error: None,
        },
        Some((req.agent, generation)),
    )
}

/// Dispatch a command to an agent on its session: claims the (link, kind)
/// slot, opens the stream in a task, feeds the reply to `handle_reply` and
/// releases the slot. Returns false when the agent has no session or a
/// command of that kind is already outstanding on the link.
pub fn dispatch(state: &AppState, agent: &str, cmd: WorkCmd) -> bool {
    let Some(session) = state.workers.session(agent) else {
        return false;
    };
    let key = track_key(&cmd);
    if let Some((link, kind)) = &key {
        if !state.workers.begin(agent, link, *kind) {
            return false;
        }
    }
    let state = state.clone();
    let agent = agent.to_string();
    let span = tracing::info_span!("agent.dispatch", agent = %agent, kind = cmd_kind(&cmd));
    tokio::spawn(
        async move {
            let res: std::io::Result<AgentReply> = request(&*session, &cmd, deadline(&cmd)).await;
            match res {
                Ok(reply) => handle_reply(&state, reply),
                Err(e) => on_failure(&state, &agent, &cmd, &e),
            }
            if let Some((link, kind)) = key {
                state.workers.end(&agent, &link, kind);
            }
        }
        .instrument(span),
    );
    true
}

fn cmd_kind(cmd: &WorkCmd) -> &'static str {
    match cmd {
        WorkCmd::Probe {
            tier: Tier::Always, ..
        } => "probe/always",
        WorkCmd::Probe {
            tier: Tier::Throughput,
            ..
        } => "probe/throughput",
        WorkCmd::ApplyCost { .. } => "apply_cost",
        WorkCmd::Trace { .. } => "trace",
    }
}

/// A command produced no reply: the stream failed, the deadline passed, or
/// the agent closed it without answering (an always probe for an interface
/// it no longer has). Undo what the dispatch assumed.
fn on_failure(state: &AppState, agent: &str, cmd: &WorkCmd, err: &std::io::Error) {
    match cmd {
        WorkCmd::Probe {
            tier: Tier::Throughput,
            token: Some(token),
            link,
            ..
        } => {
            warn!(agent, link = %link.id(), "throughput probe unanswered: {err}");
            state.fence.release(&link.id(), token);
        }
        WorkCmd::Probe { link, .. } => {
            debug!(agent, link = %link.id(), "probe unanswered: {err}");
        }
        WorkCmd::ApplyCost { link, cost, .. } => {
            warn!(agent, link = %link.id(), cost, "apply_cost unanswered: {err}");
            state
                .workers
                .note_apply_failed(agent, link, crate::clock::now_secs());
        }
        WorkCmd::Trace { .. } => debug!(agent, "trace unanswered: {err}"),
    }
}

/// Run `f` inside a span whose W3C parent is `traceparent`, when present.
fn with_traceparent<T>(traceparent: Option<&str>, f: impl FnOnce() -> T) -> T {
    let span = tracing::info_span!("agent.report");
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    if let Some(cx) = crate::tracectx::parent_context(traceparent) {
        let _ = span.set_parent(cx);
    }
    span.in_scope(f)
}

/// Apply a command's reply: ingest a measurement, or settle a cost apply.
pub fn handle_reply(state: &AppState, reply: AgentReply) {
    match reply {
        AgentReply::Probe {
            agent,
            id,
            link,
            ts,
            rtt_ms,
            loss_pct,
            jitter_ms,
            tcp_mbps,
            udp_mbps,
            udp_jitter_ms,
            udp_loss_pct,
            util_mbps,
            state: probe_state,
            token,
            note,
            traceparent,
        } => with_traceparent(traceparent.as_deref(), || {
            if let Some(note) = &note {
                crate::api::PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
                warn!(agent, link = %link.id(), id, "probe failed: {note}");
            }
            crate::api::ingest_quality(
                state,
                crate::api::Measure {
                    link,
                    ts,
                    rtt_ms,
                    loss_pct,
                    jitter_ms,
                    tcp_mbps,
                    udp: (udp_mbps, udp_jitter_ms, udp_loss_pct),
                    util_mbps,
                    probe_state,
                    token,
                },
            )
        }),
        AgentReply::Applied {
            agent,
            id,
            link,
            cost,
            note,
            traceparent,
        } => with_traceparent(traceparent.as_deref(), || match note {
            None => state.workers.set_applied_sent(&agent, &link, cost),
            Some(note) => {
                warn!(agent, link = %link.id(), cost, id, "apply_cost failed: {note}");
                state
                    .workers
                    .note_apply_failed(&agent, &link, crate::clock::now_secs());
            }
        }),
        AgentReply::Trace { agent, id, .. } => {
            // Trace replies are read by `request_trace` on their own stream.
            warn!(agent, id, "stray trace reply ignored");
        }
    }
}

/// Ask `agent` to resolve `target` against its BIRD and await the answer.
pub async fn request_trace(
    state: &AppState,
    agent: &str,
    target: &str,
    timeout: Duration,
) -> Result<TraceHopReply, String> {
    let Some(session) = state.workers.session(agent) else {
        return Err(format!("agent '{agent}' has no session"));
    };
    let cmd = WorkCmd::Trace {
        id: crate::scheduler::cmd_id(),
        target: target.to_string(),
        traceparent: None,
    };
    let span = tracing::info_span!("trace.hop", agent = %agent, target = %target);
    let reply: AgentReply = request(&*session, &cmd, timeout)
        .instrument(span)
        .await
        .map_err(|e| format!("hop '{agent}' unanswered ({e})"))?;
    match reply {
        AgentReply::Trace {
            agent,
            prefix,
            code,
            routes,
            ..
        } => {
            info!(agent, code, routes = routes.len(), "trace hop resolved");
            Ok(TraceHopReply {
                agent,
                prefix,
                code,
                routes,
            })
        }
        _ => Err(format!("hop '{agent}' answered with the wrong reply kind")),
    }
}

/// Test double: an in-memory agent session.
#[cfg(test)]
pub mod testing {
    use super::*;
    use mielofon_transport::{read_frame, write_frame, BiStream, BoxFuture};
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tokio::io::AsyncWriteExt;

    /// What the fake agent does with a command.
    #[allow(clippy::large_enum_variant)]
    pub enum Answer {
        /// Reply with this.
        Reply(AgentReply),
        /// Close the stream without a reply (an agent dropping an unknown
        /// always probe).
        Silent,
        /// Never answer; the controller's deadline decides.
        Hang,
    }

    type Handler = dyn Fn(&WorkCmd) -> Answer + Send + Sync;

    /// A `Session` whose streams are answered by `handler`. Every command
    /// the controller dispatched is recorded in `seen`.
    pub struct FakeAgent {
        handler: Arc<Handler>,
        pub seen: Arc<Mutex<Vec<WorkCmd>>>,
    }

    impl FakeAgent {
        pub fn new(handler: impl Fn(&WorkCmd) -> Answer + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(FakeAgent {
                handler: Arc::new(handler),
                seen: Arc::new(Mutex::new(Vec::new())),
            })
        }

        /// Closes every stream without a reply.
        pub fn silent() -> Arc<Self> {
            Self::new(|_| Answer::Silent)
        }

        pub fn seen(&self) -> Vec<WorkCmd> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Session for FakeAgent {
        fn open(&self) -> BoxFuture<'_, std::io::Result<BiStream>> {
            Box::pin(async move {
                let (ours, theirs) = tokio::io::duplex(64 * 1024);
                let (our_r, our_w) = tokio::io::split(ours);
                let (mut their_r, mut their_w) = tokio::io::split(theirs);
                // The fake agent's side of the stream: the command is only
                // written after `open` returns, so read it in a task.
                let handler = self.handler.clone();
                let seen = self.seen.clone();
                tokio::spawn(async move {
                    let Ok(cmd) = read_frame::<_, WorkCmd>(&mut their_r).await else {
                        return;
                    };
                    seen.lock().unwrap().push(cmd.clone());
                    match handler(&cmd) {
                        Answer::Reply(r) => {
                            let _ = write_frame(&mut their_w, &r).await;
                            let _ = their_w.shutdown().await;
                        }
                        Answer::Silent => {
                            let _ = their_w.shutdown().await;
                        }
                        Answer::Hang => std::future::pending::<()>().await,
                    }
                });
                Ok(BiStream {
                    send: Box::new(our_w),
                    recv: Box::new(our_r),
                })
            })
        }

        fn accept(&self) -> BoxFuture<'_, std::io::Result<BiStream>> {
            Box::pin(std::future::pending())
        }

        fn closed(&self) -> BoxFuture<'_, String> {
            Box::pin(std::future::pending())
        }

        fn close(&self, _reason: &str) {}

        fn remote(&self) -> SocketAddr {
            "127.0.0.1:0".parse().unwrap()
        }
    }

    /// Register `agent` with `links` and attach a fake session answering
    /// with `handler`. Returns the fake for inspection.
    pub fn connect(
        state: &AppState,
        agent: &str,
        links: Vec<crate::model::LinkKey>,
        loopback: Option<&str>,
        handler: impl Fn(&WorkCmd) -> Answer + Send + Sync + 'static,
    ) -> Arc<FakeAgent> {
        let fake = FakeAgent::new(handler);
        state
            .workers
            .register(agent, links, loopback.map(str::to_string));
        let (_, old) = state.workers.attach(agent, fake.clone(), None);
        assert!(old.is_none());
        fake
    }

    /// Let spawned dispatch tasks run to completion.
    pub async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{connect, settle, Answer};
    use super::*;
    use crate::config::Config;
    use crate::model::{LinkKey, ProbeState, QualityRecord};
    use crate::worker::Kind;

    fn link() -> LinkKey {
        LinkKey::new("spoke-1", "hub-a", "awg_hub_a")
    }

    #[tokio::test]
    async fn failed_apply_is_not_treated_as_applied() {
        let state = AppState::new(Config::default());
        state.workers.register("spoke-1", vec![link()], None);
        state.workers.set_applied_sent("spoke-1", &link(), 50);
        handle_reply(
            &state,
            AgentReply::Applied {
                agent: "spoke-1".into(),
                id: "apply/1".into(),
                link: link(),
                cost: 50,
                note: Some("bird returned code 3".into()),
                traceparent: None,
            },
        );
        assert_eq!(state.workers.applied_sent("spoke-1", &link()), None);
    }

    #[tokio::test]
    async fn register_reseeds_known_costs_on_their_own_streams() {
        let state = AppState::new(Config::default());
        let mut rec = QualityRecord::new(
            Some(10.0),
            Some(0.0),
            Some(1.0),
            None,
            None,
            0.0,
            ProbeState::Quiet,
        );
        rec.ospf_cost = Some(20);
        state.kv.put(link(), rec);

        let fake = connect(&state, "spoke-1", vec![], None, |cmd| match cmd {
            WorkCmd::ApplyCost { id, link, cost, .. } => Answer::Reply(AgentReply::Applied {
                agent: "spoke-1".into(),
                id: id.clone(),
                link: link.clone(),
                cost: *cost,
                note: None,
                traceparent: None,
            }),
            _ => Answer::Silent,
        });
        let session: Arc<dyn Session> = fake.clone();
        let identity = None;
        let (resp, id) = register(
            &state,
            &session,
            identity,
            mielofon_proto::RegisterReq {
                agent: "spoke-1".into(),
                links: vec![link()],
                loopback: None,
            },
        );
        assert!(resp.ok);
        let (name, generation) = id.unwrap();
        assert_eq!(name, "spoke-1");
        settle().await;
        let seen = fake.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(matches!(seen[0], WorkCmd::ApplyCost { cost: 20, .. }));
        assert_eq!(state.workers.applied_sent("spoke-1", &link()), Some(20));

        // A re-register on the same session keeps the generation and seeds
        // again (the agent may have rebooted and lost the cost).
        let (resp, id) = register(
            &state,
            &session,
            Some(&(name, generation)),
            mielofon_proto::RegisterReq {
                agent: "spoke-1".into(),
                links: vec![link()],
                loopback: None,
            },
        );
        assert!(resp.ok);
        assert_eq!(id.unwrap().1, generation);
        settle().await;
        assert_eq!(fake.seen().len(), 2);
    }

    #[tokio::test]
    async fn unanswered_throughput_releases_the_fence() {
        let state = AppState::new(Config::default());
        connect(&state, "spoke-1", vec![link()], None, |_| Answer::Silent);
        let lease = state.fence.acquire("spoke-1", &link(), 120).unwrap();
        assert!(dispatch(
            &state,
            "spoke-1",
            WorkCmd::Probe {
                id: "t".into(),
                tier: Tier::Throughput,
                token: Some(lease.token.clone()),
                link: link(),
                traceparent: None,
            }
        ));
        // Outstanding: the same kind cannot be dispatched twice.
        assert!(!dispatch(
            &state,
            "spoke-1",
            WorkCmd::Probe {
                id: "t2".into(),
                tier: Tier::Throughput,
                token: None,
                link: link(),
                traceparent: None,
            }
        ));
        settle().await;
        assert!(state.fence.leases().is_empty(), "released on no reply");
        assert!(state.workers.begin("spoke-1", &link(), Kind::Throughput));
    }

    #[tokio::test]
    async fn real_quic_session_registers_dispatches_and_evicts_on_close() {
        use mielofon_proto::{AgentRequest, RegisterReq, RegisterResp};
        use mielofon_transport::{testpki, Dialer, Listener};

        let pki = testpki::pki();
        let state = AppState::new(Config::default());
        let listener = Listener::bind(pki.server, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(state.clone(), listener));

        let dialer = Dialer::new(pki.client).unwrap();
        let client = dialer
            .dial(addr, "127.0.0.1", Duration::from_secs(5))
            .await
            .unwrap();
        let resp: RegisterResp = request(
            &client,
            &AgentRequest::Register(RegisterReq {
                agent: "spoke-1".into(),
                links: vec![link()],
                loopback: Some("fd00:0:0:1::1".into()),
            }),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(resp.ok);
        settle().await;
        assert!(state.workers.has_agent("spoke-1"));
        assert_eq!(
            state.workers.loopback("spoke-1").as_deref(),
            Some("fd00:0:0:1::1")
        );

        // The controller opens a stream for a command; the agent answers it.
        assert!(dispatch(
            &state,
            "spoke-1",
            WorkCmd::ApplyCost {
                id: "apply/1".into(),
                link: link(),
                cost: 20,
                traceparent: None,
            }
        ));
        let stream = client.accept().await.unwrap();
        respond(stream, |cmd: WorkCmd| async move {
            match cmd {
                WorkCmd::ApplyCost { id, link, cost, .. } => Some(AgentReply::Applied {
                    agent: "spoke-1".into(),
                    id,
                    link,
                    cost,
                    note: None,
                    traceparent: None,
                }),
                _ => None,
            }
        })
        .await
        .unwrap();
        settle().await;
        assert_eq!(state.workers.applied_sent("spoke-1", &link()), Some(20));

        // A second session for the same name supersedes the first.
        let second = dialer
            .dial(addr, "127.0.0.1", Duration::from_secs(5))
            .await
            .unwrap();
        let resp: RegisterResp = request(
            &second,
            &AgentRequest::Register(RegisterReq {
                agent: "spoke-1".into(),
                links: vec![link()],
                loopback: None,
            }),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(resp.ok);
        let reason = client.closed().await;
        assert!(reason.contains("superseded"), "{reason}");
        settle().await;
        assert!(
            state.workers.has_agent("spoke-1"),
            "the new session owns it"
        );

        // Closing the live session evicts the agent at once.
        second.close("bye");
        settle().await;
        for _ in 0..50 {
            if !state.workers.has_agent("spoke-1") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!state.workers.has_agent("spoke-1"));
    }

    #[tokio::test]
    async fn trace_request_round_trips() {
        let state = AppState::new(Config::default());
        connect(
            &state,
            "hub-a",
            vec![],
            Some("fd00:0:0:1::2"),
            |cmd| match cmd {
                WorkCmd::Trace { id, target, .. } => Answer::Reply(AgentReply::Trace {
                    agent: "hub-a".into(),
                    id: id.clone(),
                    traceparent: None,
                    prefix: target.clone(),
                    code: 0,
                    routes: vec![],
                }),
                _ => Answer::Silent,
            },
        );
        let r = request_trace(&state, "hub-a", "fd00:0:0:1::3", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(r.prefix, "fd00:0:0:1::3");
        assert!(request_trace(&state, "nobody", "x", Duration::from_secs(1))
            .await
            .is_err());
    }
}
