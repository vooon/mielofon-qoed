'use strict';

/* mielofon-agent — ucode router agent.
 *
 * Thin executor: registers its links with the controller, long-polls
 * `POST /v1/agent/command` for commands, runs the probe it is told to run
 * (or applies the OSPF cost it is told to apply), and replies on
 * `POST /v1/agent/reply`, echoing each command's job id. No scheduling or
 * policy logic lives here.
 */

import * as log from 'log';
import { cursor } from 'uci';
import * as uloop from 'uloop';
import { create as create_client, post_json } from './client.uc';
import { new_client } from './transport.uc';
import { run_always, run_throughput } from './probes.uc';
import { apply_cost } from './cost.uc';

let cfg = {};
let links = [];
let client = null;

function float(v, def)
{
	let n = +v;

	return (n == n) ? n : def;
};

function load_config()
{
	let ctx = cursor();
	ctx.load('mielofon-agent');

	let agent = ctx.get('mielofon-agent', 'main', 'agent_name');
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

function reply(cmd, obj, done)
{
	obj.agent = cfg.agent;
	obj.id = cmd.id;

	post_json(client, '/v1/agent/reply', obj, function(err, status) {
		if (err || status != 200)
			log.WARN('reply %s failed: %s / %s\n', cmd.id, err, status);

		done();
	});
};

function always_and_reply(cmd, link, done)
{
	run_always(link, cfg, function(e, r) {
		reply(cmd, {
			kind: 'probe',
			link: { from: link.from, to: link.to, interface: link.interface },
			rtt_ms: r.rtt_ms,
			loss_pct: r.loss_pct,
			rr_tps: r.rr_tps,
			token: null,
			util_mbps: 0,
			state: 'quiet',
		}, done);
	});
};

function throughput_and_reply(cmd, link, done)
{
	run_throughput(link, cfg, function(e, r) {
		reply(cmd, {
			kind: 'probe',
			link: { from: link.from, to: link.to, interface: link.interface },
			rtt_ms: null,
			loss_pct: null,
			rr_tps: null,
			token: cmd.token || null,
			util_mbps: r.util_mbps,
			tcp_mbps: r.tcp_mbps,
			state: r.busy ? 'busy' : 'quiet',
		}, done);
	});
};

function apply_cost_and_reply(cmd, done)
{
	let link = find_link(cmd.link.interface);

	if (!link || !link.cost_command) {
		reply(cmd, { kind: 'applied', link: cmd.link, cost: cmd.cost }, done);
		return;
	}

	apply_cost(link.cost_command, link.interface, cmd.cost, function(err) {
		reply(cmd, {
			kind: 'applied',
			link: cmd.link,
			cost: cmd.cost,
			note: err || null,
		}, done);
	});
};

function handle_command(cmd, done)
{
	if (!cmd || !cmd.type) {
		done();
		return;
	}

	if (cmd.type == 'apply_cost') {
		apply_cost_and_reply(cmd, done);
		return;
	}

	if (cmd.type == 'probe') {
		let link = find_link(cmd.link.interface);

		if (!link) {
			log.WARN('unknown interface %s, skipping\n', cmd.link.interface);
			done();
			return;
		}

		if (cmd.tier == 'throughput')
			throughput_and_reply(cmd, link, done);
		else
			always_and_reply(cmd, link, done);

		return;
	}

	log.WARN('unknown command type %s\n', cmd.type);
	done();
};

function handle_commands(cmds, done)
{
	function next(i)
	{
		if (i >= length(cmds)) {
			done();
			return;
		}

		handle_command(cmds[i], function() { next(i + 1); });
	}

	next(0);
};

function parse_list(raw)
{
	if (raw == null || !length(raw))
		return [];

	try {
		return json(raw);
	}
	catch (e) {
		log.WARN('bad json response: %s\n', raw);
		return [];
	}
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
			uloop.timer(3000, function() { register(); });
			return;
		}

		let v = parse_list(raw);
		handle_commands((v && v.commands) ? v.commands : [], command_step);
	});
};

function command_step()
{
	let body = { agent: cfg.agent, timeout_ms: cfg.timeout_ms };

	post_json(client, '/v1/agent/command', body, function(err, status, raw) {
		if (err || status != 200) {
			log.WARN('command long-poll failed: %s / %s — retrying\n', err, status);
			uloop.timer(2000, function() { command_step(); });
			return;
		}

		let v = parse_list(raw);
		handle_commands((v && v.commands) ? v.commands : [], command_step);
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

register();
uloop.run();
