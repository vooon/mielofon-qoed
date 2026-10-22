//! Wall-clock helper shared by every controller module.

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix time in whole seconds (0 if the clock is before the epoch).
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
