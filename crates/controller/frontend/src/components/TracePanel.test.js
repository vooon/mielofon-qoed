import { describe, it, expect, vi, beforeEach } from 'vitest';
import { mount, flushPromises } from '@vue/test-utils';
import TracePanel from './TracePanel.vue';

const fetchTrace = vi.fn();

vi.mock('../api', () => ({
	fetchTrace: (...args) => fetchTrace(...args),
}));

beforeEach(() => {
	fetchTrace.mockReset();
});

describe('TracePanel', () => {
	it('populates the source/destination selects from the agent list', () => {
		const w = mount(TracePanel, { props: { agents: ['hub-a', 'spoke-1', 'spoke-2'] } });
		// Defaults: first → second.
		expect(w.vm.from).toBe('hub-a');
		expect(w.vm.to).toBe('spoke-1');
		const options = w.findAll('select').flatMap(s => s.findAll('option')).map(o => o.text());
		expect(options).toContain('hub-a');
		expect(options).toContain('spoke-2');
	});

	it('renders trace hops and marks broken/terminal rows', async () => {
		fetchTrace.mockResolvedValue({
			from: 'spoke-1',
			to: 'hub-b',
			complete: true,
			edges: [
				{ depth: 0, node: 'spoke-1', iface: 'awg_hub_a', to: 'hub-a', broken: false, term: false, rtt_ms: 11, loss_pct: 0, quality: 'good', ospf_cost: 10 },
				{ depth: 1, node: 'hub-a', iface: 'dummy_awg', to: 'hub-a', broken: false, term: true, rtt_ms: null, loss_pct: null, quality: null, ospf_cost: null },
				{ depth: 1, node: 'hub-x', broken: true, reason: 'no route', term: false },
			],
		});
		const w = mount(TracePanel, { props: { agents: ['spoke-1', 'hub-b'] } });
		await w.find('button').trigger('click');
		await flushPromises();
		expect(w.text()).toContain('reached');
		const rows = w.findAll('tbody tr');
		expect(rows).toHaveLength(3);
		expect(rows[1].classes()).toContain('term');
		expect(rows[2].classes()).toContain('broken');
	});

	it('surfaces a trace error message', async () => {
		fetchTrace.mockRejectedValue(new Error('hop unanswered (timeout)'));
		const w = mount(TracePanel, { props: { agents: ['spoke-1', 'hub-b'] } });
		await w.find('button').trigger('click');
		await flushPromises();
		expect(w.text()).toContain('hop unanswered');
	});
});