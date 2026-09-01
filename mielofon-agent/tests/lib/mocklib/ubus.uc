/* mielofon-agent - ubus module mock
 *
 * Replaces the real ubus module (loaded via compile-time `import` in
 * mielofon-agent.uc / expensive to reach in unit tests). `connect()` returns a
 * fake bus whose `call()` serves fixed JSON responses keyed by object+method,
 * from global.MOCK_UBUS.
 */

"use strict";

if (!exists(global, 'MOCK_UBUS'))
	global.MOCK_UBUS = {};

function connect() {
	return {
		call: (obj, method, args) => {
			let key = obj + '.' + method;
			let resp = global.MOCK_UBUS[key];

			if (resp == null)
				return null;

			if (typeof resp == 'function')
				return resp(args);

			return resp;
		},
	};
}

export { connect };