<script setup>
import { onMounted, ref, watch } from 'vue';
import { computePositions, qualityColor, qualityWidth, isBrokenLink } from '../logic';

const props = defineProps({
	nodes: { type: Array, default: () => [] },
	links: { type: Array, default: () => [] },
	selected: { type: Object, default: null },
});
const emit = defineEmits(['select']);

const el = ref(null);
let net = null;

function build() {
	if (!el.value) return;
	el.value.innerHTML = '';
	if (!props.nodes.length) return;

	const pos = computePositions(props.nodes, props.links);
	const dsNodes = props.nodes.map(n => {
		const p = pos.get(n.id) || { x: 0, y: 0 };
		return {
			id: n.id, label: n.label, group: n.group,
			x: p.x, y: p.y, fixed: { x: true, y: true },
		};
	});
	const dsEdges = props.links.map((l, i) => ({
		id: i, from: l.from, to: l.to, label: l.interface,
		width: qualityWidth(l.quality),
		color: isBrokenLink(l)
			? { color: '#e7040f', highlight: '#e7040f' }
			: { color: qualityColor(l.quality), highlight: qualityColor(l.quality) },
		dashes: isBrokenLink(l),
		title: `${l.interface} | rtt ${l.rtt_ms != null ? l.rtt_ms.toFixed(1) + ' ms' : '-'} | loss ${l.loss_pct != null ? l.loss_pct.toFixed(1) + '%' : '-'} | cost ${l.ospf_cost != null ? l.ospf_cost : '-'}`,
	}));

	const opts = {
		groups: {
			hub: { color: { background: '#2f80ed', border: '#1f5fb0' }, size: 22, shape: 'dot', font: { size: 15, color: '#1f5fb0' } },
			spoke: { color: { background: '#9aa7b4', border: '#788693' }, size: 13, shape: 'dot', font: { size: 11, color: '#788693' } },
		},
		physics: false,
		interaction: { hover: true, dragNodes: false },
		edges: { smooth: { enabled: true, type: 'continuous' }, selectionWidth: 2 },
	};
	net = new window.vis.Network(el.value, { nodes: new window.vis.DataSet(dsNodes), edges: new window.vis.DataSet(dsEdges) }, opts);
	net.fit({ animation: false });
	net.on('click', params => {
		if (params.edges && params.edges.length) {
			emit('select', props.links[params.edges[0]]);
		}
	});
	net.on('deselectNode', () => emit('select', null));
}

onMounted(build);
watch(() => props.nodes, build, { deep: true });
</script>

<template>
  <div class="meshgraph card" ref="el"></div>
</template>

<style scoped>
.meshgraph { min-height: 480px; height: 62vh; }
</style>