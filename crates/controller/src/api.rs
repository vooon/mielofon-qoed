//! HTTP API: three listeners. Members (9551) and clients (9552) are mTLS; the
//! admin listener (9553) is plain HTTP on loopback serving dashboard, metrics,
//! healthz and read-only query endpoints.

use crate::model::{LinkKey, ProbeState, Quality, QualityRecord};
use crate::quality;
use crate::state::AppState;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

// ── Request/response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AcquireReq {
    pub agent: String,
    pub link: String,
    #[serde(default = "default_ttl")]
    pub ttl_secs: u64,
}

fn default_ttl() -> u64 {
    120
}

#[derive(Debug, Serialize)]
pub struct AcquireResp {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseReq {
    pub link: String,
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct QualityReq {
    pub link: LinkKey,
    pub ts: Option<u64>,
    pub rtt_ms: f64,
    pub loss_pct: f64,
    pub rr_tps: f64,
    #[serde(default)]
    pub tcp_mbps: Option<f64>,
    #[serde(default)]
    pub udp_mbps: Option<f64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<Quality>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ospf_cost: Option<u32>,
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
}

/// Query filter for `GET /v1/policy` and `GET /v1/quality`.
#[derive(Debug, Deserialize)]
pub struct LinkQuery {
    pub from: String,
    pub to: String,
    pub interface: String,
}

// ── Members+clients handlers (mTLS) ───────────────────────────────────────

pub async fn fence_acquire(
    State(state): State<AppState>,
    Json(req): Json<AcquireReq>,
) -> Json<AcquireResp> {
    if req.agent.is_empty() || req.link.is_empty() {
        return Json(AcquireResp {
            ok: false,
            token: None,
            ttl: None,
            holder: None,
            reason: Some("invalid"),
        });
    }
    match state
        .fence
        .acquire(&req.agent, &req.link, req.ttl_secs.max(1))
    {
        Ok(l) => Json(AcquireResp {
            ok: true,
            token: Some(l.token),
            ttl: Some(l.ttl_secs),
            holder: None,
            reason: None,
        }),
        Err(l) => Json(AcquireResp {
            ok: false,
            token: None,
            ttl: None,
            holder: Some(l.holder),
            reason: Some("held"),
        }),
    }
}

pub async fn fence_release(
    State(state): State<AppState>,
    Json(req): Json<ReleaseReq>,
) -> Json<AcquireResp> {
    let ok = state.fence.release(&req.link, &req.token);
    Json(AcquireResp {
        ok,
        token: None,
        ttl: None,
        holder: None,
        reason: if ok { None } else { Some("notheld") },
    })
}

pub async fn post_quality(
    State(state): State<AppState>,
    Json(req): Json<QualityReq>,
) -> Result<Json<QualityResp>, StatusCode> {
    let (quality, ospf_cost) = ingest_quality(
        &state,
        Measure {
            link: req.link,
            ts: req.ts,
            rtt_ms: req.rtt_ms,
            loss_pct: req.loss_pct,
            rr_tps: req.rr_tps,
            tcp_mbps: req.tcp_mbps,
            udp_mbps: req.udp_mbps,
            util_mbps: req.util_mbps,
            probe_state: req.state,
            token: req.token,
        },
    );
    Ok(Json(QualityResp {
        accepted: true,
        quality,
        ospf_cost,
    }))
}

/// A single measurement to ingest (avoids a 11-arg function; clippy too_many_arguments).
struct Measure {
    link: LinkKey,
    ts: Option<u64>,
    rtt_ms: f64,
    loss_pct: f64,
    rr_tps: f64,
    tcp_mbps: Option<f64>,
    udp_mbps: Option<f64>,
    util_mbps: f64,
    probe_state: ProbeState,
    token: Option<String>,
}

/// Shared measurement ingest: classify, store (LWW), and release the fence
/// when a gated throughput report echoes its token.
fn ingest_quality(state: &AppState, m: Measure) -> (Option<Quality>, Option<u32>) {
    let mut rec = QualityRecord::new(
        m.rtt_ms,
        m.loss_pct,
        m.rr_tps,
        m.tcp_mbps,
        m.udp_mbps,
        m.util_mbps,
        m.probe_state,
    );
    if let Some(ts) = m.ts {
        rec.ts = ts;
    }

    let quality = quality::classify(&state.cfg.quality, &rec);
    rec.quality = quality;
    rec.ospf_cost = quality.map(quality::cost_for_quality);

    let ospf_cost = rec.ospf_cost; // Copy
    let link_id = m.link.id();
    state.kv.put(m.link, rec);
    bump_reports();

    // A gated throughput report carries the fence token — release the lease.
    if let Some(t) = m.token {
        state.fence.release(&link_id, &t);
    }

    (quality, ospf_cost)
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
    state.workers.register(&req.agent, req.links);

    // Seed current policy for the agent's links.
    let mut commands = Vec::new();
    for (agent, link) in state.workers.owned_links() {
        if agent != req.agent {
            continue;
        }
        if let Some(cost) = state.kv.get(&link).and_then(|r| r.ospf_cost) {
            if state.workers.applied_sent(&agent, &link) != Some(cost) {
                state.workers.set_applied_sent(&agent, &link, cost);
                commands.push(crate::worker::WorkCmd::ApplyCost {
                    id: format!("apply/{}/{}", agent, link.id()),
                    link: link.clone(),
                    cost,
                });
            }
        }
    }
    Json(crate::worker::RegisterResp { ok: true, commands })
}

/// Pull (and drain) pending commands for an agent.
pub async fn work_pull(
    State(state): State<AppState>,
    Json(req): Json<crate::worker::WorkReq>,
) -> Json<crate::worker::WorkResp> {
    let commands = state.workers.drain(&req.agent);
    Json(crate::worker::WorkResp { commands })
}

/// Acknowledge an applied OSPF cost.
pub async fn apply_ack(
    State(state): State<AppState>,
    Json(req): Json<crate::worker::ApplyAckReq>,
) -> Json<serde_json::Value> {
    state
        .workers
        .set_applied_sent(&req.agent, &req.link, req.cost);
    Json(serde_json::json!({"ok": true}))
}

/// Long-poll command fetch.
#[derive(Debug, Deserialize)]
pub struct CommandReq {
    pub agent: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    30_000
}

/// Long-polled work fetch: returns queued commands immediately, otherwise
/// holds the request until a command is queued (scheduler `notify`) or the
/// timeout expires. `timeout_ms == 0` behaves like the plain drain endpoint.
pub async fn command_long_poll(
    State(state): State<AppState>,
    Json(req): Json<CommandReq>,
) -> Json<crate::worker::WorkResp> {
    let timeout = std::time::Duration::from_millis(req.timeout_ms.min(120_000));
    if timeout.is_zero() {
        return Json(crate::worker::WorkResp {
            commands: state.workers.drain(&req.agent),
        });
    }
    let notify = state.workers.notify(&req.agent);
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let commands = state.workers.drain(&req.agent);
        if !commands.is_empty() {
            return Json(crate::worker::WorkResp { commands });
        }
        tokio::select! {
            _ = notify.notified() => continue,
            _ = tokio::time::sleep_until(deadline) => {
                return Json(crate::worker::WorkResp { commands: Vec::new() });
            }
        }
    }
}

/// A reply for a previously dispatched command. Every reply echoes the
/// command's job identifier (`id`).
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentReply {
    Probe {
        agent: String,
        id: String,
        link: LinkKey,
        ts: Option<u64>,
        rtt_ms: f64,
        loss_pct: f64,
        rr_tps: f64,
        #[serde(default)]
        tcp_mbps: Option<f64>,
        #[serde(default)]
        udp_mbps: Option<f64>,
        util_mbps: f64,
        state: ProbeState,
        #[serde(default)]
        token: Option<String>,
    },
    Applied {
        agent: String,
        id: String,
        link: LinkKey,
        cost: u32,
    },
}

/// `/v1/agent/reply`: agent→controller results for previously issued commands.
pub async fn agent_reply(
    State(state): State<AppState>,
    Json(reply): Json<AgentReply>,
) -> Json<serde_json::Value> {
    match reply {
        AgentReply::Probe {
            agent: _,
            id,
            link,
            ts,
            rtt_ms,
            loss_pct,
            rr_tps,
            tcp_mbps,
            udp_mbps,
            util_mbps,
            state: probe_state,
            token,
        } => {
            let (quality, ospf_cost) = ingest_quality(
                &state,
                Measure {
                    link,
                    ts,
                    rtt_ms,
                    loss_pct,
                    rr_tps,
                    tcp_mbps,
                    udp_mbps,
                    util_mbps,
                    probe_state,
                    token,
                },
            );
            Json(serde_json::json!({
                "ok": true, "id": id, "quality": quality, "ospf_cost": ospf_cost,
            }))
        }
        AgentReply::Applied {
            agent,
            id,
            link,
            cost,
        } => {
            state.workers.set_applied_sent(&agent, &link, cost);
            Json(serde_json::json!({"ok": true, "id": id}))
        }
    }
}

// ── Admin handlers (plain HTTP on loopback) ───────────────────────────────

pub async fn index(State(state): State<AppState>) -> Html<String> {
    Html(crate::dashboard::render(&state))
}

pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut body = String::new();
    body.push_str(&format!(
        "# HELP mielofon_quality_reports_total quality reports accepted\n# TYPE mielofon_quality_reports_total counter\nmielofon_quality_reports_total {}\n",
        reports_total()
    ));
    body.push_str("# HELP mielofon_links number of known links\n# TYPE mielofon_links gauge\n");
    body.push_str(&format!("mielofon_links {}\n", state.kv.len()));

    for (key, rec) in state.kv.all() {
        let labels = format!(
            "from=\"{}\",to=\"{}\",iface=\"{}\"",
            key.from, key.to, key.interface
        );
        body.push_str(&format!(
            "mielofon_link_rtt_ms{{{labels}}} {}\n",
            rec.rtt_ms
        ));
        body.push_str(&format!(
            "mielofon_link_loss_pct{{{labels}}} {}\n",
            rec.loss_pct
        ));
        let quality = rec
            .quality
            .map(|q| format!("{:?}", q).to_lowercase())
            .unwrap_or_else(|| "unknown".to_string());
        body.push_str(&format!(
            "mielofon_link_quality{{{labels}}} {}\n",
            quality_label_value(&quality)
        ));
        if let Some(cost) = rec.ospf_cost {
            body.push_str(&format!("mielofon_link_ospf_cost{{{labels}}} {cost}\n"));
        }
    }

    let mut response = body.into_response();
    if let Ok(h) = header::HeaderValue::from_str("text/plain; version=0.0.4") {
        response.headers_mut().insert(header::CONTENT_TYPE, h);
    }
    response
}

fn quality_label_value(q: &str) -> u8 {
    match q {
        "good" => 0,
        "acceptable" => 1,
        "poor" => 2,
        "bad" => 3,
        _ => 99,
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
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
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
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        )
            .into_response(),
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
    })
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

pub fn members_router() -> Router<AppState> {
    Router::new()
        .route("/v1/ping", get(ping))
        .route("/v1/gossip/exchange", post(crate::gossip::exchange))
}

pub fn clients_router() -> Router<AppState> {
    Router::new()
        .route("/v1/quality", post(post_quality))
        .route("/v1/fence/acquire", post(fence_acquire))
        .route("/v1/fence/release", post(fence_release))
        .route("/v1/agent/register", post(register_agent))
        .route("/v1/agent/work", post(work_pull))
        .route("/v1/agent/command", post(command_long_poll))
        .route("/v1/agent/reply", post(agent_reply))
        .route("/v1/apply/ack", post(apply_ack))
}

pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/quality/all", get(get_quality_all))
        .route("/v1/quality", get(get_quality))
        .route("/v1/policy", get(get_policy))
        .route("/v1/status", get(get_status))
}

// ── Prometheus counters ────────────────────────────────────────────────────

fn bump_reports() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed);
}

fn reports_total() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.load(Ordering::Relaxed)
}
