/*
 * mielofon-probe - per-link measurement state (link.hpp)
 *
 * A managed link is one directed mesh tunnel the ucode agent told us to probe.
 * The daemon pings `target` from `source` (when given) on its own cadence,
 * keeps a rolling window of round-trip times, and derives the always-tier
 * metrics the controller consumes: rtt (rolling avg), jitter (stddev of the
 * window) and loss (fraction of the window that timed out).
 *
 * Nothing here talks to the network; the ICMP loop feeds it and ubus/metrics
 * read it, so the window math is pure and trivially testable.
 */

#pragma once

#include <cmath>
#include <cstdint>
#include <deque>
#include <string>

namespace probe
{

/// Always-tier probe parameters, pushed by the agent via `configure`.
struct Params {
	double quiet_max_mbps = 15.0;
	int iperf_port = 5201;
	/// Separate UDP listener port for the UDP throughput leg (the TCP and UDP
	/// iperf3 servers may run on different ports). Defaults to iperf_port.
	int udp_port = 0;
	double udp_rate_mbps = 20.0;
	/// Ping interval in seconds (fractional allowed).
	double ping_interval = 1.0;
};

/// One managed link.
struct Link {
	std::string interface; ///< local tunnel iface name (awg_*)
	std::string target;    ///< far side address (mesh /127 peer)
	std::string source;    ///< local address to bind, if any

	/// Rolling window of completed RTTs (ms), newest last.
	std::deque<double> rtts;
	/// Timeouts in the window (loss candidates), newest last.
	std::deque<bool> outcomes;
	/// Max samples in the rolling window.
	size_t window = 60;

	/// Per-link counters for the textfile/status.
	uint64_t sent = 0;
	uint64_t received = 0;
	uint64_t errors = 0;
	uint64_t last_ttl = 0;
	/// Unix seconds of the last completed probe cycle.
	uint64_t last_ts = 0;
	/// Instantaneous utilisation sampled just before the last probe (Mbps).
	double util_mbps = 0.0;

	void record_sent(uint64_t now) { ++sent; last_ts = now; }

	/// Record a successful reply. `rtt` in ms.
	void record_reply(double rtt_ms, uint64_t ttl, uint64_t now)
	{
		++received;
		last_ttl = ttl;
		last_ts = now;
		rtts.push_back(rtt_ms);
		outcomes.push_back(true);
		trim();
	}

	/// Record a timeout (no reply).
	void record_timeout()
	{
		errors++;
		outcomes.push_back(false);
		trim();
	}

	void trim()
	{
		while (rtts.size() > window)
			rtts.pop_front();
		while (outcomes.size() > window)
			outcomes.pop_front();
	}

	/// Rolling average RTT (ms); nullopt if no replies in window.
	bool rtt(double &out) const
	{
		if (rtts.empty())
			return false;
		double sum = 0.0;
		for (double r : rtts)
			sum += r;
		out = sum / static_cast<double>(rtts.size());
		return true;
	}

	/// Population std-dev of the RTT window = jitter (ms); false if <2 samples.
	bool jitter(double &out) const
	{
		if (rtts.size() < 2)
			return false;
		double mean = 0.0;
		for (double r : rtts)
			mean += r;
		mean /= static_cast<double>(rtts.size());
		double var = 0.0;
		for (double r : rtts) {
			double d = r - mean;
			var += d * d;
		}
		out = sqrt(var / static_cast<double>(rtts.size()));
		return true;
	}

	/// Loss percent over the whole window (timeouts / total). -1 if no data.
	double loss() const
	{
		if (outcomes.empty())
			return -1.0;
		size_t timeouts = 0;
		for (bool ok : outcomes)
			if (!ok)
				++timeouts;
		return 100.0 * static_cast<double>(timeouts) / static_cast<double>(outcomes.size());
	}
};

} // namespace probe