//! Rolling ping statistics per link: rtt (mean), jitter (population stddev of
//! the RTT window — the classifier's thresholds are tuned to this definition)
//! and loss (share of timed-out pings). Pure math, no I/O.
//!
//! One window holds the outcome of the last `size` pings, a reply with its
//! rtt or a timeout, so an rtt ages out exactly like the loss it belongs to:
//! a link that has answered nothing for a whole window reports no rtt, not
//! the rtt it had before it went dark.

use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Window {
    size: usize,
    /// `Some(rtt_ms)` = reply, `None` = timeout; newest last.
    outcomes: VecDeque<Option<f64>>,
    pub sent: u64,
    pub received: u64,
    pub lost: u64,
}

/// Measurements a reply carries to the controller; unmeasured = `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Snapshot {
    pub rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub loss_pct: Option<f64>,
}

impl Window {
    pub fn new(size: usize) -> Self {
        Window {
            size: size.max(2),
            outcomes: VecDeque::new(),
            sent: 0,
            received: 0,
            lost: 0,
        }
    }

    fn push(&mut self, outcome: Option<f64>) {
        self.outcomes.push_back(outcome);
        while self.outcomes.len() > self.size {
            self.outcomes.pop_front();
        }
    }

    pub fn record_reply(&mut self, rtt_ms: f64) {
        self.received += 1;
        self.push(Some(rtt_ms));
    }

    pub fn record_timeout(&mut self) {
        self.lost += 1;
        self.push(None);
    }

    pub fn snapshot(&self) -> Snapshot {
        let rtts: Vec<f64> = self.outcomes.iter().flatten().copied().collect();
        let n = rtts.len() as f64;
        let mean = (n > 0.0).then(|| rtts.iter().sum::<f64>() / n);
        let jitter = match mean {
            Some(m) if rtts.len() >= 2 => {
                Some((rtts.iter().map(|r| (r - m).powi(2)).sum::<f64>() / n).sqrt())
            }
            _ => None,
        };
        let loss = (!self.outcomes.is_empty()).then(|| {
            let lost = self.outcomes.iter().filter(|o| o.is_none()).count();
            100.0 * lost as f64 / self.outcomes.len() as f64
        });
        Snapshot {
            rtt_ms: mean,
            jitter_ms: jitter,
            loss_pct: loss,
        }
    }
}

/// RTT of a matched echo reply.
///
/// `mono` is send→read on the monotonic clock; it includes any time the reply
/// sat in the socket buffer before it was read. `kernel`, when the kernel
/// stamped the reply (`SO_TIMESTAMPNS`), is send→arrival on the realtime
/// clock and excludes that delay, so it is preferred — unless it cannot be
/// right (negative, or longer than the monotonic span: a realtime clock step).
pub fn select_rtt(kernel: Option<i128>, mono: Duration) -> f64 {
    let mono_ns = mono.as_nanos() as i128;
    match kernel {
        Some(k) if k >= 0 && k <= mono_ns => k as f64 / 1e6,
        _ => mono_ns as f64 / 1e6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_math() {
        let mut w = Window::new(4);
        w.record_reply(10.0);
        w.record_reply(20.0);
        w.record_timeout();
        w.record_reply(30.0);
        let s = w.snapshot();
        assert!((s.rtt_ms.unwrap() - 20.0).abs() < 1e-9);
        assert!((s.jitter_ms.unwrap() - 8.1649658).abs() < 1e-6);
        assert!((s.loss_pct.unwrap() - 25.0).abs() < 1e-9);
        assert_eq!((w.received, w.lost), (3, 1));

        // The window trims to the newest `size` samples.
        for i in 0..8 {
            w.record_reply(1.0 + i as f64);
        }
        assert!((w.snapshot().rtt_ms.unwrap() - 6.5).abs() < 1e-9);
        assert_eq!(w.snapshot().loss_pct, Some(0.0));
    }

    #[test]
    fn unmeasured_is_none_not_zero() {
        let w = Window::new(60);
        assert_eq!(w.snapshot(), Snapshot::default());
        let mut one = Window::new(60);
        one.record_reply(5.0);
        assert_eq!(one.snapshot().jitter_ms, None);
        let mut lost = Window::new(60);
        lost.record_timeout();
        assert_eq!(lost.snapshot().rtt_ms, None);
        assert_eq!(lost.snapshot().loss_pct, Some(100.0));
    }

    #[test]
    fn rtt_ages_out_with_the_window() {
        // A link that went dark must not keep reporting its old rtt next to
        // 100% loss: the stale replies leave the window with the outcomes.
        let mut w = Window::new(4);
        w.record_reply(20.0);
        w.record_reply(22.0);
        for _ in 0..3 {
            w.record_timeout();
        }
        let s = w.snapshot();
        assert_eq!(s.rtt_ms, Some(22.0), "one reply still inside the window");
        assert_eq!(s.loss_pct, Some(75.0));
        w.record_timeout();
        let s = w.snapshot();
        assert_eq!(s.rtt_ms, None);
        assert_eq!(s.jitter_ms, None);
        assert_eq!(s.loss_pct, Some(100.0));
    }

    #[test]
    fn late_read_reply_keeps_its_real_rtt() {
        // A 20 ms reply read 9 s late must not become a 9000 ms sample.
        let rtt = select_rtt(Some(20_000_000), Duration::from_secs(9));
        assert!((rtt - 20.0).abs() < 1e-9);
        // No stamp / clock step: monotonic.
        assert!((select_rtt(None, Duration::from_millis(15)) - 15.0).abs() < 1e-9);
        assert!((select_rtt(Some(-5), Duration::from_millis(15)) - 15.0).abs() < 1e-9);
        assert!((select_rtt(Some(i128::MAX), Duration::from_millis(15)) - 15.0).abs() < 1e-9);
    }
}
