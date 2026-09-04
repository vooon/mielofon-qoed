//! Mesh map: graph data + embedded dashboard page (vis-network).
//!
//! The page renders hubs on a ring and spokes radiating out ("star" layout),
//! with edge thickness proportional to quality class and broken links drawn
//! red/dashed. Pure client-side rendering: the page fetches `/v1/graph` (JSON)
//! and `/v1/status`, no other backend endpoints involved.
//!
//! NOTE: the inline page keeps JS/CSS inside a `##`-quoted raw string because
//! `r#"` would be terminated by the CSS `#` color literals.

use crate::model::{ProbeState, Quality};
use crate::state::AppState;
use std::collections::BTreeMap;

/// A node in the map.
#[derive(serde::Serialize)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    /// "hub" for cluster members, "spoke" otherwise.
    pub group: String,
}

/// A directional link (agent -> peer tunnel) with the latest measurement.
#[derive(serde::Serialize)]
pub struct GraphLink {
    pub from: String,
    pub to: String,
    pub interface: String,
    pub rtt_ms: Option<f64>,
    pub loss_pct: Option<f64>,
    pub rr_tps: Option<f64>,
    pub util_mbps: f64,
    pub state: String,
    pub quality: Option<String>,
    pub ospf_cost: Option<u32>,
    /// Unix seconds of the last probe.
    pub ts: u64,
}

/// Build the graph: nodes = cluster members + every from/to seen in the KV;
/// links = one per quality record.
pub fn graph_part(state: &AppState) -> (Vec<GraphNode>, Vec<GraphLink>) {
    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();

    for name in state.cfg.members.keys() {
        nodes.insert(
            name.clone(),
            GraphNode {
                id: name.clone(),
                label: name.clone(),
                group: "hub".into(),
            },
        );
    }

    let mut links: Vec<GraphLink> = Vec::new();
    for (key, rec) in state.kv.all() {
        for id in [&key.from, &key.to] {
            nodes.entry(id.clone()).or_insert_with(|| GraphNode {
                id: id.clone(),
                label: id.clone(),
                group: if state.cfg.members.contains_key(id) {
                    "hub".to_string()
                } else {
                    "spoke".to_string()
                },
            });
        }
        links.push(GraphLink {
            from: key.from,
            to: key.to,
            interface: key.interface,
            rtt_ms: rec.rtt_ms,
            loss_pct: rec.loss_pct,
            rr_tps: rec.rr_tps,
            util_mbps: rec.util_mbps,
            state: match rec.state {
                ProbeState::Quiet => "quiet",
                ProbeState::Busy => "busy",
                ProbeState::Conflict => "conflict",
            }
            .into(),
            quality: rec
                .quality
                .map(|q| match q {
                    Quality::Good => "good",
                    Quality::Acceptable => "acceptable",
                    Quality::Poor => "poor",
                    Quality::Bad => "bad",
                })
                .map(str::to_string),
            ospf_cost: rec.ospf_cost,
            ts: rec.ts,
        });
    }

    (nodes.into_values().collect(), links)
}

/// The embedded vis-network script (Apache-2.0, visjs).
pub const VIS_NETWORK_JS: &[u8] = include_bytes!("../static/vis-network.min.js");

/// Render the map page. JS builds: ring of hubs, star of spokes, edge width by
/// quality, broken links red/dashed, hover shows iface/rtt/loss/cost. A trace
/// controlbar runs the controller's ECMP-aware end-to-end path trace
/// (`GET /v1/trace`) and renders the hop DAG as a table (parallel rows for
/// equal-cost branches, broken hops red/dashed), with an optional 5s refresh.
pub fn page() -> String {
    r##"
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><title>mielofon — mesh map</title>
<style>
body{font-family:system-ui,sans-serif;margin:0;background:#f6f8fa}
header{padding:12px 20px;border-bottom:1px solid #ddd;background:#fff;display:flex;gap:24px;align-items:baseline;font-size:14px}
#graph{width:100vw;height:calc(100vh - 150px)}
#tracebar{margin-left:auto;white-space:nowrap}
#traceout{max-height:38vh;overflow:auto;border-top:1px solid #ddd;background:#fff;padding:6px 20px;font-size:12px}
#traceout table{border-collapse:collapse;width:100%}
#traceout th,#traceout td{padding:3px 12px;text-align:left;border-bottom:1px solid #eee;white-space:nowrap}
#traceout tr.term td{background:#f0fff4}
#traceout tr.broken td{color:#e7040f;font-style:italic}
#traceout .meta{color:#666;padding:3px 0}
.legend{display:flex;gap:16px;align-items:center}
.legend .lbl{opacity:.7}
.swatch{width:14px;height:4px;border-radius:2px;display:inline-block;margin-right:4px}
</style>
</head>
<body>
<header>
  <b>mielofon — mesh map</b><span id="node"></span><span id="meta"></span>
  <span class="legend">
    <span class="lbl">quality:</span>
    <span><span class="swatch" style="background:#19a974"></span>good</span>
    <span><span class="swatch" style="background:#ffd700"></span>acceptable</span>
    <span><span class="swatch" style="background:#ff971a"></span>poor</span>
    <span><span class="swatch" style="background:#e7040f"></span>bad</span>
    <span><span class="swatch" style="background:#999;height:0;border-top:2px dashed #e7040f"></span>broken/no&nbsp;data</span>
  </span>
  <span id="tracebar">
    <b>trace</b>
    <select id="t-from"></select>&rarr;
    <select id="t-to"></select>
    <button id="t-run">Trace</button>
    <label><input type="checkbox" id="t-auto"> auto 5s</label>
  </span>
</header>
<div id="graph"></div>
<div id="traceout"></div>
<script src="/static/vis-network.min.js"></script>
<script>
"use strict";

const hubRadius = 260;      // hubs sit on a ring of this radius
const spokeRadius = 520;    // spokes radiate outwards to this radius

function colorOf(q) {
	switch (q) {
	case "good": return "#19a974";
	case "acceptable": return "#ffd700";
	case "poor": return "#ff971a";
	case "bad": return "#e7040f";
	}
	return "#999";
}

function widthOf(q) {
	if (q === "good") return 1.5;
	if (q === "acceptable") return 2.5;
	if (q === "poor") return 4;
	if (q === "bad") return 6;
	return 1;
}

function isBroken(link) {
	return link.state === "conflict" || link.quality === "bad" || link.quality == null;
}

function place(nodes, links) {
	const byId = {};
	nodes.forEach(n => byId[n.id] = n);
	const hubs = nodes.filter(n => n.group === "hub");
	const spokes = nodes.filter(n => n.group !== "hub");

	// Hubs on a ring.
	hubs.forEach((n, i) => {
		const a = 2 * Math.PI * i / Math.max(1, hubs.length) - Math.PI / 2;
		n.x = Math.cos(a) * hubRadius;
		n.y = Math.sin(a) * hubRadius;
	});

	// A spoke sits at the outward point toward the centroid of its hub
	// neighbours: star-like fan around the ring.
	spokes.forEach(s => {
		const ties = links.filter(l => l.from === s.id || l.to === s.id);
		let cx = 0, cy = 0, k = 0;
		ties.forEach(l => {
			const peer = (l.from === s.id) ? byId[l.to] : byId[l.from];
			if (peer) { cx += peer.x; cy += peer.y; k++; }
		});
		if (k) { cx /= k; cy /= k; }
		const d = Math.hypot(cx, cy) || 1;
		s.x = cx / d * spokeRadius;
		s.y = cy / d * spokeRadius;
	});
}

function render(node, graph) {
	const nodes = graph.nodes.map(n => ({
		id: n.id, label: n.label, group: n.group,
		x: n.x, y: n.y,
		fixed: { x: true, y: true },
	}));
	const edges = graph.links.map(l => ({
		from: l.from, to: l.to, label: l.interface,
		width: widthOf(l.quality),
		color: isBroken(l) ? { color: "#e7040f" } : { color: colorOf(l.quality) },
		dashes: isBroken(l),
		title: l.interface + " | rtt " +
			(l.rtt_ms != null ? l.rtt_ms.toFixed(1) + " ms" : "-") +
			" | loss " + (l.loss_pct != null ? l.loss_pct.toFixed(1) + "%" : "-") +
			" | cost " + (l.ospf_cost != null ? l.ospf_cost : "-"),
	}));
	const opts = {
		groups: {
			hub: {
				color: { background: "#2f80ed", border: "#1f5fb0" },
				size: 22, shape: "dot", font: { size: 16, color: "#1f5fb0" },
			},
			spoke: {
				color: { background: "#9aa7b4", border: "#788693" },
				size: 14, shape: "dot", font: { size: 12, color: "#788693" },
			},
		},
		physics: false,
		interaction: { hover: true, dragNodes: false },
		edges: { smooth: { enabled: true, type: "continuous" }, selectionWidth: 2 },
	};
	return new vis.Network(node, { nodes: new vis.DataSet(nodes), edges: new vis.DataSet(edges) }, opts);
}

// ── trace controlbar ────────────────────────────────────────────────────

function fillSelects(nodes) {
	const names = nodes.map(n => n.id).sort();
	for (const id of ["t-from", "t-to"]) {
		const sel = document.getElementById(id);
		sel.innerHTML = "";
		names.forEach(n => {
			const o = document.createElement("option");
			o.value = n; o.textContent = n;
			sel.appendChild(o);
		});
	}
	// The page's own node is a sensible default source when present.
	const mine = document.getElementById("node").textContent.replace(/^node:\s*/, "").split(/\s*\|\s*/)[0];
	if (names.indexOf(mine) >= 0) document.getElementById("t-from").value = mine;
}

function cell(text, cls) {
	const td = document.createElement("td");
	td.textContent = text;
	if (cls) td.className = cls;
	return td;
}

function renderTrace(res) {
	const out = document.getElementById("traceout");
	out.innerHTML = "";

	const meta = document.createElement("div");
	meta.className = "meta";
	meta.textContent = res.from + " → " + (res.to || res.prefix) +
		"  prefix " + res.prefix +
		(res.complete ? "  ✓ reached" : "  ✗ not reached") +
		"  " + res.edges.length + " hops";
	out.appendChild(meta);

	const table = document.createElement("table");
	const head = document.createElement("tr");
	["depth", "node", "egress", "to", "status", "rtt", "loss", "quality", "cost"].forEach(h => {
		const th = document.createElement("th");
		th.textContent = h;
		head.appendChild(th);
	});
	table.appendChild(head);

	res.edges.forEach(e => {
		const row = document.createElement("tr");
		if (e.term) row.className = "term";
		else if (e.broken) row.className = "broken";
		row.appendChild(cell(e.depth + 1));
		row.appendChild(cell(e.node));
		row.appendChild(cell(e.iface || "-"));
		row.appendChild(cell(e.term ? "reached" : (e.to || "-")));
		row.appendChild(cell(e.broken ? (e.reason ? e.reason : "broken") : "ok"));
		row.appendChild(cell(e.rtt_ms != null ? e.rtt_ms.toFixed(1) + " ms" : "-"));
		row.appendChild(cell(e.loss_pct != null ? e.loss_pct.toFixed(1) + "%" : "-"));
		row.appendChild(cell(e.quality || "-"));
		row.appendChild(cell(e.ospf_cost != null ? e.ospf_cost : "-"));
		table.appendChild(row);
	});

	out.appendChild(table);
}

let traceTimer = null;

function runTrace() {
	const from = document.getElementById("t-from").value;
	const to = document.getElementById("t-to").value;
	if (!from || !to) return;
	fetch("/v1/trace?from=" + encodeURIComponent(from) + "&to=" + encodeURIComponent(to))
		.then(r => {
			if (!r.ok) return r.json().then(j => { throw new Error(j.error); });
			return r.json();
		})
		.then(renderTrace)
		.catch(err => {
			document.getElementById("traceout").textContent = "trace failed: " + err.message;
		});
}

function scheduleAuto() {
	if (traceTimer) { clearInterval(traceTimer); traceTimer = null; }
	if (document.getElementById("t-auto").checked)
		traceTimer = setInterval(runTrace, 5000);
}

document.getElementById("t-run").addEventListener("click", runTrace);
document.getElementById("t-auto").addEventListener("change", scheduleAuto);

fetch("/v1/status").then(r => r.json()).then(s => {
	document.getElementById("node").textContent =
		"node: " + s.node + " | ready: " + s.ready +
		" | members: " + (s.members || []).join(", ");
}).catch(() => {});

fetch("/v1/graph").then(r => r.json()).then(graph => {
	place(graph.nodes, graph.links);
	render(document.getElementById("graph"), graph);
	document.getElementById("meta").textContent =
		graph.nodes.length + " nodes | " + graph.links.length + " links";
	fillSelects(graph.nodes);
	runTrace();
});
</script>
</body>
</html>
"##
    .to_string()
}
