/*
 * mielofon-probe - Prometheus textfile writer (metrics.cpp)
 *
 * Minimal Prometheus text exposition builder for doubles + per-link gauges,
 * then an atomic temp+rename write (same pattern as obserwrt's metrics.cpp).
 * No HTTP server.
 */

#include "metrics.hpp"

#include <cstdio>
#include <cstring>
#include <string>

#ifndef MIELOFON_PROBE_VERSION
#define MIELOFON_PROBE_VERSION "dev"
#endif

namespace probe
{

namespace
{

/// JSON-style label escaping for \ " and newline in the value.
std::string escape(const std::string &s)
{
	std::string out;
	out.reserve(s.size());
	for (char c : s) {
		switch (c) {
		case '\\':
			out += "\\\\";
			break;
		case '"':
			out += "\\\"";
			break;
		case '\n':
			out += "\\n";
			break;
		default:
			out += c;
		}
	}
	return out;
}

} // namespace

std::string render_textfile(const std::vector<Snapshot> &snapshots)
{
	std::string out;

	out += "# HELP mielofon_probe_build_info Build and version information.\n";
	out += "# TYPE mielofon_probe_build_info gauge\n";
	out += "mielofon_probe_build_info{version=\"" MIELOFON_PROBE_VERSION "\"} 1\n";
	out += "# HELP mielofon_probe_rtt_ms Rolling average RTT (ms).\n";
	out += "# TYPE mielofon_probe_rtt_ms gauge\n";
	out += "# HELP mielofon_probe_jitter_ms RTT jitter (ms, std-dev of window).\n";
	out += "# TYPE mielofon_probe_jitter_ms gauge\n";
	out += "# HELP mielofon_probe_loss_pct Packet loss over the window (%).\n";
	out += "# TYPE mielofon_probe_loss_pct gauge\n";
	out += "# HELP mielofon_probe_util_mbps Interface utilisation (Mbps).\n";
	out += "# TYPE mielofon_probe_util_mbps gauge\n";
	out += "# HELP mielofon_probe_ttl Last received TTL.\n";
	out += "# TYPE mielofon_probe_ttl gauge\n";
	out += "# HELP mielofon_probe_pings_sent_total Echo requests sent.\n";
	out += "# TYPE mielofon_probe_pings_sent_total counter\n";
	out += "# HELP mielofon_probe_pings_received_total Echo replies received.\n";
	out += "# TYPE mielofon_probe_pings_received_total counter\n";
	out += "# HELP mielofon_probe_pings_errors_total Probe errors.\n";
	out += "# TYPE mielofon_probe_pings_errors_total counter\n";

	for (const Snapshot &s : snapshots) {
		const std::string lbl =
		    "{interface=\"" + escape(s.interface) + "\"}";

		// Only emit values we actually measured (unset stays absent).
		if (s.have_rtt)
			out += "mielofon_probe_rtt_ms" + lbl + " " +
			       std::to_string(s.rtt_ms) + "\n";
		if (s.have_jitter)
			out += "mielofon_probe_jitter_ms" + lbl + " " +
			       std::to_string(s.jitter_ms) + "\n";
		if (s.loss_pct >= 0.0)
			out += "mielofon_probe_loss_pct" + lbl + " " +
			       std::to_string(s.loss_pct) + "\n";
		out += "mielofon_probe_util_mbps" + lbl + " " +
		       std::to_string(s.util_mbps) + "\n";
		out += "mielofon_probe_ttl" + lbl + " " +
		       std::to_string(s.ttl) + "\n";
		out += "mielofon_probe_pings_sent_total" + lbl + " " +
		       std::to_string(s.sent) + "\n";
		out += "mielofon_probe_pings_received_total" + lbl + " " +
		       std::to_string(s.received) + "\n";
		out += "mielofon_probe_pings_errors_total" + lbl + " " +
		       std::to_string(s.errors) + "\n";
	}

	return out;
}

bool write_textfile(const std::string &path, const std::string &body)
{
	const std::string tmp = path + ".tmp";
	FILE *f = fopen(tmp.c_str(), "w");
	if (f == nullptr)
		return false;
	size_t written = fwrite(body.data(), 1, body.size(), f);
	bool ok = (written == body.size()) && (fclose(f) == 0);
	if (!ok) {
		std::remove(tmp.c_str());
		return false;
	}
	return std::rename(tmp.c_str(), path.c_str()) == 0;
}

} // namespace probe