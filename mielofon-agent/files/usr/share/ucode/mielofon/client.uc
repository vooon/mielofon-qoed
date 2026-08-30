'use strict';

/* mTLS HTTPS client for the controller (ucode-mod-uclient).
 *
 * Uses the low-level uclient API so mutual TLS works: ssl_init() accepts
 * cert_file / key_file / ca_files[] / verify. A single in-flight request is
 * allowed at a time (the agent is single-threaded under uloop); callers must
 * sequence their requests.
 *
 * The uclient transport constructor is injected via `create()` (production
 * passes `transport.uc`'s new_client; unit tests pass a fake) so the reserved
 * `new` name never has to leak here and requests are testable without a live
 * controller.
 */

import * as log from 'log';

export function create(opts)
{
	return {
		base_url: opts.base_url,
		tls: opts.tls,
		timeout_ms: opts.timeout_ms,
		new_client: opts.new_client,
		active: false,
	};
};

/* POST a JSON body, then invoke cb(err, status, raw_body) on completion. */
export function post_json(cl, path, body, cb)
{
	if (cl.active)
		die('concurrent uclient request');

	cl.active = true;

	let url = cl.base_url + path;
	let buf = '';
	let done = false;
	let ucl = null;

	function finish(err, status)
	{
		if (done)
			return;

		done = true;
		cl.active = false;

		if (ucl != null)
			ucl.free();

		cb(err, status, buf);
	}

	ucl = cl.new_client(url, null, {
		data_read: function() {
			let chunk = ucl.read();

			if (chunk != null && length(chunk))
				buf += chunk;
		},
		data_eof: function() {
			let st = ucl.status();
			finish(null, st ? st.status : null);
		},
		error: function(code) {
			finish('uclient error ' + code, null);
		},
	});

	if (ucl == null) {
		cl.active = false;
		cb('client create failed', null, buf);
		return;
	}

	let payload = (body == null) ? '' : sprintf('%J', body);

	ucl.ssl_init({
		cert_file: cl.tls.cert,
		key_file: cl.tls.key,
		ca_files: [ cl.tls.cacert ],
		verify: true,
	});

	ucl.set_timeout(cl.timeout_ms);

	ucl.request({
		method: 'POST',
		headers: {
			'content-type': 'application/json',
		},
		post_data: payload,
	});
};