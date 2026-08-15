//! Quality classification and per-link OSPF cost derivation.
//!
//! The controller never writes a "dead/broken" state/cost — hard outages are
//! owned by the underlay's dead-interval. Only `good/acceptable/poor/bad` are
//! assigned, at conservative thresholds.

use crate::config::Quality as QualityCfg;
use crate::model::{ProbeState, Quality, QualityRecord};

/// Conservative OSPF cost bands (higher = less preferred).
pub fn cost_for_quality(q: Quality) -> u32 {
    match q {
        Quality::Good => 10,
        Quality::Acceptable => 20,
        Quality::Poor => 50,
        Quality::Bad => 100,
    }
}

/// Classify a measurement, honouring a busy/conflict probe state (a busy link
/// must never be reported degraded — skip classification).
pub fn classify(cfg: &QualityCfg, rec: &QualityRecord) -> Option<Quality> {
    if rec.state == ProbeState::Busy {
        return None; // no measurement of real quality while busy
    }
    Some(classify_best_effort(cfg, rec))
}

/// Fallible, best-effort classification using the worst applicable dimension.
fn classify_best_effort(cfg: &QualityCfg, rec: &QualityRecord) -> Quality {
    let rtt_score = score_rtt(cfg, rec.rtt_ms);
    let loss_score = score_loss(cfg, rec.loss_pct);
    let rr_score = score_rr(cfg, rec.rr_tps);
    let tcp_score = rec.tcp_mbps.map(|m| score_tcp(cfg, m)).unwrap_or(0);

    worst([rtt_score, loss_score, rr_score, tcp_score])
}

fn score_rtt(cfg: &QualityCfg, ms: f64) -> u8 {
    if ms <= cfg.rtt_good_ms {
        0
    } else if ms <= cfg.rtt_poor_ms {
        1
    } else if ms <= cfg.rtt_bad_ms {
        2
    } else {
        3
    }
}

fn score_loss(cfg: &QualityCfg, pct: f64) -> u8 {
    if pct <= cfg.loss_good_pct {
        0
    } else if pct <= cfg.loss_poor_pct {
        1
    } else {
        3
    }
}

fn score_rr(cfg: &QualityCfg, tps: f64) -> u8 {
    // Higher tps = better.
    if tps >= cfg.rr_tps_good {
        0
    } else if tps >= cfg.rr_tps_poor {
        1
    } else {
        2
    }
}

fn score_tcp(cfg: &QualityCfg, mbps: f64) -> u8 {
    // Higher throughput = better; this catches low-RTT-but-throttled links.
    if mbps >= cfg.tcp_mbps_good {
        0
    } else if mbps >= cfg.tcp_mbps_poor {
        1
    } else {
        2
    }
}

fn worst(scores: [u8; 4]) -> Quality {
    let m = scores.into_iter().max().unwrap_or(0);
    match m {
        0 => Quality::Good,
        1 => Quality::Acceptable,
        2 => Quality::Poor,
        _ => Quality::Bad,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProbeState, QualityRecord};

    fn rec(rtt: f64, loss: f64, tps: f64, tcp: Option<f64>) -> QualityRecord {
        QualityRecord::new(rtt, loss, tps, tcp, None, 0.0, ProbeState::Quiet)
    }

    #[test]
    fn good_link_classifies_good() {
        let cfg = QualityCfg::default();
        assert_eq!(
            classify(&cfg, &rec(15.0, 0.0, 90.0, Some(80.0))),
            Some(Quality::Good)
        );
    }

    #[test]
    fn low_rtt_but_throttled_still_penalised() {
        let cfg = QualityCfg::default();
        // LTT 15ms but only 1.5 Mbps through (the key failure mode).
        assert_eq!(
            classify(&cfg, &rec(15.0, 0.0, 90.0, Some(1.5))),
            Some(Quality::Poor)
        );
    }

    #[test]
    fn busy_link_not_classified() {
        let cfg = QualityCfg::default();
        let mut r = rec(1000.0, 99.0, 1.0, Some(0.1));
        r.state = ProbeState::Busy;
        assert_eq!(classify(&cfg, &r), None);
    }

    #[test]
    fn cost_is_conservative() {
        assert_eq!(cost_for_quality(Quality::Good), 10);
        assert_eq!(cost_for_quality(Quality::Bad), 100);
    }
}
