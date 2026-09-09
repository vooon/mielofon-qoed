// Thin JSON client for the mielofon admin API. All endpoints are served on
// the admin listener (loopback HTTP). Errors surface as thrown Error with the
// server's message when the body carries one.

async function http(path, query) {
	const url = query ? path + '?' + new URLSearchParams(query).toString() : path;
	const res = await fetch(url);
	if (!res.ok) {
		let msg = 'HTTP ' + res.status;
		try {
			const body = await res.json();
			if (body && body.error) msg = body.error;
		} catch {
			/* keep default message */
		}
		throw new Error(msg);
	}
	return res.json();
}

export function fetchGraph() {
	return http('/v1/graph');
}

export function fetchStatus() {
	return http('/v1/status');
}

export function fetchTrace(from, to) {
	return http('/v1/trace', { from, to });
}

export function fetchTs(link, since) {
	return http('/v1/ts', {
		from: link.from,
		to: link.to,
		interface: link.interface,
		since: since || Math.floor(Date.now() / 1000) - 3600,
	});
}