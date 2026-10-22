//! HTTP API: the admin listener (9553) is plain HTTP on loopback serving the
//! dashboard, metrics, healthz, the read-only query endpoints and the tooling
//! ingest. Neither agents nor members speak HTTP: agents hold a QUIC session
//! on the client listener (9552, see `session.rs`), members on the cluster
//! listener (9551, see `gossip.rs`).

use crate::model::{LinkKey, ProbeState, QualityRecord};
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

// ── Request/response types ────────────────────────────────────────────────

/// `POST /v1/quality` (admin): a bare measurement without a command to
/// answer, for tooling and the e2e harness. Agents answer commands on their
/// session streams.
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
    #[serde(default)]
    pub udp_mbps: Option<f64>,
    #[serde(default)]
    pub udp_jitter_ms: Option<f64>,
    #[serde(default)]
    pub udp_loss_pct: Option<f64>,
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
    /// Replicated node directory: router id → registered name.
    pub nodes: Vec<(String, crate::model::NodeEntry)>,
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

// ── Measurement ingest ────────────────────────────────────────────────────

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
            udp: (req.udp_mbps, req.udp_jitter_ms, req.udp_loss_pct),
            util_mbps: req.util_mbps,
            probe_state: req.state,
            token: req.token,
        },
    );
    Json(QualityResp { accepted: true })
}

/// A single measurement to ingest.
pub(crate) struct Measure {
    pub link: LinkKey,
    pub ts: Option<u64>,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub tcp_mbps: Option<f64>,
    /// UDP leg: (mbps, jitter_ms, loss_pct).
    pub udp: (Option<f64>, Option<f64>, Option<f64>),
    pub util_mbps: f64,
    pub probe_state: ProbeState,
    pub token: Option<String>,
}

/// Shared measurement ingest: append the raw sample to the replicated store and
/// release the fence when a gated throughput report echoes its token.
///
/// No classification or KV write happens here — policy is derived asynchronously
/// by the classifier from a window of stored samples (measurement and decision
/// are decoupled). Storing exactly what arrives means a failed probe clears the
/// stale dimension in the *sample* instead of resurrecting it (e.g. a 100%-loss
/// ping must not keep an old rtt); the derived live record reflects the window.
pub(crate) fn ingest_quality(state: &AppState, m: Measure) {
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
        )
        .with_udp(m.udp.0, m.udp.1, m.udp.2),
    );

    REPORTS.fetch_add(1, Ordering::Relaxed);

    // A gated throughput report carries the fence token — release the lease.
    if let Some(t) = m.token {
        state.fence.release(&m.link.id(), &t);
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
    let series: [(&str, &str, Dim); 9] = [
        ("mielofon_link_rtt_ms", "gauge", |r| r.rtt_ms),
        ("mielofon_link_loss_pct", "gauge", |r| r.loss_pct),
        ("mielofon_link_jitter_ms", "gauge", |r| r.jitter_ms),
        ("mielofon_link_tcp_mbps", "gauge", |r| r.tcp_mbps),
        ("mielofon_link_udp_mbps", "gauge", |r| r.udp_mbps),
        ("mielofon_link_udp_jitter_ms", "gauge", |r| r.udp_jitter_ms),
        ("mielofon_link_udp_loss_pct", "gauge", |r| r.udp_loss_pct),
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
        nodes: state.kv.nodes(),
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

// ── Router assembly ───────────────────────────────────────────────────────

/// One-line HTTP access log: method, uri, status, duration. `/healthz` and `/readyz` are health-check hot paths
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

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/v1/graph", get(frontend_graph))
        .route("/v1/trace", get(get_trace))
        .route("/v1/ts", get(get_ts))
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/quality/all", get(get_quality_all))
        .route("/v1/quality", get(get_quality).post(post_quality))
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
pub(crate) static PROBE_FAILURES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn admin_ingest_is_stored_and_readable() {
        let state = AppState::new(Config::default());
        let app = admin_router().with_state(state.clone());
        let link = LinkKey::new("spoke-1", "hub-a", "awg0");
        let body = serde_json::json!({
            "link": link, "rtt_ms": 21.0, "loss_pct": 0.0, "util_mbps": 0.0, "state": "quiet"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/v1/quality")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let now = crate::clock::now_secs();
        let req = Request::builder()
            .uri(format!(
                "/v1/ts?from=spoke-1&to=hub-a&interface=awg0&since={}&until={}",
                now - 1,
                now + 2
            ))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["samples"].as_array().unwrap().len(), 1);
    }
}
