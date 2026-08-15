//! Concurrent last-write-wins store of per-link quality records.

use crate::model::{LinkKey, QualityRecord};
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
pub struct LwwStore {
    inner: RwLock<HashMap<LinkKey, QualityRecord>>,
}

impl LwwStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite by timestamp (LWW). Returns the previous record, if any.
    pub fn put(&self, key: LinkKey, mut rec: QualityRecord) -> Option<QualityRecord> {
        let mut map = self.inner.write().expect("kv lock poisoned");
        let prev = map.get(&key);
        let prev_ts = prev.map(|p| p.ts).unwrap_or(0);
        if rec.ts < prev_ts {
            return prev.cloned();
        }
        if let Some(p) = prev {
            if p.ts == rec.ts {
                // best-effort conflict stamping is the caller's job; keep newest by craft
                rec.ts = rec.ts.max(p.ts);
            }
        }
        map.insert(key, rec.clone());
        Some(rec)
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

    /// Replace the entire store with `records` (used by gossip anti-entropy merge).
    pub fn merge(&self, records: &[(LinkKey, QualityRecord)]) {
        let mut map = self.inner.write().expect("kv lock poisoned");
        for (k, v) in records {
            let prev = map.get(k).map(|p| p.ts).unwrap_or(0);
            if v.ts >= prev {
                map.insert(k.clone(), v.clone());
            }
        }
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
        let mut old = QualityRecord::new(10.0, 0.0, 80.0, Some(70.0), None, 0.0, ProbeState::Quiet);
        old.ts = 100;
        store.put(key.clone(), old);
        let mut new = QualityRecord::new(200.0, 90.0, 5.0, None, None, 0.0, ProbeState::Conflict);
        new.ts = 200;
        store.put(key.clone(), new.clone());
        assert_eq!(store.get(&key).unwrap().ts, 200);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn older_does_not_override() {
        let store = LwwStore::new();
        let key = LinkKey::new("a", "p", "if0");
        let mut fresh = QualityRecord::new(1.0, 0.0, 1.0, None, None, 0.0, ProbeState::Quiet);
        fresh.ts = 500;
        store.put(key.clone(), fresh);
        let mut stale = QualityRecord::new(2.0, 0.0, 1.0, None, None, 0.0, ProbeState::Quiet);
        stale.ts = 400;
        store.put(key.clone(), stale);
        assert_eq!(store.get(&key).unwrap().ts, 500);
    }
}
