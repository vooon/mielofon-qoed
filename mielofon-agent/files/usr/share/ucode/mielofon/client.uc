'use strict';

/* mTLS HTTPS client for the controller (ucode-mod-uclient).
 *
 * Uses the low-level uclient API so mutual TLS works: ssl_init() accepts
 * cert_file / key_file / ca_files[] / verify. A single in-flight request is
 * allowed at a time (the agent is single-threaded under uloop); callers must
 * sequence their requests.
 *
 * Callback closures are hoisted to module scope and route through the single
 * `req_state` slot, which is cleared on completion. Keeping the handlers
 * module-scoped — instead of allocating a fresh set of nested closures per
 * request — is what keeps the agent's RSS flat under an indefinite
 * long-poll loop: a completed request leaves no captured scope behind.
 *
 * The uclient transport constructor is injected via `create()` (production
 * passes `transport.uc`'s new_client; unit tests pass a fake) so the reserved
 * `new` name never has to leak here and requests are testable without a live
 * controller.
 */

let handler = null;

/* Current request state { ucl, buf, cb } — null when idle. */
let req_state = null;

function finish(err, status)
{
	if (req_state == null)
		return;

	let st = req_state;
	req_state = null;

	st.cl.active = false;

	if (st.ucl != null)
		st.ucl.free();

	st.cb(err, status, st.buf);
}

function handle_data_read()
{
	if (req_state == null)
		return;

	let chunk = req_state.ucl.read();

	if (chunk != null && length(chunk))
		req_state.buf += chunk;
}

function handle_data_eof()
{
	if (req_state == null)
		return;

	let st = req_state.ucl.status();
	finish(null, st ? st.status : null);
}

function handle_error(code)
{
	finish('uclient error ' + code, null);
}

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

/* POST a JSON body, then invoke cb(err, status, raw_body) on completion.
 * One request at a time: `cl.active` guards against reentrancy. */
export function post_json(cl, path, body, cb)
{
	if (cl.active)
		die('concurrent uclient request');

	cl.active = true;

	if (handler == null)
		handler = {
			data_read: handle_data_read,
			data_eof: handle_data_eof,
			error: handle_error,
		};

	req_state = { cl: cl, ucl: null, buf: '', cb: cb };

	let url = cl.base_url + path;
	let ucl = cl.new_client(url, null, handler);

	if (ucl == null) {
		req_state = null;
		cl.active = false;
		cb('client create failed', null, '');
		return;
	}

	req_state.ucl = ucl;

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
