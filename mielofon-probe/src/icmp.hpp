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
#include "snapshot.hpp"
#include "throughput.hpp"

#include <sys/types.h>

#include <functional>
#include <memory>
#include <optional>
#include <string>
#include <unordered_map>
#include <vector>

namespace probe
{

/// Outcome of a gated throughput test on a link.
struct ThroughputOutcome {
	/// Link was busy (util above quiet_max) — no test was run.
	bool busy = false;
	/// Utilisation at gate time (Mbps).
	double util_mbps = 0.0;
	/// TCP throughput result (nullopt on failure).
	std::optional<probe::ThroughputResult> tcp;
	/// UDP throughput result (nullopt on failure/skip).
	std::optional<probe::ThroughputResult> udp;
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

	/// Completion callback of an asynchronous throughput test.
	using ThroughputDone = std::function<void(const ThroughputOutcome &)>;

	/// Start a gated TCP+UDP throughput test on `interface` via libiperf3.
	///
	/// The test runs in a forked child so the ICMP loop keeps running; ping
	/// sending is paused for the test plus a short grace period, because the
	/// test loads the node's uplink and would otherwise poison every link's
	/// RTT window with self-inflicted queueing.
	///
	/// Returns the outcome directly when no test is started — unknown link,
	/// quiet gate (util above quiet_max_mbps: busy), a test already running
	/// (busy), or fork failure — and `done` is never called. Otherwise returns
	/// nullopt and calls `done` exactly once from the event loop when the
	/// child exits or is killed by the watchdog.
	std::optional<ThroughputOutcome> begin_throughput(const std::string &interface,
	                                                  int duration, ThroughputDone done);

	/// Internals reached by the throughput child's uloop callbacks.
	void finish_throughput();
	void kill_throughput();

	/// Current params (for config echo / debug).
	const Params &params() const { return params_; }

	/// Internals reached by the uloop callbacks (icmp.cpp). The daemon is
	/// single-instance; `Peer` is opaque (defined in the TU) and the callbacks
	/// reach these through a TU-local instance pointer.
	void tick();
	void recv_probe(void *peer);

private:
	void send_probe(void *peer);
	void *find_peer(const std::string &interface) const;
	bool pings_paused() const;

	Params params_;
	/// In-flight throughput test (one at a time): child pid, result pipe,
	/// gate-time utilisation and the completion callback.
	pid_t test_pid_ = -1;
	int test_fd_ = -1;
	double test_util_mbps_ = 0.0;
	ThroughputDone test_done_;
	/// Monotonic µs until which pings stay paused after a test.
	uint64_t pause_until_us_ = 0;
	std::unordered_map<std::string, void *> peers_;
	CounterSampler counters_;
	ThroughputRunner throughput_;
};

} // namespace probe