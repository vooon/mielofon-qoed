'use strict';

/* mielofon-agent — ucode router agent.
 *
 * Thin executor: registers its links with the controller, long-polls
 * `POST /v1/agent/command` for commands, runs the probe it is told to run
 * (or applies the OSPF cost it is told to apply), and replies on
 * `POST /v1/agent/reply`, echoing each command's job id. No scheduling or
 * policy logic lives here.
 *
 * Control flow is a single flat pump: the controller's commands are drained
 * one at a time from `queue`, each producing exactly one reply, and when the
 * queue is empty the long poll is re-armed. `step()` is the one and only
 * continuation, invoked from every async completion — no nested callback /
 * closure chain is ever constructed, so the stack depth and the number of
 * live closures stay constant under an indefinite poll loop.
 */

import * as log from 'log';
import { ulog_open, ulog_threshold, ULOG_SYSLOG, LOG_DAEMON, LOG_CRIT } from 'log';

ulog_open(ULOG_SYSLOG, LOG_DAEMON, 'mielofon-agent');
import { cursor } from 'uci';
import { connect as ubus_connect } from 'ubus';
import * as uloop from 'uloop';
import * as digest from 'digest';
import { create as create_client, post_json } from './client.uc';
import { new_client } from './transport.uc';
import { discover as discover_links, loopback_address } from './autodiscover.uc';
import { run_always, run_throughput, configure_links, needs_configure } from './probes.uc';
import { apply_cost, query_route } from './cost.uc';
import * as metrics from './metrics.uc';
import { float, parse_json, default_agent_name } from './utils.uc';

let cfg = {};
let links = [];
let client = null;

/* The node's mesh loopback (from `network.interface.<loopback_iface>
 * status`), reported at register so the controller's trace walker can resolve
 * this node as a trace destination. Populated by refresh_links(). */
let loopback = null;

/* State-machine state. `queue` holds commands drained from the controller.
 * Exactly one async step (a reply post or the long poll) is in flight at any
 * time, so no per-command closure chain is ever kept alive. */
let queue = [];

/* Periodic re-discovery: at boot the tunnels/BGP peers come up a few seconds
 * apart, so the first `register()` may report only a subset of links. These
 * track the last registered link set so a later refresh can spot a change and
 * re-register (serialized through the pump, one request in flight at a time). */
let reg_sig = '';
let need_register = false;

/* Signature of the link set + probe params last pushed to the resident
 * `mielofon-probe` daemon; `configure` is re-issued only on a real change
 * (each call resets the daemon's counter baseline, so configuring every cycle
 * would reset the utilisation-gate baseline). */
let probe_configured_sig = '';
/* Whether the daemon is currently configured for our link set (false while it
 * is absent/restarting) and whether a retry is already scheduled. */
let probe_configured = false;
let probe_retry = false;

function load_config()
{
	let ctx = cursor();

	if (ctx == null)
		die('uci unavailable');

	ctx.load('mielofon-agent');

	let agent = ctx.get('mielofon-agent', 'main', 'agent_name');
	if (agent == null || !length(agent))
		agent = default_agent_name();

	let controller_url = ctx.get('mielofon-agent', 'main', 'controller_url');
	let cacert = ctx.get('mielofon-agent', 'main', 'cacert');
	let cert = ctx.get('mielofon-agent', 'main', 'cert');
	let key = ctx.get('mielofon-agent', 'main', 'key');

	let proto = ctx.get('mielofon-agent', 'main', 'ospf_protocol');
	if (proto == null || !length(proto))
		proto = 'mesh_v3';

	cfg = {
		agent: agent,
		url: controller_url,
		cacert: cacert,
		cert: cert,
		key: key,
		ospf_protocol: proto,
		iface_prefix: ctx.get('mielofon-agent', 'main', 'iface_prefix') || 'awg_',
		loopback_iface: ctx.get('mielofon-agent', 'main', 'loopback_iface') || 'dummy_awg',
		excludes: [],
		log_level: ctx.get('mielofon-agent', 'main', 'log_level') || 'notice',
		timeout_ms: int(ctx.get('mielofon-agent', 'main', 'command_timeout_ms') || '30000'),
		quiet_max_mbps: float(ctx.get('mielofon-agent', 'main', 'quiet_max_mbps'), 15.0),
		ping_interval: ctx.get('mielofon-agent', 'main', 'ping_interval') || '1',
		tcp_duration: ctx.get('mielofon-agent', 'main', 'tcp_duration') || '4',
		iperf_port: int(ctx.get('mielofon-agent', 'main', 'iperf_port') || '5201'),
		udp_port: int(ctx.get('mielofon-agent', 'main', 'udp_port') || '0'),
		udp_rate_mbps: float(ctx.get('mielofon-agent', 'main', 'udp_rate_mbps'), 20.0),
	};

	/* ulog threshold from `log_level` (debug < info < notice < warning < err);
	 * 'notice' keeps the registration summary, 'warning' silences routine
	 * spam without hiding connectivity errors. */
	// ucode-lsp disable-next-line nullable-argument   # `cfg.log_level` is a uci string ("debug".."err")
	ulog_threshold(cfg.log_level);

	/* Interfaces never to manage (probe/cost), autodiscovered or re-registered
	 * on reload. */
	ctx.foreach('mielofon-agent', 'exclude', function(s) {
		if (!s.interface)
			return true;

		push(cfg.excludes, s.interface);
		return true;
	});
};

/* Signature of the managed link set + probe params last pushed to the resident
 * probe daemon, so `configure` is re-issued only when something actually
 * changed. */
function probe_sig()
{
	sort(links, function(a, b) {
		if (a.interface < b.interface) return -1;
		if (a.interface > b.interface) return 1;
		return 0;
	});

	let s = sprintf('%s/%s/%s/%s/%s/%s',
		cfg.quiet_max_mbps, cfg.iperf_port, cfg.udp_port,
		cfg.udp_rate_mbps, cfg.ping_interval, cfg.tcp_duration);

	for (let l in links)
		s += l.interface + (l.target || '') + (l.source || '');

	return digest.sha1(s);
};

/* Reconcile the resident probe daemon with the discovered link set + params.
 * `configure` is pushed when the signature changed, or when the daemon
 * (re)appears with an empty configuration — a respawned daemon comes up
 * config-free, so its empty status is the restart signal. On failure (daemon
 * not registered yet) a short retry is scheduled, so the agent waits for a
 * late probe instead of giving up. Returns true when the daemon is configured. */
function sync_probe(bus)
{
	if (bus == null)
		bus = ubus_connect();

	let configured = false;

	if (bus != null) {
		let psig = probe_sig();

		if (needs_configure(bus, links, psig, probe_configured_sig)) {
			configured = configure_links(bus, links, cfg);

			if (configured)
				probe_configured_sig = psig;
		} else {
			configured = true;
		}
	}

	if (configured && !probe_configured)
		log.NOTE('mielofon-probe configured (%d links)\n', length(links));
	else if (!configured && probe_configured)
		log.WARN('mielofon-probe went away; will reconfigure\n');

	probe_configured = configured;

	/* Daemon absent/restarting: retry soon so a probe that starts after the
	 * agent (or after a respawn) is configured as soon as it registers. Keep
	 * a single timer in flight. No retry loop when there is nothing to manage. */
	if (!configured && length(links) && !probe_retry) {
		probe_retry = true;
		uloop.timer(3000, function() {
			probe_retry = false;
			sync_probe(null);
		});
	}

	return configured;
};

/* Discover links from BIRD/OSPF on `bird status` + per-interface netifd
 * status. Fills module-level `links`; falls back to an empty set (recovery
 * happens on the next re-register/retry cycle). */
function refresh_links()
{
	let bus = ubus_connect();

	if (bus == null) {
		log.WARN('ubus connect failed during link discovery\n');
		links = [];
		metrics.set_links(links);
		return;
	}

	let all = discover_links(cfg, {
		status: function() {
			return bus.call('bird', 'status', {});
		},
		iface: function(name) {
			return bus.call('network.interface.' + name, 'status', {});
		},
	});

	links = all;

	for (let l in all)
		l.from = cfg.agent;

	metrics.set_links(links);

	/* The mesh loopback for trace destination resolution. A fresh ubus
	 * snapshot each refresh: the interface may not be up until BIRD/Owrt
	 * settle, and a later cycle picks it up. */
	loopback = null;
	let ls = bus.call('network.interface.' + cfg.loopback_iface, 'status', {});

	if (ls != null)
		loopback = loopback_address(ls);

	/* Keep the resident probe daemon in sync with the discovered link set +
	 * params (and recover if it restarted or starts late). */
	sync_probe(bus);
};

function find_link(iface)
{
	for (let l in links)
		if (l.interface == iface)
			return l;

	return null;
};

/* Stamp agent/id/traceparent on a reply and POST it; cb runs when done. */
function reply_send(cmd, obj, cb)
{
	obj.agent = cfg.agent;
	obj.id = cmd.id;

	// Echo the dispatch span's traceparent so the controller can correlate
	// this reply back into the originating trace.
	if (cmd.traceparent != null)
		obj.traceparent = cmd.traceparent;

	post_json(client, '/v1/agent/reply', obj, function(err, status) {
		if (err || status != 200)
			log.WARN('reply %s failed: %s / %s\n', cmd.id, err, status);

		cb();
	});
};

/* Execute one command; always completes via `cb`. */
function run_command(cmd, cb)
{
	metrics.counters.commands_received++;

	if (!cmd || !cmd.type) {
		metrics.counters.commands_errored++;
		cb();
		return;
	}

	if (cmd.type == 'apply_cost') {
		let link = find_link(cmd.link.interface);

		if (!link) {
			metrics.counters.commands_errored++;
			reply_send(cmd, { kind: 'applied', link: cmd.link, cost: cmd.cost }, cb);
			return;
		}

		apply_cost(cmd.link.interface, cmd.cost, function(err) {
			if (err)
				metrics.counters.commands_errored++;
			else
				metrics.counters.commands_succeeded++;

			reply_send(cmd, {
				kind: 'applied',
				link: cmd.link,
				cost: cmd.cost,
				note: err || null,
			}, cb);
		});

		return;
	}

	if (cmd.type == 'trace') {
		let bus = ubus_connect();
		let r = query_route(bus, cmd.target);

		metrics.counters.commands_succeeded++;
		reply_send(cmd, {
			kind: 'trace',
			prefix: cmd.target,
			code: r.code,
			routes: r.routes,
		}, cb);

		return;
	}

	if (cmd.type != 'probe') {
		metrics.counters.commands_errored++;
		log.WARN('unknown command type %s\n', cmd.type);
		cb();
		return;
	}

	let link = find_link(cmd.link.interface);

	if (!link) {
		metrics.counters.commands_errored++;
		log.WARN('unknown interface %s, skipping\n', cmd.link.interface);
		cb();
		return;
	}

	/* The resident `mielofon-probe` daemon owns the measurement; connect once
	 * and pass the bus to the harvest/trigger helpers. A missing daemon is
	 * handled inside `probes.uc` (report an unmeasured/busy result). */
	let bus = ubus_connect();

	if (cmd.tier == 'throughput') {
		run_throughput(bus, link, cfg, function(e, r) {
			metrics.record_throughput(link, r);
			metrics.counters.commands_succeeded++;

			reply_send(cmd, {
				kind: 'probe',
				link: { from: link.from, to: link.to, interface: link.interface },
				rtt_ms: r.rtt_ms,
				loss_pct: r.loss_pct,
				jitter_ms: r.jitter_ms,
				token: cmd.token || null,
				util_mbps: r.util_mbps,
				tcp_mbps: r.tcp_mbps,
				state: r.busy ? 'busy' : 'quiet',
			}, cb);
		});

		return;
	}

	run_always(bus, link, function(e, r) {
		metrics.record_always(link, r);
		metrics.counters.commands_succeeded++;

		reply_send(cmd, {
			kind: 'probe',
			link: { from: link.from, to: link.to, interface: link.interface },
			rtt_ms: r.rtt_ms,
			loss_pct: r.loss_pct,
			jitter_ms: r.jitter_ms,
			token: null,
			util_mbps: 0,
			state: 'quiet',
		}, cb);
	});
};

function enqueue_commands(v)
{
	let cmds = (v && v.commands) ? v.commands : [];

	for (let c in cmds)
		push(queue, c);
};

/* Serialized signature of the discovered link set — used to detect when a
 * periodic refresh observes a change (e.g. a tunnel that was still coming up
 * at boot) and a re-registration is needed. */
function links_sig()
{
	sort(links, function(a, b) {
		if (a.interface < b.interface) return -1;
		if (a.interface > b.interface) return 1;
		return 0;
	});

	let s = '';

	for (let l in links)
		s += l.from + l.to + l.interface;

	return digest.sha1(s);
};

/* Single flat continuation: register when the discovered link set changed
 * (or on first start), else pop the next command, else re-arm the long poll.
 * Everything is inlined here because ucode has no hoisting — a chain of
 * mutually recursive named functions would fail at runtime. Never recurses
 * through a chain of per-command closures. */
function pump()
{
	if (need_register) {
		need_register = false;

		let body = { agent: cfg.agent, links: [], loopback: loopback };

		/* Re-discover on every attempt: picks up links added/reconfigured
		 * and self-heals if a transient BIRD/ubus outage returned none. */
		refresh_links();

		for (let l in links)
			push(body.links, { from: l.from, to: l.to, interface: l.interface });

		log.NOTE('registering %s with %d links\n', cfg.agent, length(links));

		post_json(client, '/v1/agent/register', body, function(err, status, raw) {
			if (err || status != 200) {
				log.WARN('register failed: %s / %s — retrying\n', err, status);
				uloop.timer(3000, pump);
				return;
			}

			reg_sig = links_sig();
			enqueue_commands(parse_json(raw));
			pump();
		});

		return;
	}

	if (length(queue)) {
		let cmd = shift(queue);

		run_command(cmd, function() { pump(); });
		return;
	}

	let body = { agent: cfg.agent, timeout_ms: cfg.timeout_ms };

	post_json(client, '/v1/agent/command', body, function(err, status, raw) {
		if (err || status != 200) {
			log.WARN('command long-poll failed: %s / %s — retrying\n', err, status);
			uloop.timer(2000, pump);
			return;
		}

		enqueue_commands(parse_json(raw));
		pump();
	});
};

/* Periodic re-discovery: refresh links and mark for re-registration when the
 * set changed. The pump serializes the actual POST (single in-flight). */
function resync()
{
	refresh_links();

	if (links_sig() != reg_sig)
		need_register = true;
};

load_config();

if (!cfg.agent || !cfg.url || !cfg.cert || !cfg.key) {
	/* critical: the agent cannot start at all — exit(1) after this */
	log.ulog(LOG_CRIT, 'mielofon-agent: incomplete configuration\n');
	exit(1);
}

client = create_client({
	base_url: cfg.url,
	tls: { cacert: cfg.cacert, cert: cfg.cert, key: cfg.key },
	timeout_ms: cfg.timeout_ms + 10000,
	new_client: new_client,
});

if (metrics.init()) {
	metrics.write();
	/* metrics.interval() returns seconds; uloop.interval() wants ms. A unit
	 * mismatch here makes the textfile rewrite every 20 ms (~50/s) instead of
	 * every 20 s — rendering allocates gauges/buffers faster than the ucode GC
	 * reclaims them, which shows up as an unbounded RSS climb. */
	uloop.interval(metrics.interval() * 1000, function() { metrics.write(); });
}

/* This ucode build runs with reference counting only: the mark-and-sweep GC
 * is opt-in (the `-g` CLI flag or explicit gc()). The agent is meant to run
 * for months on a router, so run a periodic GC as a safety net against any
 * reference cycle that sneaks in — every 15 minutes is cheap and bounds RSS. */
uloop.interval(900000, gc);

/* Re-discover periodically (tunnels and BGP peers come up a few seconds
 * apart at boot, so the first register may see a partial link set). The
 * pump re-registers when the discovered set differs from what was last
 * registered. */
uloop.interval(30000, resync);

/* Reconcile the resident probe daemon on a shorter cadence as well: a
 * crash/respawn comes up config-free and must be reconfigured well before the
 * next 30s re-discovery. A first configure that failed (probe still starting)
 * is retried by sync_probe() itself every 3s. */
uloop.interval(15000, function() { sync_probe(null); });

/* Initial registration. */
refresh_links();
reg_sig = links_sig();
need_register = true;

/* Exit promptly on SIGTERM/SIGINT: flush the textfile, then leave. Without a
 * handler ucode would swallow the signal and procd would SIGKILL us after its
 * grace period — every stop/restart would cost a 5s timeout. */
function on_shutdown()
{
	/* write() no-ops when the textfile is disabled */
	metrics.write();
	exit(0);
}
signal('SIGTERM', on_shutdown);
signal('SIGINT', on_shutdown);

pump();
uloop.run();
