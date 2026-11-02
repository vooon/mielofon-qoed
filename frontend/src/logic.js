// Pure, dependency-free helpers for the mielofon dashboard.
//
// Everything in this module is DOM-free and side-effect-free so it can be unit
// tested under vitest (see `src/*.test.js`). Components import these helpers;
// the tests import them directly.

export const COLOR = {
	good: '#19a974',
	acceptable: '#d9a400',
	poor: '#f26d00',
	bad: '#e7040f',
	none: '#c3c9d4',
};

export const QUALITY_ORDER = ['good', 'acceptable', 'poor', 'bad'];

// Edge styling.
export function qualityColor(q) {
	return COLOR[q] || COLOR.none;
}

export function qualityWidth(q) {
	if (q === 'good') return 1.5;
	if (q === 'acceptable') return 2.5;
	if (q === 'poor') return 4;
	if (q === 'bad') return 6;
	return 1;
}

// Hub→spoke edges are the mesh's leaves (costed out of transit by the
// controller); they are drawn lighter so the hub core stays readable.
export function isHubSpokeLink(link, hubs) {
	return hubs.has(link.from) && !hubs.has(link.to);
}

// Deterministic positions: hubs on an inner ring, spokes on an outer ring,
// both in name order, so the map looks the same on every load and never
// needs physics. Both rings start at the top (12 o'clock) and go clockwise,
// so an odd ring reads as an upright star — one point up, never one node
// sitting at the bottom. `existing` keeps nodes where they already are
// (dragged or placed earlier); only new nodes get ring slots.
export function ringLayout(nodes, existing = {}, inner = 220, outer = 460) {
	const byName = (a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0);
	const hubs = nodes.filter(n => n.group === 'hub').sort(byName);
	const spokes = nodes.filter(n => n.group !== 'hub').sort(byName);
	const out = {};
	const place = (list, radius, phase) =>
		list.forEach((n, i) => {
			if (existing[n.id]) {
				out[n.id] = existing[n.id];
				return;
			}
			const a = phase + (2 * Math.PI * i) / Math.max(list.length, 1);
			out[n.id] = { x: Math.round(radius * Math.cos(a)) || 0, y: Math.round(radius * Math.sin(a)) || 0 };
		});
	place(hubs, hubs.length === 1 ? 0 : inner, -Math.PI / 2);
	place(spokes, outer, -Math.PI / 2);
	return out;
}

export function isBrokenLink(link) {
	// Busy is "no measurement while the link is in use", never degraded — it is
	// surfaced separately (state column / chips), not drawn as broken.
	if (link.state === 'busy') return false;
	return link.state === 'conflict' || link.quality === 'bad' || link.quality == null;
}

// Per-link quality census.
export function linkStats(links) {
	const stats = { good: 0, acceptable: 0, poor: 0, bad: 0, busy: 0, nodata: 0 };
	links.forEach(l => {
		if (l.state === 'busy') stats.busy += 1;
		else stats[l.quality || 'nodata'] += 1;
	});
	return stats;
}

export function sortLinks(links) {
	return links.slice().sort((a, b) => {
		const ka = a.from + '\u0000' + a.to + '\u0000' + a.interface;
		const kb = b.from + '\u0000' + b.to + '\u0000' + b.interface;
		return ka < kb ? -1 : ka > kb ? 1 : 0;
	});
}

// Frank the "trace table rows" from a TraceResult: flatten ECMP branches and
// mark terminal / broken hops. Pure and testable.
export function traceRows(trace) {
	return (trace.edges || []).map((e, i) => ({
		depth: e.depth + 1,
		node: e.node,
		iface: e.iface || '\u2014',
		to: e.term ? 'reached' : e.to || '\u2014',
		status: e.broken ? (e.reason || 'broken') : 'ok',
		rtt: e.rtt_ms != null ? e.rtt_ms.toFixed(1) : '\u2014',
		loss: e.loss_pct != null ? e.loss_pct.toFixed(1) : '\u2014',
		quality: e.quality || '\u2014',
		cost: e.ospf_cost != null ? e.ospf_cost : '\u2014',
		term: e.term,
		broken: e.broken,
		_ord: i,
	}));
}

// Passive tier (docs/passive-quality.md): the alternate-marking blocks of
// a link out of its raw samples, shaped for the loss-versus-rate plot.
// Blocks are the samples carrying a block number; the classifier writes the
// joined loss and rate back onto them, so a block without `mark_loss_pct`
// is one whose peer half has not arrived (plotted as a gap, never as zero).
// `under` lists the blocks a throughput test overlapped (flag bit 1), as
// [start, end] spans for shading; `reset` blocks (bit 0) are skipped.
export function blockSeries(samples) {
	const blocks = (samples || [])
		.filter(s => s.mark_n != null && !((s.mark_flags || 0) & 1))
		.sort((a, b) => a.ts - b.ts);
	return {
		xs: blocks.map(b => b.ts),
		rate: blocks.map(b => (b.mark_rate_mbps != null ? b.mark_rate_mbps : null)),
		loss: blocks.map(b => (b.mark_loss_pct != null ? b.mark_loss_pct : null)),
		under: blocks.filter(b => (b.mark_flags || 0) & 2).map(b => [b.ts, b.ts + (b.mark_secs || 0)]),
		joined: blocks.filter(b => b.mark_loss_pct != null).length,
	};
}

export const REGIME_TEXT = {
	clean: 'clean',
	sparse: 'sparse loss',
	bursty: 'bursty loss',
	size_selective: 'size-selective loss',
	under_test: 'under test',
};

export function regimeText(r) {
	return REGIME_TEXT[r] || '\u2014';
}

// Humanized cell value ("—" for unset, fixed decimals for numbers).
export function cellText(v, digits) {
	if (v == null) return '\u2014';
	if (typeof v === 'number') return v.toFixed(digits == null ? 1 : digits);
	return String(v);
}
