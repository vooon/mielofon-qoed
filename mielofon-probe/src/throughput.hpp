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
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <optional>
#include <string>
#include <utility>

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

/*
 * Wire format between the forked test child and the daemon: one line per leg,
 * `<tag> <mbps> <jitter_ms> <loss_pct>\n`, an unset value written as `-`. A
 * leg that produced no result is written as a bare `<tag> -\n`.
 */

namespace detail
{

inline void encode_opt(std::string &out, const std::optional<double> &v)
{
	char buf[40];
	if (v)
		std::snprintf(buf, sizeof(buf), " %.17g", *v);
	else
		std::snprintf(buf, sizeof(buf), " -");
	out += buf;
}

/// Parse one ` <num|->` token at `p`; advances `p`. False on garbage.
inline bool decode_opt(const char *&p, std::optional<double> &v)
{
	while (*p == ' ')
		++p;
	if (*p == '-' && (p[1] == ' ' || p[1] == '\n' || p[1] == '\0')) {
		++p;
		v.reset();
		return true;
	}
	char *end = nullptr;
	double d = std::strtod(p, &end);
	if (end == p)
		return false;
	p = end;
	v = d;
	return true;
}

} // namespace detail

/// Serialise both legs of a test for the child -> daemon pipe.
inline std::string encode_results(const std::optional<ThroughputResult> &tcp,
                                  const std::optional<ThroughputResult> &udp)
{
	std::string out;
	const std::pair<const char *, const std::optional<ThroughputResult> *> legs[] = {
		{ "tcp", &tcp },
		{ "udp", &udp },
	};
	for (const auto &[tag, leg] : legs) {
		out += tag;
		if (*leg) {
			detail::encode_opt(out, (*leg)->mbps);
			detail::encode_opt(out, (*leg)->udp_jitter_ms);
			detail::encode_opt(out, (*leg)->udp_loss_pct);
		} else {
			out += " -";
		}
		out += '\n';
	}
	return out;
}

/// Parse `encode_results` output. Unknown or malformed lines are skipped, so
/// a truncated pipe read (child killed mid-write) yields unset legs, never a
/// fabricated 0.
inline void decode_results(const std::string &in, std::optional<ThroughputResult> &tcp,
                           std::optional<ThroughputResult> &udp)
{
	tcp.reset();
	udp.reset();
	size_t pos = 0;
	while (pos < in.size()) {
		size_t nl = in.find('\n', pos);
		if (nl == std::string::npos)
			break; // incomplete trailing line
		std::string line = in.substr(pos, nl - pos);
		pos = nl + 1;

		std::optional<ThroughputResult> *leg = nullptr;
		if (line.rfind("tcp ", 0) == 0)
			leg = &tcp;
		else if (line.rfind("udp ", 0) == 0)
			leg = &udp;
		else
			continue;

		const char *p = line.c_str() + 4;
		if (std::strcmp(p, "-") == 0)
			continue;
		ThroughputResult r;
		if (detail::decode_opt(p, r.mbps) && detail::decode_opt(p, r.udp_jitter_ms) &&
		    detail::decode_opt(p, r.udp_loss_pct) && r.mbps)
			*leg = r;
	}
}

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