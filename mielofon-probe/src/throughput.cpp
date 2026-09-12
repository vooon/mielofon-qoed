/*
 * mielofon-probe - libiperf3 TCP+UDP throughput runner (throughput.cpp)
 *
 * Runs a gated throughput test in-process via libiperf3 and reads the result
 * from the JSON output. TCP gives tcp_mbps; UDP additionally yields jitter +
 * loss. Failures surface as nullopt, never a fabricated 0.
 */

#include "throughput.hpp"

#include "iperf_json.hpp"

// iperf_api.h only pulls <stdatomic.h> when HAVE_STDATOMIC_H is defined; from
// C++ it otherwise emits a #warning (fatal under -Werror). Include the header
// up front and declare the capability so the library header is happy.
#include <stdatomic.h>
#define HAVE_STDATOMIC_H 1
#include <iperf_api.h>

#include <cstdio>
#include <cstring>

namespace probe
{
// set_protocol/get_protocol are exported by libiperf but not declared in the
// public iperf_api.h (the CLI selects them via iperf_parse_arguments instead);
// the shared lib ships them, so declare them here for direct use.
extern "C" int set_protocol(struct iperf_test *, int);

ThroughputRunner::ThroughputRunner(Params &params) : params_(params) {}

namespace
{

/// Run one iperf3 client test with the given protocol to `port`; parse JSON.
std::optional<ThroughputResult> run_client(const Link &link, int duration,
                                           int protocol, int port,
                                           uint64_t rate_bps)
{
	struct iperf_test *test = iperf_new_test();
	if (test == nullptr)
		return std::nullopt;

	// iperf_new_test() zeroes the struct but does NOT initialise the protocol
	// list; set_protocol() walks it. iperf_defaults() must be called first
	// (the CLI reaches it via iperf_parse_arguments), otherwise set_protocol
	// dereferences an empty SLIST and segfaults.
	iperf_defaults(test);

	iperf_set_test_role(test, 'c');
	iperf_set_test_server_hostname(test, link.target.c_str());
	iperf_set_test_server_port(test, port);
	iperf_set_test_duration(test, duration);
	iperf_set_test_json_output(test, 1);
	if (set_protocol(test, protocol) != 0) {
		iperf_free_test(test);
		return std::nullopt;
	}
	if (rate_bps > 0)
		iperf_set_test_rate(test, rate_bps);
	if (!link.source.empty())
		iperf_set_test_bind_address(test, link.source.c_str());

	int rc = iperf_run_client(test);

	std::optional<ThroughputResult> out;
	if (rc >= 0) {
		const char *json = iperf_get_test_json_output_string(test);
		if (json != nullptr) {
			IperfResult r = parse_iperf_json(json);
			if (r.mbps) {
				ThroughputResult res;
				res.mbps = r.mbps;
				res.udp_jitter_ms = r.jitter_ms;
				res.udp_loss_pct = r.loss_pct;
				out = res;
			}
		}
	}

	iperf_free_test(test);
	return out;
}

} // namespace

std::optional<ThroughputResult> ThroughputRunner::run_tcp(const Link &link,
                                                          int duration)
{
	return run_client(link, duration, static_cast<int>(Ptcp),
	                  params_.iperf_port, 0);
}

std::optional<ThroughputResult> ThroughputRunner::run_udp(const Link &link,
                                                          int duration)
{
	// rate in bits/sec = Mbps * 1e6. The UDP leg targets its own listener
	// port when configured (udp_port), else the shared iperf_port.
	uint64_t rate = static_cast<uint64_t>(params_.udp_rate_mbps * 1e6);
	int port = params_.udp_port > 0 ? params_.udp_port : params_.iperf_port;
	return run_client(link, duration, static_cast<int>(Pudp), port, rate);
}

} // namespace probe