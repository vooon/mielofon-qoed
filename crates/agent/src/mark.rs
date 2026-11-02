//! Passive tier: alternate marking (RFC 9341) of the managed tunnels.
//!
//! Every `block_secs` the node switches the colour of all its links (DSCP
//! bit 0 of the inner header, set by the fw4 include's `mielofon_a` /
//! `mielofon_b` chains) and, half a block later, reads the per-element
//! counters of the sets that counted the block which just ended: what this
//! node sent on each interface (`tx`) and what arrived from the peer on it
//! (`rx`), per length band. The controller joins one end's `tx` with the
//! other end's `rx` for the same block number into the link's real loss at
//! its real rate. See `docs/passive-quality.md`.
//!
//! Block numbers come from the wall clock (`floor(unix_time / block_secs)`),
//! so both ends of a tunnel count the same packets under the same number;
//! NTP-grade sync is plenty for a 10 s block. The colour of block `n` is
//! `A` when `n` is even, `B` when odd.
//!
//! The agent never writes rules. Each boundary it applies one transaction
//! that asserts set membership and the colour map for the block that
//! begins (idempotent, so a firewall reload that emptied everything is
//! repaired at the next boundary), and each read compares the kernel
//! handle of every set with the previous read: a changed handle means the
//! table was rebuilt and the counters restarted, so the block is reported
//! with `reset` and its numbers are not used.

use crate::nft::{assert_script, parse_sets, Colour, Nft, SetCounters, COUNTER_SETS};
use anyhow::{bail, Context, Result};
use mielofon_proto::MarkBlock;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Finished blocks per interface, drained into the next always-probe reply
/// of that link. Shared between the block task and the agent.
pub type BlockQueue = Arc<Mutex<HashMap<String, VecDeque<MarkBlock>>>>;

/// Blocks kept per interface while no always probe picks them up (a
/// controller session outage): ~10 minutes at the default block.
const QUEUE_CAP: usize = 64;

/// Drain the finished blocks of `iface`, oldest first.
pub fn take_blocks(queue: &BlockQueue, iface: &str) -> Vec<MarkBlock> {
    queue
        .lock()
        .expect("block queue")
        .get_mut(iface)
        .map(|q| q.drain(..).collect())
        .unwrap_or_default()
}

fn push_block(queue: &BlockQueue, iface: &str, b: MarkBlock) {
    let mut map = queue.lock().expect("block queue");
    let q = map.entry(iface.to_string()).or_default();
    if q.len() >= QUEUE_CAP {
        q.pop_front();
    }
    q.push_back(b);
}

/// What one read produced, for the metrics counters.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReadOutcome {
    pub blocks: usize,
    pub resets: usize,
}

#[derive(Default)]
struct MarkState {
    /// Previous read of each counter set, with the block it was read for:
    /// the baseline for the next delta, valid only for the next block of
    /// the same colour (`n + 2`). A read that failed or was missed leaves
    /// the baseline stale, and a delta over it would span two blocks.
    last: HashMap<String, (u64, SetCounters)>,
}

pub struct Marker<N> {
    nft: N,
    /// `family table`, e.g. `inet fw4`.
    table: String,
    family: String,
    table_name: String,
    pub block: Duration,
    pub queue: BlockQueue,
    state: Mutex<MarkState>,
}

impl<N: Nft> Marker<N> {
    pub fn new(nft: N, table: &str, block: Duration, queue: BlockQueue) -> Self {
        let mut parts = table.split_whitespace();
        let family = parts.next().unwrap_or("inet").to_string();
        let table_name = parts.next().unwrap_or("fw4").to_string();
        Marker {
            nft,
            table: format!("{family} {table_name}"),
            family,
            table_name,
            block,
            queue,
            state: Mutex::new(MarkState::default()),
        }
    }

    /// Block number of a wall-clock instant.
    pub fn block_of(&self, unix_secs: f64) -> u64 {
        (unix_secs / self.block.as_secs_f64()).floor() as u64
    }

    /// Start block `n`: assert membership and point every managed interface
    /// at the chain of `n`'s colour. Called at the block boundary.
    pub async fn flip(&self, ifaces: &[String], n: u64) -> Result<()> {
        let script = assert_script(&self.table, ifaces, Colour::of_block(n));
        self.nft
            .apply(&script)
            .await
            .with_context(|| format!("marking: assert block {n}"))
    }

    /// Finish block `n`: read the counters of its colour and queue one
    /// block per interface that had a baseline. Called half a block after
    /// the boundary that ended `n`.
    pub async fn read(&self, ifaces: &[String], n: u64, under_test: bool) -> Result<ReadOutcome> {
        let reply = self
            .nft
            .list_sets(&self.family)
            .await
            .context("marking: list sets")?;
        let sets = parse_sets(&reply, &self.table_name);
        let c = Colour::of_block(n).letter();
        let names = [
            format!("mf_tx_{c}_s"),
            format!("mf_tx_{c}_l"),
            format!("mf_rx_{c}_s"),
            format!("mf_rx_{c}_l"),
        ];
        for name in &names {
            if !sets.contains_key(name) {
                bail!(
                    "marking: set {name} missing from {} (include not loaded?)",
                    self.table
                );
            }
        }

        let mut out = ReadOutcome::default();
        let mut st = self.state.lock().expect("marker state");
        // The baseline of this colour is the read of block n − 2. Anything
        // else (first read, a failed or missed read in between) means the
        // counters cover more than this block: no block, new baseline.
        let baseline = |name: &str| -> Option<&SetCounters> {
            let (read_for, prev) = st.last.get(name)?;
            (*read_for + 2 == n).then_some(prev)
        };
        if names.iter().any(|name| baseline(name).is_none()) {
            if let Some(stale) = names.iter().find_map(|name| st.last.get(name)) {
                tracing::debug!(
                    "marking: block {n}: baseline is from block {}, block skipped",
                    stale.0
                );
            }
        }
        // The table was rebuilt since the previous read of this colour: the
        // counters restarted somewhere inside the block.
        let rebuilt = names
            .iter()
            .any(|name| baseline(name).is_some_and(|prev| prev.handle != sets[name].handle));

        for iface in ifaces {
            // Δ(packets, bytes) of one set since its previous read; `None`
            // without a baseline (first read, or the element is new).
            let delta = |name: &str| -> Option<(u64, u64)> {
                let now = sets[name].elems.get(iface)?;
                let prev = baseline(name)?.elems.get(iface)?;
                Some((now.0.checked_sub(prev.0)?, now.1.checked_sub(prev.1)?))
            };
            let tx_s = delta(&names[0]);
            let tx_l = delta(&names[1]);
            let rx_s = delta(&names[2]);
            let rx_l = delta(&names[3]);
            let had_baseline = names
                .iter()
                .all(|name| baseline(name).is_some_and(|prev| prev.elems.contains_key(iface)));
            if !had_baseline {
                continue;
            }
            // A counter that went backwards without a handle change is a
            // reset too (a flushed set re-added by hand).
            let reset =
                rebuilt || tx_s.is_none() || tx_l.is_none() || rx_s.is_none() || rx_l.is_none();
            let secs = self.block.as_secs() as u32;
            let b = if reset {
                MarkBlock {
                    n,
                    secs,
                    reset: true,
                    under_test,
                    ..MarkBlock::default()
                }
            } else {
                let (tx_s, tx_l, rx_s, rx_l) =
                    (tx_s.unwrap(), tx_l.unwrap(), rx_s.unwrap(), rx_l.unwrap());
                MarkBlock {
                    n,
                    secs,
                    tx_pkts_s: tx_s.0,
                    tx_pkts_l: tx_l.0,
                    tx_bytes: tx_s.1 + tx_l.1,
                    rx_pkts_s: rx_s.0,
                    rx_pkts_l: rx_l.0,
                    rx_bytes: rx_s.1 + rx_l.1,
                    reset: false,
                    under_test,
                }
            };
            if b.reset {
                out.resets += 1;
            }
            out.blocks += 1;
            push_block(&self.queue, iface, b);
        }

        // Only this colour's sets move their baseline: the other colour is
        // counting the block in progress.
        for name in names {
            let cur = sets[&name].clone();
            st.last.insert(name, (n, cur));
        }
        // Forget sets the include no longer has (keeps the map bounded if
        // the table name changes under us).
        st.last.retain(|k, _| COUNTER_SETS.contains(&k.as_str()));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nft::fake::FakeNft;
    use serde_json::{json, Value};

    /// A `list sets` reply for the fw4 table with every marking set at
    /// `handle` and the given `(iface → (pkts, bytes))` per set.
    fn listing(handle: u64, counts: &[(&str, &str, u64, u64)]) -> Value {
        let mut sets = Vec::new();
        for name in COUNTER_SETS {
            let elems: Vec<Value> = counts
                .iter()
                .filter(|(set, _, _, _)| *set == name)
                .map(|(_, iface, p, b)| {
                    json!({ "elem": { "val": iface, "counter": { "packets": p, "bytes": b } } })
                })
                .collect();
            sets.push(
                json!({ "set": { "family": "inet", "name": name, "table": "fw4",
                                       "type": "ifname", "handle": handle, "elem": elems } }),
            );
        }
        json!({ "nftables": sets })
    }

    fn marker(listings: Vec<Value>) -> Marker<FakeNft> {
        let nft = FakeNft::default();
        *nft.listings.lock().unwrap() = listings.into_iter().map(Ok).collect();
        Marker::new(
            nft,
            "inet fw4",
            Duration::from_secs(10),
            BlockQueue::default(),
        )
    }

    fn a_counts(iface: &str, scale: u64) -> Vec<(&'static str, String, u64, u64)> {
        // tx short/long, rx short/long of colour A, growing with `scale`.
        vec![
            ("mf_tx_a_s", iface.into(), 10 * scale, 1000 * scale),
            ("mf_tx_a_l", iface.into(), 100 * scale, 140_000 * scale),
            ("mf_rx_a_s", iface.into(), 9 * scale, 900 * scale),
            ("mf_rx_a_l", iface.into(), 80 * scale, 112_000 * scale),
        ]
    }

    fn as_refs<'a>(
        v: &'a [(&'static str, String, u64, u64)],
    ) -> Vec<(&'static str, &'a str, u64, u64)> {
        v.iter()
            .map(|(s, i, p, b)| (*s, i.as_str(), *p, *b))
            .collect()
    }

    #[tokio::test]
    async fn first_read_sets_the_baseline_and_the_second_yields_the_block() {
        let c1 = a_counts("awg_hub_a", 1);
        let c2 = a_counts("awg_hub_a", 3);
        let m = marker(vec![listing(7, &as_refs(&c1)), listing(7, &as_refs(&c2))]);
        let ifaces = vec!["awg_hub_a".to_string()];

        // Block 10 (colour A): baseline only.
        let out = m.read(&ifaces, 10, false).await.unwrap();
        assert_eq!(out, ReadOutcome::default());
        assert!(take_blocks(&m.queue, "awg_hub_a").is_empty());

        // Block 12 (colour A again): the delta is the block.
        let out = m.read(&ifaces, 12, true).await.unwrap();
        assert_eq!(out.blocks, 1);
        assert_eq!(out.resets, 0);
        let blocks = take_blocks(&m.queue, "awg_hub_a");
        assert_eq!(
            blocks,
            vec![MarkBlock {
                n: 12,
                secs: 10,
                tx_pkts_s: 20,
                tx_pkts_l: 200,
                tx_bytes: 2000 + 280_000,
                rx_pkts_s: 18,
                rx_pkts_l: 160,
                rx_bytes: 1800 + 224_000,
                reset: false,
                under_test: true,
            }]
        );
        // Drained.
        assert!(take_blocks(&m.queue, "awg_hub_a").is_empty());
    }

    #[tokio::test]
    async fn a_rebuilt_table_marks_the_block_reset() {
        let c1 = a_counts("awg_hub_a", 5);
        let c2 = a_counts("awg_hub_a", 1); // restarted: lower, new handle
        let c3 = a_counts("awg_hub_a", 2);
        let m = marker(vec![
            listing(7, &as_refs(&c1)),
            listing(42, &as_refs(&c2)),
            listing(42, &as_refs(&c3)),
        ]);
        let ifaces = vec!["awg_hub_a".to_string()];
        m.read(&ifaces, 10, false).await.unwrap();
        let out = m.read(&ifaces, 12, false).await.unwrap();
        assert_eq!(
            out,
            ReadOutcome {
                blocks: 1,
                resets: 1
            }
        );
        let b = take_blocks(&m.queue, "awg_hub_a");
        assert_eq!(b.len(), 1);
        assert!(b[0].reset);
        assert_eq!(b[0].n, 12);
        assert_eq!(b[0].tx_bytes, 0, "a reset block carries no numbers");
        // The read after the rebuild has a clean baseline again.
        let out = m.read(&ifaces, 14, false).await.unwrap();
        assert_eq!(
            out,
            ReadOutcome {
                blocks: 1,
                resets: 0
            }
        );
        assert_eq!(take_blocks(&m.queue, "awg_hub_a")[0].tx_pkts_s, 10);
    }

    /// A missed or failed read leaves a baseline from an older block: the
    /// delta over it would cover two blocks, so the next read only sets a
    /// fresh baseline.
    #[tokio::test]
    async fn a_stale_baseline_yields_no_block() {
        let c1 = a_counts("awg_hub_a", 1);
        let c2 = a_counts("awg_hub_a", 3);
        let c3 = a_counts("awg_hub_a", 4);
        let m = marker(vec![
            listing(7, &as_refs(&c1)),
            listing(7, &as_refs(&c2)),
            listing(7, &as_refs(&c3)),
        ]);
        // The read of block 12 fails (an nft timeout): the fake serves the
        // listing meant for it to block 14 instead.
        m.nft
            .listings
            .lock()
            .unwrap()
            .insert(1, Err("nft list sets: timed out".into()));
        let ifaces = vec!["awg_hub_a".to_string()];
        m.read(&ifaces, 10, false).await.unwrap();
        assert!(m.read(&ifaces, 12, false).await.is_err());
        // Block 14: the baseline is block 10's — two blocks of counts.
        let out = m.read(&ifaces, 14, false).await.unwrap();
        assert_eq!(out, ReadOutcome::default());
        assert!(take_blocks(&m.queue, "awg_hub_a").is_empty());
        // Block 16 has a proper baseline again.
        let out = m.read(&ifaces, 16, false).await.unwrap();
        assert_eq!(out.blocks, 1);
        assert_eq!(take_blocks(&m.queue, "awg_hub_a")[0].tx_pkts_s, 10);
    }

    #[tokio::test]
    async fn a_counter_going_backwards_is_a_reset_too() {
        let c1 = a_counts("awg_hub_a", 5);
        let c2 = a_counts("awg_hub_a", 1);
        let m = marker(vec![listing(7, &as_refs(&c1)), listing(7, &as_refs(&c2))]);
        let ifaces = vec!["awg_hub_a".to_string()];
        m.read(&ifaces, 10, false).await.unwrap();
        let out = m.read(&ifaces, 12, false).await.unwrap();
        assert_eq!(
            out,
            ReadOutcome {
                blocks: 1,
                resets: 1
            }
        );
    }

    #[tokio::test]
    async fn an_interface_without_baseline_is_skipped_not_zeroed() {
        let c1 = a_counts("awg_hub_a", 1);
        let mut c2 = a_counts("awg_hub_a", 2);
        c2.extend(a_counts("awg_hub_b", 1));
        let m = marker(vec![listing(7, &as_refs(&c1)), listing(7, &as_refs(&c2))]);
        let ifaces = vec!["awg_hub_a".to_string(), "awg_hub_b".to_string()];
        m.read(&ifaces, 10, false).await.unwrap();
        let out = m.read(&ifaces, 12, false).await.unwrap();
        assert_eq!(out.blocks, 1);
        assert_eq!(take_blocks(&m.queue, "awg_hub_a").len(), 1);
        assert!(take_blocks(&m.queue, "awg_hub_b").is_empty());
    }

    #[tokio::test]
    async fn a_missing_set_is_an_error_not_a_block() {
        let m = marker(vec![json!({ "nftables": [] })]);
        let err = m
            .read(&["awg_hub_a".to_string()], 10, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("mf_tx_a_s missing"), "{err}");
    }

    #[tokio::test]
    async fn flip_applies_the_assert_script_for_the_blocks_colour() {
        let m = marker(vec![]);
        m.flip(&["awg_hub_a".to_string()], 11).await.unwrap();
        let scripts = m.nft.scripts.lock().unwrap();
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].contains("\"awg_hub_a\" : jump mielofon_b"));
        assert!(scripts[0].contains("add element inet fw4 mf_rx_b_l { \"awg_hub_a\" }"));
    }

    #[test]
    fn queue_is_bounded_oldest_dropped() {
        let q = BlockQueue::default();
        for n in 0..(QUEUE_CAP as u64 + 5) {
            push_block(
                &q,
                "awg_hub_a",
                MarkBlock {
                    n,
                    ..Default::default()
                },
            );
        }
        let blocks = take_blocks(&q, "awg_hub_a");
        assert_eq!(blocks.len(), QUEUE_CAP);
        assert_eq!(blocks[0].n, 5);
    }

    #[test]
    fn block_number_is_wall_clock_over_block_length() {
        let m = marker(vec![]);
        assert_eq!(m.block_of(1_747_000_005.9), 174_700_000);
        assert_eq!(m.block_of(1_747_000_010.0), 174_700_001);
    }
}
