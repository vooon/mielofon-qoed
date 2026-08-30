'use strict';

/* OSPF cost application via rpcd-mod-bird over ubus.
 *
 * The agent only executes the cost it is told to apply. The per-link
 * `cost_command` holds an operator-provided BIRD CLI command; `{interface}`
 * and `{cost}` are substituted, then the command is passed to `bird query`
 * (raw passthrough over the BIRD control socket). Empty command => no-op.
 */

import * as log from 'log';
import { connect } from 'ubus';

export function apply_cost(command, iface, cost, cb)
{
	let cmd = replace(command, '{interface}', iface);
	cmd = replace(cmd, '{cost}', cost);

	let bus = connect();

	if (!bus) {
		cb('ubus connect failed');
		return;
	}

	let res = bus.call('bird', 'query', { command: cmd });

	if (res == null) {
		cb('bird query failed: ' + (bus.error() || 'unknown'));
		return;
	}

	if (res.code != 0) {
		log.WARN('bird returned code %s: %s\n', res.code, res.stdout);
		cb('bird returned code ' + res.code);
		return;
	}

	log.NOTE('bird: %s\n', res.stdout);
	cb(null);
};
