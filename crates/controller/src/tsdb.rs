//! Miniature embedded, replicated time-series store.
//!
//! Appends one raw sample per ingested quality report (always probes every 15s,
//! throughput every 300s), rolls each sample into a 60s per-link bucket
//! (min/avg/max per dimension), and replicates raw samples across the cluster
//! through the anti-entropy gossip loop (see `gossip.rs`). Buckets are a local
//! rollup — only raw samples travel.
//!
//! Storage is in two layers:
//!   - hot in-memory rings per link (raw bounded by `raw_retention_secs`,
//!     buckets bounded by `agg_retention_secs`) that serve queries and gossip;
//!   - an optional durable copy in an embedded redb file (pure-Rust ACID KV),
//!     flushed in batches by a background task and reloaded at boot, so history
//!     survives controller restarts.
//!
//! Each directed link has exactly one producer (the probing agent), so per-link
//! timestamps are monotonic and gossip `merge_delta` skips anything older than
//! the last ts it saw for a link (idempotent resends are harmless).

use crate::config::TsConfig;
use crate::model::{LinkKey, ProbeState};
use crate::store::{Store, WindowSpec, WindowView};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Length of a per-link aggregate bucket, in seconds.
pub const BUCKET_SECS: u64 = 60;
/// Grows the raw ring by this margin over the retention window (15s cadence).
const RAW_RING_CAP: usize = (7200 * 3) / 15;
/// How many samples a single gossip delta may carry.
pub const DELTA_LIMIT: usize = 2000;

const RAW_TABLE: TableDefinition<(&str, u64), Vec<u8>> = TableDefinition::new("ts_raw");
const AGG_TABLE: TableDefinition<(&str, u64), Vec<u8>> = TableDefinition::new("ts_agg");

fn state_code(s: ProbeState) -> u8 {
    match s {
        ProbeState::Quiet => 0,
        ProbeState::Busy => 1,
        ProbeState::Conflict => 2,
    }
}

/// One ingested measurement. Timestamps are whole seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RawSample {
    pub ts: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rr_tps: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp_mbps: Option<f32>,
    /// Link utilization at the probe moment (Mbps).
    pub util_mbps: f32,
    /// `ProbeState` code (0 quiet, 1 busy, 2 conflict).
    pub state: u8,
}

impl RawSample {
    pub fn from_dims(
        state: ProbeState,
        ts: u64,
        rtt_ms: Option<f64>,
        loss_pct: Option<f64>,
        rr_tps: Option<f64>,
        tcp_mbps: Option<f64>,
        util_mbps: f64,
    ) -> Self {
        // `f64::NAN` (unset on the wire) maps to None.
        let opt = |v: Option<f64>| v.filter(|x| x.is_finite()).map(|x| x as f32);
        RawSample {
            ts,
            rtt_ms: opt(rtt_ms),
            loss_pct: opt(loss_pct),
            rr_tps: opt(rr_tps),
            tcp_mbps: opt(tcp_mbps),
            util_mbps: util_mbps as f32,
            state: state_code(state),
        }
    }
}

/// Running min/avg/max/count aggregate for one dimension.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct DimAgg {
    pub n: u32,
    pub sum: f64,
    pub min: f32,
    pub max: f32,
}

impl DimAgg {
    fn merge(&mut self, v: f32) {
        if self.n == 0 {
            self.n = 1;
            self.sum = v as f64;
            self.min = v;
            self.max = v;
            return;
        }
        self.n += 1;
        self.sum += v as f64;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    fn avg(&self) -> Option<f32> {
        (self.n > 0).then(|| (self.sum / self.n as f64) as f32)
    }

    fn view(self) -> Option<DimView> {
        (self.n > 0).then(|| DimView {
            n: self.n,
            min: self.min,
            avg: self.avg().unwrap_or(0.0),
            max: self.max,
        })
    }
}

/// One 60s per-link aggregate bucket.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Bucket {
    pub bucket: u64,
    /// Number of raw samples folded in.
    pub n: u32,
    pub rtt: DimAgg,
    pub loss: DimAgg,
    pub rr: DimAgg,
    pub tcp: DimAgg,
    pub util: DimAgg,
}

impl Bucket {
    fn add(&mut self, s: &RawSample) {
        self.bucket = s.ts - s.ts % BUCKET_SECS;
        self.n += 1;
        if let Some(v) = s.rtt_ms {
            self.rtt.merge(v);
        }
        if let Some(v) = s.loss_pct {
            self.loss.merge(v);
        }
        if let Some(v) = s.rr_tps {
            self.rr.merge(v);
        }
        if let Some(v) = s.tcp_mbps {
            self.tcp.merge(v);
        }
        self.util.merge(s.util_mbps);
    }
}

/// Serialized bucket view for the query API.
#[derive(Debug, Clone, Serialize)]
pub struct DimView {
    pub n: u32,
    pub min: f32,
    pub avg: f32,
    pub max: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct BucketView {
    pub ts: u64,
    pub n: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt: Option<DimView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss: Option<DimView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rr: Option<DimView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp: Option<DimView>,
    pub util: DimView,
}

/// Result of a time-series query.
#[derive(Debug, Clone, Serialize)]
pub struct TsQueryResp {
    pub link: LinkKey,
    pub since: u64,
    pub until: u64,
    pub samples: Vec<RawSample>,
    pub buckets: Vec<BucketView>,
}

/// Thread-safe store handed to the API/gossip paths.
pub struct Tsdb {
    cfg: TsConfig,
    seq: AtomicU64,
    by_link: RwLock<HashMap<LinkKey, VecDeque<RawSample>>>,
    buckets: RwLock<HashMap<LinkKey, VecDeque<Bucket>>>,
    /// (seq, link, sample), ascending seq — the gossip delta source.
    recent: Mutex<VecDeque<(u64, LinkKey, RawSample)>>,
    /// Highest local sample seq each peer has been sent (gossip watermark).
    sent: Mutex<HashMap<String, u64>>,
    /// Raw samples not yet written to redb (batched by the flush task).
    pending: Mutex<Vec<(LinkKey, RawSample)>>,
    /// Mutated buckets not yet written to redb.
    dirty: Mutex<HashMap<(LinkKey, u64), Bucket>>,
    /// Durable copy, opened when `cfg.path` is non-empty.
    db: Mutex<Option<Database>>,
}

impl Tsdb {
    pub fn new(cfg: TsConfig) -> Self {
        let tsdb = Tsdb {
            cfg,
            seq: AtomicU64::new(1),
            by_link: RwLock::new(HashMap::new()),
            buckets: RwLock::new(HashMap::new()),
            recent: Mutex::new(VecDeque::new()),
            sent: Mutex::new(HashMap::new()),
            pending: Mutex::new(Vec::new()),
            dirty: Mutex::new(HashMap::new()),
            db: Mutex::new(None),
        };
        tsdb.load();
        tsdb
    }

    // ── ingest ───────────────────────────────────────────────────────────

    /// Append a locally-ingested sample (probe reply or POST /v1/quality).
    pub fn append(&self, link: &LinkKey, s: RawSample) {
        let _ = self.ingest(link, s);
    }

    /// Core ingest: dedup against the per-link window and return true when the
    /// sample was new (not already seen at this ts).
    fn ingest(&self, link: &LinkKey, s: RawSample) -> bool {
        {
            let mut map = self.by_link.write().expect("tsdb by_link write");
            let ring = map.entry(link.clone()).or_default();
            if ring.back().is_some_and(|b| b.ts >= s.ts) {
                // Duplicate (resend) or out-of-order — drop, keep newest.
                return false;
            }
            let pos = ring.partition_point(|x| x.ts < s.ts);
            ring.insert(pos, s);
            if ring.len() > RAW_RING_CAP {
                ring.pop_front();
            }
        }

        let bucket_ts = s.ts - s.ts % BUCKET_SECS;
        {
            let mut map = self.buckets.write().expect("tsdb buckets write");
            let ring = map.entry(link.clone()).or_default();
            let pos = ring.partition_point(|b| b.bucket < bucket_ts);
            if pos == ring.len() || ring[pos].bucket != bucket_ts {
                let mut b = Bucket::default();
                b.add(&s);
                ring.insert(pos, b);
            } else {
                ring[pos].add(&s);
            }
            self.dirty
                .lock()
                .expect("tsdb dirty lock")
                .insert((link.clone(), bucket_ts), ring[pos]);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        {
            let mut recent = self.recent.lock().expect("tsdb recent lock");
            recent.push_back((seq, link.clone(), s));
        }

        if self.cfg.path.is_empty() {
            return true;
        }
        self.pending
            .lock()
            .expect("tsdb pending lock")
            .push((link.clone(), s));
        true
    }

    /// Merge samples replicated from a peer (idempotent via the per-link ts
    /// window, exactly like a local append after dedup).
    pub fn merge_delta(&self, samples: &[(LinkKey, RawSample)]) {
        let mut accepted = 0;
        for (link, s) in samples {
            if self.ingest(link, *s) {
                accepted += 1;
            }
        }
        if accepted > 0 {
            tracing::trace!(accepted, "gossip tsdb delta accepted");
        }
    }

    // ── gossip deltas ─────────────────────────────────────────────────────

    /// Samples with seq > `after` (max `limit`), plus the seq of the last one
    /// taken. The caller advances the per-peer watermark with `note_sent`.
    pub fn take_delta(&self, after: u64, limit: usize) -> (Vec<(u64, LinkKey, RawSample)>, u64) {
        let recent = self.recent.lock().expect("tsdb recent lock");
        let start = recent.partition_point(|(seq, _, _)| *seq <= after);
        let n = (recent.len() - start).min(limit);
        let mut out = Vec::with_capacity(n);
        let mut last = after;
        for (seq, link, s) in recent.iter().skip(start).take(n) {
            last = *seq;
            out.push((*seq, link.clone(), *s));
        }
        (out, last)
    }

    pub fn sent_watermark(&self, peer: &str) -> u64 {
        self.sent
            .lock()
            .expect("tsdb sent lock")
            .get(peer)
            .copied()
            .unwrap_or(0)
    }

    pub fn note_sent(&self, peer: &str, watermark: u64) {
        self.sent
            .lock()
            .expect("tsdb sent lock")
            .insert(peer.to_string(), watermark);
    }

    // ── query ─────────────────────────────────────────────────────────────

    pub fn query(&self, link: &LinkKey, since: u64, until: u64) -> TsQueryResp {
        let samples = {
            let map = self.by_link.read().expect("tsdb by_link read");
            let ring = map.get(link);
            let mut out = Vec::new();
            if let Some(ring) = ring {
                for s in ring.iter().filter(|s| s.ts >= since && s.ts < until) {
                    out.push(*s);
                }
            }
            out
        };
        let buckets = {
            let map = self.buckets.read().expect("tsdb buckets read");
            let ring = map.get(link);
            let mut out = Vec::new();
            if let Some(ring) = ring {
                for b in ring
                    .iter()
                    .filter(|b| b.bucket >= since && b.bucket < until)
                {
                    out.push(BucketView {
                        ts: b.bucket,
                        n: b.n,
                        rtt: b.rtt.view(),
                        loss: b.loss.view(),
                        rr: b.rr.view(),
                        tcp: b.tcp.view(),
                        util: b.util.view().unwrap_or(DimView {
                            n: 0,
                            min: 0.0,
                            avg: 0.0,
                            max: 0.0,
                        }),
                    });
                }
            }
            out
        };
        TsQueryResp {
            link: link.clone(),
            since,
            until,
            samples,
            buckets,
        }
    }

    /// All links with at least one sample in memory.
    pub fn links(&self) -> Vec<LinkKey> {
        self.by_link
            .read()
            .expect("tsdb by_link read")
            .keys()
            .cloned()
            .collect()
    }

    // ── retention + durability ────────────────────────────────────────────

    /// Drop memory-ring samples/buckets outside the retention windows.
    pub fn prune(&self, now: u64) {
        let raw_cut = now.saturating_sub(self.cfg.raw_retention_secs);
        {
            let mut map = self.by_link.write().expect("tsdb by_link write");
            for ring in map.values_mut() {
                while ring.front().is_some_and(|s| s.ts < raw_cut) {
                    ring.pop_front();
                }
            }
            map.retain(|_, ring| !ring.is_empty());
        }
        let agg_cut = now.saturating_sub(self.cfg.agg_retention_secs);
        {
            let mut map = self.buckets.write().expect("tsdb buckets write");
            for ring in map.values_mut() {
                while ring.front().is_some_and(|b| b.bucket < agg_cut) {
                    ring.pop_front();
                }
            }
            map.retain(|_, ring| !ring.is_empty());
        }
        {
            let raw_cut = now.saturating_sub(self.cfg.raw_retention_secs);
            let mut recent = self.recent.lock().expect("tsdb recent lock");
            while recent.front().is_some_and(|(_, _, s)| s.ts < raw_cut) {
                recent.pop_front();
            }
        }
    }

    /// Durability: flush pending samples + dirty buckets to redb and prune
    /// redb rows outside the retention windows. No-op when persistence is off.
    pub fn flush(&self) {
        let db_guard = self.db.lock().expect("tsdb db lock");
        let Some(db) = db_guard.as_ref() else {
            // Memory-only mode: nothing to flush.
            return;
        };
        let raw: Vec<(LinkKey, RawSample)> = self
            .pending
            .lock()
            .expect("tsdb pending lock")
            .drain(..)
            .collect();
        let bucket_list: Vec<(LinkKey, u64, Bucket)> = {
            let mut dirty = self.dirty.lock().expect("tsdb dirty lock");
            let out = dirty
                .drain()
                .map(|((link, bucket), b)| (link, bucket, b))
                .collect();
            out
        };
        let now = now_secs();
        let write = db.begin_write();
        let write = match write {
            Ok(w) => w,
            Err(e) => {
                warn!("tsdb: redb begin_write failed: {e}");
                // Re-queue so nothing is lost.
                self.pending.lock().expect("tsdb pending lock").extend(raw);
                return;
            }
        };
        let result: anyhow::Result<()> = (|| {
            let raw_cut = now.saturating_sub(self.cfg.raw_retention_secs);
            let agg_cut = now.saturating_sub(self.cfg.agg_retention_secs);

            {
                let mut t = write.open_table(RAW_TABLE)?;
                for (link, s) in &raw {
                    t.insert(
                        (link.id().as_str(), s.ts),
                        serde_json::to_vec(s).unwrap_or_default(),
                    )?;
                }
            }
            // Prune expired raw rows: collect stale keys first — redb iterators
            // borrow the table, so a second pass does the removes.
            let stale_raw: Vec<(String, u64)> = {
                let t = write.open_table(RAW_TABLE)?;
                let mut out = Vec::new();
                for key in t.iter()? {
                    let item = key?;
                    let (link, ts) = item.0.value();
                    if ts < raw_cut {
                        out.push((link.to_string(), ts));
                    }
                }
                out
            };
            if !stale_raw.is_empty() {
                let mut t = write.open_table(RAW_TABLE)?;
                for (link, ts) in stale_raw {
                    t.remove((link.as_str(), ts))?;
                }
            }

            {
                let mut t = write.open_table(AGG_TABLE)?;
                for (link, bucket, b) in &bucket_list {
                    let v = serde_json::to_vec(b).unwrap_or_default();
                    t.insert((link.id().as_str(), *bucket), v)?;
                }
            }
            let stale_agg: Vec<(String, u64)> = {
                let t = write.open_table(AGG_TABLE)?;
                let mut out = Vec::new();
                for key in t.iter()? {
                    let item = key?;
                    let (link, bucket) = item.0.value();
                    if bucket < agg_cut {
                        out.push((link.to_string(), bucket));
                    }
                }
                out
            };
            if !stale_agg.is_empty() {
                let mut t = write.open_table(AGG_TABLE)?;
                for (link, bucket) in stale_agg {
                    t.remove((link.as_str(), bucket))?;
                }
            }

            write.commit()?;
            Ok(())
        })();
        if let Err(e) = result {
            warn!("tsdb: redb flush failed: {e:#}");
        }
    }

    /// Open the redb file (if configured) and hydrate the memory rings.
    fn load(&self) {
        let path = self.cfg.path.as_str();
        if path.is_empty() {
            return;
        }
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Database::create(path) {
            Ok(db) => {
                let read = match db.begin_read() {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("tsdb: redb begin_read failed: {e}");
                        return;
                    }
                };
                if let Ok(t) = read.open_table(RAW_TABLE) {
                    if let Ok(iter) = t.iter() {
                        for item in iter.flatten() {
                            let (link, _ts) = item.0.value();
                            if let Ok(s) = serde_json::from_slice(&item.1.value()) {
                                let key = parse_link_id(link);
                                let _ = self.ingest(&key, s);
                            }
                        }
                    }
                }
                if let Ok(t) = read.open_table(AGG_TABLE) {
                    if let Ok(iter) = t.iter() {
                        for item in iter.flatten() {
                            let (link, bucket) = item.0.value();
                            if let Ok(b) = serde_json::from_slice(&item.1.value()) {
                                let key = parse_link_id(link);
                                let mut map = self.buckets.write().expect("tsdb buckets write");
                                let ring = map.entry(key).or_default();
                                let pos = ring.partition_point(|x| x.bucket < bucket);
                                if pos == ring.len() || ring[pos].bucket != bucket {
                                    ring.insert(pos, b);
                                }
                            }
                        }
                    }
                }
                *self.db.lock().expect("tsdb db lock") = Some(db);
                info_loaded(path);
            }
            Err(e) => warn!("tsdb: cannot open {} for durability: {e}", path),
        }
    }
}

fn parse_link_id(id: &str) -> LinkKey {
    // link id is "from/to/interface" — from/to/iface have no '/'.
    let parts: Vec<&str> = id.splitn(3, '/').collect();
    if parts.len() == 3 {
        LinkKey::new(parts[0], parts[1], parts[2])
    } else {
        LinkKey::new(id, "", "")
    }
}

// ── Store impl (the measurement-store envelope) ──────────────────────────

impl Store for Tsdb {
    fn append(&self, link: &LinkKey, s: RawSample) {
        Tsdb::append(self, link, s);
    }

    fn query(&self, link: &LinkKey, since: u64, until: u64) -> TsQueryResp {
        Tsdb::query(self, link, since, until)
    }

    /// Collapse recent raw samples into the classifier's per-dimension view.
    ///
    /// Semantics follow the RRD approach the classifier wanted: no "no data"
    /// gaps are fabricated. The always-tier dims (rtt/loss/rr) are taken from
    /// the most recent sample within `always_fresh_secs`; the gated throughput
    /// dim is the most recent value within `tcp_fresh_secs`, else the last-known
    /// carried within `tcp_carry_secs`. The per-link timestamps are monotonic
    /// (single producer), so scanning the ring backward from the newest sample
    /// yields exactly the "latest within each horizon" per dimension.
    fn window(&self, link: &LinkKey, spec: &WindowSpec, now: u64) -> Option<WindowView> {
        let map = self.by_link.read().expect("tsdb by_link read");
        let ring = map.get(link)?;

        // Newest sample overall anchors ts/state/util.
        let newest = ring.iter().rev().find(|s| {
            // Only samples inside the widest horizon count as "current".
            now.saturating_sub(s.ts) <= spec.tcp_carry_secs
        })?;
        let mut view = WindowView {
            ts: newest.ts,
            state: code_to_state(newest.state),
            util_mbps: newest.util_mbps as f64,
            ..Default::default()
        };

        // Always-tier dims: freshest value within always_fresh_secs.
        for s in ring.iter().rev() {
            let age = now.saturating_sub(s.ts);
            if age > spec.always_fresh_secs {
                break;
            }
            if view.rtt_ms.is_none() {
                view.rtt_ms = s.rtt_ms.map(|v| v as f64);
            }
            if view.loss_pct.is_none() {
                view.loss_pct = s.loss_pct.map(|v| v as f64);
            }
            if view.rr_tps.is_none() {
                view.rr_tps = s.rr_tps.map(|v| v as f64);
            }
        }

        // Throughput dim: freshest value within tcp_fresh_secs, else the
        // last-known value carried within tcp_carry_secs.
        let mut carried = false;
        for s in ring.iter().rev() {
            let age = now.saturating_sub(s.ts);
            if age > spec.tcp_carry_secs {
                break;
            }
            if let Some(v) = s.tcp_mbps {
                carried = age > spec.tcp_fresh_secs;
                view.tcp_mbps = Some(v as f64);
                break;
            }
        }
        view.tcp_carried = carried;

        Some(view)
    }

    fn merge_delta(&self, samples: &[(LinkKey, RawSample)]) {
        Tsdb::merge_delta(self, samples);
    }

    fn take_delta(&self, after: u64, limit: usize) -> (Vec<(u64, LinkKey, RawSample)>, u64) {
        Tsdb::take_delta(self, after, limit)
    }

    fn sent_watermark(&self, peer: &str) -> u64 {
        Tsdb::sent_watermark(self, peer)
    }

    fn note_sent(&self, peer: &str, watermark: u64) {
        Tsdb::note_sent(self, peer, watermark);
    }

    fn links(&self) -> Vec<LinkKey> {
        Tsdb::links(self)
    }

    fn prune(&self, now: u64) {
        Tsdb::prune(self, now);
    }

    fn flush(&self) {
        Tsdb::flush(self);
    }
}

fn code_to_state(code: u8) -> ProbeState {
    match code {
        0 => ProbeState::Quiet,
        1 => ProbeState::Busy,
        _ => ProbeState::Conflict,
    }
}

fn info_loaded(_path: &str) {}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Background durability + retention loop.
pub async fn flush_loop(store: Arc<dyn Store>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        let now = now_secs();
        store.prune(now);
        store.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TsConfig;
    use crate::model::{LinkKey, ProbeState};

    fn links() -> (LinkKey, LinkKey) {
        (
            LinkKey::new("spoke-1", "hub-a", "awg_hub_a"),
            LinkKey::new("spoke-1", "hub-b", "awg_hub_b"),
        )
    }

    fn sample(ts: u64, rtt: f64) -> RawSample {
        RawSample::from_dims(
            ProbeState::Quiet,
            ts,
            Some(rtt),
            Some(0.0),
            Some(100.0),
            None,
            0.0,
        )
    }

    fn window_spec() -> WindowSpec {
        WindowSpec {
            always_fresh_secs: 60,
            tcp_fresh_secs: 1200,
            tcp_carry_secs: 3600,
        }
    }

    #[test]
    fn window_carries_latest_throughput_despite_fresh_always_probes() {
        let tsdb = Tsdb::new(TsConfig::default());
        let a = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        let base = 1_700_000_000_u64;
        // A throughput sample 300s ago (older than always_fresh but within
        // tcp_fresh), then always probes with no tcp dim every 15s after it.
        tsdb.append(
            &a,
            RawSample::from_dims(ProbeState::Quiet, base, None, None, None, Some(80.0), 0.0),
        );
        for i in 1..=20 {
            tsdb.append(&a, sample(base + i * 15, 15.0 + i as f64));
        }
        let v = tsdb.window(&a, &window_spec(), base + 300).expect("window");
        // tcp is carried from the older throughput sample, not wiped by always.
        assert_eq!(v.tcp_mbps, Some(80.0));
        assert!(!v.tcp_carried, "still within tcp_fresh");
        assert!(v.rtt_ms.is_some());
    }

    #[test]
    fn window_carries_tcp_beyond_fresh_but_within_carry() {
        let tsdb = Tsdb::new(TsConfig::default());
        let a = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        let base = 1_700_000_000_u64;
        tsdb.append(
            &a,
            RawSample::from_dims(ProbeState::Quiet, base, None, None, None, Some(50.0), 0.0),
        );
        // Now 2000s later: past tcp_fresh (1200) but within carry (3600).
        tsdb.append(&a, sample(base + 2000, 20.0));
        let v = tsdb
            .window(&a, &window_spec(), base + 2000)
            .expect("window");
        assert_eq!(v.tcp_mbps, Some(50.0));
        assert!(v.tcp_carried, "beyond tcp_fresh, still carried");
        // Always dims come from the fresh probe at base+2000.
        assert_eq!(v.rtt_ms, Some(20.0));
    }

    #[test]
    fn window_drops_tcp_past_carry_horizon() {
        let tsdb = Tsdb::new(TsConfig::default());
        let a = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        let base = 1_700_000_000_u64;
        tsdb.append(
            &a,
            RawSample::from_dims(ProbeState::Quiet, base, None, None, None, Some(50.0), 0.0),
        );
        // Beyond the 3600s carry horizon => the link has no current data.
        let v = tsdb.window(&a, &window_spec(), base + 7200);
        assert!(v.is_none());
    }

    #[test]
    fn append_and_query_monotonic() {
        let tsdb = Tsdb::new(TsConfig::default()); // memory-only in tests
        let (a, _) = links();
        // Base aligned to the 60s bucket boundary (1_700_000_040 % 60 == 0).
        let base = 1_700_000_040_u64;
        for i in 0..10 {
            tsdb.append(&a, sample(base + i, 10.0 + i as f64));
        }
        let r = tsdb.query(&a, base, base + 100);
        assert_eq!(r.samples.len(), 10);
        assert_eq!(r.samples[0].ts, base);
        // Aggregation: all 10 in one 60s bucket.
        assert_eq!(r.buckets.len(), 1);
        let b = r.buckets[0].clone();
        assert_eq!(b.ts, base);
        assert_eq!(b.n, 10);
        let v = b.loss.unwrap();
        assert_eq!(v.min, 0.0);
        let avg = b.rtt.map(|x| x.avg).unwrap();
        assert!((avg - 14.5).abs() < 1e-3, "avg {avg}");
    }

    #[test]
    fn duplicate_ts_is_dropped() {
        let tsdb = Tsdb::new(TsConfig::default());
        let (a, _) = links();
        tsdb.append(&a, sample(100, 5.0));
        tsdb.append(&a, sample(100, 5.0)); // resend
        assert_eq!(tsdb.query(&a, 0, 1000).samples.len(), 1);
    }

    #[test]
    fn retention_prunes_old_rings() {
        let tsdb = Tsdb::new(TsConfig::default());
        let (a, _) = links();
        tsdb.append(&a, sample(1000, 5.0));
        tsdb.append(&a, sample(2000, 5.0));
        // raw_retention default 7200 → nothing drops at now=10000 minus...
        tsdb.prune(4000); // 4000 - 7200 < 1000 → keeps both
        assert_eq!(tsdb.query(&a, 0, 99999).samples.len(), 2);
        tsdb.prune(100000); // cutoff 92800 → both dropped
        assert_eq!(tsdb.query(&a, 0, 99999).samples.len(), 0);
    }

    #[test]
    fn delta_watermark_traffic() {
        let tsdb = Tsdb::new(TsConfig::default());
        let a = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        for i in 0..5 {
            tsdb.append(&a, sample(2000 + i, 1.0));
        }
        let (d, wm) = tsdb.take_delta(0, 100);
        assert_eq!(d.len(), 5);
        assert!(wm >= d[4].0);
        // After the watermark only the tail remains.
        let (d2, wm2) = tsdb.take_delta(wm, 100);
        assert!(d2.is_empty());
        let _ = wm2;
    }

    #[test]
    fn delta_limit_caps_batch() {
        let tsdb = Tsdb::new(TsConfig::default());
        let a = LinkKey::new("s", "h", "i");
        for i in 0..10 {
            tsdb.append(&a, sample(3000 + i, 1.0));
        }
        let (d, wm) = tsdb.take_delta(0, 3);
        assert_eq!(d.len(), 3);
        assert_eq!(d[2].0, wm);
        let (d2, _) = tsdb.take_delta(wm, 100);
        assert_eq!(d2.len(), 7);
    }

    #[test]
    fn merge_delta_ignores_replay() {
        let tsdb = Tsdb::new(TsConfig::default());
        let a = LinkKey::new("s", "h", "i");
        tsdb.merge_delta(&[(a.clone(), sample(100, 1.0))]);
        tsdb.merge_delta(&[(a.clone(), sample(100, 1.0))]); // replay
        tsdb.merge_delta(&[(a.clone(), sample(101, 2.0))]);
        assert_eq!(tsdb.query(&a, 0, 1000).samples.len(), 2);
    }

    #[test]
    fn redb_roundtrip_persists_history() {
        let dir = std::env::temp_dir().join(format!("mielofon-tsdb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ts.redb").to_string_lossy().to_string();
        let cfg = TsConfig {
            path,
            raw_retention_secs: 7200,
            agg_retention_secs: 7200,
        };

        let a = LinkKey::new("spoke-1", "hub-a", "awg_hub_a");
        // Samples within the retention window (recent wall-clock, bucket-aligned).
        let base = now_secs() - (now_secs() % BUCKET_SECS) - 100;
        {
            let tsdb = Tsdb::new(cfg.clone());
            for i in 0..5 {
                tsdb.append(&a, sample(base + i, 3.0));
            }
            tsdb.flush();
        }
        // Re-open from disk: raw history and bucket survive.
        let tsdb = Tsdb::new(cfg.clone());
        let r = tsdb.query(&a, 0, u64::MAX);
        assert_eq!(r.samples.len(), 5, "raw history must survive flush+reload");
        assert_eq!(r.buckets.len(), 1);
        assert_eq!(r.buckets[0].n, 5);

        std::fs::remove_dir_all(&dir).ok();
    }
}
