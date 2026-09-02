'use strict';

/* Probe executors. The agent only runs the probe it is told to run and returns
 * raw numbers; it makes no scheduling or policy decisions.
 *
 * Each high-level export owns its command construction *and* parsing, so the
 * caller never builds raw command strings. Output is captured via fs.popen()
 * (a pipe to the process stdout), which stays on the event loop-friendly fs
 * module rather than shell redirects to a temp file.
 *
 * NOTE: ucode has no function hoisting — everything is declared before use,
 * hence the parsers/helpers sit above the executors below.
 */

import { readfile, popen, error } from 'fs';
import * as metrics from './metrics.uc';

/* ── parsers ────────────────────────────────────────────────────────────── */

export function parse_ping(stdout)
{
	let loss = -1.0;
	let rtt = null;
	let lines = split(stdout, '\n');
	let li = 0;

	for (li = 0; li < length(lines); li++) {
		let m = match(lines[li], /([0-9.]+)% packet loss/);
		if (m)
			loss = +m[1];

		/* iputils: "rtt min/avg/max/mdev = a/b/c/d ms";
		 * busybox: "round-trip min/avg/max = a/b/c ms". Take the average. */
		let r = match(lines[li], /(rtt|round-trip) min\/avg\/max(\/mdev)? = ([0-9.]+)\/([0-9.]+)\//);
		if (r)
			rtt = +r[4];
	}

	return { loss: loss, rtt: rtt };
};

/* netperf TCP_RR runs print a numeric table; the trans/sec rate is the last
 * column of the data rows. Return the largest positive value seen. */
export function parse_transaction_rate(stdout)
{
	let best = 0.0;
	let lines = split(stdout, '\n');
	let li = 0;

	for (li = 0; li < length(lines); li++) {
		let toks = filter(split(trim(lines[li]), ' '), length);

		if (length(toks) < 5)
			continue;

		let v = +toks[length(toks) - 1];

		if (v > best && v < 1e9)
			best = v;
	}

	return best;
};

/* iperf3 -J JSON: pick the report's bits_per_second (Mbps, largest value). */
export function parse_iperf3(raw)
{
	let best = null;
	let all = match(raw, /"bits_per_second"\s*:\s*([0-9.eE+-]+)/g);

	if (all != null) {
		for (let m in all) {
			let bits = +m[1];

			if (bits > 1)
				best = bits / 1000000.0;
		}
	}

	return best;
};

/* ── link utilization ────────────────────────────────────────────────────── */

export function bytes_sum(iface)
{
	let rx = readfile('/sys/class/net/' + iface + '/statistics/rx_bytes');
	let tx = readfile('/sys/class/net/' + iface + '/statistics/tx_bytes');

	let a = (rx != null) ? int(rx) : 0;
	let b = (tx != null) ? int(tx) : 0;

	return a + b;
};

/* Sample instantaneous utilization (Mbps) over a 1s window. */
export function util_mbps(iface)
{
	let a = bytes_sum(iface);
	sleep(1000);
	let b = bytes_sum(iface);

	return ((b - a) * 8) / 1000000.0;
};

/* ── command execution ───────────────────────────────────────────────────── */

/* Run a shell command; cb(err, stdout). */
export function run(shell_cmd, cb)
{
	let pipe = popen(shell_cmd, 'r');

	if (pipe == null) {
		cb('popen failed: ' + (error() || 'unknown'));
		return;
	}

	let out = '';

	while (true) {
		let chunk = pipe.read(4096);

		if (chunk == null || !length(chunk))
			break;

		out += chunk;
	}

	pipe.close();
	cb(null, out);
};

/* ── per-tool command builders ───────────────────────────────────────────── */

function ping_command(link, cfg)
{
	/* Busybox ping rejects fractional `-i`; only pass it for integer values. */
	let ival = (cfg.ping_interval >= 1) ? ` -i ${cfg.ping_interval}` : '';
	let src = (link.source != null) ? ` -I ${link.source}` : '';

	return `ping -q -c ${cfg.ping_count} -W 1${ival}${src} ${link.target}`;
};

function netperf_command(link, cfg)
{
	let src = (link.source != null) ? ` -L ${link.source}` : '';

	return `netperf -l ${cfg.rr_duration} -t TCP_RR -H ${link.target}${src}`;
};

function iperf_command(link, cfg)
{
	let src = (link.source != null) ? ` -B ${link.source}` : '';
	let port = (cfg.iperf_port != null && cfg.iperf_port != 5201) ? ` -p ${cfg.iperf_port}` : '';

	return `iperf3 -c ${link.target} -t ${cfg.tcp_duration} -f m -J${port}${src}`;
};

/* ── executors (order: everthing above is already declared) ─────────────── */

/* Always-on tier: RTT + loss, then transaction rate. */
export function run_always(link, cfg, cb)
{
	metrics.counters.probe_ping++;
	run(ping_command(link, cfg), function(ping_err, ping_out) {
		let p = parse_ping(ping_out);

		metrics.counters.probe_netperf++;
		run(netperf_command(link, cfg), function(rr_err, rr_out) {
			/* a probe that could not be answered (nothing to measure) is an
			 * error, distinct from a busy link */
			if (ping_err || p.rtt == null)
				metrics.counters.probe_errors.ping = (metrics.counters.probe_errors.ping || 0) + 1;

			let tps = parse_transaction_rate(rr_out);
			if (rr_err || tps <= 0)
				metrics.counters.probe_errors.netperf = (metrics.counters.probe_errors.netperf || 0) + 1;

			cb(null, {
				rtt_ms: (p.rtt != null) ? p.rtt : null,
				loss_pct: p.loss,
				rr_tps: tps,
			});
		});
	});
};

/* Gated throughput tier: quiet gate first, then iperf3. */
export function run_throughput(link, cfg, cb)
{
	let util = util_mbps(link.interface);

	if (util > cfg.quiet_max_mbps) {
		metrics.counters.probe_busy++;
		cb(null, { busy: true, util_mbps: util, tcp_mbps: null });
		return;
	}

	metrics.counters.probe_iperf++;
	run(iperf_command(link, cfg), function(e, out) {
		let tcp = parse_iperf3(out);

		if (e || tcp == null)
			metrics.counters.probe_errors.iperf = (metrics.counters.probe_errors.iperf || 0) + 1;

		cb(null, {
			busy: (tcp == null),
			util_mbps: util,
			tcp_mbps: tcp,
		});
	});
};
