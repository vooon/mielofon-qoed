<script setup>
import { onBeforeUnmount, ref, watch } from 'vue';
import uPlot from 'uplot';
import 'uplot/dist/uPlot.min.css';
import { fetchTs } from '../api';

const props = defineProps({
	link: { type: Object, default: null },
});

const error = ref('');
const label = ref('');
const hint = ref('');
const el = ref(null);
let plot = null;

const MS = 'ms';
const PCT = 'pct';
const RATE = 'rate';
const MBPS = 'mbps';

const seriesDefs = [
	{ key: 'rtt', label: 'rtt', scale: MS, stroke: '#2f80ed', width: 1.5, fmt: v => v.toFixed(1) + ' ms', points: false, gaps: false },
	{ key: 'loss', label: 'loss', scale: PCT, stroke: '#e7040f', width: 1, fmt: v => v.toFixed(1) + '%', points: false, gaps: false },
	{ key: 'util', label: 'util', scale: PCT, stroke: '#19a974', width: 1, fmt: v => v.toFixed(1) + '%', points: false, gaps: false },
	{ key: 'rr', label: 'rr', scale: RATE, stroke: '#d9a400', width: 1, fmt: v => v.toFixed(1) + ' tps', points: false, gaps: false },
	// tcp throughput is probed on a sparse 5-minute cadence, so connect the
	// samples (spanGaps) and keep small point markers so isolated dots remain
	// legible without a fabricated dense line.
	{ key: 'tcp', label: 'tcp', scale: MBPS, stroke: '#9d5cff', width: 1.75, fmt: v => v.toFixed(2) + ' Mbps', points: { show: true, size: 3 }, gaps: true },
];

// uPlot axis `values` functions receive (self, splits) and must return the
// label strings, one per split value.
function fmtAxis(_self, splits) {
	return splits.map(v => (v >= 1000 ? (v / 1000).toFixed(1) + ' s' : v + ' ms'));
}

function fmtAxisMbps(_self, splits) {
	return splits.map(v => (v >= 10 ? v.toFixed(0) : v.toFixed(1)));
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
			[RATE]: { time: false },
			[MBPS]: { time: false, min: 0, max: tcpMax },
			[PCT]: { min: 0, max: 100, time: false },
		},
		axes: [
			{ scale: 'x', stroke: '#8a94a6', grid: { stroke: '#eef1f5', width: 1 } },
			{ scale: 'ms', stroke: '#8a94a6', grid: { stroke: '#eef1f5', width: 1 }, values: fmtAxis, label: 'rtt/ms' },
			{ scale: PCT, side: 1, stroke: '#8a94a6', grid: { stroke: '#eef1f5', width: 1 } },
			{ scale: RATE, show: false },
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

function destroyPlot() {
	if (plot) {
		plot.destroy();
		plot = null;
	}
}

function render(buckets) {
	destroyPlot();
	if (!el.value || !buckets || !buckets.length) {
		hint.value = 'no history in the last hour yet.';
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
}

watch(
	() => props.link,
	async (link) => {
		error.value = '';
		hint.value = '';
		label.value = link ? link.from + ' → ' + link.to + ' ' + link.interface : '';
		// If the selected link changes while mounted, redraw. The el ref may be
		// unset on first setup pass (component not yet mounted) — the mounted
		// hook handles the initial draw.
		if (link && el.value) render(null);
		if (!link) {
			destroyPlot();
			return;
		}
		try {
			const ts = await fetchTs(link);
			render(ts.buckets || []);
		} catch (err) {
			error.value = String(err.message || err);
		}
	},
	{ immediate: true },
);

onBeforeUnmount(destroyPlot);
</script>

<template>
  <div class="panel card">
    <h3>link history</h3>
    <p v-if="!label" class="hint">select a link on the map to see its rtt/loss history.</p>
    <template v-else>
      <div class="meta">{{ label }}</div>
      <div v-if="error" class="error">{{ error }}</div>
      <div v-else-if="hint" class="hint">{{ hint }}</div>
      <div ref="el" class="trend"></div>
    </template>
  </div>
</template>

<style scoped>
.panel { padding: 12px 14px; }
.panel h3 { font-size: 12px; text-transform: uppercase; color: var(--muted); letter-spacing: 0.04em; margin-bottom: 10px; }
.meta { color: var(--muted); margin-bottom: 6px; font-size: 12px; }
.hint { color: var(--muted); font-size: 13px; }
.error { color: var(--bad); font-size: 13px; }
.trend :deep(.u-title) { display: none; }
.trend :deep(.u-legend) { font-size: 11px; }
.trend :deep(.u-off) { opacity: 0.45; }
</style>