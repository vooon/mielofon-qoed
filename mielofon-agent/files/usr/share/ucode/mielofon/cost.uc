'use strict';

/* OSPF cost application via rpcd-mod-bird over ubus.
 *
 * The agent only executes the cost it is told to apply, via the rpcd
 * `bird set_ospf_cost {interface, cost}` method. That method is a config
 * editor: BIRD has no small CLI command to change a single interface cost,
 * so rpcd derives the runtime config from the pristine /etc/bird.conf,
 * validates it with `configure check`, and applies it. No raw BIRD CLI
 * string ever leaves this module — there is intentionally no `cost_command`
 * template fallback (it was a wrong concept).
 *
 * BIRD keeps one configuration and one undo level, so rpcd serializes
 * reconfigurations with flock and fails fast with `code 5` when another
 * apply is in flight; that is transient, retry it briefly.
 */

import * as log from 'log';
import * as uloop from 'uloop';
import { connect } from 'ubus';
import * as metrics from './metrics.uc';

const RETRY_MS = 2000;
const MAX_TRIES = 5;

export function apply_cost(iface, cost, cb, tries)
{
	let n = (tries || 0) + 1;

	metrics.counters.apply_cost++;

	if (n > MAX_TRIES) {
		metrics.counters.apply_cost_errors++;
		cb('bird reconfiguration busy after ' + MAX_TRIES + ' tries');
		return;
	}

	let bus = connect();

	if (!bus) {
		metrics.counters.apply_cost_errors++;
		cb('ubus connect failed');
		return;
	}

	let res = bus.call('bird', 'set_ospf_cost', { interface: iface, cost: cost });

	if (res == null) {
		metrics.counters.apply_cost_errors++;
		cb('bird set_ospf_cost failed: ' + (bus.error() || 'unknown'));
		return;
	}

	if (res.code == 5) {
		/* transient busy — confirmed error only if we exhaust retries */
		metrics.counters.apply_cost--;
		log.NOTE('bird set_ospf_cost busy, retrying (%s/%s)\n', n, MAX_TRIES);
		uloop.timer(RETRY_MS, function() { apply_cost(iface, cost, cb, n); });
		return;
	}

	if (res.code != 0) {
		metrics.counters.apply_cost_errors++;
		log.WARN('bird set_ospf_cost returned code %s: %s\n', res.code, res.stdout);
		cb('bird returned code ' + res.code);
		return;
	}

	log.NOTE('bird set_ospf_cost: %s\n', res.stdout);
	cb(null);
};

/* BIRD route lookup for the controller's end-to-end trace walker (rpcd-mod-
 * bird >= 0.5.0): `bird route {prefix}` returns `{code, routes:[...]}` with
 * ECMP-aware next hops. The reply is passed through untouched — the controller
 * owns the ECMP parsing. Older rpcd snapshots have no `route` method, which
 * renders as an unresolvable hop, not an agent error. */
export function query_route(bus, target)
{
	if (target == null || !length(target))
		return { code: 2, routes: [] };

	if (bus == null)
		return { code: 1, routes: [] };

	let res = bus.call('bird', 'route', { prefix: target });

	if (res == null)
		return { code: 1, routes: [] };

	let code = (res.code != null) ? int(res.code) : 1;
	let routes = (res.routes != null) ? res.routes : [];

	return { code: code, routes: routes };
};
