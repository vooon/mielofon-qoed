'use strict';

/* Shared helpers for the mielofon agent modules (obserwrt-style util.uc). */

import { readfile } from 'fs';

/* Parse a string/uci value as a float; returns `def` on missing/invalid. */
export function float(v, def)
{
	if (v == null || !length(v))
		return def;

	let n = +v;

	return (n == n) ? n : def;
};

/* Decode a JSON string without throwing; returns null on empty/invalid. */
export function parse_json(s)
{
	if (s == null || !length(s))
		return null;

	try {
		return json(s);
	}
	catch (e) {
		return null;
	}
};

/* Default agent identity: the router hostname, or a constant fallback. */
export function default_agent_name()
{
	let hn = readfile('/proc/sys/kernel/hostname');

	if (hn != null) {
		hn = trim(hn);

		if (length(hn))
			return hn;
	}

	return 'mielofon-agent';
};
