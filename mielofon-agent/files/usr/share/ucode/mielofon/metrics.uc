'use strict';

/* mielofon-agent - Prometheus self-observability (metrics.uc)
 *
 * Publishes per-link probe gauges via the node-exporter textfile collector
 * (no HTTP server): the file is rewritten atomically (temp + rename) by a
 * dedicated uloop timer at `prometheus_interval` seconds. Enabled only when
 * the `main` UCI option `prometheus_textfile` is set to an output path.
 *
 * The metric formatting (gauge()/metric(), label escaping, govalue) is adapted
 * from OpenWrt's prometheus-node-exporter-ucode (metrics.uc), Apache-2.0;
 * attribution is retained per that license. obserwrt uses the same helpers.
 */

import { cursor } from 'uci';
import { writefile, rename, error as fs_error } from 'fs';
import { WARN } from 'log';

let active = false;
let file = '';
let interval_s = 20;
let out = '';
let state = {};
let configured = {};   /* link_key -> {from,to,interface} from discovery */

/* Shared counters, bumped from the agent / probes / cost modules (exported
 * object: importers mutate fields directly, e.g. metrics.counters.ping++).
 * Only referenced while rendering; values reset on agent restart. */
export let counters = {
	commands_received: 0,   /* commands drained from the controller */
	commands_succeeded: 0,  /* commands that completed without error */
	commands_errored: 0,    /* commands that hit an error/unknown path */
	probe_ping: 0,          /* ping(1) invocations */
	probe_netperf: 0,       /* netperf TCP_RR invocations */
	probe_iperf: 0,         /* iperf3 throughput runs */
	probe_busy: 0,          /* throughput probes skipped because link busy */
	apply_cost: 0,          /* OSPF cost applies that succeeded */
	apply_cost_errors: 0,   /* OSPF cost applies that failed */
};

/* ---- node-exporter metric formatting (adapted) ---------------------- */

function puts(...s) { out += join('', s) + '\n'; }

function govalue(value)
{
	if (value == Infinity)
		return '+Inf';
	else if (value == -Infinity)
		return '-Inf';
	else if (value != value)
		return 'NaN';
	else if (type(value) in [ 'int', 'double' ])
		return value;
	else if (type(value) in [ 'bool', 'string' ])
		return +value;

	return null;
}

function metric(name, mtype, help, skipdecl)
{
	let decl = skipdecl == true ? false : true;

	/* NOTE: avoid `func = yld; return func` self-referential closures here.
	 * This ucode build runs with reference counting only (GC is opt-in via
	 * the -g flag), so a closure that references its own binding is cyclic
	 * and leaks every time render() runs. Nothing here chains on the yielded
	 * function's return value, so the closure just returns nothing. */
	return function(labels, value) {
		let v = govalue(value);

		if (v == null) {
			puts('skipping metric: unsupported value for ' + name);
			return;
		}

		let labels_str = '';
		if (length(labels)) {
			let sep = '';
			let s;
			labels_str = '{';
			for (let l in labels) {
				if (labels[l] == null)
					s = '';
				else if (type(labels[l]) == 'string') {
					s = replace(labels[l], '\\', '\\\\');
					s = replace(s, '"', '\\"');
					s = replace(s, '\n', '\\n');
				}
				else {
					s = govalue(labels[l]);
					if (!s)
						continue;
				}

				labels_str += sep + l + '="' + s + '"';
				sep = ',';
			}
			labels_str += '}';
		}

		if (decl) {
			if (help)
				puts('# HELP ' + name + ' ' + help);
			puts('# TYPE ' + name + ' ' + mtype);
			decl = false;
		}

		puts(name + labels_str + ' ' + v);
	};
}

function gauge(name, help, skipdecl) { return metric(name, 'gauge', help, skipdecl); }

function counter(name, help, skipdecl) { return metric(name, 'counter', help, skipdecl); }

/* ---- state ----------------------------------------------------------- */

function link_key(link)
{
	return link.from + '/' + link.to + '/' + link.interface;
}

function by_link(link)
{
	let k = link_key(link);
	let e = state[k];

	if (e == null) {
		e = state[k] = {
			from: link.from,
			to: link.to,
			interface: link.interface,
		};
	}

	return e;
}

export function by_configured(link)
{
	let k = link_key(link);
	let e = configured[k];

	if (e == null) {
		e = configured[k] = {
			from: link.from,
			to: link.to,
			interface: link.interface,
		};
	}

	return e;
};

/* Track the links the agent discovered (autodiscover.uc) so the textfile can
 * report configured links even before their first probe lands. */
export function set_links(links)
{
	let seen = {};

	for (let l in links) {
		if (!l.interface)
			continue;

		by_configured(l);
		seen[link_key(l)] = true;
	}

	/* drop links that disappeared from discovery */
	for (let k in configured)
		if (!exists(seen, k))
			delete configured[k];
};

export function record_always(link, r)
{
	let e = by_link(link);

	e.rtt_ms = r.rtt_ms;
	e.loss_pct = r.loss_pct;
	e.rr_tps = r.rr_tps;
	e.last_unixtime = time();
};

export function record_throughput(link, r)
{
	let e = by_link(link);

	e.util_mbps = r.util_mbps;
	e.tcp_mbps = r.tcp_mbps;
	e.busy = r.busy ? 1 : 0;
	e.last_unixtime = time();
};

/* ---- config ---------------------------------------------------------- */

/* Read the output path and write interval from the `main` section. Returns
 * true when enabled. */
export function init()
{
	let ctx = cursor();
	ctx.load('mielofon-agent');

	file = ctx.get('mielofon-agent', 'main', 'prometheus_textfile') || '';
	interval_s = int(ctx.get('mielofon-agent', 'main', 'prometheus_interval') || '20');

	active = (file != '');
	return active;
};

export function interval()
{
	return interval_s;
};

/* Build the textfile snapshot (exported for unit tests). */
export function render()
{
	out = '';

	gauge('mielofon_agent_up', 'Agent is up and reporting.')({}, 1);
	gauge('mielofon_agent_links', 'Number of links the agent has probed.')({}, length(keys(state)));
	gauge('mielofon_agent_links_configured', 'Number of links the agent discovered (autodiscover).')({}, length(keys(configured)));

	/* Command / probe / cost counters — single series with a label, typed
	 * counter (they are monotonically increasing since agent start). */
	let cmds = counter('mielofon_agent_commands_total', 'Commands drained from the controller, by outcome.');
	let probes = counter('mielofon_agent_probe_total', 'Individual probe tool runs.');
	let cost = counter('mielofon_agent_apply_cost_total', 'OSPF cost applies by outcome.');

	cmds({ result: 'received' }, counters.commands_received);
	cmds({ result: 'succeeded' }, counters.commands_succeeded);
	cmds({ result: 'errored' }, counters.commands_errored);

	probes({ kind: 'ping' }, counters.probe_ping);
	probes({ kind: 'netperf' }, counters.probe_netperf);
	probes({ kind: 'iperf' }, counters.probe_iperf);
	probes({ kind: 'busy' }, counters.probe_busy);

	cost({ result: 'ok' }, counters.apply_cost);
	cost({ result: 'error' }, counters.apply_cost_errors);

	/* Create each per-link gauge ONCE so TYPE/HELP are emitted a single time;
	 * repeated calls only add sampled lines with the label set. */
	let cfg = gauge('mielofon_agent_link_configured', 'Link discovered/managed by the agent (1 if present).');
	let when = gauge('mielofon_agent_last_probe_unixtime', 'Latest probe timestamp (unix seconds).');
	let rtt = gauge('mielofon_agent_link_rtt_ms', 'Latest RTT (ms).');
	let loss = gauge('mielofon_agent_link_loss_pct', 'Latest packet loss (%).');
	let tps = gauge('mielofon_agent_link_rr_tps', 'Latest TCP_RR transaction rate.');
	let tcp = gauge('mielofon_agent_link_tcp_mbps', 'Latest TCP throughput (Mbps).');
	let util = gauge('mielofon_agent_link_util_mbps', 'Latest link utilization (Mbps).');
	let busy = gauge('mielofon_agent_link_busy', 'Link busy at last throughput probe (1 if busy).');

	for (let k in configured) {
		let e = configured[k];
		let labels = { from: e.from, to: e.to, interface: e.interface };

		cfg(labels, 1);
	}

	for (let k in state) {
		let e = state[k];
		let labels = { from: e.from, to: e.to, interface: e.interface };

		if (exists(e, 'last_unixtime'))
			when(labels, e.last_unixtime);
		if (exists(e, 'rtt_ms') && e.rtt_ms != null)
			rtt(labels, e.rtt_ms);
		if (exists(e, 'loss_pct') && e.loss_pct != null)
			loss(labels, e.loss_pct);
		if (exists(e, 'rr_tps') && e.rr_tps != null)
			tps(labels, e.rr_tps);
		if (exists(e, 'tcp_mbps') && e.tcp_mbps != null)
			tcp(labels, e.tcp_mbps);
		if (exists(e, 'util_mbps') && e.util_mbps != null)
			util(labels, e.util_mbps);
		if (exists(e, 'busy'))
			busy(labels, e.busy);
	}

	return out;
};

/* Write the textfile-collector snapshot atomically. A missing/unwritable
 * target is not fatal: it logs a warning and is retried on the next tick. */
export function write()
{
	if (!active)
		return;

	let text = render();
	let tmp = file + '.tmp';

	if (writefile(tmp, text) === null) {
		WARN('mielofon-agent: cannot write %s: %s', file, fs_error());
		return;
	}

	if (rename(tmp, file) === null) {
		WARN('mielofon-agent: cannot rename %s: %s', file, fs_error());
		return;
	}
};
