import { describe, it, expect } from 'vitest';
import {
	qualityColor,
	qualityWidth,
	isBrokenLink,
	computePositions,
	linkStats,
	sortLinks,
	traceRows,
	sparklineSeries,
	cellText,
} from './logic';

// ── colors / widths ──────────────────────────────────────────────────

describe('qualityColor', () => {
	it('maps known classes and falls back to "none"', () => {
		expect(qualityColor('good')).toBe('#19a974');
		expect(qualityColor('acceptable')).toBe('#d9a400');
		expect(qualityColor('poor')).toBe('#f26d00');
		expect(qualityColor('bad')).toBe('#e7040f');
		expect(qualityColor('weird')).toBe('#c3c9d4');
		expect(qualityColor(null)).toBe('#c3c9d4');
	});
});

describe('isBrokenLink', () => {
	it('flags conflict, bad quality and missing quality as broken', () => {
		expect(isBrokenLink({ state: 'conflict' })).toBe(true);
		expect(isBrokenLink({ state: 'quiet', quality: 'bad' })).toBe(true);
		expect(isBrokenLink({ state: 'quiet', quality: null })).toBe(true);
		expect(isBrokenLink({ state: 'quiet', quality: 'poor' })).toBe(false);
		expect(isBrokenLink({ state: 'busy', quality: null })).toBe(false); // busy ≠ degraded
	});
});

// ── layout ───────────────────────────────────────────────────────────

describe('computePositions', () => {
	const node = (id, group = 'spoke') => ({ id, label: id, group });

	it('places unique hubs evenly on a ring of the requested radius', () => {
		const nodes = [node('cr', 'hub'), node('kr', 'hub'), node('rr', 'hub'), node('nr', 'hub')];
		const pos = computePositions(nodes, [], { hubRadius: 100, spokeRadius: 240 });
		const r = (id) => Math.hypot(pos.get(id).x, pos.get(id).y);
		expect(r('cr')).toBeCloseTo(100, 5);
		expect(r('kr')).toBeCloseTo(100, 5);
		expect(r('rr')).toBeCloseTo(100, 5);
		expect(r('nr')).toBeCloseTo(100, 5);
		// Adjacent hubs are 90° apart (4 hubs → ring starts at -90°).
		const a = (id) => Math.atan2(pos.get(id).y, pos.get(id).x);
		expect(a('cr')).toBeCloseTo(-Math.PI / 2, 5);
		expect(a('kr')).toBeCloseTo(0, 5);
	});

	it('places a spoke toward the centroid of its hub neighbours at spokeRadius', () => {
		const nodes = [node('hub-a', 'hub'), node('hub-b', 'hub'), node('s1')];
		const links = [
			{ from: 's1', to: 'hub-a', interface: 'awg_a' },
			{ from: 's1', to: 'hub-b', interface: 'awg_b' },
		];
		const pos = computePositions(nodes, links, { hubRadius: 100, spokeRadius: 240 });
		const s = pos.get('s1');
		expect(Math.hypot(s.x, s.y)).toBeCloseTo(240, 5);
		// The spoke is on the outward ray; its angle bisects the two hubs.
		const hubA = pos.get('hub-a');
		const hubB = pos.get('hub-b');
		const mid = { x: (hubA.x + hubB.x) / 2, y: (hubA.y + hubB.y) / 2 };
		expect(Math.atan2(s.y, s.x)).toBeCloseTo(Math.atan2(mid.y, mid.x), 5);
	});

	it('fans unlinked spokes on an outer ring so nothing hides at origin', () => {
		const nodes = [node('hub-a', 'hub'), node('orphan-1'), node('orphan-2')];
		const pos = computePositions(nodes, [], { hubRadius: 100, spokeRadius: 240 });
		expect(pos.has('orphan-1')).toBe(true);
		expect(Math.hypot(pos.get('orphan-1').x, pos.get('orphan-1').y)).toBeCloseTo(240, 5);
		expect(Math.hypot(pos.get('orphan-2').x, pos.get('orphan-2').y)).toBeCloseTo(240, 5);
		const a1 = Math.atan2(pos.get('orphan-1').y, pos.get('orphan-1').x);
		const a2 = Math.atan2(pos.get('orphan-2').y, pos.get('orphan-2').x);
		expect(a1).not.toBeCloseTo(a2, 1); // distinct positions
	});

	it('never mutates its inputs', () => {
		const nodes = [{ id: 'hub-a', group: 'hub' }];
		const links = [];
		const snapshot = JSON.stringify(nodes);
		computePositions(nodes, links);
		expect(JSON.stringify(nodes)).toBe(snapshot);
	});
});

// ── census ───────────────────────────────────────────────────────────

describe('linkStats', () => {
	it('counts busy separately and never treats busy as degraded', () => {
		const links = [
			{ state: 'quiet', quality: 'good' },
			{ state: 'quiet', quality: 'bad' },
			{ state: 'quiet', quality: null },
			{ state: 'busy', quality: null },
			{ state: 'busy', quality: 'good' },
		];
		const s = linkStats(links);
		expect(s.good).toBe(1);
		expect(s.bad).toBe(1);
		expect(s.nodata).toBe(1);
		expect(s.busy).toBe(2);
	});
});

// ── sorting ──────────────────────────────────────────────────────────

describe('sortLinks', () => {
	it('sorts stably by from, then to, then interface, without mutating input', () => {
		const links = [
			{ from: 'spoke-b', to: 'hub-a', interface: 'awg_a' },
			{ from: 'spoke-a', to: 'hub-b', interface: 'awg_b' },
			{ from: 'spoke-a', to: 'hub-a', interface: 'awg_a' },
		];
		const out = sortLinks(links);
		expect(out.map(l => l.from)).toEqual(['spoke-a', 'spoke-a', 'spoke-b']);
		expect(out.map(l => l.to)).toEqual(['hub-a', 'hub-b', 'hub-a']);
		expect(links[0].from).toBe('spoke-b'); // original untouched
	});
});

// ── trace rows ───────────────────────────────────────────────────────

describe('traceRows', () => {
	it('flattens ECMP edges and marks terminal/broken hops', () => {
		const trace = {
			edges: [
				{ depth: 0, node: 'A', iface: 'awg_b', to: 'B', broken: false, term: false, rtt_ms: 11, loss_pct: 0 },
				{ depth: 1, node: 'B', iface: 'dummy_awg', to: 'B', broken: false, term: true, rtt_ms: null, loss_pct: null },
				{ depth: 1, node: 'C', broken: true, reason: 'no route', term: false },
			],
		};
		const rows = traceRows(trace);
		expect(rows).toHaveLength(3);
		expect(rows[0].depth).toBe(1);
		expect(rows[1].to).toBe('reached');
		expect(rows[1].term).toBe(true);
		expect(rows[2].broken).toBe(true);
		expect(rows[2].status).toBe('no route');
		expect(rows[2].iface).toBe('\u2014');
	});
});

// ── sparkline ────────────────────────────────────────────────────────

describe('sparklineSeries', () => {
	it('returns null without any rtt-bearing bucket', () => {
		expect(sparklineSeries([])).toBeNull();
		expect(sparklineSeries([{ ts: 1, n: 2 }])).toBeNull();
	});

	it('normalises x to [0,1] and y by the rtt max (floored at 100)', () => {
		// 20 rtt-bearing buckets over 19 seconds.
		const buckets = [];
		for (let i = 0; i < 20; i++) {
			buckets.push({ ts: 1000 + i, n: 1, rtt: { avg: i + 1, max: i + 1 } });
		}
		const s = sparklineSeries(buckets);
		expect(s.rtt).toHaveLength(20);
		expect(s.rtt[0].x).toBeCloseTo(0, 5);
		expect(s.rtt[19].x).toBeCloseTo(1, 5);
		// max = 20 < 100 → y scaled by 100.
		expect(s.rtt[19].y).toBeCloseTo(20 / 100, 5);
		expect(s.max).toBe(100);
	});

	it('raises the ceiling to the largest max seen', () => {
		const buckets = [
			{ ts: 0, rtt: { avg: 10, max: 90 } },
			{ ts: 60, rtt: { avg: 400, max: 900 } },
		];
		const s = sparklineSeries(buckets);
		expect(s.max).toBe(900);
		expect(s.rtt[1].y).toBeCloseTo(400 / 900, 5);
	});

	it('excludes loss when absent but includes it when present', () => {
		const s = sparklineSeries([
			{ ts: 0, rtt: { avg: 5, max: 5 } },
			{ ts: 60, rtt: { avg: 5, max: 5 }, loss: { avg: 3 } },
		]);
		expect(s.loss).toHaveLength(1);
		expect(s.loss[0].y).toBeCloseTo(3 / 100, 5);
	});
});

// ── cell formatting ──────────────────────────────────────────────────

describe('cellText', () => {
	it('renders numbers with the given digits and a dash for null', () => {
		expect(cellText(11.234)).toBe('11.2');
		expect(cellText(1.5, 2)).toBe('1.50');
		expect(cellText(null)).toBe('\u2014');
		expect(cellText(undefined, 3)).toBe('\u2014');
		expect(cellText('nr', 0)).toBe('nr');
	});
});