//! BIRD actions through rpcd-mod-bird (see `docs/rpcd-mod-bird-handoff.md`).
//!
//! `set_ospf_cost` is a config editor on the rpcd side (derive the runtime
//! config, `configure check`, apply), serialised there with flock; `code 5`
//! means another reconfiguration is in flight and is retried briefly.

use crate::bus::Bus;
use mielofon_proto::RouteRep;
use serde_json::json;
use std::time::Duration;
use tracing::{info, warn};

const BUSY_CODE: i64 = 5;
const MAX_TRIES: u32 = 5;

/// Apply an OSPF cost; `Err` carries the reason reported to the controller.
pub async fn set_ospf_cost(
    bus: &impl Bus,
    iface: &str,
    cost: u32,
    retry_delay: Duration,
) -> Result<(), String> {
    for attempt in 1..=MAX_TRIES {
        let res = bus
            .call(
                "bird",
                "set_ospf_cost",
                json!({ "interface": iface, "cost": cost }),
            )
            .await
            .map_err(|e| format!("bird set_ospf_cost: {e:#}"))?;
        let code = res["code"].as_i64().unwrap_or(-1);
        let stdout = res["stdout"].as_str().unwrap_or("").trim();
        match code {
            0 => {
                info!("cost {iface} = {cost}: {stdout}");
                return Ok(());
            }
            BUSY_CODE if attempt < MAX_TRIES => {
                info!("cost {iface}: BIRD reconfiguration busy, retry {attempt}/{MAX_TRIES}");
                tokio::time::sleep(retry_delay).await;
            }
            BUSY_CODE => break,
            _ => {
                warn!("cost {iface} = {cost}: code {code}: {stdout}");
                return Err(format!("bird returned code {code}"));
            }
        }
    }
    Err(format!("bird reconfiguration busy after {MAX_TRIES} tries"))
}

/// Resolve `target` the way BIRD forwards to it (rpcd-mod-bird >= 0.5.0).
/// Returns `(code, routes)`: 0 resolved, 1 unresolved — also when the rpcd
/// plugin has no `route` method (an unresolvable hop, not an agent error) —
/// and 2 for an empty target.
pub async fn route(bus: &impl Bus, target: &str) -> (i64, Vec<RouteRep>) {
    if target.is_empty() {
        return (2, Vec::new());
    }
    let Ok(res) = bus.call("bird", "route", json!({ "prefix": target })).await else {
        return (1, Vec::new());
    };
    let code = res["code"].as_i64().unwrap_or(1);
    // Parse route by route: one entry the contract does not cover is dropped
    // (and logged), not the whole resolution.
    let routes = res["routes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            serde_json::from_value::<RouteRep>(r.clone())
                .inspect_err(|e| warn!("route {target}: skipping entry: {e}"))
                .ok()
        })
        .collect();
    (code, routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::fake::FakeBus;

    #[tokio::test]
    async fn applies_cost_retrying_busy() {
        let bus = FakeBus::default();
        bus.on("bird.set_ospf_cost", Ok(json!({ "code": 5, "stdout": "" })));
        bus.on(
            "bird.set_ospf_cost",
            Ok(json!({ "code": 0, "stdout": "Reconfigured" })),
        );
        assert_eq!(
            set_ospf_cost(&bus, "awg_hub_a", 25, Duration::ZERO).await,
            Ok(())
        );
        let calls = bus.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, json!({ "interface": "awg_hub_a", "cost": 25 }));
    }

    #[tokio::test]
    async fn cost_errors_are_reported() {
        let bus = FakeBus::default();
        bus.on(
            "bird.set_ospf_cost",
            Ok(json!({ "code": 3, "stdout": "no such iface" })),
        );
        assert_eq!(
            set_ospf_cost(&bus, "awg_x", 25, Duration::ZERO).await,
            Err("bird returned code 3".into())
        );

        let busy = FakeBus::default();
        busy.on("bird.set_ospf_cost", Ok(json!({ "code": 5 })));
        let err = set_ospf_cost(&busy, "awg_x", 25, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(err.contains("busy"));
        assert_eq!(busy.calls.lock().unwrap().len(), MAX_TRIES as usize);

        let missing = FakeBus::default();
        assert!(set_ospf_cost(&missing, "awg_x", 25, Duration::ZERO)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn route_passes_ecmp_through() {
        let bus = FakeBus::default();
        bus.on(
            "bird.route",
            Ok(json!({ "code": 0, "routes": [{
                "prefix": "fd00:0:0:1::3/128", "primary": "via", "proto": "ospf3",
                "preference": 150, "cost": 10, "local_pref": null, "as_path": null,
                "next_hops": [
                    { "kind": "via", "addr": "fd00:0:0:1::2", "iface": "awg_hub_a", "cost": 10 },
                    { "kind": "via", "addr": "fd00:0:0:1::9", "iface": "awg_hub_b", "cost": 10 }
                ]
            }]})),
        );
        let (code, routes) = route(&bus, "fd00:0:0:1::3").await;
        assert_eq!(code, 0);
        assert_eq!(routes[0].next_hops.len(), 2);

        // Old rpcd without `route`: unresolvable hop.
        let old = FakeBus::default();
        assert_eq!(route(&old, "fd00:0:0:1::ff").await.0, 1);
        assert_eq!(route(&old, "").await.0, 2);
    }
}
