<script setup>
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { qualityColor, qualityWidth, isBrokenLink } from '../logic';

const props = defineProps({
	nodes: { type: Array, default: () => [] },
	links: { type: Array, default: () => [] },
	selected: { type: Object, default: null },
});
const emit = defineEmits(['select']);

const el = ref(null);
let net = null;
let nodes = null;
let edges = null;

// Layout: run vis-network's built-in physics (default barnesHut + springs),
// which spreads nodes naturally like BIRD's map, and freeze it once
// stabilized so nodes stay where they land and stay draggable. On a topology
// change the physics briefly re-run to settle new nodes, then freeze again.
const PHYSICS = {
	enabled: true,
	stabilization: { enabled: true, iterations: 1000 },
};
let physicsOn = true;
let firstStable = true;

// Stable edge key: a (from,to,interface) triple is unique in /v1/graph.
const edgeKey = l => `${l.from}\u0000${l.to}\u0000${l.interface}`;

function enablePhysics() {
	if (!net || physicsOn) return;
	physicsOn = true;
	net.setOptions({ physics: PHYSICS });
}

function freezePhysics() {
	if (!net || !physicsOn) return;
	physicsOn = false;
	net.setOptions({ physics: { enabled: false } });
}

function nodeOpts(n) {
	return { id: n.id, label: n.label, group: n.group };
}

function edgeOpts(l) {
	return {
		id: edgeKey(l),
		from: l.from,
		to: l.to,
		label: l.interface,
		width: qualityWidth(l.quality),
		color: isBrokenLink(l)
			? { color: '#e7040f', highlight: '#e7040f' }
			: { color: qualityColor(l.quality), highlight: qualityColor(l.quality) },
		dashes: isBrokenLink(l),
		title: `${l.interface} | rtt ${l.rtt_ms != null ? l.rtt_ms.toFixed(1) + ' ms' : '-'} | loss ${l.loss_pct != null ? l.loss_pct.toFixed(1) + '%' : '-'} | cost ${l.ospf_cost != null ? l.ospf_cost : '-'}`,
	};
}

function build() {
	if (!el.value) return;
	el.value.innerHTML = '';
	if (net) net.destroy();
	net = null;
	physicsOn = true;
	firstStable = true;
	if (!props.nodes.length) return;

	nodes = new window.vis.DataSet(props.nodes.map(nodeOpts));
	edges = new window.vis.DataSet(props.links.map(edgeOpts));

	const opts = {
		groups: {
			hub: { color: { background: '#2f80ed', border: '#1f5fb0' }, size: 22, shape: 'dot', font: { size: 15, color: '#1f5fb0' } },
			spoke: { color: { background: '#9aa7b4', border: '#788693' }, size: 13, shape: 'dot', font: { size: 11, color: '#788693' } },
		},
		physics: PHYSICS,
		interaction: { hover: true, dragNodes: true },
		edges: { smooth: { enabled: true, type: 'continuous' }, selectionWidth: 2 },
	};
	net = new window.vis.Network(el.value, { nodes, edges }, opts);

	net.on('stabilized', () => {
		if (firstStable) {
			net.fit({ animation: false });
			firstStable = false;
		}
		freezePhysics();
	});

	net.on('click', params => {
		if (params.edges && params.edges.length) {
			emit('select', props.links.find(l => edgeKey(l) === params.edges[0]) || null);
		}
	});
	net.on('deselectNode', () => emit('select', null));
}

// Patch the existing network in place; positions survive. If the topology
// changed (nodes/edges added or removed), re-run physics briefly so the new
// pieces settle into the layout, then freeze again.
function update(topologyChanged) {
	if (!net || !props.nodes.length) {
		build();
		return;
	}

	const wantedNodes = new Set(props.nodes.map(n => n.id));
	const wantedEdgeKeys = new Set(props.links.map(edgeKey));

	// Node diff by id.
	const known = new Set(nodes.getIds());
	props.nodes.filter(n => known.has(n.id)).forEach(n => nodes.update(nodeOpts(n)));
	props.nodes.filter(n => !known.has(n.id)).forEach(n => nodes.add(nodeOpts(n)));

	// Edge diff by stable key.
	const knownEdges = new Set(edges.getIds());
	knownEdges.forEach(k => {
		if (!wantedEdgeKeys.has(k)) edges.remove(k);
	});
	props.links.forEach(l => {
		if (knownEdges.has(edgeKey(l))) edges.update(edgeOpts(l));
		else edges.add(edgeOpts(l));
	});

	// Drop nodes that vanished from the graph.
	nodes.get().forEach(n => {
		if (!wantedNodes.has(n.id)) nodes.remove(n.id);
	});

	if (topologyChanged) {
		firstStable = false;
		enablePhysics();
		// Re-freeze shortly after the new topology settles; physics will keep
		// moving only until the layout converges.
		net.once('stabilized', () => freezePhysics());
	}
}

let prevIds = new Set();
let prevEdgeKeys = new Set();

watch(
	() => [props.nodes, props.links],
	() => {
		const ids = new Set(props.nodes.map(n => n.id));
		const keys = new Set(props.links.map(edgeKey));
		const topologyChanged =
			ids.size !== prevIds.size ||
			keys.size !== prevEdgeKeys.size ||
			[...ids].some(id => !prevIds.has(id)) ||
			[...keys].some(k => !prevEdgeKeys.has(k));

		if (!net) {
			build();
		} else {
			update(topologyChanged);
		}
		prevIds = ids;
		prevEdgeKeys = keys;
	},
	{ deep: true }
);

onMounted(build);
onBeforeUnmount(() => {
	if (net) net.destroy();
});
</script>

<template>
  <div class="meshgraph card" ref="el"></div>
</template>

<style scoped>
.meshgraph { min-height: 480px; height: 62vh; }
</style>