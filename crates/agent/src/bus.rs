//! ubus access by running the `ubus` CLI.
//!
//! The agent's ubus traffic is low-rate (config load, discovery every 30 s,
//! cost applies, trace lookups), so a subprocess per call is simpler and more
//! robust than binding libubus. Everything that needs ubus takes a [`Bus`] so
//! tests can feed fixtures instead.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio::process::Command;

/// A ubus method caller.
pub trait Bus: Send + Sync {
    /// `ubus call <object> <method> <args>`; the reply object, or `Null` when
    /// the method returned no data.
    fn call(
        &self,
        object: &str,
        method: &str,
        args: Value,
    ) -> impl Future<Output = Result<Value>> + Send;
}

/// [`Bus`] backed by the `ubus` command line tool.
pub struct UbusCli {
    /// Per-call timeout, passed to `ubus -t` and enforced on the process.
    pub timeout: Duration,
}

impl Default for UbusCli {
    fn default() -> Self {
        // rpcd's `bird set_ospf_cost` validates and reloads the BIRD config,
        // which can take a few seconds on a small router.
        UbusCli {
            timeout: Duration::from_secs(30),
        }
    }
}

impl Bus for UbusCli {
    async fn call(&self, object: &str, method: &str, args: Value) -> Result<Value> {
        let secs = self.timeout.as_secs().max(1);
        let mut cmd = Command::new("ubus");
        cmd.args(["-S", "-t", &secs.to_string(), "call", object, method])
            .arg(args.to_string())
            .kill_on_drop(true);

        let out = tokio::time::timeout(self.timeout + Duration::from_secs(5), cmd.output())
            .await
            .map_err(|_| anyhow!("ubus call {object} {method}: timed out"))?
            .with_context(|| format!("ubus call {object} {method}: spawn"))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!("ubus call {object} {method}: {} {}", out.status, err.trim());
        }

        let text = String::from_utf8_lossy(&out.stdout);
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).with_context(|| format!("ubus call {object} {method}: reply"))
    }
}

#[cfg(test)]
pub mod fake {
    //! In-memory [`Bus`] for tests: canned replies keyed by `object.method`.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeBus {
        pub replies: Mutex<HashMap<String, Vec<Result<Value, String>>>>,
        pub calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeBus {
        /// Queue a reply for `object.method` (replies are consumed in order;
        /// the last one is repeated).
        pub fn on(&self, key: &str, reply: Result<Value, String>) {
            self.replies
                .lock()
                .unwrap()
                .entry(key.to_string())
                .or_default()
                .push(reply);
        }
    }

    impl Bus for FakeBus {
        async fn call(&self, object: &str, method: &str, args: Value) -> Result<Value> {
            let key = format!("{object}.{method}");
            self.calls.lock().unwrap().push((key.clone(), args));
            let mut map = self.replies.lock().unwrap();
            let Some(q) = map.get_mut(&key) else {
                bail!("ubus call {key}: not found");
            };
            let r = if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            };
            r.map_err(|e| anyhow!(e))
        }
    }
}
