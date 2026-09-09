//! Embedded static frontend (the dashboard SPA) served by the admin listener.
//!
//! The Vue 3 build output lives in `crates/controller/frontend/dist` and is
//! compiled into the binary with `rust-embed` (Rust's answer to `go:embed`).
//! `build.rs` rebuilds it on demand so the daemon always embeds the current
//! bundle. A single lookup function resolves a request path (including
//! `index.html` fallback) to a (content-type, body) pair.

use axum::http::header;
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "frontend/dist/"]
struct Frontend;

/// The bundled vis-network library (Apache-2.0, visjs), separate from the Vue
/// build so it is not re-emitted by Vite.
pub const VIS_NETWORK_JS: &[u8] = include_bytes!("../static/vis-network.min.js");

fn mime(path: &str) -> &'static str {
    if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "text/html; charset=utf-8"
    }
}

fn respond(data: Vec<u8>, path: &str) -> Response {
    let mut resp = data.into_response();
    if let Ok(h) = header::HeaderValue::from_str(mime(path)) {
        resp.headers_mut().insert(header::CONTENT_TYPE, h);
    }
    resp
}

/// Resolve a request path to a static response. `/` returns the SPA shell;
/// `/assets/*` and `/static/*` return the exact embedded file. The bundled
/// vis-network library is served from `/static/vis-network.min.js`.
/// Returns `None` for a missing asset (caller decides on 404).
pub fn get(path: &str) -> Option<Response> {
    let rel = path.trim_start_matches('/');

    if rel == "static/vis-network.min.js" || rel == "vis-network.min.js" {
        return Some(respond(VIS_NETWORK_JS.to_vec(), "vis-network.min.js"));
    }

    let file = if rel.is_empty() || rel == "." {
        "index.html"
    } else {
        rel
    };
    let data = Frontend::get(file).map(|f| f.data.to_vec())?;
    Some(respond(data, file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn body_bytes(resp: Response) -> Vec<u8> {
        resp.into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn serves_spa_shell() {
        let resp = get("/").expect("index served");
        let html = String::from_utf8(body_bytes(resp).await).unwrap();
        assert!(html.contains("<div id=\"app\"></div>"));
        assert!(html.contains("vis-network"));
        let ct = get("/")
            .and_then(|r| r.headers().get(header::CONTENT_TYPE).cloned())
            .expect("content-type");
        assert!(ct.to_str().unwrap().contains("text/html"));
    }

    #[tokio::test]
    async fn serves_embedded_vis_network() {
        let resp = get("/static/vis-network.min.js").expect("vis served");
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let js = body_bytes(resp).await;
        assert!(String::from_utf8_lossy(&js).contains("vis") && js.len() > 100_000);
        assert!(ct.contains("javascript"));
    }

    #[tokio::test]
    async fn unknown_asset_404s() {
        assert!(get("/assets/does-not-exist.js").is_none());
    }
}
