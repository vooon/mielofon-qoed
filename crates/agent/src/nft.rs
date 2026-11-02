//! nftables access by running the `nft` CLI.
//!
//! The passive tier (`mark.rs`) needs two things from the kernel every
//! block: apply one atomic transaction that asserts set membership and the
//! colour of every managed link, and read the per-element counters of the
//! marking sets. Both are low-rate (one call each per `mark_block_secs`), so
//! a subprocess per call is as simple and robust as the `ubus` wrapper
//! (`bus.rs`), and keeps the agent free of a libnftables binding.
//! Everything that needs nftables takes an [`Nft`] so tests feed fixtures.
//!
//! The agent writes **no rules**: the ruleset is the fw4 include the package
//! ships (`files/usr/share/nftables.d/table-post/mielofon.nft`); the agent
//! only moves interface names between sets and updates the colour map.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// The counter sets of the include, by direction, colour and length band.
pub const COUNTER_SETS: [&str; 8] = [
    "mf_tx_a_s",
    "mf_tx_a_l",
    "mf_tx_b_s",
    "mf_tx_b_l",
    "mf_rx_a_s",
    "mf_rx_a_l",
    "mf_rx_b_s",
    "mf_rx_b_l",
];

/// The verdict map selecting a link's egress colour.
pub const COLOUR_MAP: &str = "mf_colour";

/// An nftables caller.
pub trait Nft: Send + Sync {
    /// Apply a script (`nft -f -`): one atomic transaction.
    fn apply(&self, script: &str) -> impl Future<Output = Result<()>> + Send;

    /// `nft -j list sets <family>`: every set of the family with its
    /// elements and per-element counters. (Several `list set` commands in
    /// one invocation do not work — nft's cache is built for the first one
    /// only — so the family-wide listing is the one-call read.)
    fn list_sets(&self, family: &str) -> impl Future<Output = Result<Value>> + Send;
}

/// [`Nft`] backed by the `nft` command line tool.
pub struct NftCli {
    pub timeout: Duration,
}

impl Default for NftCli {
    fn default() -> Self {
        NftCli {
            timeout: Duration::from_secs(10),
        }
    }
}

impl NftCli {
    async fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<Vec<u8>> {
        let mut cmd = Command::new("nft");
        cmd.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let what = format!("nft {}", args.join(" "));
        let mut child = cmd.spawn().with_context(|| format!("{what}: spawn"))?;
        if let Some(text) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            pipe.write_all(text.as_bytes())
                .await
                .with_context(|| format!("{what}: write script"))?;
            drop(pipe);
        }
        let out = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow!("{what}: timed out"))??;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("{what}: {} {}", out.status, err.trim());
        }
        Ok(out.stdout)
    }
}

impl Nft for NftCli {
    async fn apply(&self, script: &str) -> Result<()> {
        self.run(&["-f", "-"], Some(script)).await.map(|_| ())
    }

    async fn list_sets(&self, family: &str) -> Result<Value> {
        let out = self.run(&["-j", "list", "sets", family], None).await?;
        serde_json::from_slice(&out).context("nft list sets: reply")
    }
}

/// One set's per-element counters as `nft -j list sets` reports them:
/// `(interface → (packets, bytes))`, plus the set's kernel handle. The
/// handle changes when the table is rebuilt (a firewall reload), which is
/// how the block task notices that every counter restarted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetCounters {
    pub handle: u64,
    pub elems: HashMap<String, (u64, u64)>,
}

/// Pick the marking sets of `table` out of a `list sets` reply.
pub fn parse_sets(reply: &Value, table: &str) -> HashMap<String, SetCounters> {
    let mut out = HashMap::new();
    let Some(items) = reply.get("nftables").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        let Some(set) = item.get("set") else {
            continue;
        };
        if set.get("table").and_then(Value::as_str) != Some(table) {
            continue;
        }
        let Some(name) = set.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !COUNTER_SETS.contains(&name) {
            continue;
        }
        let mut sc = SetCounters {
            handle: set.get("handle").and_then(Value::as_u64).unwrap_or(0),
            elems: HashMap::new(),
        };
        for e in set
            .get("elem")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // `{"elem": {"val": "awg_hub_a", "counter": {"packets": n, "bytes": m}}}`;
            // an element without a counter statement is a plain string.
            let Some(el) = e.get("elem") else {
                continue;
            };
            let Some(iface) = el.get("val").and_then(Value::as_str) else {
                continue;
            };
            let c = el.get("counter");
            let pkts = c
                .and_then(|c| c.get("packets"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let bytes = c
                .and_then(|c| c.get("bytes"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            sc.elems.insert(iface.to_string(), (pkts, bytes));
        }
        out.insert(name.to_string(), sc);
    }
    out
}

/// The colour of a block: `A` for even block numbers, `B` for odd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    A,
    B,
}

impl Colour {
    pub fn of_block(n: u64) -> Colour {
        if n.is_multiple_of(2) {
            Colour::A
        } else {
            Colour::B
        }
    }

    pub fn letter(self) -> char {
        match self {
            Colour::A => 'a',
            Colour::B => 'b',
        }
    }
}

/// Quote an interface name for an nft element list. Interface names are
/// restricted by the kernel (no quotes, no whitespace), but a name that does
/// not fit the script grammar must never reach `nft -f`.
fn quoted(iface: &str) -> Option<String> {
    let ok = !iface.is_empty()
        && iface.len() < 16
        && iface
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
    ok.then(|| format!("\"{iface}\""))
}

/// The per-block assertion transaction: every managed interface is in all
/// eight counter sets (re-adding an existing element is a no-op, so the
/// script is the same whether the firewall was just reloaded or not), and
/// the colour map holds exactly the managed interfaces, each sent to the
/// chain of `colour`. Rebuilding the map in one transaction means no packet
/// ever sees it empty.
pub fn assert_script(table: &str, ifaces: &[String], colour: Colour) -> String {
    let names: Vec<String> = ifaces.iter().filter_map(|i| quoted(i)).collect();
    let mut s = String::new();
    if !names.is_empty() {
        let list = names.join(", ");
        for set in COUNTER_SETS {
            s.push_str(&format!("add element {table} {set} {{ {list} }}\n"));
        }
    }
    s.push_str(&format!("flush map {table} {COLOUR_MAP}\n"));
    if !names.is_empty() {
        let chain = format!("mielofon_{}", colour.letter());
        let pairs: Vec<String> = names
            .iter()
            .map(|n| format!("{n} : jump {chain}"))
            .collect();
        s.push_str(&format!(
            "add element {table} {COLOUR_MAP} {{ {} }}\n",
            pairs.join(", ")
        ));
    }
    s
}

#[cfg(test)]
pub mod fake {
    //! In-memory [`Nft`] for tests: records scripts, serves canned listings.

    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeNft {
        pub scripts: Mutex<Vec<String>>,
        /// Listings served in order; the last one repeats.
        pub listings: Mutex<Vec<Result<Value, String>>>,
        pub fail_apply: Mutex<bool>,
    }

    impl Nft for FakeNft {
        async fn apply(&self, script: &str) -> Result<()> {
            self.scripts.lock().unwrap().push(script.to_string());
            if *self.fail_apply.lock().unwrap() {
                bail!("nft -f -: fake failure");
            }
            Ok(())
        }

        async fn list_sets(&self, _family: &str) -> Result<Value> {
            let mut q = self.listings.lock().unwrap();
            if q.is_empty() {
                bail!("nft list sets: no fixture");
            }
            let r = if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            };
            r.map_err(|e| anyhow!(e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The shape `nft -j list sets inet` produced on nftables 1.1.6 for a
    /// set with per-element counters, an empty counter set and a foreign set.
    pub fn listing() -> Value {
        json!({ "nftables": [
            { "metainfo": { "version": "1.1.6", "release_name": "x", "json_schema_version": 1 } },
            { "set": { "family": "inet", "name": "mf_tx_a_s", "table": "fw4", "type": "ifname",
                       "handle": 7, "stmt": [{ "counter": null }],
                       "elem": [ { "elem": { "val": "awg_hub_a", "counter": { "packets": 4, "bytes": 416 } } },
                                 { "elem": { "val": "awg_hub_b", "counter": { "packets": 0, "bytes": 0 } } } ] } },
            { "set": { "family": "inet", "name": "mf_tx_b_s", "table": "fw4", "type": "ifname",
                       "handle": 9, "stmt": [{ "counter": null }] } },
            { "set": { "family": "inet", "name": "mf_tx_a_s", "table": "other", "type": "ifname",
                       "handle": 1, "elem": [ { "elem": { "val": "eth0", "counter": { "packets": 99, "bytes": 99 } } } ] } },
            { "set": { "family": "inet", "name": "fw4_zone", "table": "fw4", "type": "ifname",
                       "handle": 2, "elem": [ "eth0" ] } }
        ]})
    }

    #[test]
    fn parses_marking_sets_of_the_table_only() {
        let sets = parse_sets(&listing(), "fw4");
        assert_eq!(sets.len(), 2, "{sets:?}");
        let a = &sets["mf_tx_a_s"];
        assert_eq!(a.handle, 7);
        assert_eq!(a.elems["awg_hub_a"], (4, 416));
        assert_eq!(a.elems["awg_hub_b"], (0, 0));
        assert!(sets["mf_tx_b_s"].elems.is_empty());
        assert_eq!(sets["mf_tx_b_s"].handle, 9);
    }

    #[test]
    fn assert_script_is_idempotent_membership_plus_colour_map() {
        let s = assert_script(
            "inet fw4",
            &["awg_hub_a".into(), "bad name".into(), "awg_hub_b".into()],
            Colour::B,
        );
        for set in COUNTER_SETS {
            assert!(s.contains(&format!(
                "add element inet fw4 {set} {{ \"awg_hub_a\", \"awg_hub_b\" }}\n"
            )));
        }
        assert!(s.contains("flush map inet fw4 mf_colour\n"));
        assert!(s.ends_with(
            "add element inet fw4 mf_colour { \"awg_hub_a\" : jump mielofon_b, \"awg_hub_b\" : jump mielofon_b }\n"
        ));
        assert!(!s.contains("bad name"), "unquotable names never reach nft");
    }

    #[test]
    fn assert_script_with_no_links_only_empties_the_map() {
        let s = assert_script("inet fw4", &[], Colour::A);
        assert_eq!(s, "flush map inet fw4 mf_colour\n");
    }

    #[test]
    fn colour_alternates_by_block() {
        assert_eq!(Colour::of_block(0), Colour::A);
        assert_eq!(Colour::of_block(1), Colour::B);
        assert_eq!(Colour::of_block(174_700_001), Colour::B);
    }
}
