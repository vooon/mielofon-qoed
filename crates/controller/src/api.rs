//! HTTP API: three listeners. Members (9551) and clients (9552) are mTLS; the
//! admin listener (9553) is plain HTTP on loopback serving dashboard, metrics,
//! healthz and read-only query endpoints.

use crate::model::{LinkKey, ProbeState, QualityRecord};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::warn;

pub use mielofon_proto::{AgentReply, CommandReq};

// ── Request/response types ────────────────────────────────────────────────

/// `POST /v1/quality`: a bare measurement without a command to answer (used
/// by tooling and the e2e harness; agents answer commands on `/v1/agent/reply`).
#[derive(Debug, Deserialize)]
pub struct QualityReq {
    pub link: LinkKey,
    pub ts: Option<u64>,
    /// Always-tier dimensions. Always probes always send what they measured
    /// (`null` when unmeasurable); throughput replies echo the agent's last
    /// always-tier measurement so these stay populated across fence-gated
    /// runs. Stored verbatim — no history carry-forward on the controller.
    #[serde(default)]
    pub rtt_ms: Option<f64>,
    #[serde(default)]
    pub loss_pct: Option<f64>,
    #[serde(default)]
    pub jitter_ms: Option<f64>,
    #[serde(default)]
    pub tcp_mbps: Option<f64>,
    pub util_mbps: f64,
    pub state: ProbeState,
    /// Echo of the fence token from a gated throughput command. When present,
    /// the controller releases the lease for the link.
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QualityResp {
    pub accepted: bool,
}

#[derive(Debug, Serialize)]
pub struct StatusResp {
    pub node: String,
    pub ready: bool,
    pub uptime_secs: u64,
    pub links: usize,
    pub leases: Vec<crate::fence::Lease>,
    pub members: Vec<String>,
    /// Per-member liveness reported by gossip (name -> health).
    pub peers: Vec<(String, crate::state::PeerHealthEntry)>,
    /// Agents currently registered with this node.
    pub agents: Vec<String>,
}

/// Query filter for `GET /v1/policy` and `GET /v1/quality`.
#[derive(Debug, Deserialize)]
pub struct LinkQuery {
    pub from: String,
    pub to: String,
    pub interface: String,
}

/// Trace query: `from` (source agent), `to` (destination node name, resolved
/// from its reported mesh loopback) or a raw `prefix` override.
#[derive(Debug, Deserialize)]
pub struct TraceQuery {
    pub from: String,
    pub to: Option<String>,
    pub prefix: Option<String>,
}

/// Time-series query window for a link (raw samples + 60s buckets).
#[derive(Debug, Deserialize)]
pub struct TsQuery {
    pub from: String,
    pub to: String,
    pub interface: String,
    pub since: Option<u64>,
    pub until: Option<u64>,
}

fn not_found(msg: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

// ── Measurement ingest (clients listener) ─────────────────────────────────

pub async fn post_quality(
    State(state): State<AppState>,
    Json(req): Json<QualityReq>,
) -> Json<QualityResp> {
    ingest_quality(
        &state,
        Measure {
            link: req.link,
            ts: req.ts,
            rtt_ms: req.rtt_ms,
            loss_pct: req.loss_pct,
            jitter_ms: req.jitter_ms,
            tcp_mbps: req.tcp_mbps,
            util_mbps: req.util_mbps,
            probe_state: req.state,
            token: req.token,
        },
    );
    Json(QualityResp { accepted: true })
}

/// A single measurement to ingest.
struct Measure {
    link: LinkKey,
    ts: Option<u64>,
    rtt_ms: Option<f64>,
    loss_pct: Option<f64>,
    jitter_ms: Option<f64>,
    tcp_mbps: Option<f64>,
    util_mbps: f64,
    probe_state: ProbeState,
    token: Option<String>,
}

/// Shared measurement ingest: append the raw sample to the replicated store and
/// release the fence when a gated throughput report echoes its token.
///
/// No classification or KV write happens here — policy is derived asynchronously
/// by the classifier from a window of stored samples (measurement and decision
/// are decoupled). Storing exactly what arrives means a failed probe clears the
/// stale dimension in the *sample* instead of resurrecting it (e.g. a 100%-loss
/// ping must not keep an old rtt); the derived live record reflects the window.
fn ingest_quality(state: &AppState, m: Measure) {
    let ts = m.ts.unwrap_or_else(crate::clock::now_secs);

    state.tsdb.append(
        &m.link,
        crate::tsdb::RawSample::from_dims(
            m.probe_state,
            ts,
            m.rtt_ms,
            m.loss_pct,
            m.jitter_ms,
            m.tcp_mbps,
            m.util_mbps,
        ),
    );

    REPORTS.fetch_add(1, Ordering::Relaxed);

    // A gated throughput report carries the fence token — release the lease.
    if let Some(t) = m.token {
        state.fence.release(&m.link.id(), &t);
    }
}

// ── Agent pull endpoints (clients listener) ───────────────────────────────

/// Register an agent and its managed links; the response carries the current
/// policy snapshot (apply_cost commands) so a fresh agent converges immediately.
pub async fn register_agent(
    State(state): State<AppState>,
    Json(req): Json<crate::worker::RegisterReq>,
) -> Json<crate::worker::RegisterResp> {
    if req.agent.is_empty() {
        return Json(crate::worker::RegisterResp {
            ok: false,
            commands: Vec::new(),
        });
    }
    state.workers.register(&req.agent, req.links, req.loopback);

    // Seed current policy for the agent's links. `register` forgot every
    // cost previously sent, so each known cost is re-issued.
    let mut commands = Vec::new();
    for link in state.workers.links_of(&req.agent) {
        if let Some(cost) = state.kv.get(&link).and_then(|r| r.ospf_cost) {
            state.workers.set_applied_sent(&req.agent, &link, cost);
            commands.push(crate::worker::WorkCmd::ApplyCost {
                id: crate::scheduler::cmd_id(),
                link,
                cost,
                traceparent: None,
            });
        }
    }
    Json(crate::worker::RegisterResp { ok: true, commands })
}

/// Long-polled work fetch: returns queued commands immediately, otherwise
/// holds the request until a command is queued (scheduler `notify`) or the
/// timeout expires. `timeout_ms == 0` drains without waiting. An agent this
/// node does not know (never registered, evicted, or the controller
/// restarted) gets `404` so it re-registers instead of polling into the void.
pub async fn command_long_poll(
    State(state): State<AppState>,
    Json(req): Json<CommandReq>,
) -> Response {
    let timeout = std::time::Duration::from_millis(req.timeout_ms.min(120_000));
    let deadline = tokio::time::Instant::now() + timeout;
    let mut notify = None;
    loop {
        match state.workers.drain(&req.agent) {
            None => return not_found("agent not registered"),
            Some(commands) if !commands.is_empty() => {
                return Json(crate::worker::WorkResp { commands }).into_response();
            }
            Some(_) => {}
        }
        if timeout.is_zero() {
            break;
        }
        let notify = notify.get_or_insert_with(|| state.workers.notify(&req.agent));
        tokio::select! {
            _ = notify.notified() => continue,
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    Json(crate::worker::WorkResp {
        commands: Vec::new(),
    })
    .into_response()
}

/// Run `f` inside a span whose W3C parent is `traceparent`, when present.
/// Returns the span's return value.
fn with_traceparent<T>(traceparent: Option<&str>, f: impl FnOnce() -> T) -> T {
    let span = tracing::info_span!("agent.report");
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    if let Some(cx) = crate::tracectx::parent_context(traceparent) {
        let _ = span.set_parent(cx);
    }
    span.in_scope(f)
}

/// `/v1/agent/reply`: agent→controller results for previously issued commands.
pub async fn agent_reply(
    State(state): State<AppState>,
    Json(reply): Json<AgentReply>,
) -> Json<serde_json::Value> {
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
            util_mbps,
            state: probe_state,
            token,
            note,
            traceparent,
        } => {
            with_traceparent(traceparent.as_deref(), || {
                if let Some(note) = &note {
                    PROBE_FAILURES.fetch_add(1, Ordering::Relaxed);
                    warn!(agent, link = %link.id(), id, "probe failed: {note}");
                }
                ingest_quality(
                    &state,
                    Measure {
                        link,
                        ts,
                        rtt_ms,
                        loss_pct,
                        jitter_ms,
                        tcp_mbps,
                        util_mbps,
                        probe_state,
                        token,
                    },
                )
            });
            Json(serde_json::json!({"ok": true, "id": id}))
        }
        AgentReply::Applied {
            agent,
            id,
            link,
            cost,
            note,
            traceparent,
        } => {
            with_traceparent(traceparent.as_deref(), || match note {
                None => state.workers.set_applied_sent(&agent, &link, cost),
                Some(note) => {
                    warn!(agent, link = %link.id(), cost, id, "apply_cost failed: {note}");
                    state
                        .workers
                        .note_apply_failed(&agent, &link, crate::clock::now_secs());
                }
            });
            Json(serde_json::json!({"ok": true, "id": id}))
        }
        AgentReply::Trace {
            agent,
            id,
            traceparent,
            prefix,
            code,
            routes,
        } => {
            // Hand the hop resolution back to the trace walker, which is
            // awaiting it by command id.
            let delivered = with_traceparent(traceparent.as_deref(), || {
                state.trace.complete(
                    &id,
                    crate::trace::TraceHopReply {
                        agent,
                        prefix,
                        code,
                        routes,
                    },
                )
            });
            Json(serde_json::json!({"ok": delivered, "id": id}))
        }
    }
}

// ── Frontend (SPA served from disk) ───────────────────────────────────────

/// Serve the dashboard SPA from the configured `[frontend] root_dir`. A single
/// catch-all handles the shell (`/` → `index.html`), the Vite asset bundle and
/// the bundled vis-network library (`/assets/*`) from disk.
pub async fn frontend(State(state): State<AppState>, path: Option<Path<String>>) -> Response {
    let root = &state.cfg.frontend.root_dir;
    let rel = path.map(|Path(p)| p).unwrap_or_default();
    match crate::assets::get(root, &rel) {
        Some(r) => r,
        None => not_found("not found"),
    }
}

/// Frontend graph data: nodes + per-link measurements + registered agents.
/// Sanitized — node ids are the placeholder agent/hub names, addresses never
/// leave the daemon.
pub async fn frontend_graph(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (nodes, links) = crate::map::graph_part(&state);
    Json(serde_json::json!({
        "nodes": nodes,
        "links": links,
        "agents": state.workers.agents(),
    }))
}

// ── Admin handlers (plain HTTP on loopback) ───────────────────────────────

/// Escape a Prometheus label value (backslash, double quote, newline).
fn prom_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    use std::fmt::Write as _;
    let mut body = String::new();
    let _ = writeln!(
        body,
        "# HELP mielofon_quality_reports_total quality reports accepted\n# TYPE mielofon_quality_reports_total counter\nmielofon_quality_reports_total {}",
        REPORTS.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        body,
        "# HELP mielofon_probe_failures_total probe replies that carried a failure note\n# TYPE mielofon_probe_failures_total counter\nmielofon_probe_failures_total {}",
        PROBE_FAILURES.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        body,
        "# HELP mielofon_links number of known links\n# TYPE mielofon_links gauge\nmielofon_links {}",
        state.kv.len()
    );
    let _ = writeln!(
        body,
        "# HELP mielofon_agents number of registered agents\n# TYPE mielofon_agents gauge\nmielofon_agents {}",
        state.workers.len()
    );
    let _ = writeln!(
        body,
        "# HELP mielofon_build_info build and version information\n# TYPE mielofon_build_info gauge\nmielofon_build_info{{version=\"{}\",revision=\"{}\",rustc=\"{}\"}} 1",
        env!("MIELOFON_VERSION"),
        env!("MIELOFON_GIT_REV"),
        prom_label(env!("MIELOFON_RUSTC")),
    );

    // Per-link gauges. An unmeasured dimension is simply absent — never a
    // sentinel value that a dashboard could mistake for a measurement.
    type Dim = fn(&QualityRecord) -> Option<f64>;
    let records = state.kv.all();
    let series: [(&str, &str, Dim); 6] = [
        ("mielofon_link_rtt_ms", "gauge", |r| r.rtt_ms),
        ("mielofon_link_loss_pct", "gauge", |r| r.loss_pct),
        ("mielofon_link_jitter_ms", "gauge", |r| r.jitter_ms),
        ("mielofon_link_tcp_mbps", "gauge", |r| r.tcp_mbps),
        ("mielofon_link_quality", "gauge", |r| {
            r.quality.map(|q| quality_label_value(q) as f64)
        }),
        ("mielofon_link_ospf_cost", "gauge", |r| {
            r.ospf_cost.map(f64::from)
        }),
    ];
    for (name, kind, get) in series {
        let _ = writeln!(body, "# TYPE {name} {kind}");
        for (key, rec) in &records {
            if let Some(v) = get(rec) {
                let _ = writeln!(
                    body,
                    "{name}{{from=\"{}\",to=\"{}\",iface=\"{}\"}} {v}",
                    prom_label(&key.from),
                    prom_label(&key.to),
                    prom_label(&key.interface)
                );
            }
        }
    }

    let mut response = body.into_response();
    if let Ok(h) = header::HeaderValue::from_str("text/plain; version=0.0.4") {
        response.headers_mut().insert(header::CONTENT_TYPE, h);
    }
    response
}

fn quality_label_value(q: crate::model::Quality) -> u8 {
    use crate::model::Quality;
    match q {
        Quality::Good => 0,
        Quality::Acceptable => 1,
        Quality::Poor => 2,
        Quality::Bad => 3,
    }
}

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

pub async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.is_ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}

pub async fn get_quality_all(State(state): State<AppState>) -> Json<Vec<(LinkKey, QualityRecord)>> {
    Json(state.kv.all())
}

pub async fn get_quality(State(state): State<AppState>, Query(q): Query<LinkQuery>) -> Response {
    let key = LinkKey::new(q.from, q.to, q.interface);
    match state.kv.get(&key) {
        Some(rec) => (StatusCode::OK, Json(rec)).into_response(),
        None => not_found("not found"),
    }
}

pub async fn get_policy(State(state): State<AppState>, Query(q): Query<LinkQuery>) -> Response {
    let key = LinkKey::new(q.from, q.to, q.interface);
    match state.kv.get(&key) {
        Some(rec) => Json(serde_json::json!({
            "link": key,
            "quality": rec.quality,
            "ospf_cost": rec.ospf_cost,
            "ts": rec.ts,
        }))
        .into_response(),
        None => not_found("not found"),
    }
}

pub async fn get_status(State(state): State<AppState>) -> Json<StatusResp> {
    let uptime = state.started_at.elapsed().as_secs();
    Json(StatusResp {
        node: state.cfg.node.name.clone(),
        ready: state.is_ready(),
        uptime_secs: uptime,
        links: state.kv.len(),
        leases: state.fence.leases(),
        members: state.cfg.members.keys().cloned().collect(),
        peers: state.peers.snapshot(),
        agents: state.workers.agents(),
    })
}

/// Run an end-to-end, ECMP-aware trace from a source agent to a destination
/// node (resolved via its registered mesh loopback) or a raw prefix.
pub async fn get_trace(State(state): State<AppState>, Query(q): Query<TraceQuery>) -> Response {
    match crate::trace::run_trace(
        &state,
        crate::trace::TraceRequest {
            from: q.from,
            to: q.to,
            prefix: q.prefix,
        },
    )
    .await
    {
        Ok(res) => (StatusCode::OK, Json(res)).into_response(),
        Err(crate::trace::TraceError::BadRequest(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
    }
}

/// History for one link: raw samples and 60s aggregate buckets in `[since,
/// until)` (defaults: last hour). `since > until` is rejected.
pub async fn get_ts(State(state): State<AppState>, Query(q): Query<TsQuery>) -> Response {
    let key = crate::model::LinkKey::new(q.from, q.to, q.interface);
    let now = crate::clock::now_secs();
    let until = q.until.unwrap_or(now);
    let since = q.since.unwrap_or_else(|| until.saturating_sub(3600));
    if since > until {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "since must be <= until"})),
        )
            .into_response();
    }
    (StatusCode::OK, Json(state.tsdb.query(&key, since, until))).into_response()
}

/// Members-listener liveness probe. Nodes ping each other over the mTLS
/// members port; the gossip loop measures this round-trip for /v1/status.
pub async fn ping(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "node": state.cfg.node.name,
        "ok": true,
    }))
}

// ── Router assembly ───────────────────────────────────────────────────────

/// One-line HTTP access log: listener label (cluster/client/admin), method,
/// uri, status, duration. `/healthz` and `/readyz` are health-check hot paths
/// (proxies poll them constantly) and are downgraded to `debug` to avoid
/// flooding the daemon log.
async fn access_log(
    server: &'static str,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let probe = uri.path() == "/healthz" || uri.path() == "/readyz";
    let started = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let ms = started.elapsed().as_millis();
    if probe {
        tracing::debug!(server, %method, %uri, status, ms, "http");
    } else {
        tracing::info!(server, %method, %uri, status, ms, "http");
    }
    resp
}

pub fn cluster_router() -> Router<AppState> {
    Router::new()
        .route("/v1/ping", get(ping))
        .route("/v1/gossip/exchange", post(crate::gossip::exchange))
        .layer(axum::middleware::from_fn(|req, next| {
            access_log("cluster", req, next)
        }))
}

pub fn client_router() -> Router<AppState> {
    Router::new()
        .route("/v1/quality", post(post_quality))
        .route("/v1/agent/register", post(register_agent))
        .route("/v1/agent/command", post(command_long_poll))
        .route("/v1/agent/reply", post(agent_reply))
        .layer(axum::middleware::from_fn(|req, next| {
            access_log("client", req, next)
        }))
}

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/v1/graph", get(frontend_graph))
        .route("/v1/trace", get(get_trace))
        .route("/v1/ts", get(get_ts))
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/quality/all", get(get_quality_all))
        .route("/v1/quality", get(get_quality))
        .route("/v1/policy", get(get_policy))
        .route("/v1/status", get(get_status))
        .route("/", get(frontend))
        .route("/{*path}", get(frontend))
        .layer(axum::middleware::from_fn(|req, next| {
            access_log("admin", req, next)
        }))
}

// ── Prometheus counters ────────────────────────────────────────────────────

/// Quality reports accepted since start (probe replies + `POST /v1/quality`).
static REPORTS: AtomicU64 = AtomicU64::new(0);
/// Probe replies that carried a failure note (e.g. iperf3 could not run).
static PROBE_FAILURES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(
        state: &AppState,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let app = client_router().with_state(state.clone());
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn unknown_agent_poll_is_404() {
        let state = AppState::new(Config::default());
        let (status, body) = call(
            &state,
            "/v1/agent/command",
            serde_json::json!({ "agent": "spoke-1", "timeout_ms": 0 }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "agent not registered");

        let (status, _) = call(
            &state,
            "/v1/agent/register",
            serde_json::json!({ "agent": "spoke-1", "links": [] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(
            &state,
            "/v1/agent/command",
            serde_json::json!({ "agent": "spoke-1", "timeout_ms": 0 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["commands"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn long_poll_wakes_on_push_before_wait() {
        // A command queued between the poll's drain and its wait must still
        // wake the poll (Notify permit semantics), not sit for the full timeout.
        let state = AppState::new(Config::default());
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        state.workers.register("spoke-1", vec![link.clone()], None);
        let pusher = {
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                state.workers.push(
                    "spoke-1",
                    crate::worker::WorkCmd::Probe {
                        id: "x".into(),
                        tier: crate::worker::Tier::Always,
                        token: None,
                        link,
                        traceparent: None,
                    },
                );
            })
        };
        let started = std::time::Instant::now();
        let (status, body) = call(
            &state,
            "/v1/agent/command",
            serde_json::json!({ "agent": "spoke-1", "timeout_ms": 10_000 }),
        )
        .await;
        pusher.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["commands"].as_array().unwrap().len(), 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn failed_apply_is_not_treated_as_applied() {
        let state = AppState::new(Config::default());
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        state.workers.register("spoke-1", vec![link.clone()], None);
        state.workers.set_applied_sent("spoke-1", &link, 50);
        let (status, _) = call(
            &state,
            "/v1/agent/reply",
            serde_json::json!({
                "kind": "applied", "agent": "spoke-1", "id": "apply/1",
                "link": link, "cost": 50, "note": "bird returned code 3"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state.workers.applied_sent("spoke-1", &link), None);
    }

    #[tokio::test]
    async fn register_reseeds_known_costs() {
        let state = AppState::new(Config::default());
        let link = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
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
        state.kv.put(link.clone(), rec);
        for _ in 0..2 {
            // Every registration re-seeds, even a repeat with no change.
            let (_, body) = call(
                &state,
                "/v1/agent/register",
                serde_json::json!({ "agent": "spoke-1", "links": [link] }),
            )
            .await;
            assert_eq!(body["commands"][0]["type"], "apply_cost");
            assert_eq!(body["commands"][0]["cost"], 20);
        }
    }
}
