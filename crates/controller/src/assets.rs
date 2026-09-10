//! Static frontend (the dashboard SPA) served by the admin listener.
//!
//! The Vue 3 build output lives in `frontend/dist` (repo root) and is
//! installed by the OpenWrt package into the configured `[frontend] root_dir`
//! (default `/usr/share/mielofon/dashboard`). The daemon serves those files
//! straight off disk — no embedding — so a package update can ship a new
//! bundle without rebuilding the daemon.
//!
//! A single lookup function resolves a request path (including `index.html`
//! fallback) to a (content-type, body) pair. The lookup is rooted and refuses
//! any path that would escape `root_dir`.

use axum::http::header;
use axum::response::{IntoResponse, Response};
use std::path::{Component, Path, PathBuf};

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

/// Resolve a request path to an absolute, root-confined file path inside
/// `root_dir`. `/` maps to `index.html` (SPA fallback). Returns `None` for any
/// path that would escape the root (e.g. `..`) so the caller can 404 safely.
fn resolve(root_dir: &str, rel: &str) -> Option<PathBuf> {
    let root = Path::new(root_dir);
    // Trims any leading '/' so an incoming path (with or without the slash
    // axum's catch-all strips) is always joined relative to root_dir.
    let rel = rel.trim_start_matches('/');
    let rel = if rel.is_empty() || rel == "." {
        "index.html"
    } else {
        rel
    };
    let candidate = root.join(rel);

    // Reject traversal: the joined path must never contain a ParentDir ("..")
    // or a Windows-style Prefix component, otherwise an asset path could escape
    // root_dir. The leading RootDir of the (absolute) root and CurDir of the
    // SPA root are fine; rel is already stripped of any leading '/' and ".".
    for comp in candidate.components() {
        if matches!(comp, Component::ParentDir | Component::Prefix(_)) {
            return None;
        }
    }
    Some(candidate)
}

/// Serve a file from the frontend `root_dir`. Returns `None` for a missing or
/// out-of-root file (caller decides on 404).
pub fn get(root_dir: &str, rel: &str) -> Option<Response> {
    let path = resolve(root_dir, rel)?;
    let data = std::fs::read(&path).ok()?;
    let name = path.file_name()?.to_str()?;
    let mut resp = data.into_response();
    if let Ok(h) = header::HeaderValue::from_str(mime(name)) {
        resp.headers_mut().insert(header::CONTENT_TYPE, h);
    }
    Some(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        assert!(resolve("/usr/share/mielofon/dashboard", "../config.toml").is_none());
        assert!(resolve("/usr/share/mielofon/dashboard", "assets/../../ca.pem").is_none());
        assert!(resolve("/usr/share/mielofon/dashboard", "..").is_none());
        // Absolute input is rooted (leading '/' stripped), never an escape.
        let p = resolve("/usr/share/mielofon/dashboard", "/etc/passwd").expect("absolute rooted");
        assert_eq!(p, Path::new("/usr/share/mielofon/dashboard/etc/passwd"),);
    }

    #[test]
    fn maps_root_to_index() {
        let p = resolve("/srv", "/").expect("index mapped");
        assert_eq!(p, Path::new("/srv/index.html"));
        assert_eq!(resolve("/srv", ""), resolve("/srv", "/"));
    }

    #[test]
    fn resolves_nested_assets() {
        let p = resolve("/srv", "/assets/index-x.js").expect("asset resolved");
        assert_eq!(p, Path::new("/srv/assets/index-x.js"));
    }
}
