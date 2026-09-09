<script setup>
import { ref, watch } from 'vue';
import { fetchTrace } from '../api';
import { traceRows, cellText } from '../logic';

const props = defineProps({
	agents: { type: Array, default: () => [] },
});

const from = ref('');
const to = ref('');
const result = ref(null);
const error = ref('');
const running = ref(false);
const autos = ref(false);
let timer = null;

watch(
	() => props.agents,
	(list) => {
		// Default: first agent → second agent. This is just a starting point,
		// the operator fills in what they actually want to trace.
		if (list.length >= 2 && !from.value) {
			from.value = list[0];
			to.value = list[1];
		}
	},
	{ immediate: true },
);

async function run() {
	if (!from.value || !to.value) return;
	running.value = true;
	error.value = '';
	try {
		result.value = await fetchTrace(from.value, to.value);
	} catch (err) {
		error.value = String(err.message || err);
		result.value = null;
	} finally {
		running.value = false;
	}
}

function toggleAuto() {
	if (timer) {
		clearInterval(timer);
		timer = null;
	}
	if (autos.value) timer = setInterval(run, 5000);
}

const rows = () => (result.value ? traceRows(result.value) : []);
</script>

<template>
  <div class="panel card">
    <h3>path trace</h3>
    <div class="row">
      <select v-model="from"><option v-for="a in agents" :key="a" :value="a">{{ a }}</option></select>
      <span class="arrow">→</span>
      <select v-model="to"><option v-for="a in agents" :key="a" :value="a">{{ a }}</option></select>
      <button :disabled="running || !from || !to" @click="run">{{ running ? '…' : 'trace' }}</button>
      <label class="auto"><input type="checkbox" v-model="autos" @change="toggleAuto" /> auto</label>
    </div>
    <div v-if="error" class="error">{{ error }}</div>
    <p v-else-if="!result" class="hint">pick a source and destination to trace the path.</p>
    <template v-else>
      <div class="meta">
        {{ result.from }} → {{ result.to }} · {{ result.complete ? 'reached' : 'not reached' }} · {{ result.edges.length }} hops
      </div>
      <table>
        <thead>
          <tr><th>#</th><th>node</th><th>egress</th><th>to</th><th>rtt</th><th>loss</th><th>quality</th><th>cost</th></tr>
        </thead>
        <tbody>
          <tr v-for="r in rows()" :key="r._ord" :class="{ term: r.term, broken: r.broken }">
            <td>{{ r.depth }}</td>
            <td>{{ r.node }}</td>
            <td>{{ r.iface }}</td>
            <td>{{ cellText(r.to === 'reached' ? '✓' : r.to) }}</td>
            <td>{{ r.rtt }}</td>
            <td>{{ r.loss }}</td>
            <td>{{ r.quality }}</td>
            <td>{{ r.cost }}</td>
          </tr>
        </tbody>
      </table>
    </template>
  </div>
</template>

<style scoped>
.panel { padding: 12px 14px; }
.panel h3 { font-size: 12px; text-transform: uppercase; color: var(--muted); letter-spacing: 0.04em; margin-bottom: 10px; }
.row { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
.arrow { color: var(--muted); }
.auto { font-size: 12px; color: var(--muted); display: inline-flex; align-items: center; gap: 4px; margin-left: auto; }
.meta { color: var(--muted); margin: 8px 0 6px; font-size: 12px; }
.hint { color: var(--muted); font-size: 13px; }
.error { color: var(--bad); font-size: 13px; margin-top: 8px; }
table { width: 100%; border-collapse: collapse; font-size: 12px; margin-top: 4px; }
th, td { padding: 3px 8px; text-align: left; border-bottom: 1px solid var(--line); white-space: nowrap; }
th { background: #fafbfc; color: var(--muted); font-size: 11px; text-transform: uppercase; }
tr.term td { background: #f0fff4; font-weight: 600; }
tr.broken td { color: var(--bad); font-style: italic; }
</style>