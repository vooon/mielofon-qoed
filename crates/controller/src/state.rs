//! Shared application state for router handlers.

use crate::config::Config;
use crate::fence::Fence;
use crate::kv::LwwStore;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

/// Readiness: true once the node has initialised listeners/gossip and can serve.
pub struct Ready(pub AtomicBool);

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub kv: Arc<LwwStore>,
    pub fence: Arc<Fence>,
    pub ready: Arc<Ready>,
    pub started_at: Arc<Instant>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        AppState {
            cfg: Arc::new(cfg),
            kv: Arc::new(LwwStore::new()),
            fence: Arc::new(Fence::new()),
            ready: Arc::new(Ready(AtomicBool::new(false))),
            started_at: Arc::new(Instant::now()),
        }
    }

    pub fn set_ready(&self, v: bool) {
        self.ready.0.store(v, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}
