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
import { ulog_open, ULOG_SYSLOG, LOG_DAEMON } from 'log';

ulog_open(ULOG_SYSLOG, LOG_DAEMON, 'mielofon-agent');
import { cursor } from 'uci';
import * as uloop from 'uloop';
import { create as create_client, post_json } from './client.uc';
import { new_client } from './transport.uc';
import { run_always, run_throughput } from './probes.uc';
import { apply_cost } from './cost.uc';
import * as metrics from './metrics.uc';
import { float, parse_json, default_agent_name } from './utils.uc';

let cfg = {};
let links = [];
let client = null;

/* State-machine state. `queue` holds commands drained from the controller.
 * Exactly one async step (a reply post or the long poll) is in flight at any
 * time, so no per-command closure chain is ever kept alive. */
let queue = [];

function load_config()
{
	let ctx = cursor();
	ctx.load('mielofon-agent');

	let agent = ctx.get('mielofon-agent', 'main', 'agent_name');
	if (agent == null || !length(agent))
		agent = default_agent_name();

	let controller_url = ctx.get('mielofon-agent', 'main', 'controller_url');
	let cacert = ctx.get('mielofon-agent', 'main', 'cacert');
	let cert = ctx.get('mielofon-agent', 'main', 'cert');
	let key = ctx.get('mielofon-agent', 'main', 'key');

	cfg = {
		agent: agent,
		url: controller_url,
		cacert: cacert,
		cert: cert,
		key: key,
		timeout_ms: int(ctx.get('mielofon-agent', 'main', 'command_timeout_ms') || '30000'),
		quiet_max_mbps: float(ctx.get('mielofon-agent', 'main', 'quiet_max_mbps'), 15.0),
		ping_count: int(ctx.get('mielofon-agent', 'main', 'ping_count') || '3'),
		ping_interval: ctx.get('mielofon-agent', 'main', 'ping_interval') || '0.2',
		rr_duration: ctx.get('mielofon-agent', 'main', 'rr_duration') || '4',
		tcp_duration: ctx.get('mielofon-agent', 'main', 'tcp_duration') || '4',
	};

	ctx.foreach('mielofon-agent', 'link', function(s) {
		if (!s.interface || !s.target)
			return true;

		push(links, {
			from: s.from || agent,
			to: s.to,
			interface: s.interface,
			target: s.target,
			source: s.source || null,
			cost_command: s.cost_command || null,
		});

		return true;
	});
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
	if (!cmd || !cmd.type) {
		cb();
		return;
	}

	if (cmd.type == 'apply_cost') {
		let link = find_link(cmd.link.interface);

		if (!link || !link.cost_command) {
			reply_send(cmd, { kind: 'applied', link: cmd.link, cost: cmd.cost }, cb);
			return;
		}

		apply_cost(link.cost_command, link.interface, cmd.cost, function(err) {
			reply_send(cmd, {
				kind: 'applied',
				link: cmd.link,
				cost: cmd.cost,
				note: err || null,
			}, cb);
		});

		return;
	}

	if (cmd.type != 'probe') {
		log.WARN('unknown command type %s\n', cmd.type);
		cb();
		return;
	}

	let link = find_link(cmd.link.interface);

	if (!link) {
		log.WARN('unknown interface %s, skipping\n', cmd.link.interface);
		cb();
		return;
	}

	if (cmd.tier == 'throughput') {
		run_throughput(link, cfg, function(e, r) {
			metrics.record_throughput(link, r);

			reply_send(cmd, {
				kind: 'probe',
				link: { from: link.from, to: link.to, interface: link.interface },
				rtt_ms: null,
				loss_pct: null,
				rr_tps: null,
				token: cmd.token || null,
				util_mbps: r.util_mbps,
				tcp_mbps: r.tcp_mbps,
				state: r.busy ? 'busy' : 'quiet',
			}, cb);
		});

		return;
	}

	run_always(link, cfg, function(e, r) {
		metrics.record_always(link, r);

		reply_send(cmd, {
			kind: 'probe',
			link: { from: link.from, to: link.to, interface: link.interface },
			rtt_ms: r.rtt_ms,
			loss_pct: r.loss_pct,
			rr_tps: r.rr_tps,
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

/* Single flat continuation: pop the next command if any, otherwise re-arm
 * the long poll. Never recurses through a chain of per-command closures. */
function step()
{
	if (length(queue)) {
		let cmd = shift(queue);

		run_command(cmd, function() { step(); });
		return;
	}

	command_step();
};

function register()
{
	let body = { agent: cfg.agent, links: [] };

	for (let l in links)
		push(body.links, { from: l.from, to: l.to, interface: l.interface });

	log.NOTE('registering %s with %d links\n', cfg.agent, length(links));

	post_json(client, '/v1/agent/register', body, function(err, status, raw) {
		if (err || status != 200) {
			log.WARN('register failed: %s / %s — retrying\n', err, status);
			uloop.timer(3000, register);
			return;
		}

		enqueue_commands(parse_json(raw));
		step();
	});
};

function command_step()
{
	let body = { agent: cfg.agent, timeout_ms: cfg.timeout_ms };

	post_json(client, '/v1/agent/command', body, function(err, status, raw) {
		if (err || status != 200) {
			log.WARN('command long-poll failed: %s / %s — retrying\n', err, status);
			uloop.timer(2000, command_step);
			return;
		}

		enqueue_commands(parse_json(raw));
		step();
	});
};

load_config();

if (!cfg.agent || !cfg.url || !cfg.cert || !cfg.key) {
	log.ERR('mielofon-agent: incomplete configuration\n');
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

register();
uloop.run();
