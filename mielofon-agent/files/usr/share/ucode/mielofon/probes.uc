'use strict';

/* Probe executors. The agent only runs the probe it is told to run and returns
 * raw numbers; it makes no scheduling or policy decisions.
 *
 * Command output is captured by shell redirect to a temp file + fs.readfile:
 * the deployment ucode snapshot's `uloop.process` stdout stream API is not
 * dependable, and a probe phase is single-in-flight anyway, so blocking
 * `system()` is the simplest robust option.
 */

import { readfile, unlink } from 'fs';

const OUT = '/tmp/mielofon-probe.out';

/* Run a shell command; cb(null, stdout) or cb(err). */
export function run(shell_cmd, cb)
{
	system(shell_cmd + ' > ' + OUT + ' 2>&1');

	let out = readfile(OUT);
	if (out == null)
		out = '';

	unlink(OUT);
	cb(null, out);
};

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

		let r = match(lines[li], /rtt min\/avg\/max\/mdev = ([0-9.]+)\//);
		if (r)
			rtt = +r[1];
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