<script setup>
import { onMounted, reactive, ref } from 'vue';
import { fetchGraph, fetchStatus } from './api';
import { linkStats } from './logic';
import MeshGraph from './components/MeshGraph.vue';
import TracePanel from './components/TracePanel.vue';
import TrendPanel from './components/TrendPanel.vue';
import LinkTable from './components/LinkTable.vue';

const status = reactive({ node: '', ready: false, uptime_secs: 0, members: [] });
const graph = reactive({ nodes: [], links: [], agents: [] });

// The link selected in the graph → the trend panel follows it.
const selected = ref(null);

// Keys for TrendPanel: re-fetch when a new link is selected.
const selectedKey = ref('');

function onSelect(link) {
	selected.value = link;
	selectedKey.value = link ? link.from + '/' + link.to + '/' + link.interface : '';
}

onMounted(async () => {
	try {
		Object.assign(status, await fetchStatus());
	} catch {
		/* status is informational only */
	}
	try {
		Object.assign(graph, await fetchGraph());
	} catch (err) {
		graph.nodes = [];
		graph.links = [];
		graph.agents = [];
		graph.error = String(err.message || err);
	}
});

const stats = () => linkStats(graph.links);
const fmtUp = () => {
	const s = status.uptime_secs;
	const h = Math.floor(s / 3600);
	const m = Math.floor((s % 3600) / 60);
	return h + 'h ' + m + 'm';
};
</script>

<template>
  <div class="app">
    <header class="topbar">
      <h1>mielofon</h1>
      <span class="sub">{{ status.node }} · {{ status.ready ? 'ready' : 'starting' }} · up {{ fmtUp() }}</span>
      <span class="members" v-if="status.members.length">members: {{ status.members.join(', ') }}</span>
    </header>

    <div class="chips" v-if="!graph.error">
      <span class="chip"><i class="dot" style="background:var(--good)"></i>good <b>{{ stats().good }}</b></span>
      <span class="chip"><i class="dot" style="background:var(--acc)"></i>acceptable <b>{{ stats().acceptable }}</b></span>
      <span class="chip"><i class="dot" style="background:var(--poor)"></i>poor <b>{{ stats().poor }}</b></span>
      <span class="chip"><i class="dot" style="background:var(--bad)"></i>bad <b>{{ stats().bad }}</b></span>
      <span class="chip"><i class="dot" style="background:var(--poor)"></i>busy <b>{{ stats().busy }}</b></span>
      <span class="chip"><i class="dot" style="background:var(--none)"></i>no data <b>{{ stats().nodata }}</b></span>
      <span class="chip">nodes <b>{{ graph.nodes.length }}</b></span>
      <span class="chip">links <b>{{ graph.links.length }}</b></span>
    </div>

    <div class="alert" v-if="graph.error">{{ graph.error }}</div>

    <main class="layout">
      <MeshGraph
        :nodes="graph.nodes"
        :links="graph.links"
        :selected="selected"
        @select="onSelect"
      />
      <aside class="side">
        <TracePanel :agents="graph.agents" />
        <TrendPanel :link="selected" :key="selectedKey" />
      </aside>
    </main>

    <LinkTable :links="graph.links" @select="onSelect" />
  </div>
</template>

<style scoped>
.app { display: flex; flex-direction: column; height: 100%; }
.topbar {
	display: flex; gap: 20px; align-items: baseline; padding: 14px 20px;
	background: var(--card); border-bottom: 1px solid var(--line);
}
.topbar h1 { font-size: 18px; }
.sub { color: var(--muted); font-size: 13px; }
.members { margin-left: auto; color: var(--muted); font-size: 12px; }
.chips { display: flex; gap: 8px; flex-wrap: wrap; padding: 10px 20px; font-size: 13px; }
.chip { background: var(--card); border: 1px solid var(--line); border-radius: 10px; padding: 4px 10px; white-space: nowrap; }
.chip b { font-weight: 600; }
.dot { display: inline-block; width: 9px; height: 9px; border-radius: 50%; margin-right: 5px; }
.alert { margin: 10px 20px; padding: 10px 14px; background: #fff0f0; color: var(--bad); border: 1px solid #ffc2c2; border-radius: 8px; }
.layout {
	display: grid; grid-template-columns: 1fr 360px; gap: 16px;
	padding: 0 20px 16px; min-height: 0; flex: 1;
}
.side { display: flex; flex-direction: column; gap: 12px; min-width: 0; overflow: auto; }
</style>