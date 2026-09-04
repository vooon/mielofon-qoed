//! Shared application state for router handlers.

use crate::config::Config;
use crate::fence::Fence;
use crate::kv::LwwStore;
use crate::worker::WorkerRegistry;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Readiness: true once the node has initialised listeners/gossip and can serve.
pub struct Ready(pub AtomicBool);

/// Per-membership peer liveness, populated by the gossip loop.
#[derive(Debug, Clone, Serialize)]
pub struct PeerHealthEntry {
    /// Unix epoch seconds of the last successful exchange/ping with the peer.
    pub last_ok_ts: Option<u64>,
    /// Measured round-trip of the last successful push, in milliseconds.
    pub rtt_ms: Option<u64>,
}

/// Liveness map keyed by member name (see `Config.members`).
#[derive(Default)]
pub struct PeerHealth {
    inner: RwLock<HashMap<String, PeerHealthEntry>>,
}

impl PeerHealth {
    pub fn new() -> Self {
        PeerHealth::default()
    }

    /// Record a successful round-trip with `peer`.
    pub fn mark_ok(&self, peer: &str, rtt_ms: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.insert(
            peer.to_string(),
            PeerHealthEntry {
                last_ok_ts: Some(now),
                rtt_ms: Some(rtt_ms),
            },
        );
    }

    /// All known peer entries (name -> health).
    pub fn snapshot(&self) -> Vec<(String, PeerHealthEntry)> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub kv: Arc<LwwStore>,
    pub fence: Arc<Fence>,
    pub workers: Arc<WorkerRegistry>,
    pub peers: Arc<PeerHealth>,
    pub ready: Arc<Ready>,
    pub started_at: Arc<Instant>,
    /// Trace walker bookkeeping (pending hop replies + result TTL cache).
    pub trace: Arc<crate::trace::TraceRegistry>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        AppState {
            cfg: Arc::new(cfg),
            kv: Arc::new(LwwStore::new()),
            fence: Arc::new(Fence::new()),
            workers: Arc::new(WorkerRegistry::new()),
            peers: Arc::new(PeerHealth::new()),
            ready: Arc::new(Ready(AtomicBool::new(false))),
            started_at: Arc::new(Instant::now()),
            trace: Arc::new(crate::trace::TraceRegistry::new()),
        }
    }

    pub fn set_ready(&self, v: bool) {
        self.ready.0.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}
