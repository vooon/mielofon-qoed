//! Embedded static dashboard served on the admin listener. Sanitized — uses
//! only placeholder node labels and the live KV view.

use crate::state::AppState;

pub fn render(state: &AppState) -> String {
    let mut rows = String::new();
    for (key, rec) in state.kv.all() {
        let quality = rec
            .quality
            .map(|q| format!("{:?}", q).to_lowercase())
            .unwrap_or_else(|| "—".to_string());
        let cost = rec
            .ospf_cost
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".to_string());
        let state = format!("{:?}", rec.state).to_lowercase();
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            key.from, key.to, key.interface,
            rec.rtt_ms.map(|v| format!("{v:.1}")).unwrap_or_else(|| "—".to_string()),
            rec.loss_pct.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".to_string()),
            state, quality, cost
        ));
    }

    format!(
        r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>mielofon — {name}</title>
<style>body{{font-family:monospace;margin:2em}}table{{border-collapse:collapse}}td,th{{border:1px solid #888;padding:4px 8px;text-align:left}}</style>
</head>
<body>
<h1>mielofon — {name} ({ready})</h1>
<p>links: {n} · uptime: {up}s</p>
<table><thead><tr><th>from</th><th>to</th><th>iface</th><th>rtt ms</th><th>loss %</th><th>state</th><th>quality</th><th>cost</th></tr></thead>
<tbody>{rows}</tbody></table>
</body></html>"#,
        name = state.cfg.node.name,
        ready = if state.is_ready() {
            "ready"
        } else {
            "not ready"
        },
        n = state.kv.len(),
        up = state.started_at.elapsed().as_secs(),
    )
}
