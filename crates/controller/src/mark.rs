//! Passive tier, controller half: join the alternate-marking blocks of a
//! tunnel's two halves into loss and rate, convert the loss to the TCP rate
//! it allows, and label the loss shape (`docs/passive-quality.md`).
//!
//! The agent at each end counts every packet of a block it sends on the
//! tunnel (`tx`) and every one that arrives from the peer (`rx`). Loss of
//! the `A→B` direction in block `n` is therefore `tx(A→B, n) − rx(B→A, n)`:
//! the sender's and the receiver's counts of the same packets. The halves
//! come from two agents, usually registered at two members, and meet here
//! through sample replication. A block with only one half is skipped, not
//! guessed; the join is retried on every classifier pass while both samples
//! are within retention.

use crate::config::{Classifier as ClassifierCfg, LossModel};
use crate::model::Regime;
use crate::tsdb::{RawSample, MARK_RESET, MARK_UNDER_TEST};
use std::collections::HashMap;

/// One joined block of a directed link.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Block {
    pub n: u64,
    /// Block start (unix seconds) and length.
    pub ts: u64,
    pub secs: u32,
    /// Packets the sender counted, per length band (short ≤ 1000 B, long).
    pub sent_s: u64,
    pub sent_l: u64,
    /// Packets the receiver counted.
    pub recv_s: u64,
    pub recv_l: u64,
    pub bytes: u64,
    /// A throughput test on either end overlapped the block.
    pub under_test: bool,
}

impl Block {
    pub fn sent(&self) -> u64 {
        self.sent_s + self.sent_l
    }

    pub fn lost_s(&self) -> u64 {
        self.sent_s.saturating_sub(self.recv_s)
    }

    pub fn lost_l(&self) -> u64 {
        self.sent_l.saturating_sub(self.recv_l)
    }

    pub fn lost(&self) -> u64 {
        self.lost_s() + self.lost_l()
    }

    /// Loss of the block in percent (`None` when nothing was sent).
    pub fn loss_pct(&self) -> Option<f64> {
        (self.sent() > 0).then(|| pct(self.lost(), self.sent()))
    }

    /// The sender's rate over the block.
    pub fn rate_mbps(&self) -> f64 {
        self.bytes as f64 * 8.0 / (f64::from(self.secs.max(1)) * 1e6)
    }
}

fn pct(part: u64, whole: u64) -> f64 {
    part as f64 * 100.0 / whole as f64
}

/// Join the block samples of a directed link (`own`, its `tx` counters)
/// with those of its reverse half (`rev`, the peer's `rx` counters). Blocks
/// whose counters restarted on either end are dropped; a block counted on
/// different lengths at the two ends cannot be the same packets and is
/// dropped as well. The result is sorted by block number.
pub fn join(own: &[RawSample], rev: &[RawSample]) -> Vec<Block> {
    let rx: HashMap<u64, &RawSample> = rev
        .iter()
        .filter(|s| s.is_block())
        .filter_map(|s| s.mark_n.map(|n| (n, s)))
        .collect();
    let flag = |s: &RawSample, bit: u8| s.mark_flags.unwrap_or(0) & bit != 0;
    let mut out: Vec<Block> = own
        .iter()
        .filter(|s| s.is_block())
        .filter_map(|tx| {
            let n = tx.mark_n?;
            let rx = rx.get(&n)?;
            if flag(tx, MARK_RESET) || flag(rx, MARK_RESET) {
                return None;
            }
            let secs = tx.mark_secs?;
            if rx.mark_secs != Some(secs) {
                return None;
            }
            Some(Block {
                n,
                ts: tx.ts,
                secs,
                sent_s: u64::from(tx.mark_tx_s?),
                sent_l: u64::from(tx.mark_tx_l?),
                recv_s: u64::from(rx.mark_rx_s?),
                recv_l: u64::from(rx.mark_rx_l?),
                bytes: tx.mark_tx_bytes?,
                under_test: flag(tx, MARK_UNDER_TEST) || flag(rx, MARK_UNDER_TEST),
            })
        })
        .collect();
    out.sort_by_key(|b| b.n);
    out.dedup_by_key(|b| b.n);
    out
}

/// The passive tier's contribution to a link's window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    /// `Σ lost / Σ sent` over the loss window, `None` below the sample
    /// floor (`mark_min_pkts`) or without a joined block.
    pub loss_pct: Option<f64>,
    /// The same per length band, each with its own floor.
    pub loss_s_pct: Option<f64>,
    pub loss_l_pct: Option<f64>,
    /// The highest block rate within `tcp_fresh_secs`.
    pub rate_mbps: Option<f64>,
    /// Block start of that highest-rate block.
    pub rate_ts: Option<u64>,
    /// The loss shape, `None` without a joined block in the loss window.
    pub regime: Option<Regime>,
    /// Blocks the aggregate was taken over.
    pub blocks: usize,
    /// Sent/lost packets per band over the loss window (for the counters).
    pub sent_s: u64,
    pub sent_l: u64,
    pub lost_s: u64,
    pub lost_l: u64,
}

/// Summarise the joined blocks of a link at `now`.
///
/// The loss window is asymmetric: when the most recent `mark_window_secs`
/// show any loss, the aggregate covers only them so the demotion is fast;
/// when they are clean it covers `mark_recover_secs`, so a bad block inside
/// the last hour still counts and the class recovers only after an hour of
/// clean blocks. Blocks under a throughput test are excluded from the loss
/// (a test fills the link, and what it drops is the test's own congestion),
/// but their rate counts: the bytes crossed. Every block is an exact count,
/// so the aggregate has no sampling error, only the floor.
pub fn summarize(blocks: &[Block], cfg: &ClassifierCfg, now: u64) -> Summary {
    let mut out = Summary::default();
    let age = |b: &Block| now.saturating_sub(b.ts + u64::from(b.secs));

    for b in blocks.iter().filter(|b| age(b) <= cfg.tcp_fresh_secs) {
        let rate = b.rate_mbps();
        if out.rate_mbps.is_none_or(|r| rate > r) {
            out.rate_mbps = Some(rate);
            out.rate_ts = Some(b.ts);
        }
    }

    let recent: Vec<&Block> = blocks
        .iter()
        .filter(|b| !b.under_test && age(b) <= cfg.mark_recover_secs)
        .collect();
    let fast: Vec<&Block> = recent
        .iter()
        .copied()
        .filter(|b| age(b) <= cfg.mark_window_secs)
        .collect();
    let under_test = blocks
        .iter()
        .filter(|b| age(b) <= cfg.mark_window_secs)
        .any(|b| b.under_test);
    let window: &[&Block] = if fast.iter().any(|b| b.lost() > 0) {
        &fast
    } else {
        &recent
    };
    if window.is_empty() {
        if under_test {
            out.regime = Some(Regime::UnderTest);
        }
        return out;
    }

    out.blocks = window.len();
    for b in window {
        out.sent_s += b.sent_s;
        out.sent_l += b.sent_l;
        out.lost_s += b.lost_s();
        out.lost_l += b.lost_l();
    }
    let floor = cfg.mark_min_pkts;
    let sent = out.sent_s + out.sent_l;
    let lost = out.lost_s + out.lost_l;
    if sent >= floor {
        out.loss_pct = Some(pct(lost, sent));
    }
    if out.sent_s >= floor {
        out.loss_s_pct = Some(pct(out.lost_s, out.sent_s));
    }
    if out.sent_l >= floor {
        out.loss_l_pct = Some(pct(out.lost_l, out.sent_l));
    }
    out.regime = Some(regime(window, &out));
    out
}

/// The loss shape of the blocks the aggregate was taken over: the early
/// warning that the border middlebox changed its behaviour. `sparse` is
/// loss spread over most blocks at a steady rate (a policer or random
/// drops), `bursty` is loss concentrated in few blocks (an outage, a burst
/// of DPI resets), `size_selective` is the large-packet band losing at
/// least five times the small one with both bands sampled.
fn regime(window: &[&Block], s: &Summary) -> Regime {
    let lost: u64 = window.iter().map(|b| b.lost()).sum();
    if lost == 0 {
        return Regime::Clean;
    }
    if let (Some(small), Some(large)) = (s.loss_s_pct, s.loss_l_pct) {
        if large > 0.0 && large >= 5.0 * small {
            return Regime::SizeSelective;
        }
    }
    let per_block: Vec<f64> = window.iter().filter_map(|b| b.loss_pct()).collect();
    let lossy = per_block.iter().filter(|p| **p > 0.0).count();
    let mean = per_block.iter().sum::<f64>() / per_block.len().max(1) as f64;
    let max = per_block.iter().copied().fold(0.0, f64::max);
    if lossy * 2 > per_block.len() && max <= 3.0 * mean {
        Regime::Sparse
    } else {
        Regime::Bursty
    }
}

/// The TCP rate (Mbps) a loss ratio supports at `rtt_ms` under loss-based
/// congestion control. `None` for zero loss: no loss is no constraint,
/// never an infinite rate.
pub fn implied_tcp_mbps(
    loss_pct: f64,
    rtt_ms: f64,
    model: LossModel,
    mss_bytes: u32,
) -> Option<f64> {
    // NaN fails both tests, like zero: nothing can be derived from it.
    if loss_pct.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        || rtt_ms.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
    {
        return None;
    }
    let p = loss_pct / 100.0;
    let rtt = rtt_ms / 1000.0;
    let mss_bits = f64::from(mss_bytes) * 8.0;
    let bps = match model {
        // The Reno/cubic long-run bound (Mathis et al., 1997).
        LossModel::Mathis => 1.22 * mss_bits / (rtt * p.sqrt()),
        // RFC 8337: a target needs `target_run_length = 3 × W²` packets
        // between losses, `W = ceil(target × RTT / MSS)`. The largest
        // window the measured loss still allows is `floor(1 / sqrt(3p))`.
        LossModel::Mbm => (1.0 / (3.0 * p).sqrt()).floor() * mss_bits / rtt,
    };
    Some(bps / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClassifierCfg {
        ClassifierCfg {
            tcp_fresh_secs: 1200,
            mark_window_secs: 300,
            mark_recover_secs: 3600,
            mark_min_pkts: 2000,
            ..ClassifierCfg::default()
        }
    }

    fn tx(n: u64, s: u32, l: u32, bytes: u64, flags: u8) -> RawSample {
        RawSample {
            ts: n * 10,
            mark_n: Some(n),
            mark_secs: Some(10),
            mark_tx_s: Some(s),
            mark_tx_l: Some(l),
            mark_rx_s: Some(0),
            mark_rx_l: Some(0),
            mark_tx_bytes: Some(bytes),
            mark_rx_bytes: Some(0),
            mark_flags: Some(flags),
            ..RawSample::default()
        }
    }

    fn rx(n: u64, s: u32, l: u32, flags: u8) -> RawSample {
        RawSample {
            ts: n * 10,
            mark_n: Some(n),
            mark_secs: Some(10),
            mark_tx_s: Some(0),
            mark_tx_l: Some(0),
            mark_rx_s: Some(s),
            mark_rx_l: Some(l),
            mark_tx_bytes: Some(0),
            mark_rx_bytes: Some(0),
            mark_flags: Some(flags),
            ..RawSample::default()
        }
    }

    fn block(n: u64, sent_s: u64, sent_l: u64, lost_s: u64, lost_l: u64, bytes: u64) -> Block {
        Block {
            n,
            ts: n * 10,
            secs: 10,
            sent_s,
            sent_l,
            recv_s: sent_s - lost_s,
            recv_l: sent_l - lost_l,
            bytes,
            under_test: false,
        }
    }

    #[test]
    fn join_pairs_tx_with_the_peers_rx_by_block_number() {
        let own = vec![
            tx(100, 10, 20, 30_000, 0),
            tx(101, 10, 20, 30_000, 0),
            tx(102, 10, 20, 30_000, MARK_RESET),
            tx(103, 10, 20, 30_000, 0),
            // Not a block: an always sample.
            RawSample {
                ts: 1035,
                rtt_ms: Some(1.0),
                ..RawSample::default()
            },
        ];
        let mut other_len = rx(103, 10, 20, 0);
        other_len.mark_secs = Some(5);
        let rev = vec![
            rx(100, 10, 18, 0),
            rx(102, 10, 20, 0),
            rx(101, 9, 20, MARK_UNDER_TEST),
            other_len,
        ];
        let j = join(&own, &rev);
        assert_eq!(j.iter().map(|b| b.n).collect::<Vec<_>>(), vec![100, 101]);
        assert_eq!(j[0].lost_l(), 2);
        assert_eq!(j[0].lost_s(), 0);
        assert_eq!(j[0].rate_mbps(), 0.024);
        assert!(!j[0].under_test);
        assert_eq!(j[1].lost(), 1);
        assert!(j[1].under_test, "either end's flag marks the block");
    }

    #[test]
    fn more_received_than_sent_is_no_loss() {
        let b = Block {
            recv_s: 50,
            ..block(1, 10, 0, 0, 0, 0)
        };
        assert_eq!(b.lost(), 0);
        assert_eq!(b.loss_pct(), Some(0.0));
    }

    #[test]
    fn clean_window_aggregates_the_recovery_span() {
        let now = 10_000;
        // Six clean blocks in the last minute, one lossy block 20 min ago.
        let mut blocks: Vec<Block> = (994..1000)
            .map(|n| block(n, 500, 500, 0, 0, 1_000_000))
            .collect();
        blocks.insert(0, block(880, 500, 500, 10, 10, 20_000_000));
        let s = summarize(&blocks, &cfg(), now);
        assert_eq!(s.blocks, 7);
        assert_eq!(s.loss_pct, Some(20.0 / 7000.0 * 100.0));
        assert_eq!(s.regime, Some(Regime::Bursty));
        assert_eq!(s.rate_mbps, Some(16.0));
        assert_eq!(s.rate_ts, Some(8800));
        // Beyond the recovery span the bad block no longer counts.
        let s = summarize(&blocks, &cfg(), now + 3000);
        assert_eq!(s.blocks, 6);
        assert_eq!(s.loss_pct, Some(0.0));
        assert_eq!(s.regime, Some(Regime::Clean));
        assert_eq!(s.rate_mbps, None, "rate horizon is tcp_fresh_secs");
    }

    #[test]
    fn loss_in_the_fast_window_narrows_the_aggregate() {
        let now = 10_000;
        // Ten minutes of blocks; the fast window (300 s) holds the 31 that
        // ended within it, blocks 969..999.
        let mut blocks: Vec<Block> = (940..1000).map(|n| block(n, 500, 500, 0, 0, 100)).collect();
        // One packet lost in most of them at a steady rate: sparse.
        for b in blocks.iter_mut().filter(|b| b.n >= 975) {
            b.recv_s -= 1;
        }
        let s = summarize(&blocks, &cfg(), now);
        assert_eq!(s.blocks, 31, "{s:?}");
        assert_eq!(s.loss_pct, Some(25.0 / 31_000.0 * 100.0));
        assert_eq!(s.loss_s_pct, Some(25.0 / 15_500.0 * 100.0));
        assert_eq!(s.loss_l_pct, Some(0.0));
        assert_eq!(s.regime, Some(Regime::Sparse));
        // Loss in a few blocks only: bursty.
        for b in blocks.iter_mut().filter(|b| b.n >= 975 && b.n < 997) {
            b.recv_s += 1;
        }
        let s = summarize(&blocks, &cfg(), now);
        assert_eq!(s.blocks, 31);
        assert_eq!(s.regime, Some(Regime::Bursty));
    }

    #[test]
    fn size_selective_needs_both_bands_sampled() {
        let now = 10_000;
        let mut blocks: Vec<Block> = (990..1000).map(|n| block(n, 300, 300, 0, 5, 100)).collect();
        let s = summarize(&blocks, &cfg(), now);
        assert_eq!(s.regime, Some(Regime::SizeSelective));
        assert_eq!(s.loss_l_pct, Some(50.0 / 3000.0 * 100.0));
        // Too few small packets to say anything about that band.
        for b in blocks.iter_mut() {
            b.sent_s = 100;
            b.recv_s = 100;
        }
        let s = summarize(&blocks, &cfg(), now);
        assert_eq!(s.loss_s_pct, None);
        assert_eq!(s.regime, Some(Regime::Sparse));
    }

    #[test]
    fn under_test_blocks_count_for_rate_but_not_loss() {
        let now = 10_000;
        let mut blocks: Vec<Block> = (995..1000)
            .map(|n| block(n, 500, 500, 20, 20, 1_000_000))
            .collect();
        for b in blocks.iter_mut() {
            b.under_test = true;
        }
        let s = summarize(&blocks, &cfg(), now);
        assert_eq!(s.loss_pct, None);
        assert_eq!(s.rate_mbps, Some(0.8));
        assert_eq!(s.regime, Some(Regime::UnderTest));
        assert_eq!(s.blocks, 0);
    }

    #[test]
    fn below_the_floor_the_loss_is_unmeasured() {
        let blocks = vec![block(999, 100, 100, 1, 0, 100)];
        let s = summarize(&blocks, &cfg(), 10_000);
        assert_eq!(s.loss_pct, None);
        assert_eq!(s.blocks, 1);
        // The shape is still labelled: one lossy block of one is "most".
        assert_eq!(s.regime, Some(Regime::Sparse));
        assert_eq!(summarize(&[], &cfg(), 10_000), Summary::default());
    }

    #[test]
    fn implied_rate_follows_the_models() {
        // The worked value of docs/passive-quality.md: 0.05 % at 60 ms ≈ 10 Mbps.
        let m = implied_tcp_mbps(0.05, 60.0, LossModel::Mathis, 1348).unwrap();
        assert!((m - 9.8).abs() < 0.1, "{m}");
        let m = implied_tcp_mbps(1.0, 60.0, LossModel::Mathis, 1348).unwrap();
        assert!((m - 2.19).abs() < 0.05, "{m}");
        // MBM: W = floor(1/sqrt(3p)) = 25 at 0.05 %, 25 × 10784 b / 0.06 s.
        let m = implied_tcp_mbps(0.05, 60.0, LossModel::Mbm, 1348).unwrap();
        assert!((m - 4.49).abs() < 0.05, "{m}");
        assert_eq!(implied_tcp_mbps(0.0, 60.0, LossModel::Mathis, 1348), None);
        assert_eq!(implied_tcp_mbps(1.0, 0.0, LossModel::Mathis, 1348), None);
    }
}
