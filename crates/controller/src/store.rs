//! Measurement-store envelope.
//!
//! The controller depends on this narrow trait instead of a concrete engine:
//! measurement ingest, the classifier, gossip replication and the tsdb query
//! endpoint all talk to a `Store`. The concrete implementation is the embedded
//! redb-backed ring in `tsdb.rs`; an RRD-style multi-tier rollup or an external
//! TSDB could back the same interface without touching any consumer.
//!
//! `window()` is the classifier's input: a per-link collapse of recent raw
//! samples with RRD-inspired gap/unknown semantics. Each dimension has its own
//! freshness horizon (always-tier dims are fresh by nature; the gated throughput
//! dim carries its last-known value for a longer window), so sparse probes stop
//! blanking dimensions that are merely less-frequently measured.

use crate::model::{LinkKey, ProbeState};
use crate::tsdb::{RawSample, TsQueryResp};

/// Window-collapse parameters for `Store::window` (see `config::Classifier`).
#[derive(Debug, Clone, Copy)]
pub struct WindowSpec {
    /// Freshness window for rtt/loss/rr (seconds).
    pub always_fresh_secs: u64,
    /// Primary fresh window for tcp throughput (seconds).
    pub tcp_fresh_secs: u64,
    /// Hard horizon for carrying the last-known tcp value (seconds).
    pub tcp_carry_secs: u64,
}

/// A per-link collapse of the recent raw-sample window, in the units the
/// classifier and dashboard consume. An `Error`-like "gap" is represented as
/// `None` (RRD `NaN`), and a dimension past its freshness window is not
/// carried.
#[derive(Debug, Clone, Default)]
pub struct WindowView {
    /// Timestamp of the most recent sample folded in.
    pub ts: u64,
    /// State of the most recent sample.
    pub state: ProbeState,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    /// RTT jitter (ms) from the most recent ping within `always_fresh_secs`.
    pub jitter_ms: Option<f64>,
    /// Most recent throughput within `tcp_fresh_secs`, else the last-known
    /// value carried within `tcp_carry_secs`.
    pub tcp_mbps: Option<f64>,
    pub util_mbps: f64,
    /// True when `tcp_mbps` was carried from beyond its primary fresh window
    /// (still within the carry horizon).
    pub tcp_carried: bool,
}

/// The measurement-store interface. All methods are cheap and wait-free where
/// bounded: consumers call them from the request path and background loops.
pub trait Store: Send + Sync {
    /// Append one locally-ingested sample (probe reply or `POST /v1/quality`).
    fn append(&self, link: &LinkKey, s: RawSample);

    /// Raw samples + 60s buckets in `[since, until)` for a link.
    fn query(&self, link: &LinkKey, since: u64, until: u64) -> TsQueryResp;

    /// Collapse recent samples for a link into the classifier's per-dimension
    /// view (per-dimension freshness + last-known carry). `None` when the link
    /// has no sample within the carry horizon.
    fn window(&self, link: &LinkKey, spec: &WindowSpec, now: u64) -> Option<WindowView>;

    /// Merge samples replicated from a peer (idempotent).
    fn merge_delta(&self, samples: &[(LinkKey, RawSample)]);

    /// Fresh samples with seq > `after` (max `limit`), plus the last seq taken.
    fn take_delta(&self, after: u64, limit: usize) -> (Vec<(u64, LinkKey, RawSample)>, u64);

    /// Per-peer gossip watermark.
    fn sent_watermark(&self, peer: &str) -> u64;
    fn note_sent(&self, peer: &str, watermark: u64);

    /// All links with at least one sample in memory.
    fn links(&self) -> Vec<LinkKey>;

    /// Retention pruning (raw + aggregate rings).
    fn prune(&self, now: u64);

    /// Durable flush to disk (no-op when persistence is off).
    fn flush(&self);
}
