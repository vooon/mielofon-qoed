<script setup>
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { Network, DataSet } from 'vis-network/standalone';
import 'vis-network/styles/vis-network.css';
import { qualityColor, qualityWidth, isBrokenLink, isHubSpokeLink, ringLayout } from '../logic';

const props = defineProps({
	nodes: { type: Array, default: () => [] },
	links: { type: Array, default: () => [] },
	selected: { type: Object, default: null },
});
const emit = defineEmits(['select']);

const el = ref(null);
// Interface names on every edge are unreadable past a handful of links;
// off by default, the selected edge always shows its own.
const showLabels = ref(false);
// Hub→spoke edges (leaves, costed out of transit) can be hidden to read the
// hub core alone.
const showLeaves = ref(true);
let net = null;
let nodes = null;
let edges = null;
let selectedKey = null;

// Stable edge key: a (from,to,interface) triple is unique in /v1/graph.
const edgeKey = l => `${l.from}\u0000${l.to}\u0000${l.interface}`;

function hubSet() {
	return new Set(props.nodes.filter(n => n.group === 'hub').map(n => n.id));
}

function nodeOpts(n, pos) {
	const o = { id: n.id, label: n.label, group: n.group };
	if (pos) {
		o.x = pos.x;
		o.y = pos.y;
	}
	return o;
}

function edgeOpts(l, hubs) {
	const leaf = isHubSpokeLink(l, hubs);
	const broken = isBrokenLink(l);
	const color = broken ? '#e7040f' : qualityColor(l.quality);
	const key = edgeKey(l);
	return {
		id: key,
		from: l.from,
		to: l.to,
		label: showLabels.value || key === selectedKey ? l.interface : undefined,
		hidden: leaf && !showLeaves.value,
		width: leaf ? Math.max(1, qualityWidth(l.quality) * 0.6) : qualityWidth(l.quality),
		// Show the from→to direction the record encodes.
		arrows: { to: { enabled: true, scaleFactor: leaf ? 0.3 : 0.5 } },
		color: { color, highlight: color, hover: color, opacity: leaf ? 0.45 : 1 },
		dashes: broken,
		// The two directions of one tunnel bend away from each other.
		smooth: { enabled: true, type: 'curvedCW', roundness: 0.12 },
		title: `${l.from} → ${l.to} · ${l.interface}\nrtt ${l.rtt_ms != null ? l.rtt_ms.toFixed(1) + ' ms' : '-'} · loss ${l.loss_pct != null ? l.loss_pct.toFixed(1) + '%' : '-'} · cost ${l.ospf_cost != null ? l.ospf_cost : '-'}`,
	};
}

function currentPositions() {
	if (!net || !nodes) return {};
	const ids = nodes.getIds();
	return ids.length ? net.getPositions(ids) : {};
}

function build() {
	if (!el.value) return;
	el.value.innerHTML = '';
	if (net) net.destroy();
	net = null;
	if (!props.nodes.length) return;

	const pos = ringLayout(props.nodes);
	const hubs = hubSet();
	nodes = new DataSet(props.nodes.map(n => nodeOpts(n, pos[n.id])));
	edges = new DataSet(props.links.map(l => edgeOpts(l, hubs)));

	const opts = {
		groups: {
			hub: { color: { background: '#2f80ed', border: '#1f5fb0' }, size: 24, shape: 'dot', font: { size: 15, color: '#1f5fb0', face: 'system-ui' } },
			spoke: { color: { background: '#9aa7b4', border: '#788693' }, size: 14, shape: 'dot', font: { size: 13, color: '#4f5b68', face: 'system-ui' } },
		},
		// Deterministic ring layout, no physics: the map looks the same on
		// every load and nothing moves unless dragged.
		physics: { enabled: false },
		layout: { improvedLayout: false },
		interaction: { hover: true, dragNodes: true, tooltipDelay: 120 },
		edges: {
			selectionWidth: 2,
			hoverWidth: 1,
			font: { size: 11, color: '#4f5b68', strokeWidth: 3, strokeColor: '#ffffff', align: 'top', face: 'system-ui' },
			labelHighlightBold: false,
		},
	};
	net = new Network(el.value, { nodes, edges }, opts);
	net.fit({ animation: false });

	net.on('click', params => {
		const key = params.edges && params.edges.length ? params.edges[0] : null;
		selectedKey = key;
		relabel();
		emit('select', key ? props.links.find(l => edgeKey(l) === key) || null : null);
	});
}

// Re-apply label visibility to every edge (toggle or selection change).
function relabel() {
	if (!edges) return;
	const hubs = hubSet();
	edges.update(props.links.map(l => edgeOpts(l, hubs)));
}

// Patch the existing network in place; positions survive. New nodes get a
// ring slot computed around the ones already placed.
function update() {
	if (!net || !props.nodes.length) {
		build();
		return;
	}
	const pos = ringLayout(props.nodes, currentPositions());
	const hubs = hubSet();
	const wantedNodes = new Set(props.nodes.map(n => n.id));
	const wantedEdgeKeys = new Set(props.links.map(edgeKey));

	const known = new Set(nodes.getIds());
	props.nodes.filter(n => known.has(n.id)).forEach(n => nodes.update(nodeOpts(n)));
	props.nodes.filter(n => !known.has(n.id)).forEach(n => nodes.add(nodeOpts(n, pos[n.id])));

	const knownEdges = new Set(edges.getIds());
	knownEdges.forEach(k => {
		if (!wantedEdgeKeys.has(k)) edges.remove(k);
	});
	props.links.forEach(l => {
		if (knownEdges.has(edgeKey(l))) edges.update(edgeOpts(l, hubs));
		else edges.add(edgeOpts(l, hubs));
	});

	nodes.get().forEach(n => {
		if (!wantedNodes.has(n.id)) nodes.remove(n.id);
	});
}

watch(
	() => [props.nodes, props.links],
	() => {
		if (!net) build();
		else update();
	},
	{ deep: true }
);
watch([showLabels, showLeaves], relabel);

onMounted(build);
onBeforeUnmount(() => {
	if (net) net.destroy();
});
</script>

<template>
  <div class="meshgraph card">
    <div class="canvas" ref="el"></div>
    <div class="controls">
      <label><input type="checkbox" v-model="showLabels" /> interface names</label>
      <label><input type="checkbox" v-model="showLeaves" /> hub→spoke links</label>
      <button class="fit" @click="net && net.fit({ animation: true })" title="Fit the map to the view">fit</button>
    </div>
  </div>
</template>

<style scoped>
.meshgraph { position: relative; min-height: 480px; height: 62vh; }
.canvas { position: absolute; inset: 0; }
.controls {
	position: absolute; top: 10px; left: 12px; display: flex; gap: 14px; align-items: center;
	padding: 6px 10px; background: rgba(255, 255, 255, 0.88); border: 1px solid var(--line);
	border-radius: 8px; font-size: 12px; color: var(--muted); z-index: 1;
}
.controls label { display: flex; align-items: center; gap: 5px; cursor: pointer; }
.fit {
	background: var(--card); color: var(--text); border: 1px solid var(--line);
	border-radius: 6px; padding: 2px 10px; font-size: 12px;
}
.fit:hover { border-color: var(--blue); background: var(--card); }
</style>
