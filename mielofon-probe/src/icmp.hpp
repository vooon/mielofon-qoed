/*
 * mielofon-probe - ICMP latency/jitter/loss loop (icmp.hpp)
 *
 * Owns one raw socket per managed link (ICMPv6 for the mesh, ICMPv4 for
 * completeness) and probes each on the shared ping cadence. A per-link probe
 * is one echo; the reply is matched by id+seq and turned into an RTT, each
 * send that goes unanswered before the next tick is a timeout. The loop is
 * driven by a single uloop timeout (tick) plus per-socket uloop_fd callbacks.
 *
 * The loop is ubus/metrics-agnostic: it exposes an immutable `snapshot()` the
 * ubus status handler and the textfile writer consume, and a `configure()`
 * that reconciles the managed link set + Params from a ubus configure call.
 *
 * The daemon is a single instance; the uloop callbacks are file-local in
 * icmp.cpp and reach the sole instance + fd->Peer map through TUs-internal
 * globals, so no libubus/uloop types leak into this header.
 */

#pragma once

#include "counters.hpp"
#include "link.hpp"

#include <memory>
#include <string>
#include <unordered_map>
#include <vector>

namespace probe
{

/// One per-link measurement snapshot (plain values; both ubus and textfile
/// consumers format these).
struct Snapshot {
	std::string interface;
	std::string target;

	double rtt_ms = 0.0;
	bool have_rtt = false;
	double jitter_ms = 0.0;
	bool have_jitter = false;
	/// Loss % over the window; -1 when no data.
	double loss_pct = -1.0;
	double util_mbps = 0.0;

	uint64_t ttl = 0;
	uint64_t ts = 0;

	uint64_t sent = 0;
	uint64_t received = 0;
	uint64_t errors = 0;
};

class IcmpLoop
{
public:
	IcmpLoop();
	~IcmpLoop();

	IcmpLoop(const IcmpLoop &) = delete;
	IcmpLoop &operator=(const IcmpLoop &) = delete;

	/// Replace the managed links + params. Opens/closes sockets to match.
	/// Returns the number of links that could not be opened (best effort).
	size_t configure(std::vector<Link> links, Params params);

	/// Immutable per-link measurement snapshot.
	std::vector<Snapshot> snapshot() const;

	/// Current params (for config echo / debug).
	const Params &params() const { return params_; }

	/// Internals reached by the uloop callbacks (icmp.cpp). The daemon is
	/// single-instance; `Peer` is opaque (defined in the TU) and the callbacks
	/// reach these through a TU-local instance pointer.
	void tick();
	void recv_probe(void *peer);

private:
	void send_probe(void *peer);

	Params params_;
	std::unordered_map<std::string, void *> peers_;
	CounterSampler counters_;
};

} // namespace probe