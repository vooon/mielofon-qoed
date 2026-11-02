import { describe, it, expect } from 'vitest';
import {
	qualityColor,
	qualityWidth,
	isBrokenLink,
	linkStats,
	sortLinks,
	traceRows,
	cellText,
	ringLayout,
	isHubSpokeLink,
	blockSeries,
	regimeText,
} from './logic';

describe('blockSeries', () => {
	const samples = [
		{ ts: 1020, rtt_ms: 5 },
		{ ts: 1010, mark_n: 101, mark_secs: 10, mark_flags: 2, mark_rate_mbps: 8.5, mark_loss_pct: 0.1 },
		{ ts: 1000, mark_n: 100, mark_secs: 10, mark_rate_mbps: 1.2, mark_loss_pct: 0 },
		{ ts: 1030, mark_n: 103, mark_secs: 10, mark_flags: 1, mark_rate_mbps: 9 },
		{ ts: 1040, mark_n: 104, mark_secs: 10 },
	];
	it('keeps the blocks in time order, drops resets and leaves unjoined ones as gaps', () => {
		const s = blockSeries(samples);
		expect(s.xs).toEqual([1000, 1010, 1040]);
		expect(s.rate).toEqual([1.2, 8.5, null]);
		expect(s.loss).toEqual([0, 0.1, null]);
		expect(s.under).toEqual([[1010, 1020]]);
		expect(s.joined).toBe(2);
	});
	it('is empty without blocks', () => {
		expect(blockSeries([{ ts: 1, rtt_ms: 2 }]).xs).toEqual([]);
		expect(blockSeries(undefined).joined).toBe(0);
	});
});

describe('regimeText', () => {
	it('names the shapes and dashes the unknown', () => {
		expect(regimeText('size_selective')).toBe('size-selective loss');
		expect(regimeText(null)).toBe('\u2014');
	});
});

describe('ringLayout', () => {
	const nodes = [
		{ id: 'hub-b', group: 'hub' },
		{ id: 'hub-a', group: 'hub' },
		{ id: 'node-1', group: 'spoke' },
		{ id: 'node-0', group: 'spoke' },
	];
	it('puts hubs on the inner ring and spokes on the outer one, in name order', () => {
		const pos = ringLayout(nodes, {}, 100, 200);
		const r = p => Math.round(Math.hypot(p.x, p.y));
		expect(r(pos['hub-a'])).toBe(100);
		expect(r(pos['hub-b'])).toBe(100);
		expect(r(pos['node-0'])).toBe(200);
		expect(r(pos['node-1'])).toBe(200);
		// hub-a takes the first slot (top), regardless of input order.
		expect(pos['hub-a']).toEqual({ x: 0, y: -100 });
		expect(pos['hub-a']).not.toEqual(pos['hub-b']);
		expect(pos['node-0']).not.toEqual(pos['node-1']);
	});
	it('keeps nodes that already have a position', () => {
		const pos = ringLayout(nodes, { 'hub-a': { x: 7, y: 9 } }, 100, 200);
		expect(pos['hub-a']).toEqual({ x: 7, y: 9 });
		expect(Math.round(Math.hypot(pos['hub-b'].x, pos['hub-b'].y))).toBe(100);
	});
	it('centres a lone hub', () => {
		expect(ringLayout([{ id: 'hub-a', group: 'hub' }])['hub-a']).toEqual({ x: 0, y: 0 });
	});
	it('draws an odd ring as an upright star: first node on top, none at the bottom', () => {
		const five = ['node-0', 'node-1', 'node-2', 'node-3', 'node-4'].map(id => ({ id, group: 'spoke' }));
		const pos = ringLayout(five, {}, 100, 200);
		expect(pos['node-0']).toEqual({ x: 0, y: -200 });
		Object.values(pos).forEach(p => expect(p.y).toBeLessThan(200));
	});
});

describe('isHubSpokeLink', () => {
	const hubs = new Set(['hub-a', 'hub-b']);
	it('is true only from a hub towards a non-hub', () => {
		expect(isHubSpokeLink({ from: 'hub-a', to: 'node-0' }, hubs)).toBe(true);
		expect(isHubSpokeLink({ from: 'node-0', to: 'hub-a' }, hubs)).toBe(false);
		expect(isHubSpokeLink({ from: 'hub-a', to: 'hub-b' }, hubs)).toBe(false);
	});
});

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
