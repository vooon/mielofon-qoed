//! Concurrent last-write-wins store of per-link quality records, plus the
//! node directory (router id → registered name) replicated the same way.

use crate::model::{LinkKey, NodeEntry, QualityRecord};
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct LwwStore {
    inner: RwLock<HashMap<LinkKey, QualityRecord>>,
    nodes: RwLock<HashMap<String, NodeEntry>>,
}

impl LwwStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite by timestamp (LWW: an equal or newer `ts` wins).
    /// Returns false when an older record was rejected.
    pub fn put(&self, key: LinkKey, rec: QualityRecord) -> bool {
        let mut map = self.inner.write().expect("kv lock poisoned");
        if map.get(&key).is_some_and(|prev| rec.ts < prev.ts) {
            return false;
        }
        map.insert(key, rec);
        true
    }

    pub fn get(&self, key: &LinkKey) -> Option<QualityRecord> {
        self.inner
            .read()
            .expect("kv lock poisoned")
            .get(key)
            .cloned()
    }

    pub fn all(&self) -> Vec<(LinkKey, QualityRecord)> {
        let map = self.inner.read().expect("kv lock poisoned");
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("kv lock poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// LWW-merge a peer's records (gossip anti-entropy).
    pub fn merge(&self, records: &[(LinkKey, QualityRecord)]) {
        let mut map = self.inner.write().expect("kv lock poisoned");
        for (k, v) in records {
            let prev = map.get(k).map(|p| p.ts).unwrap_or(0);
            if v.ts >= prev {
                map.insert(k.clone(), v.clone());
            }
        }
    }

    // ── node directory ───────────────────────────────────────────────

    /// Record that `rid` registered as `name` with `links` at `ts` (LWW).
    /// Returns true when the directory changed.
    pub fn set_node(&self, rid: &str, name: &str, links: Vec<LinkKey>, ts: u64) -> bool {
        let mut map = self.nodes.write().expect("kv lock poisoned");
        let entry = NodeEntry {
            name: name.to_string(),
            ts,
            links,
        };
        match map.get(rid) {
            Some(prev) if prev.ts > ts => false,
            Some(prev) if prev.ts == ts && prev.name == entry.name && prev.links == entry.links => {
                false
            }
            _ => {
                map.insert(rid.to_string(), entry);
                true
            }
        }
    }

    /// The directory entry of `rid`.
    pub fn node(&self, rid: &str) -> Option<NodeEntry> {
        self.nodes
            .read()
            .expect("kv lock poisoned")
            .get(rid)
            .cloned()
    }

    /// The name registered for `rid`, if any member has seen it.
    pub fn node_name(&self, rid: &str) -> Option<String> {
        self.nodes
            .read()
            .expect("kv lock poisoned")
            .get(rid)
            .map(|e| e.name.clone())
    }

    pub fn nodes(&self) -> Vec<(String, NodeEntry)> {
        let map = self.nodes.read().expect("kv lock poisoned");
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// LWW-merge a peer's directory. Returns true when anything changed.
    /// Names are never expired: a node keeps its name until it registers
    /// under another one.
    pub fn merge_nodes(&self, nodes: &[(String, NodeEntry)]) -> bool {
        let mut map = self.nodes.write().expect("kv lock poisoned");
        let mut changed = false;
        for (rid, e) in nodes {
            let newer = match map.get(rid) {
                Some(prev) => {
                    e.ts > prev.ts
                        || (e.ts == prev.ts && (e.name != prev.name || e.links != prev.links))
                }
                None => true,
            };
            if newer {
                map.insert(rid.clone(), e.clone());
                changed = true;
            }
        }
        changed
    }

    /// Drop records older than `max_age_secs` (LWW store expires stale entries).
    pub fn prune(&self, now_secs: u64, max_age_secs: u64) {
        let mut map = self.inner.write().expect("kv lock poisoned");
        map.retain(|_, v| now_secs.saturating_sub(v.ts) <= max_age_secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProbeState;

    #[test]
    fn lww_keeps_latest_ts() {
        let store = LwwStore::new();
        let key = LinkKey::new("hub-a", "hub-b", "awg0");
        let mut old = QualityRecord::new(
            Some(10.0),
            Some(0.0),
            Some(80.0),
            Some(70.0),
            None,
            0.0,
            ProbeState::Quiet,
        );
        old.ts = 100;
        store.put(key.clone(), old);
        let mut new = QualityRecord::new(
            Some(200.0),
            Some(90.0),
            Some(5.0),
            None,
            None,
            0.0,
            ProbeState::Conflict,
        );
        new.ts = 200;
        store.put(key.clone(), new.clone());
        assert_eq!(store.get(&key).unwrap().ts, 200);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn node_directory_is_lww_and_never_expires() {
        let store = LwwStore::new();
        let l = vec![LinkKey::new("node-0.example", "hub-a", "awg_hub_a")];
        assert!(store.set_node("192.0.2.1", "node-0.example", l.clone(), 100));
        assert!(
            !store.set_node("192.0.2.1", "node-0.example", l.clone(), 100),
            "same again"
        );
        assert!(
            store.set_node("192.0.2.1", "node-0.example", vec![], 100),
            "links changed"
        );
        assert!(
            !store.set_node("192.0.2.1", "old.example", vec![], 50),
            "older loses"
        );
        assert!(store.set_node("192.0.2.1", "renamed.example", l.clone(), 200));
        assert_eq!(
            store.node_name("192.0.2.1").as_deref(),
            Some("renamed.example")
        );
        assert_eq!(store.node("192.0.2.1").unwrap().links, l);
        assert!(store.merge_nodes(&[(
            "192.0.2.2".into(),
            NodeEntry {
                name: "hub-a".into(),
                ts: 1,
                links: vec![]
            }
        )]));
        assert!(!store.merge_nodes(&[(
            "192.0.2.1".into(),
            NodeEntry {
                name: "stale.example".into(),
                ts: 150,
                links: vec![]
            }
        )]));
        store.prune(10_000, 10);
        assert_eq!(store.nodes().len(), 2, "pruning links leaves names alone");
    }

    #[test]
    fn older_does_not_override() {
        let store = LwwStore::new();
        let key = LinkKey::new("a", "p", "if0");
        let mut fresh = QualityRecord::new(
            Some(1.0),
            Some(0.0),
            Some(1.0),
            None,
            None,
            0.0,
            ProbeState::Quiet,
        );
        fresh.ts = 500;
        store.put(key.clone(), fresh);
        let mut stale = QualityRecord::new(
            Some(2.0),
            Some(0.0),
            Some(1.0),
            None,
            None,
            0.0,
            ProbeState::Quiet,
        );
        stale.ts = 400;
        assert!(!store.put(key.clone(), stale));
        assert_eq!(store.get(&key).unwrap().ts, 500);
    }
}
