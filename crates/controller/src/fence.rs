//! Probe mutual-exclusion fence (soft lease).
//!
//! Only the intrusive throughput tier needs the fence. A throughput test
//! occupies **both endpoints** of its link: the `from` node runs `iperf3 -c`
//! and the `to` node's single `iperf3 -s` serves it, and a node busy in
//! either role cannot take part in another test (the server answers "busy",
//! the client would measure two tests sharing an uplink). A lease is granted
//! only when neither endpoint is in a live lease, so tests on disjoint node
//! pairs (`hub-a→hub-b` and `hub-c→hub-d`) run concurrently while
//! `node-0→hub-b` waits for `node-0→hub-c` and for `hub-a→hub-b` alike.
//! Expiry without release lets another test take the nodes over. Because
//! cluster consistency is eventual, a rare overlap is tolerated and reported
//! by the agent as `conflict`.

use crate::clock::now_secs;
use mielofon_proto::LinkKey;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Lease {
    pub holder: String,
    /// Directed link id (`from/to/interface`); the key the reply releases.
    pub link: String,
    /// The two nodes the test occupies (client, server).
    pub from: String,
    pub to: String,
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

    /// Attempt to acquire the fence for a throughput test on `link`. Succeeds
    /// when neither endpoint of the link is occupied by a live lease (expired
    /// leases are dropped on the way). Returns a lease with a fresh token, or
    /// the live lease that occupies one of the endpoints.
    pub fn acquire(&self, agent: &str, link: &LinkKey, ttl_secs: u64) -> Result<Lease, Box<Lease>> {
        let now = now_secs();
        let mut map = self.lease.lock().expect("fence lock poisoned");
        map.retain(|_, l| !l.expired(now));
        if let Some(busy) = map.values().find(|l| l.occupies(link)) {
            return Err(Box::new(busy.clone()));
        }
        let lease = new_lease(agent, link, ttl_secs, now);
        map.insert(lease.link.clone(), lease.clone());
        Ok(lease)
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

    /// Whether a test on `link` would share a node with this lease.
    fn occupies(&self, link: &LinkKey) -> bool {
        [&self.from, &self.to]
            .iter()
            .any(|n| **n == link.from || **n == link.to)
    }
}

fn new_lease(agent: &str, link: &LinkKey, ttl_secs: u64, now: u64) -> Lease {
    Lease {
        holder: agent.to_string(),
        link: link.id(),
        from: link.from.clone(),
        to: link.to.clone(),
        token: uuid4(),
        issued_at: now,
        ttl_secs,
    }
}

/// Fresh unique token for a newly acquired lease.
fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(from: &str, to: &str) -> LinkKey {
        LinkKey::new(from, to, format!("awg_{to}"))
    }

    #[test]
    fn acquire_then_block_second() {
        let f = Fence::new();
        let ok = f.acquire("node-0", &link("node-0", "hub-a"), 30).unwrap();
        assert_eq!(ok.holder, "node-0");
        let err = f
            .acquire("node-0", &link("node-0", "hub-a"), 30)
            .unwrap_err();
        assert_eq!(err.holder, "node-0");
    }

    #[test]
    fn a_node_is_in_one_test_at_a_time_in_either_role() {
        let f = Fence::new();
        // node-0 -> hub-b occupies node-0 (client) and hub-b (server).
        let held = f.acquire("node-0", &link("node-0", "hub-b"), 30).unwrap();
        // Same client, other server: blocked (one iperf3 -c per node).
        assert_eq!(
            f.acquire("node-0", &link("node-0", "hub-c"), 30)
                .unwrap_err()
                .link,
            held.link
        );
        // Other client, same server: blocked (one iperf3 -s per node).
        assert!(f.acquire("hub-a", &link("hub-a", "hub-b"), 30).is_err());
        // The server of a running test cannot start its own client test.
        assert!(f.acquire("hub-b", &link("hub-b", "hub-c"), 30).is_err());
        // The reverse half is the same two nodes.
        assert!(f.acquire("hub-b", &link("hub-b", "node-0"), 30).is_err());
        // A disjoint pair runs concurrently.
        assert!(f.acquire("hub-a", &link("hub-a", "hub-c"), 30).is_ok());
        assert_eq!(f.leases().len(), 2);
        // Released: node-0 and hub-b are free again.
        assert!(f.release(&held.link, &held.token));
        assert!(f.acquire("node-0", &link("node-0", "hub-b"), 30).is_ok());
    }

    #[test]
    fn expired_lease_can_be_taken_over() {
        let f = Fence::new();
        let l1 = f.acquire("node-0", &link("node-0", "hub-a"), 0).unwrap();
        // ttl 0 => already expired: the nodes are free and the entry dropped.
        let l2 = f.acquire("hub-a", &link("hub-a", "node-0"), 30).unwrap();
        assert_eq!(l2.holder, "hub-a");
        assert_eq!(f.leases().len(), 1);
        let _ = l1;
    }

    #[test]
    fn release_only_with_token() {
        let f = Fence::new();
        let l = f.acquire("node-0", &link("node-0", "hub-a"), 30).unwrap();
        assert!(!f.release(&l.link, "nope"));
        assert!(f.release(&l.link, &l.token));
        assert!(f.acquire("hub-a", &link("hub-a", "node-0"), 30).is_ok());
    }
}
