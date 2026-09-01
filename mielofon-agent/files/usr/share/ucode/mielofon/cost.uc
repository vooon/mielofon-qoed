'use strict';

/* OSPF cost application via rpcd-mod-bird over ubus.
 *
 * The agent only executes the cost it is told to apply. Preferred path is the
 * rpcd `bird set_ospf_cost {interface, cost}` reconfiguration method (it
 * derives the runtime config from the pristine /etc/bird.conf, validates with
 * `configure check`, and applies it) — no raw BIRD CLI string is built here.
 * On devices still running an older rpcd-mod-bird without that method, the
 * agent falls back to `bird query` running an operator-provided `cost_command`
 * template with `{interface}`/`{cost}` substituted. Empty command => no-op.
 */

import * as log from 'log';
import { connect } from 'ubus';

export function apply_cost(command, iface, cost, cb)
{
	let bus = connect();

	if (!bus) {
		cb('ubus connect failed');
		return;
	}

	/* Prefer the structured rpcd method. The ucode ubus binding returns null
	 * for an unknown method, which is indistinguishable from a transport
	 * failure — so when it is null we fall back to the template path for
	 * older rpcd-mod-bird deployments. */
	let res = bus.call('bird', 'set_ospf_cost', { interface: iface, cost: cost });

	if (res != null) {
		if (res.code != 0) {
			log.WARN('bird set_ospf_cost returned code %s: %s\n', res.code, res.stdout);
			cb('bird returned code ' + res.code);
			return;
		}

		log.NOTE('bird set_ospf_cost: %s\n', res.stdout);
		cb(null);
		return;
	}

	if (!command) {
		cb('no cost mechanism available (set_ospf_cost missing, no cost_command)');
		return;
	}

	let cmd = replace(command, '{interface}', iface);
	cmd = replace(cmd, '{cost}', cost);

	res = bus.call('bird', 'query', { command: cmd });

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