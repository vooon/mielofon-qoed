/*
 * mielofon-probe - iperf3 JSON result parser (iperf_json.hpp)
 *
 * Parses the `iperf3 -J`-style JSON output (libiperf3 produces the same on
 * `iperf_get_test_json_output` when JSON output is enabled) into plain result
 * numbers. Kept free of libiperf3 so it can be unit-tested on a plain C++
 * toolchain with only json-c.
 *
 * Structure consumed (iperf3 JSON):
 *   end.sum_received.bits_per_second  — TCP/UDP received bitrate
 *   end.sum.jitter_ms / loss_pct      — UDP jitter + loss (sum-received level
 *                                       is used when present, else sum)
 */

#pragma once

#include <optional>
#include <string>

namespace probe
{

/// Parsed throughput result (absent fields stay unset).
struct IperfResult {
	std::optional<double> mbps;
	/// UDP jitter (ms), UDP tests only.
	std::optional<double> jitter_ms;
	/// UDP loss %, UDP tests only.
	std::optional<double> loss_pct;
};

/// Parse iperf3 JSON text; returns an empty result on unparseable input
/// (fields are left unset, never fabricated as 0).
IperfResult parse_iperf_json(const std::string &json);

} // namespace probe