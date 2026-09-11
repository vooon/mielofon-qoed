/*
 * mielofon-probe - per-link measurement snapshot (snapshot.hpp)
 *
 * A plain, dependency-free value type describing one link's latest
 * measurement window. Shared by the ICMP loop (producer), the ubus `status`
 * handler and the Prometheus textfile writer (consumers). Isolated in its own
 * header so metrics.hpp / tests can use it without pulling in libubox or
 * libiperf3 headers.
 */

#pragma once

#include <cstdint>
#include <string>

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

} // namespace probe