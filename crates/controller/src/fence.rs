//! Probe mutual-exclusion fence (soft lease).
//!
//! Only the intrusive throughput tier needs the fence. A lease is granted when
//! no other agent holds it; expiry without release lets another agent take
//! over. Because cluster consistency is eventual, a rare overlap is tolerated
//! and reported by the agent as `conflict`.

use crate::model::ProbeState;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Lease {
    pub holder: String,
    pub link: String,
    pub token: String,
    pub issued_at: u64,
    pub ttl_secs: u64,
}

pub struct Fence {
    lease: Mutex<HashMap<String, Lease>>,
}

impl Fence {
    pub fn new() -> Self {
        Fence {
            lease: Mutex::new(HashMap::new()),
        }
    }

    /// Attempt to acquire the fence for a throughput probe. Succeeds if idle or
    /// the previous lease expired. Returns a lease with a fresh token, or an
    /// existing lease describing who holds it.
    pub fn acquire(&self, agent: &str, link: &str, ttl_secs: u64) -> Result<Lease, Lease> {
        let now = now_secs();
        let mut map = self.lease.lock().expect("fence lock poisoned");
        match map.entry(link.to_string()) {
            Entry::Occupied(mut e) => {
                let held = e.get_mut();
                if held.expired(now) {
                    *held = new_lease(agent, link, ttl_secs, now);
                    Ok(held.clone())
                } else {
                    Err(held.clone())
                }
            }
            Entry::Vacant(v) => {
                let lease = new_lease(agent, link, ttl_secs, now);
                v.insert(lease.clone());
                Ok(lease)
            }
        }
    }

    /// Release a lease if the token matches.
    pub fn release(&self, link: &str, token: &str) -> bool {
        let mut map = self.lease.lock().expect("fence lock poisoned");
        if let Some(l) = map.get(link) {
            if l.token == token {
                map.remove(link);
                return true;
            }
        }
        false
    }

    /// Non-destructive snapshot of all leases (for `/v1/status`).
    pub fn leases(&self) -> Vec<Lease> {
        let map = self.lease.lock().expect("fence lock poisoned");
        map.values().cloned().collect()
    }
}

impl Default for Fence {
    fn default() -> Self {
        Self::new()
    }
}

impl Lease {
    pub fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.issued_at) >= self.ttl_secs
    }

    /// What the agent should report if it overlaps an existing lease.
    pub fn overlap_state(&self) -> ProbeState {
        ProbeState::Conflict
    }
}

fn new_lease(agent: &str, link: &str, ttl_secs: u64, now: u64) -> Lease {
    Lease {
        holder: agent.to_string(),
        link: link.to_string(),
        token: uuid4(),
        issued_at: now,
        ttl_secs,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Minimal v4-style token generator (no external dep). Not cryptographically
/// meaningful for the fence; it only guards accidental release mistmatching.
fn uuid4() -> String {
    let mut buf = String::new();
    for (i, b) in rand_bytes(16).iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            buf.push('-');
        }
        let _ = std::fmt::Write::write_fmt(&mut buf, format_args!("{:02x}", b));
    }
    buf
}

/// Simple non-crypto 16 random bytes from `/dev/urandom`.
fn rand_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    let mut out = vec![0u8; n];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_then_block_second() {
        let f = Fence::new();
        let ok = f.acquire("agent-1", "sp1-cr", 30).unwrap();
        assert_eq!(ok.holder, "agent-1");
        let err = f.acquire("agent-2", "sp1-cr", 30).unwrap_err();
        assert_eq!(err.holder, "agent-1");
    }

    #[test]
    fn expired_lease_can_be_taken_over() {
        let f = Fence::new();
        let l1 = f.acquire("agent-1", "sp1-cr", 0).unwrap();
        // ttl 0 => already expired
        let l2 = f.acquire("agent-2", "sp1-cr", 30).unwrap();
        assert_eq!(l2.holder, "agent-2");
        let _ = l1;
    }

    #[test]
    fn release_only_with_token() {
        let f = Fence::new();
        let l = f.acquire("agent-1", "sp1-cr", 30).unwrap();
        assert!(!f.release("sp1-cr", "nope"));
        assert!(f.release("sp1-cr", &l.token));
        assert!(f.acquire("agent-2", "sp1-cr", 30).is_ok());
    }
}
