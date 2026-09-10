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
	{ key: 'rtt', label: 'rtt', scale: MS, stroke: '#2f80ed', width: 1.5, fmt: v => v.toFixed(1) + ' ms' },
	{ key: 'loss', label: 'loss', scale: PCT, stroke: '#e7040f', width: 1, fmt: v => v.toFixed(1) + '%' },
	{ key: 'util', label: 'util', scale: PCT, stroke: '#19a974', width: 1, fmt: v => v.toFixed(1) + '%' },
	{ key: 'rr', label: 'rr', scale: RATE, stroke: '#d9a400', width: 1, fmt: v => v.toFixed(1) + ' tps' },
	{ key: 'tcp', label: 'tcp', scale: MBPS, stroke: '#9d5cff', width: 1.5, fmt: v => v.toFixed(2) + ' Mbps' },
];

function fmtAxis(v) {
	if (v >= 1000) return (v / 1000).toFixed(1) + ' s';
	return v + ' ms';
}

function optsFor() {
	const width = el.value ? el.value.clientWidth : 340;
	const series = [
		{}, // x/time
		...seriesDefs.map(s => ({
			label: s.label,
			scale: s.scale,
			stroke: s.stroke,
			width: s.width,
			spanGaps: false,
			points: { show: false },
			value: (_self, v) => (v == null ? '' : s.fmt(v)),
		})),
	];
	return {
		width,
		height: 200,
		legend: { show: true, live: true },
		scales: {
			x: { time: true },
			[MS]: { time: false },
			[RATE]: { time: false },
			[MBPS]: { time: false },
			[PCT]: { min: 0, max: 100, time: false },
		},
		axes: [
			{ stroke: '#8a94a6', grid: { stroke: '#334' } },
			{ stroke: '#8a94a6', grid: { stroke: '#334' }, values: fmtAxis, label: 'rtt/ms' },
			{ scale: PCT, side: 1, stroke: '#8a94a6', grid: { stroke: '#334' } },
			{ scale: RATE, show: false },
			{ scale: MBPS, show: false },
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
	plot = new uPlot(optsFor(), dataFor(buckets), el.value);
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