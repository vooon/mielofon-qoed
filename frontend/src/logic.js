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

export function isBrokenLink(link) {
	// Busy is "no measurement while the link is in use", never degraded — it is
	// surfaced separately (state column / chips), not drawn as broken.
	if (link.state === 'busy') return false;
	return link.state === 'conflict' || link.quality === 'bad' || link.quality == null;
}

// Compute a deterministic layout: hubs on a ring, spokes on the outward ray to
// the centroid of their neighbours (star), unlinked / agent-only nodes fanned
// on an outer ring. Pure: returns a Map<id, {x,y}>; never mutates input.
export function computePositions(nodes, links, opts = {}) {
	const hubRadius = opts.hubRadius != null ? opts.hubRadius : 200;
	const spokeRadius = opts.spokeRadius != null ? opts.spokeRadius : Math.round(hubRadius * 2.4);
	const byId = new Map(nodes.map(n => [n.id, n]));
	const hubs = nodes.filter(n => n.group === 'hub');
	const spokes = nodes.filter(n => n.group !== 'hub');
	const pos = new Map();

	hubs.forEach((n, i) => {
		const a = (2 * Math.PI * i) / Math.max(1, hubs.length) - Math.PI / 2;
		pos.set(n.id, { x: Math.cos(a) * hubRadius, y: Math.sin(a) * hubRadius });
	});

	const unlinked = [];
	spokes.forEach(s => {
		const ties = links.filter(l => l.from === s.id || l.to === s.id);
		let cx = 0;
		let cy = 0;
		let k = 0;
		ties.forEach(l => {
			const peer = l.from === s.id ? byId.get(l.to) : byId.get(l.from);
			if (peer && pos.has(peer.id)) {
				cx += pos.get(peer.id).x;
				cy += pos.get(peer.id).y;
				k += 1;
			}
		});
		if (k) {
			cx /= k;
			cy /= k;
			const d = Math.hypot(cx, cy) || 1;
			pos.set(s.id, { x: (cx / d) * spokeRadius, y: (cy / d) * spokeRadius });
		} else {
			pos.set(s.id, { x: 0, y: 0 });
			unlinked.push(s);
		}
	});

	unlinked.forEach((s, i) => {
		const a = (2 * Math.PI * i) / Math.max(1, unlinked.length) - Math.PI / 2;
		pos.set(s.id, { x: Math.cos(a) * spokeRadius, y: Math.sin(a) * spokeRadius });
	});

	return pos;
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

// Sparkline series from /v1/ts buckets: normalized 0..1 x/y points. Returns
// null when no bucket carries an rtt. `max` is the rtt ceiling used for the
// y axis (min 100ms so a flat low-rtt link still looks flat, not noisy).
export function sparklineSeries(buckets) {
	const withRtt = (buckets || []).filter(b => b.rtt != null);
	if (!withRtt.length) return null;
	let max = 100;
	withRtt.forEach(b => {
		if (b.rtt.max > max) max = b.rtt.max;
	});
	const span = Math.max(1, withRtt[withRtt.length - 1].ts - withRtt[0].ts);
	const x = b => (b.ts - withRtt[0].ts) / span;
	return {
		max,
		t0: withRtt[0].ts,
		t1: withRtt[withRtt.length - 1].ts,
		rtt: withRtt.map(b => ({ x: x(b), y: b.rtt.avg / max })),
		loss: withRtt.filter(b => b.loss != null).map(b => ({ x: x(b), y: b.loss.avg / 100 })),
	};
}

// Humanized cell value ("—" for unset, fixed decimals for numbers).
export function cellText(v, digits) {
	if (v == null) return '\u2014';
	if (typeof v === 'number') return v.toFixed(digits == null ? 1 : digits);
	return String(v);
}