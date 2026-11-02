<script setup>
import { onBeforeUnmount, ref, watch } from 'vue';
import uPlot from 'uplot';
import 'uplot/dist/uPlot.min.css';
import { fetchTs } from '../api';
import { blockSeries, regimeText } from '../logic';

const props = defineProps({
	link: { type: Object, default: null },
});

const error = ref('');
const label = ref('');
const hint = ref('');
const blockHint = ref('');
const el = ref(null);
const blockEl = ref(null);
let plot = null;
let blockPlot = null;

const MS = 'ms';
const PCT = 'pct';
const MBPS = 'mbps';

const seriesDefs = [
	{ key: 'rtt', label: 'rtt', scale: MS, stroke: '#2f80ed', width: 1.5, fmt: v => v.toFixed(1) + ' ms', points: false, gaps: false },
	{ key: 'loss', label: 'loss', scale: PCT, stroke: '#e7040f', width: 1, fmt: v => v.toFixed(1) + '%', points: false, gaps: false },
	{ key: 'util', label: 'util', scale: PCT, stroke: '#19a974', width: 1, fmt: v => v.toFixed(1) + '%', points: false, gaps: false },
	{ key: 'jitter', label: 'jitter', scale: MS, stroke: '#d9a400', width: 1, fmt: v => v.toFixed(2) + ' ms', points: false, gaps: false },
	// tcp throughput is probed on a sparse 5-minute cadence, so connect the
	// samples (spanGaps) and keep small point markers so isolated dots remain
	// legible without a fabricated dense line.
	{ key: 'tcp', label: 'tcp', scale: MBPS, stroke: '#9d5cff', width: 1.75, fmt: v => v.toFixed(2) + ' Mbps', points: { show: true, size: 3 }, gaps: true },
];

const axisStroke = '#8a94a6';
const grid = { stroke: '#eef1f5', width: 1 };

// uPlot axis `values` functions receive (self, splits) and must return the
// label strings, one per split value.
function fmtAxis(_self, splits) {
	return splits.map(v => (v >= 1000 ? (v / 1000).toFixed(1) + ' s' : v + ' ms'));
}

function fmtAxisMbps(_self, splits) {
	return splits.map(v => (v >= 10 ? v.toFixed(0) : v.toFixed(1)));
}

function fmtAxisPct(_self, splits) {
	return splits.map(v => (v >= 1 ? v.toFixed(1) : v.toFixed(2)) + '%');
}

function optsFor(tcpMax) {
	const width = el.value ? el.value.clientWidth : 340;
	const series = [
		{}, // x/time
		...seriesDefs.map(s => ({
			label: s.label,
			scale: s.scale,
			stroke: s.stroke,
			width: s.width,
			spanGaps: !!s.gaps,
			points: s.points || { show: false },
			value: (_self, v) => (v == null ? '' : s.fmt(v)),
		})),
	];
	// Give the sparse throughput scale an explicit ceiling so its points
	// actually render (uPlot won't auto-range a scale whose axis is hidden and
	// whose data is sparse at the window edge).
	return {
		width,
		height: 200,
		legend: { show: true, live: true },
		scales: {
			x: { time: true },
			[MS]: { time: false },
			[MBPS]: { time: false, min: 0, max: tcpMax },
			[PCT]: { min: 0, max: 100, time: false },
		},
		axes: [
			{ scale: 'x', stroke: axisStroke, grid },
			{ scale: 'ms', stroke: axisStroke, grid, values: fmtAxis, label: 'rtt/ms' },
			{ scale: PCT, side: 1, stroke: axisStroke, grid },
			{ scale: MBPS, side: 1, stroke: '#9d5cff', grid: { show: false }, values: fmtAxisMbps },
		],
		series,
	};
}

function dataFor(buckets) {
	const xs = buckets.map(b => b.ts);
	const cols = seriesDefs.map(s => buckets.map(b => (b[s.key] != null ? b[s.key].avg : null)));
	return [xs, ...cols];
}

// Passive tier: per-block rate as an area, per-block loss as points, the
// blocks a throughput test overlapped shaded (their loss is excluded from
// the classifier's aggregate, their rate is the test's).
function blockOptsFor(rateMax, lossMax, under) {
	const width = blockEl.value ? blockEl.value.clientWidth : 340;
	return {
		width,
		height: 160,
		legend: { show: true, live: true },
		scales: {
			x: { time: true },
			[MBPS]: { time: false, min: 0, max: rateMax },
			[PCT]: { time: false, min: 0, max: lossMax },
		},
		axes: [
			{ scale: 'x', stroke: axisStroke, grid },
			{ scale: MBPS, stroke: '#19a974', grid, values: fmtAxisMbps, label: 'rate/Mbps' },
			{ scale: PCT, side: 1, stroke: '#e7040f', grid: { show: false }, values: fmtAxisPct },
		],
		series: [
			{},
			{
				label: 'rate',
				scale: MBPS,
				stroke: '#19a974',
				fill: 'rgba(25, 169, 116, 0.18)',
				width: 1,
				spanGaps: false,
				points: { show: false },
				value: (_self, v) => (v == null ? '' : v.toFixed(2) + ' Mbps'),
			},
			{
				label: 'loss',
				scale: PCT,
				stroke: '#e7040f',
				width: 0,
				paths: () => null, // points only: each block is an exact count
				points: { show: true, size: 4, fill: '#e7040f' },
				value: (_self, v) => (v == null ? '' : v.toFixed(3) + '%'),
			},
		],
		hooks: {
			draw: [
				u => {
					if (!under.length) return;
					const { ctx, bbox } = u;
					ctx.save();
					ctx.fillStyle = 'rgba(138, 148, 166, 0.18)';
					under.forEach(([from, to]) => {
						const x0 = u.valToPos(from, 'x', true);
						const x1 = u.valToPos(to, 'x', true);
						ctx.fillRect(x0, bbox.top, Math.max(x1 - x0, 1), bbox.height);
					});
					ctx.restore();
				},
			],
		},
	};
}

function destroyPlots() {
	if (plot) {
		plot.destroy();
		plot = null;
	}
	if (blockPlot) {
		blockPlot.destroy();
		blockPlot = null;
	}
}

function render(buckets, samples) {
	destroyPlots();
	if (!el.value || !buckets || !buckets.length) {
		hint.value = 'no history in the last hour yet.';
		blockHint.value = '';
		return;
	}
	hint.value = '';
	// Ceiling for the sparse throughput scale: a little headroom above the
	// max tcp sample so points sit comfortably inside the plot area.
	let tcpMax = 0;
	buckets.forEach(b => {
		if (b.tcp && b.tcp.avg > tcpMax) tcpMax = b.tcp.avg;
	});
	tcpMax *= 1.15;
	plot = new uPlot(optsFor(tcpMax), dataFor(buckets), el.value);

	const blocks = blockSeries(samples);
	if (!blockEl.value || !blocks.xs.length) {
		blockHint.value = 'no marking blocks yet (passive tier off, or the include not loaded).';
		return;
	}
	if (!blocks.joined) {
		blockHint.value = 'blocks counted, none joined yet (the peer half has not arrived).';
		return;
	}
	blockHint.value = '';
	const rateMax = Math.max(...blocks.rate.filter(v => v != null), 0.1) * 1.15;
	const lossMax = Math.max(...blocks.loss.filter(v => v != null), 0.1) * 1.15;
	blockPlot = new uPlot(
		blockOptsFor(rateMax, lossMax, blocks.under),
		[blocks.xs, blocks.rate, blocks.loss],
		blockEl.value,
	);
}

watch(
	() => props.link,
	async (link) => {
		error.value = '';
		hint.value = '';
		blockHint.value = '';
		label.value = link ? link.from + ' → ' + link.to + ' ' + link.interface : '';
		// If the selected link changes while mounted, redraw. The el ref may be
		// unset on first setup pass (component not yet mounted) — the mounted
		// hook handles the initial draw.
		if (link && el.value) render(null, null);
		if (!link) {
			destroyPlots();
			return;
		}
		try {
			const ts = await fetchTs(link);
			render(ts.buckets || [], ts.samples || []);
		} catch (err) {
			error.value = String(err.message || err);
		}
	},
	{ immediate: true },
);

onBeforeUnmount(destroyPlots);
</script>

<template>
  <div class="panel card">
    <h3>link history</h3>
    <p v-if="!label" class="hint">select a link on the map to see its rtt/loss history.</p>
    <template v-else>
      <div class="meta">
        {{ label }}
        <span v-if="link && link.regime" :class="'regime rg-' + link.regime" :title="'passive loss shape'">{{ regimeText(link.regime) }}</span>
      </div>
      <div v-if="error" class="error">{{ error }}</div>
      <div v-else-if="hint" class="hint">{{ hint }}</div>
      <div ref="el" class="trend"></div>
      <template v-if="!error && !hint">
        <h4>passive loss vs rate</h4>
        <div v-if="blockHint" class="hint">{{ blockHint }}</div>
        <div ref="blockEl" class="trend"></div>
      </template>
    </template>
  </div>
</template>

<style scoped>
.panel { padding: 12px 14px; }
.panel h3 { font-size: 12px; text-transform: uppercase; color: var(--muted); letter-spacing: 0.04em; margin-bottom: 10px; }
.panel h4 { font-size: 11px; text-transform: uppercase; color: var(--muted); letter-spacing: 0.04em; margin: 12px 0 6px; }
.meta { color: var(--muted); margin-bottom: 6px; font-size: 12px; }
.regime { display: inline-block; margin-left: 8px; padding: 1px 8px; border-radius: 8px; font-size: 11px; font-weight: 600; background: var(--none); color: #fff; }
.rg-clean { background: var(--good); }
.rg-sparse, .rg-bursty { background: var(--poor); }
.rg-size_selective { background: var(--bad); }
.hint { color: var(--muted); font-size: 13px; }
.error { color: var(--bad); font-size: 13px; }
.trend :deep(.u-title) { display: none; }
.trend :deep(.u-legend) { font-size: 11px; }
.trend :deep(.u-off) { opacity: 0.45; }
</style>
