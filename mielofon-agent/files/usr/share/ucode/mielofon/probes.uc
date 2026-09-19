'use strict';

/* Probe harvest over ubus from the resident `mielofon-probe` daemon.
 *
 * The daemon owns the actual measurement: it ICMP-pings every managed link on
 * a continuous cadence and runs gated libiperf3 TCP+UDP throughput tests
 * in-process (no exec, no /tmp). The agent only pushes the discovered link set
 * + probe parameters (`configure`) and reads results back (`status`), or asks
 * for a gated throughput test (`throughput`). It therefore never spawns
 * `ping`/`iperf3`/`netperf` subprocesses and holds no measurement policy.
 *
 * NOTE: ucode has no function hoisting — helpers are declared before use.
 */

import { ulog, LOG_DEBUG } from 'log';
import { float } from './utils.uc';
import * as metrics from './metrics.uc';

const PROBE_OBJ = 'mielofon-probe';

/* Count a probe failure of `kind` (ping/iperf) in the metrics counters. */
function count_error(kind)
{
	metrics.counters.probe_errors[kind] = (metrics.counters.probe_errors[kind] || 0) + 1;
}

/* Normalize a ubus numeric field to a number, or null when absent/invalid. */
function num_or_null(v)
{
	if (v == null)
		return null;

	let n = +v;

	return (n == n) ? n : null;
}

/* Push the managed link set + probe params to the daemon (`configure`).
 * Returns true when the daemon accepted them. A missing/unreachable daemon is
 * not fatal: the agent retries on the next discovery refresh. */
export function configure_links(bus, links, cfg)
{
	if (bus == null) {
		ulog(LOG_DEBUG, 'mielofon-probe configure: no ubus\n');
		return false;
	}

	let req = {
		links: [],
		quiet_max_mbps: cfg.quiet_max_mbps,
		iperf_port: cfg.iperf_port,
		ping_interval: float(cfg.ping_interval, 1.0),
		udp_rate_mbps: cfg.udp_rate_mbps,
	};

	if (cfg.udp_port != null && int(cfg.udp_port) > 0)
		req.udp_port = int(cfg.udp_port);

	for (let l in links)
		push(req.links, { interface: l.interface, target: l.target, source: l.source });

	let res = bus.call(PROBE_OBJ, 'configure', req);

	if (res == null) {
		ulog(LOG_DEBUG, 'mielofon-probe configure failed: %s\n', bus.error() || 'unknown');
		return false;
	}

	ulog(LOG_DEBUG, 'mielofon-probe configure: %d links\n', length(req.links));
	return true;
};

/* Whether the daemon currently manages any links. A daemon that crashed and
 * respawned comes up config-free, so the agent re-pushes on empty status. */
export function has_links(bus)
{
	if (bus == null)
		return false;

	let status = bus.call(PROBE_OBJ, 'status', {});

	if (status == null || type(status) != 'object')
		return false;

	return length(keys(status)) > 0;
};

/* Whether the daemon needs its configuration (re)pushed. `sig` is the caller's
 * signature of the current link set + params and `last_sig` the one last
 * pushed; a signature change is a normal reconfigure. When the signature is
 * unchanged, an empty link set on the daemon is the signal that it restarted
 * (it comes up config-free) — only meaningful when we manage links. */
export function needs_configure(bus, links, sig, last_sig)
{
	if (bus == null)
		return true;

	if (sig != last_sig)
		return true;

	if (type(links) != 'array')
		return false;

	return length(links) > 0 && !has_links(bus);
};

/* Always-on tier: harvest the daemon's latest ICMP snapshot for `link`. */
export function run_always(bus, link, cb)
{
	metrics.counters.probe_ping++;

	if (bus == null) {
		count_error('ping');
		cb(null, { rtt_ms: null, loss_pct: null, jitter_ms: null });
		return;
	}

	let status = bus.call(PROBE_OBJ, 'status', {});
	let s = (type(status) == 'object') ? status[link.interface] : null;

	/* Absent fields mean "not measured yet"; a loss of -1 means the window is
	 * empty. Never synthesize a 0: an unmeasured dimension must not constrain
	 * the controller's classification. */
	let rtt = num_or_null((s != null) ? s.rtt_ms : null);
	let jitter = num_or_null((s != null) ? s.jitter_ms : null);
	let loss = num_or_null((s != null) ? s.loss_pct : null);

	if (loss != null && loss < 0)
		loss = null;

	if (rtt == null)
		count_error('ping');

	ulog(LOG_DEBUG, 'always %s -> %s: rtt=%s loss=%s jitter=%s\n',
		link.interface, link.target, rtt, loss, jitter);

	cb(null, {
		rtt_ms: rtt,
		loss_pct: loss,
		jitter_ms: jitter,
	});
};

/* Gated throughput tier: ask the daemon to run (and quiet-gate) the test.
 * Echoes the last always-tier measurement so the controller keeps those dims
 * without any server-side history carry-forward. */
export function run_throughput(bus, link, cfg, cb)
{
	let a = metrics.last_always(link);

	metrics.counters.probe_iperf++;

	let gate_busy = false;
	let util = 0;
	let tcp = null;

	if (bus != null) {
		let res = bus.call(PROBE_OBJ, 'throughput', {
			interface: link.interface,
			duration: int(cfg.tcp_duration) || 4,
		});

		if (res != null) {
			gate_busy = (res.busy == 1 || res.busy == true);

			let u = num_or_null(res.util_mbps);
			if (u != null)
				util = u;

			tcp = num_or_null(res.tcp_mbps);
		}
	}

	if (gate_busy)
		metrics.counters.probe_busy++;

	/* A throughput run that produced no number (daemon down, test failure) is
	 * reported `busy` so the controller never classifies it as degraded. */
	if (tcp == null)
		count_error('iperf');

	let out = {
		busy: gate_busy || (tcp == null),
		util_mbps: util,
		tcp_mbps: tcp,
		rtt_ms: a.rtt_ms,
		loss_pct: a.loss_pct,
		jitter_ms: a.jitter_ms,
	};

	ulog(LOG_DEBUG, 'throughput %s -> %s: tcp_mbps=%s busy=%s\n',
		link.interface, link.target, tcp, out.busy);

	cb(null, out);
};
