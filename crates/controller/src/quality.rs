//! Quality classification and per-link OSPF cost derivation.
//!
//! Quality classes are configurable per dimension: each class (good /
//! acceptable / poor / bad) carries optional thresholds and its own OSPF
//! cost. "Worst crossed class wins": for every dimension the metric is
//! checked against each class that pins it (upper bounds `rtt_ms`/`loss_pct`,
//! lower bounds `rr_tps`/`tcp_mbps`), and the resulting overall class is the
//! worst of the per-dimension escalations. Unset dimensions never constrain.
//!
//! The controller never writes a "dead/broken" state/cost — hard outages are
//! owned by the underlay's dead-interval.

use crate::config::Quality as QualityCfg;
use crate::model::{ProbeState, Quality, QualityRecord};

/// OSPF cost for `q`, taken from the configured class.
pub fn cost_for_quality(cfg: &QualityCfg, q: Quality) -> u32 {
    match q {
        Quality::Good => cfg.good.ospf_cost,
        Quality::Acceptable => cfg.acceptable.ospf_cost,
        Quality::Poor => cfg.poor.ospf_cost,
        Quality::Bad => cfg.bad.ospf_cost,
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

fn classify_best_effort(cfg: &QualityCfg, rec: &QualityRecord) -> Quality {
    let rtt_score = score_upper(
        rec.rtt_ms,
        [
            cfg.good.rtt_ms,
            cfg.acceptable.rtt_ms,
            cfg.poor.rtt_ms,
            cfg.bad.rtt_ms,
        ],
    );
    let loss_score = score_upper(
        rec.loss_pct,
        [
            cfg.good.loss_pct,
            cfg.acceptable.loss_pct,
            cfg.poor.loss_pct,
            cfg.bad.loss_pct,
        ],
    );
    let rr_score = score_lower(
        rec.rr_tps,
        [
            cfg.good.rr_tps,
            cfg.acceptable.rr_tps,
            cfg.poor.rr_tps,
            cfg.bad.rr_tps,
        ],
    );
    let tcp_score = if let Some(m) = rec.tcp_mbps {
        score_lower(
            m,
            [
                cfg.good.tcp_mbps,
                cfg.acceptable.tcp_mbps,
                cfg.poor.tcp_mbps,
                cfg.bad.tcp_mbps,
            ],
        )
    } else {
        0
    };

    worst([rtt_score, loss_score, rr_score, tcp_score])
}

/// Escalate the score while the metric exceeds an upper-bound (≤-ok) class
/// threshold, in class order good → acceptable → poor → bad. Escalation is
/// capped at 3 (bad), and a class that pins no threshold contributes nothing.
fn score_upper(metric: f64, thresholds: [Option<f64>; 4]) -> u8 {
    let mut score = 0;
    for (i, t) in thresholds.into_iter().enumerate() {
        if t.is_some_and(|t| metric > t) {
            score = score.max(i as u8 + 1);
        }
    }
    score.min(3)
}

/// Escalate the score while the metric drops below a lower-bound (≥-ok) class
/// threshold, in class order good → acceptable → poor → bad.
fn score_lower(metric: f64, thresholds: [Option<f64>; 4]) -> u8 {
    let mut score = 0;
    for (i, t) in thresholds.into_iter().enumerate() {
        if t.is_some_and(|t| metric < t) {
            score = score.max(i as u8 + 1);
        }
    }
    score.min(3)
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
        // LTT 15ms but only 1.5 Mbps through (the key failure mode): tcp
        // crosses good/acceptable/poor thresholds → Bad.
        assert_eq!(
            classify(&cfg, &rec(15.0, 0.0, 90.0, Some(1.5))),
            Some(Quality::Bad)
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
    fn worse_rtt_escalates_class() {
        let cfg = QualityCfg::default();
        assert_eq!(
            classify(&cfg, &rec(60.0, 0.0, 90.0, Some(80.0))),
            Some(Quality::Acceptable)
        );
        assert_eq!(
            classify(&cfg, &rec(120.0, 0.0, 90.0, Some(80.0))),
            Some(Quality::Poor)
        );
        assert_eq!(
            classify(&cfg, &rec(400.0, 0.0, 90.0, Some(80.0))),
            Some(Quality::Bad)
        );
    }

    #[test]
    fn unset_dimension_does_not_penalise() {
        // Only rtt pinned (good=123, bad=321) — everything else unset, so
        // loss/rr/tcp never escalate.
        let cfg = QualityCfg {
            good: crate::config::QualityClass {
                rtt_ms: Some(123.0),
                ..Default::default()
            },
            acceptable: Default::default(),
            poor: Default::default(),
            bad: crate::config::QualityClass {
                rtt_ms: Some(321.0),
                ..Default::default()
            },
        };
        // Terrible loss/tps but in-norm rtt → still acceptable.
        assert_eq!(
            classify(&cfg, &rec(200.0, 99.0, 1.0, Some(0.1))),
            Some(Quality::Acceptable)
        );
        // rtt over the bad line wins.
        assert_eq!(
            classify(&cfg, &rec(400.0, 0.0, 90.0, Some(80.0))),
            Some(Quality::Bad)
        );
        // below good line stays good despite awful tcp.
        assert_eq!(
            classify(&cfg, &rec(100.0, 0.0, 90.0, Some(0.1))),
            Some(Quality::Good)
        );
    }

    #[test]
    fn cost_is_conservative() {
        let cfg = QualityCfg::default();
        assert_eq!(cost_for_quality(&cfg, Quality::Good), 10);
        assert_eq!(cost_for_quality(&cfg, Quality::Bad), 100);
    }
}
