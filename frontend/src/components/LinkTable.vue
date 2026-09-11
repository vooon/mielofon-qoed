<script setup>
import { computed } from 'vue';
import { sortLinks, cellText, QUALITY_ORDER } from '../logic';

const props = defineProps({
	links: { type: Array, default: () => [] },
});
const emit = defineEmits(['select']);

const sorted = computed(() => sortLinks(props.links));

function badge(quality) {
	if (!quality) return '<span class="q q-none">—</span>';
	return '<span class="q q-' + quality + '">' + quality + '</span>';
}
</script>

<template>
  <div class="panel card table-wrap">
    <h3>links · {{ links.length }}</h3>
    <table>
      <thead>
        <tr>
          <th>from</th><th>to</th><th>iface</th><th>rtt ms</th><th>loss %</th>
          <th>jit ms</th><th>tcp Mbps</th><th>state</th><th>quality</th><th>cost</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="l in sorted"
          :key="l.from + '/' + l.to + '/' + l.interface"
          @click="emit('select', l)"
        >
          <td>{{ l.from }}</td>
          <td>{{ l.to }}</td>
          <td>{{ l.interface }}</td>
          <td>{{ cellText(l.rtt_ms) }}</td>
          <td>{{ cellText(l.loss_pct, 2) }}</td>
          <td>{{ cellText(l.jitter_ms) }}</td>
          <td>{{ cellText(l.tcp_mbps, 1) }}</td>
          <td :class="l.state === 'busy' ? 'st-busy' : ''">{{ l.state || 'quiet' }}</td>
          <td v-html="badge(l.quality)"></td>
          <td>{{ cellText(l.ospf_cost, 0) }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.panel { padding: 14px 20px; margin: 0 20px 16px; }
.panel h3 { font-size: 12px; text-transform: uppercase; color: var(--muted); letter-spacing: 0.04em; margin-bottom: 10px; }
.table-wrap { overflow: auto; }
table { width: 100%; border-collapse: collapse; font-size: 12px; }
th, td { padding: 5px 10px; text-align: left; border-bottom: 1px solid var(--line); white-space: nowrap; }
th { background: #fafbfc; color: var(--muted); font-size: 11px; text-transform: uppercase; }
tbody tr { cursor: pointer; }
tbody tr:hover { background: #f5f9ff; }
.st-busy { color: var(--poor); font-weight: 600; }
:deep(.q) { display: inline-block; min-width: 72px; padding: 1px 8px; border-radius: 8px; text-align: center; color: #fff; font-size: 11px; font-weight: 600; }
:deep(.q-good) { background: var(--good); }
:deep(.q-acceptable) { background: var(--acc); }
:deep(.q-poor) { background: var(--poor); }
:deep(.q-bad) { background: var(--bad); }
:deep(.q-none) { background: var(--none); }
</style>