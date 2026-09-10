<script setup>
import { ref, watch } from 'vue';
import { fetchTs } from '../api';
import { sparklineSeries } from '../logic';

const props = defineProps({
	link: { type: Object, default: null },
});

const error = ref('');
const label = ref('');
const points = ref(null);

watch(
	() => props.link,
	async (link) => {
		error.value = '';
		label.value = '';
		points.value = null;
		if (!link) return;
		label.value = link.from + ' → ' + link.to + ' ' + link.interface;
		try {
			const ts = await fetchTs(link);
			points.value = sparklineSeries(ts.buckets);
		} catch (err) {
			error.value = String(err.message || err);
		}
	},
	{ immediate: true },
);

// Build the SVG path string (polyline through the normalized points).
function linePath(series, key, height) {
	const pts = series[key];
	if (!pts || !pts.length) return '';
	return pts.map(p => p.x * 340 + ',' + (height - 2 - p.y * (height - 6)).toFixed(1)).join(' ');
}
</script>

<template>
  <div class="panel card">
    <h3>link history</h3>
    <p v-if="!label" class="hint">select a link on the map to see its rtt/loss history.</p>
    <template v-else>
      <div class="meta">{{ label }}</div>
      <div v-if="error" class="error">{{ error }}</div>
      <div v-else-if="!points" class="hint">no history in the last hour yet.</div>
      <svg v-else :viewBox="'0 0 340 70'" width="100%" height="70" role="img" aria-label="rtt history">
        <polyline :points="linePath(points, 'rtt', 70)" fill="none" stroke="#2f80ed" stroke-width="1.5" />
        <polyline v-if="points.loss.length" :points="linePath(points, 'loss', 70)" fill="none" stroke="#e7040f" stroke-width="1" />
        <text x="300" y="10" font-size="9" fill="#8a94a6">{{ points.max }}ms</text>
      </svg>
      <div class="legend-inline"><i class="sw" style="background:#2f80ed"></i>rtt <i class="sw" style="background:#e7040f"></i>loss</div>
    </template>
  </div>
</template>

<style scoped>
.panel { padding: 12px 14px; }
.panel h3 { font-size: 12px; text-transform: uppercase; color: var(--muted); letter-spacing: 0.04em; margin-bottom: 10px; }
.meta { color: var(--muted); margin-bottom: 6px; font-size: 12px; }
.hint { color: var(--muted); font-size: 13px; }
.error { color: var(--bad); font-size: 13px; }
.legend-inline { margin-top: 4px; font-size: 11px; color: var(--muted); display: flex; gap: 10px; align-items: center; }
.sw { display: inline-block; width: 14px; height: 3px; border-radius: 2px; margin-right: 3px; }
</style>