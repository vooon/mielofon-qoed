<script setup>
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { computePositions, qualityColor, qualityWidth, isBrokenLink } from '../logic';

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

// Stable edge key: a (from,to,interface) triple is unique in /v1/graph.
const edgeKey = l => `${l.from}\u0000${l.to}\u0000${l.interface}`;

function nodeOpts(n) {
	return {
		id: n.id,
		label: n.label,
		group: n.group,
		fixed: { x: true, y: true },
	};
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
	if (!props.nodes.length) {
		// No topology: drop any stale network so a later refresh rebuilds fresh.
		if (net) net.destroy();
		net = null;
		nodes = null;
		edges = null;
		return;
	}

	const pos = computePositions(props.nodes, props.links);
	nodes = new window.vis.DataSet(
		props.nodes.map(n => {
			const p = pos.get(n.id) || { x: 0, y: 0 };
			return { ...nodeOpts(n), x: p.x, y: p.y };
		})
	);
	edges = new window.vis.DataSet(props.links.map(edgeOpts));

	const opts = {
		groups: {
			hub: { color: { background: '#2f80ed', border: '#1f5fb0' }, size: 22, shape: 'dot', font: { size: 15, color: '#1f5fb0' } },
			spoke: { color: { background: '#9aa7b4', border: '#788693' }, size: 13, shape: 'dot', font: { size: 11, color: '#788693' } },
		},
		physics: false,
		interaction: { hover: true, dragNodes: true },
		edges: { smooth: { enabled: true, type: 'continuous' }, selectionWidth: 2 },
	};
	net = new window.vis.Network(el.value, { nodes, edges }, opts);
	net.fit({ animation: false });

	net.on('click', params => {
		if (params.edges && params.edges.length) {
			emit('select', props.links.find(l => edgeKey(l) === params.edges[0]) || null);
		}
	});
	net.on('deselectNode', () => emit('select', null));
}

// Patch the existing network in place so node positions survive refresh.
function update() {
	if (!net || !props.nodes.length) {
		build();
		return;
	}

	const wantedNodes = new Map(props.nodes.map(n => [n.id, n]));
	const wantedEdgeKeys = new Set(props.links.map(edgeKey));

	// Add new nodes at their computed slot; update labels/groups for existing
	// (positions are left untouched → the user can drag them around).
	const pos = computePositions(props.nodes, props.links);
	const known = new Set(nodes.getIds());
	props.nodes.filter(n => known.has(n.id)).forEach(n => nodes.update(nodeOpts(n)));
	props.nodes.filter(n => !known.has(n.id)).forEach(n => {
		const p = pos.get(n.id) || { x: 0, y: 0 };
		nodes.add({ ...nodeOpts(n), x: p.x, y: p.y });
	});

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
}

function shouldFit(prevNodes, prevLinks) {
	if (prevNodes.length !== props.nodes.length) return true;
	if (prevLinks.length !== props.links.length) return true;
	return false;
}

let prevNodes = [];
let prevLinks = [];

watch(
	() => [props.nodes, props.links],
	() => {
		if (net) {
			const doFit = shouldFit(prevNodes, prevLinks);
			update();
			if (doFit) {
				net.fit({ animation: { duration: 300, easingFunction: 'easeInOutQuad' } });
			}
		} else {
			build();
		}
		prevNodes = props.nodes;
		prevLinks = props.links;
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