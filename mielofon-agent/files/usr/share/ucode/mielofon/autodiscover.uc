'use strict';

/* mielofon-agent — BIRD/OSPF link auto-discovery.
 *
 * Derives the agent's outgoing link set from BIRD state instead of static
 * per-link UCI sections. Discovery is a pure function of two ubus snapshots:
 *
 *   - `bird status`  (rpcd-mod-bird, structured): pick the mesh OSPF protocol
 *     by name (nodes may run an unrelated second OSPF instance), take its
 *     point-to-point interfaces, and match each interface to a BGP peer
 *     protocol (`peer_<node>_<domain>`) to recover the peer/node name.
 *   - `network.interface.<iface> status`  (netifd): the interface's own IPv6
 *     address -> probe source; the probe target is the far side of the /127.
 *
 * No shelling out and no re-parsing of BIRD CLI text: all input is already
 * structured JSON. The `bus` object is injected so unit tests can feed fixed
 * fixtures; production passes a thin ubus adapter (see mielofon-agent.uc).
 */

/* ---- address helpers --------------------------------------------------- */

function hexval(ch)
{
	let o = ord(ch);

	if (o >= 48 && o <= 57)          /* '0'-'9' */
		return o - 48;
	if (o >= 97 && o <= 102)         /* 'a'-'f' */
		return o - 97 + 10;
	if (o >= 65 && o <= 70)          /* 'A'-'F' */
		return o - 65 + 10;

	return -1;
}

function hextoi(s)
{
	let v = 0;

	for (let i = 0; i < length(s); i++) {
		let d = hexval(substr(s, i, 1));

		if (d < 0)
			return null;

		v = v * 16 + d;
	}

	return v;
}

function is_linklocal(addr)
{
	/* global scope: fe80::/10 (top 10 bits 1111111010). */
	return addr != null && substr(addr, 0, 2) == 'fe';
}

/* Far side of a /127: flip the last address bit. `addr` is like
 * "fd01:0:0:1::10:1" -> "fd01:0:0:1::10:0". Returns null if the tail is not
 * a plain hex hextet (malformed / unusual compression). */
function flip_last_bit(addr)
{
	if (addr == null)
		return null;

	let sep = rindex(addr, ':');

	if (sep < 0)
		return null;

	let head = substr(addr, 0, sep + 1);
	let tail = substr(addr, sep + 1);

	if (!length(tail))
		tail = '0';

	let v = hextoi(tail);

	if (v == null)
		return null;

	return head + sprintf('%x', v ^ 1);
}

/* The node's mesh loopback: a global (non-link-local) IPv6 assigned to the
 * loopback interface (`dummy_awg`). The agent reports this at register so the
 * controller's trace walker can resolve a destination *node name* into the
 * prefix it asks each hop's BIRD to resolve. Returns null when absent. */
export function loopback_address(iface_status)
{
	if (iface_status == null || iface_status['ipv6-address'] == null)
		return null;

	for (let a in iface_status['ipv6-address'])
		if (!is_linklocal(a.address))
			return a.address;

	return null;
};

/* ---- BGP peer / node naming -------------------------------------------- */

/* "peer_hub_a_example_com" minus "peer_" minus "_example_com" -> "hub_a". */
export function peer_node_name(name, suffix)
{
	let n = name;

	if (n == null || substr(n, 0, 5) != 'peer_')
		return null;

	n = substr(n, 5);

	if (suffix != null && length(suffix) && rindex(n, suffix) == length(n) - length(suffix))
		n = substr(n, 0, length(n) - length(suffix));

	return length(n) ? n : null;
};

/* Interface token by a prefix convention ("awg_hub_a" -> "hub_a"). */
export function iface_token(iface, prefix)
{
	if (iface == null)
		return null;

	if (prefix != null && length(prefix) && substr(iface, 0, length(prefix)) == prefix)
		return substr(iface, length(prefix));

	return iface;
};

/* ---- select candidate links from `bird status` -------------------------- */

export function select_links(status, cfg)
{
	let links = [];

	if (status == null || status.ospf == null)
		return links;

	let ospf = null;

	for (let o in status.ospf) {
		if (o.protocol == cfg.ospf_protocol) {
			ospf = o;
			break;
		}
	}

	if (ospf == null || ospf.interfaces == null)
		return links;

	/* map node name -> present, from BGP peer protocol names */
	let nodes = {};

	if (status.bgp != null) {
		for (let b in status.bgp) {
			let n = peer_node_name(b.name, cfg.bgp_peer_suffix);

			if (n != null)
				nodes[n] = true;
		}
	}

	let excludes = cfg.excludes || [];

	for (let i in ospf.interfaces) {
		let iface = i.interface;

		/* rpcd exposes `type` since 0.4.0; tolerate older snapshots by
		 * relying on the BGP-peer match when the field is absent. */
		if (i.type != null && i.type != 'ptp')
			continue;

		if (index(excludes, iface) >= 0)
			continue;

		let token = iface_token(iface, cfg.iface_prefix);

		if (token == null || !exists(nodes, token)) {
			continue;
		}

		push(links, { interface: iface, to: token, source: null, target: null });
	}

	return links;
};

/* ---- addresses from `network.interface.<iface> status` ------------------ */

/* Return { source, target } or null when no usable global IPv6 exists. */
export function derive_address(iface_status)
{
	if (iface_status == null || iface_status['ipv6-address'] == null)
		return null;

	let addrs = iface_status['ipv6-address'];
	let chosen = null;

	for (let a in addrs) {
		if (a.mask != null && int(a.mask) == 127 && !is_linklocal(a.address)) {
			chosen = a.address;
			break;
		}
	}

	if (chosen == null) {
		for (let a in addrs) {
			if (!is_linklocal(a.address)) {
				chosen = a.address;
				break;
			}
		}
	}

	if (chosen == null)
		return null;

	let target = flip_last_bit(chosen);

	if (target == null)
		return null;

	return { source: chosen, target: target };
};

/* ---- orchestration ------------------------------------------------------ */

/* bus: { status(): bird status JSON or null, iface(name): netifd status or
 * null }. Returns the link array (may be empty); interfaces without a
 * usable address are dropped. */
export function discover(cfg, bus)
{
	let status = bus.status();
	let links = select_links(status, cfg);
	let out = [];

	for (let i = 0; i < length(links); i++) {
		let a = derive_address(bus.iface(links[i].interface));

		if (a == null)
			continue;

		links[i].source = a.source;
		links[i].target = a.target;
		push(out, links[i]);
	}

	return out;
};
