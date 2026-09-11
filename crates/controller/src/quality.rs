//! Quality classification and per-link OSPF cost derivation.
//!
//! Quality classes are configurable per dimension: each class (good /
//! acceptable / poor / bad) carries optional thresholds and its own OSPF
//! cost. "Worst crossed class wins": for every dimension the metric is
//! checked against each class that pins it (upper bounds `rtt_ms`/`loss_pct`/
//! `jitter_ms`, lower bound `tcp_mbps`), and the resulting overall class is
//! the worst of the per-dimension escalations. Unset dimensions never
//! constrain.
//!
//! The always-tier congestion signal is RTT **jitter** (derived from the ping
//! RTT distribution): it is congestion-immune and non-intrusive, so it stays
//! valid under real traffic. Real load (which can expose shaping that only
//! kicks in under tunnel usage) is the gated iperf3 throughput dimension; the
//! interface counters (`util_mbps`) are the real-traffic cross-check.
//!
//! The controller never writes a "dead/broken" state/cost — hard outages are
//! owned by the underlay's dead-interval.

/// Classification quality config + record types.
use crate::config::Quality as QualityCfg;
use crate::model::{ProbeState, Quality, QualityRecord};
use crate::store::WindowView;

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

/// Classify a measurement collapsed from a TSDB window. Dimensions that fell
/// outside their freshness/carry horizon are `None` and simply do not
/// constrain; a carried `tcp_mbps` (older but within the carry window) still
/// participates, which is the point of decoupling classification from the
/// latest datapoint.
pub fn classify_window(cfg: &QualityCfg, view: &WindowView) -> Quality {
    let rec = QualityRecord {
        ts: view.ts,
        rtt_ms: view.rtt_ms,
        loss_pct: view.loss_pct,
        jitter_ms: view.jitter_ms,
        tcp_mbps: view.tcp_mbps,
        udp_mbps: None,
        util_mbps: view.util_mbps,
        state: view.state,
        quality: None,
        ospf_cost: None,
    };
    classify_best_effort(cfg, &rec)
}

fn classify_best_effort(cfg: &QualityCfg, rec: &QualityRecord) -> Quality {
    let rtt_score = rec.rtt_ms.map(|m| {
        score_upper(
            m,
            [
                cfg.good.rtt_ms,
                cfg.acceptable.rtt_ms,
                cfg.poor.rtt_ms,
                cfg.bad.rtt_ms,
            ],
        )
    });
    let loss_score = rec.loss_pct.map(|m| {
        score_upper(
            m,
            [
                cfg.good.loss_pct,
                cfg.acceptable.loss_pct,
                cfg.poor.loss_pct,
                cfg.bad.loss_pct,
            ],
        )
    });
    let jitter_score = rec.jitter_ms.map(|m| {
        score_upper(
            m,
            [
                cfg.good.jitter_ms,
                cfg.acceptable.jitter_ms,
                cfg.poor.jitter_ms,
                cfg.bad.jitter_ms,
            ],
        )
    });
    let tcp_score = rec.tcp_mbps.map(|m| {
        score_lower(
            m,
            [
                cfg.good.tcp_mbps,
                cfg.acceptable.tcp_mbps,
                cfg.poor.tcp_mbps,
                cfg.bad.tcp_mbps,
            ],
        )
    });

    worst([
        rtt_score.unwrap_or(0),
        loss_score.unwrap_or(0),
        jitter_score.unwrap_or(0),
        tcp_score.unwrap_or(0),
    ])
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

    fn rec(
        rtt: Option<f64>,
        loss: Option<f64>,
        jitter: Option<f64>,
        tcp: Option<f64>,
    ) -> QualityRecord {
        QualityRecord::new(rtt, loss, jitter, tcp, None, 0.0, ProbeState::Quiet)
    }

    #[test]
    fn good_link_classifies_good() {
        let cfg = QualityCfg::default();
        assert_eq!(
            classify(&cfg, &rec(Some(15.0), Some(0.0), Some(2.0), Some(80.0))),
            Some(Quality::Good)
        );
    }

    #[test]
    fn low_rtt_but_throttled_still_penalised() {
        let cfg = QualityCfg::default();
        // Low RTT + low jitter but only 1.5 Mbps through (the key failure
        // mode): tcp crosses good/acceptable/poor thresholds → Bad.
        assert_eq!(
            classify(&cfg, &rec(Some(15.0), Some(0.0), Some(2.0), Some(1.5))),
            Some(Quality::Bad)
        );
    }

    #[test]
    fn busy_link_not_classified() {
        let cfg = QualityCfg::default();
        let mut r = rec(Some(1000.0), Some(99.0), Some(500.0), Some(0.1));
        r.state = ProbeState::Busy;
        assert_eq!(classify(&cfg, &r), None);
    }

    #[test]
    fn worse_rtt_escalates_class() {
        let cfg = QualityCfg::default();
        assert_eq!(
            classify(&cfg, &rec(Some(60.0), Some(0.0), Some(2.0), Some(80.0))),
            Some(Quality::Acceptable)
        );
        assert_eq!(
            classify(&cfg, &rec(Some(120.0), Some(0.0), Some(2.0), Some(80.0))),
            Some(Quality::Poor)
        );
        assert_eq!(
            classify(&cfg, &rec(Some(400.0), Some(0.0), Some(2.0), Some(80.0))),
            Some(Quality::Bad)
        );
    }

    #[test]
    fn unset_dimension_does_not_penalise() {
        // Only rtt pinned (good=123, bad=321) — everything else unset, so
        // loss/jitter/tcp never escalate.
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
        // Terrible loss/jitter/tcp but in-norm rtt → still acceptable.
        assert_eq!(
            classify(&cfg, &rec(Some(200.0), Some(99.0), Some(500.0), Some(0.1))),
            Some(Quality::Acceptable)
        );
        // rtt over the bad line wins.
        assert_eq!(
            classify(&cfg, &rec(Some(400.0), Some(0.0), Some(2.0), Some(80.0))),
            Some(Quality::Bad)
        );
        // below good line stays good despite awful tcp.
        assert_eq!(
            classify(&cfg, &rec(Some(100.0), Some(0.0), Some(2.0), Some(0.1))),
            Some(Quality::Good)
        );
    }

    #[test]
    fn low_jitter_does_not_punish_medium_rtt() {
        let cfg = QualityCfg::default();
        // Jitter is the congestion cross-check. A healthy 50ms path with low
        // jitter (2ms) classifies by rtt only → acceptable (50 > 40 good).
        assert_eq!(
            classify(&cfg, &rec(Some(50.0), Some(0.0), Some(2.0), Some(80.0))),
            Some(Quality::Acceptable)
        );
        // A long-but-healthy hub link (125ms, low jitter) is poor by rtt — not
        // dragged to bad by a jitter value that is physically normal for it.
        assert_eq!(
            classify(&cfg, &rec(Some(125.0), Some(0.0), Some(4.0), Some(80.0))),
            Some(Quality::Poor)
        );
    }

    #[test]
    fn high_jitter_escalates_class() {
        let cfg = QualityCfg::default();
        // Bufferbloat/queueing on a healthy-rtt path: jitter far above the
        // good/acceptable cutoffs drags it to bad even though rtt/tcp are fine.
        assert_eq!(
            classify(&cfg, &rec(Some(50.0), Some(0.0), Some(60.0), Some(80.0))),
            Some(Quality::Bad)
        );
        // Jitter just past good (5) but within acceptable (15) → acceptable,
        // alongside an acceptable rtt.
        assert_eq!(
            classify(&cfg, &rec(Some(50.0), Some(0.0), Some(8.0), Some(80.0))),
            Some(Quality::Acceptable)
        );
    }

    #[test]
    fn cost_is_conservative() {
        let cfg = QualityCfg::default();
        assert_eq!(cost_for_quality(&cfg, Quality::Good), 10);
        assert_eq!(cost_for_quality(&cfg, Quality::Bad), 100);
    }

    fn view(
        rtt: Option<f64>,
        loss: Option<f64>,
        jitter: Option<f64>,
        tcp: Option<f64>,
    ) -> WindowView {
        WindowView {
            rtt_ms: rtt,
            loss_pct: loss,
            jitter_ms: jitter,
            tcp_mbps: tcp,
            ..Default::default()
        }
    }

    #[test]
    fn window_classify_uses_carried_throughput() {
        // Low RTT but the carried throughput is throttled → still penalised.
        // This is the exact case the old "latest datapoint" logic lost because
        // the always probe wiped tcp_mbps.
        let cfg = QualityCfg::default();
        assert_eq!(
            classify_window(&cfg, &view(Some(15.0), Some(0.0), Some(2.0), Some(1.5))),
            Quality::Bad
        );
        // Healthy carried throughput keeps it good despite low rtt.
        assert_eq!(
            classify_window(&cfg, &view(Some(15.0), Some(0.0), Some(2.0), Some(80.0))),
            Quality::Good
        );
    }

    #[test]
    fn window_classify_unset_dims_do_not_constrain() {
        // Only rtt set; carried tcp and jitter absent → rtt alone decides.
        let cfg = QualityCfg::default();
        assert_eq!(
            classify_window(&cfg, &view(Some(400.0), None, None, None)),
            Quality::Bad
        );
        assert_eq!(
            classify_window(&cfg, &view(Some(15.0), None, None, None)),
            Quality::Good
        );
    }
}
