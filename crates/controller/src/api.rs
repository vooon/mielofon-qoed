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
    let mut rec = QualityRecord::new(
        req.rtt_ms,
        req.loss_pct,
        req.rr_tps,
        req.tcp_mbps,
        req.udp_mbps,
        req.util_mbps,
        req.state,
    );
    if let Some(ts) = req.ts {
        rec.ts = ts;
    }

    let q = quality::classify(&state.cfg.quality, &rec);
    rec.quality = q;
    rec.ospf_cost = q.map(quality::cost_for_quality);

    let ospf_cost = rec.ospf_cost; // Copy
    state.kv.put(req.link, rec);
    bump_reports();

    Ok(Json(QualityResp {
        accepted: true,
        quality: q,
        ospf_cost,
    }))
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
    })
}

// ── Router assembly ───────────────────────────────────────────────────────

pub fn members_router() -> Router<AppState> {
    Router::new().route("/v1/gossip/exchange", post(crate::gossip::exchange))
}

pub fn clients_router() -> Router<AppState> {
    Router::new()
        .route("/v1/quality", post(post_quality))
        .route("/v1/fence/acquire", post(fence_acquire))
        .route("/v1/fence/release", post(fence_release))
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
