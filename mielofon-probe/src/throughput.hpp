/*
 * mielofon-probe - libiperf3 TCP+UDP throughput runner (throughput.hpp)
 *
 * Runs a gated throughput test in-process via libiperf3 (no subprocess, no
 * exec of `iperf3`), reading results from the test's JSON output with json-c.
 * TCP gives `tcp_mbps`; UDP (at a configured rate) additionally yields
 * `udp_mbps`, UDP jitter and loss. Failures surface as a null result, never a
 * fabricated 0 — an unmeasured dimension never constrains classification on
 * the controller.
 *
 * The caller (ubus `throughput` handler) enforces the quiet gate (util from
 * the counter sampler) and serialisation (one in-flight test cluster-wide).
 */

#pragma once

#include "link.hpp"

#include <cstdint>
#include <optional>
#include <string>

namespace probe
{

/// Result of one iperf3 client run (TCP or UDP leg).
struct ThroughputResult {
	/// Mbps for TCP (stream bitrate) or UDP (received bitrate).
	std::optional<double> mbps;
	/// UDP jitter (ms); only meaningful for the UDP leg.
	std::optional<double> udp_jitter_ms;
	/// UDP loss % (lost/total packets); only meaningful for the UDP leg.
	std::optional<double> udp_loss_pct;
};

/// Runs libiperf3 client tests; constructs tests, parses JSON results.
class ThroughputRunner
{
public:
	explicit ThroughputRunner(Params &params);

	ThroughputRunner(const ThroughputRunner &) = delete;
	ThroughputRunner &operator=(const ThroughputRunner &) = delete;

	/// TCP stream test to `target` on `port`, `duration` seconds.
	/// Returns nullopt on any setup/failure (never a fabricated 0).
	std::optional<ThroughputResult> run_tcp(const Link &link, int duration);

	/// UDP test to `target` at `params_.udp_rate_mbps`, `duration` seconds.
	/// Also yields jitter + loss. Nullopt on failure.
	std::optional<ThroughputResult> run_udp(const Link &link, int duration);

private:
	Params &params_;
};

} // namespace probe