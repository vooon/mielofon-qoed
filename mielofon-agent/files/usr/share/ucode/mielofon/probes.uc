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
import { ulog, LOG_DEBUG } from 'log';
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

/* Whether a `timeout` applet is available to bound probe runs. Busybox does
 * not always include it (the live routers' busybox lacks the timeout applet),
 * so we probe once at load and degrade gracefully: with no timeout we bound
 * the probe from inside the shell (background+kill) rather than failing every
 * invocation with "timeout: not found". The package DEPENDS guarantees the
 * applet for new images. */
let timeout_ok = null;

function detect_timeout()
{
	if (timeout_ok != null)
		return timeout_ok;

	let pipe = popen('command -v timeout >/dev/null 2>&1 && echo yes || echo no', 'r');
	let out = '';

	if (pipe != null) {
		while (true) {
			let chunk = pipe.read(128);

			if (chunk == null || !length(chunk))
				break;

			out += chunk;
		}
		pipe.close();
	}

	timeout_ok = (match(out, /yes/) != null);
	ulog(LOG_DEBUG, 'busybox timeout applet: %s\n', timeout_ok ? 'present' : 'absent');
	return timeout_ok;
}

/* Bound a probe run to `secs` and merge its stderr (2>&1) so the captured
 * output carries both streams. Preferred: the `timeout` applet. On legacy
 * firmware without it, bound from inside the shell: the tool runs as a direct
 * background child (single command, so `$!` is a killable pid — no compound
 * subshell), a killer subshell SIGKILLs it after `secs`, and the parent waits.
 * The killer's own fds are redirected so it does NOT hold the pipe write-end
 * (otherwise popen can't see EOF and fast probes would be held to the full
 * bound). This keeps a black-holed TCP connect (netperf can hang ~2 min) from
 * stalling the single-threaded agent pump on routers that lack the applet. */
function bounded(secs, cmd)
{
	if (detect_timeout())
		return `timeout ${secs} ${cmd} 2>&1`;

	return `${cmd} 2>&1 & p=$!; ( sleep ${secs}; kill -9 $p 2>/dev/null ) >/dev/null 2>&1 & w=$!; wait $p 2>/dev/null; kill -9 $w 2>/dev/null`;
}

/* Run a probe command bounded to `secs` seconds; cb(err, stdout). At debug
 * level (`log_level = debug`) both the exact command string and its combined
 * output are logged — so a probe failure (netperf control error, iperf3
 * error, ping timeout) is observable without extra tooling. */
export function run(secs, tool_cmd, cb)
{
	let shell_cmd = bounded(secs, tool_cmd);

	ulog(LOG_DEBUG, 'cmd: %s\n', tool_cmd);

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
	ulog(LOG_DEBUG, 'cmd out: %s\n%s\n', shell_cmd, out);
	cb(null, out);
};

/* ── per-tool command builders ───────────────────────────────────────────── */

function ping_command(link, cfg)
{
	/* Busybox ping rejects fractional `-i`; only pass it for integer values.
	 * The bound is applied by `run()` (see `bounded`). */
	let ival = (cfg.ping_interval >= 1) ? ` -i ${cfg.ping_interval}` : '';
	let src = (link.source != null) ? ` -I ${link.source}` : '';

	return `ping -q -c ${cfg.ping_count} -W 1${ival}${src} ${link.target}`;
};

function netperf_command(link, cfg)
{
	let src = (link.source != null) ? ` -L ${link.source}` : '';

	/* `-l 4` caps the data phase but NOT the TCP connect to netserver; the
	 * bound in `run()` cuts a black-holed/refused control connection. */
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
	run(8, ping_command(link, cfg), function(ping_err, ping_out) {
		let p = parse_ping(ping_out);

		/* A ping that produced no statistics line (tool error) must not turn
		 * into a fake `-1 loss` figure: an unmeasured dimension never
		 * constrains quality on the controller. A 100%-loss ping keeps its
		 * real `loss` but has no round-trip time — iputils still prints a
		 * stale `0.000` summary line, so null the rtt explicitly. */
		if (ping_err || p.loss < 0)
			p = { loss: null, rtt: null };
		else if (p.loss >= 100)
			p.rtt = null;

		if (ping_err || p.rtt == null)
			metrics.counters.probe_errors.ping = (metrics.counters.probe_errors.ping || 0) + 1;

		metrics.counters.probe_netperf++;
		run(12, netperf_command(link, cfg), function(rr_err, rr_out) {
			let tps = parse_transaction_rate(rr_out);

			/* A failed TCP_RR run (control connect refused, test aborted) is a
			 * measurement failure, never a real "0 trans/s": report it as
			 * unmeasured so it cannot escalate the link to bad. Zero was the
			 * old behaviour and it silently re-routed whole links to cost 100. */
			if (rr_err || tps <= 0)
				metrics.counters.probe_errors.netperf = (metrics.counters.probe_errors.netperf || 0) + 1;

			let rr = (rr_err || tps <= 0) ? null : tps;

			/* Debug: the resolved numbers, alongside the raw `cmd`/`cmd out`
			 * lines above — this is the "is it really a netperf failure?"
			 * answer (output absent/aborted vs a low-but-real rate). */
			ulog(LOG_DEBUG, 'always %s -> %s: rtt=%s loss=%s rr=%s\n',
				link.interface, link.target, p.rtt, p.loss, rr);

			cb(null, {
				rtt_ms: p.rtt,
				loss_pct: p.loss,
				rr_tps: rr,
			});
		});
	});
};

/* Gated throughput tier: quiet gate first, then iperf3. Echoes the last
 * always-tier measurement (metrics.last_always) so the controller keeps the
 * rtt/loss/rr dims without any server-side history carry-forward. */
export function run_throughput(link, cfg, cb)
{
	let a = metrics.last_always(link);
	let util = util_mbps(link.interface);

	ulog(LOG_DEBUG, 'throughput %s -> %s: util=%s Mbps (gate %s)\n',
		link.interface, link.target, util, cfg.quiet_max_mbps);

	if (util > cfg.quiet_max_mbps) {
		metrics.counters.probe_busy++;
		cb(null, {
			busy: true,
			util_mbps: util,
			tcp_mbps: null,
			rtt_ms: a.rtt_ms,
			loss_pct: a.loss_pct,
			rr_tps: a.rr_tps,
		});
		return;
	}

	metrics.counters.probe_iperf++;
	run(15, iperf_command(link, cfg), function(e, out) {
		let tcp = parse_iperf3(out);

		if (e || tcp == null)
			metrics.counters.probe_errors.iperf = (metrics.counters.probe_errors.iperf || 0) + 1;

		/* Debug: gate + iperf outcome; the raw `cmd`/`cmd out` lines carry the
		 * iperf3 -J output (or its error) that sets `tcp`. */
		ulog(LOG_DEBUG, 'iperf3 %s -> %s: tcp_mbps=%s busy=%s\n',
			link.interface, link.target, tcp, (tcp == null) ? 'true' : 'false');

		cb(null, {
			busy: (tcp == null),
			util_mbps: util,
			tcp_mbps: tcp,
			rtt_ms: a.rtt_ms,
			loss_pct: a.loss_pct,
			rr_tps: a.rr_tps,
		});
	});
};
