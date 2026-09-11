/*
 * mielofon-probe - unit test harness (tests/harness.cpp)
 *
 * Host-runnable (plain C++ toolchain, json-c) unit tests for the pure,
 * dependency-free parts of the probe daemon: the per-link window math
 * (link.hpp), the byte-counter rate sampler (counters.cpp), the Prometheus
 * textfile renderer (metrics.cpp) and the iperf3 JSON result parser
 * (iperf_json.cpp). These are deliberately isolated from libubus/libiperf3 so
 * the harness runs on CI without the OpenWrt target headers.
 *
 * Follows obserwrt's golden-vector harness style: CHECK macros + a nonzero
 * exit on any failure.
 */

#include <cstdio>
#include <cstdlib>
#include <string>
#include <unordered_map>
#include <vector>

#include "counters.hpp"
#include "iperf_json.hpp"
#include "link.hpp"
#include "metrics.hpp"
#include "snapshot.hpp"

static int g_failures = 0;

#define CHECK(cond)                                                                                \
	do {                                                                                       \
		if (!(cond)) {                                                                     \
			std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);       \
			g_failures++;                                                              \
		}                                                                                  \
	} while (0)

#define CHECK_EQ(a, b)                                                                             \
	do {                                                                                       \
		if (!((a) == (b))) {                                                               \
			std::fprintf(stderr, "FAIL %s:%d: %s (%lld) != %s (%lld)\n", __FILE__,     \
			             __LINE__, #a, (long long)(a), #b, (long long)(b));            \
			g_failures++;                                                              \
		}                                                                                  \
	} while (0)

/* ---- link window math -------------------------------------------------- */

static void test_link_window()
{
	probe::Link l;
	l.window = 4;

	l.record_sent(1000);
	l.record_reply(10.0, 64, 1001);
	l.record_sent(1002);
	l.record_reply(20.0, 64, 1003);
	l.record_sent(1004);
	l.record_timeout(); // no reply
	l.record_sent(1006);
	l.record_reply(30.0, 64, 1007);

	double rtt = 0;
	CHECK(l.rtt(rtt));
	CHECK(rtt > 19.9 && rtt < 20.1); // (10+20+30)/3

	double jit = 0;
	CHECK(l.jitter(jit));
	CHECK(jit > 8.0 && jit < 9.0); // stddev of {10,20,30}

	// loss over the window: 1 timeout of 4 outcomes -> 25%
	CHECK_EQ((long long)(l.loss() * 10), 250);

	// sent/received/errors counters
	CHECK_EQ((long long)l.sent, 4);
	CHECK_EQ((long long)l.received, 3);
	CHECK_EQ((long long)l.errors, 1);
	CHECK_EQ((long long)l.last_ts, 1007);

	// window trims: push 8 replies, only last 4 kept
	probe::Link t;
	t.window = 4;
	for (int i = 0; i < 8; i++)
		t.record_reply(1.0 + i, 64, 2000 + i);
	CHECK_EQ((long long)t.rtts.size(), 4);
	CHECK(t.rtt(rtt) && rtt > 6.4 && rtt < 6.6); // avg of {5,6,7,8}

	// <2 samples -> no jitter
	probe::Link one;
	one.record_reply(5.0, 64, 1);
	CHECK(!one.jitter(jit));
}

/* ---- counter rate sampler ---------------------------------------------- */

static void test_counters()
{
	// The sampler reads sysfs files which don't exist on a bare host, so we
	// exercise the rate computation via a fake by checking the interface
	// contract: sampling once yields no value (needs two points), and the
	// rate is derived from the byte delta / interval.
	//
	// (read_counters is static; the numeric path is covered indirectly by the
	// integration build. Here we just verify reset() clears state and that a
	// fresh sampler reports nothing for unknown ifaces without crashing.)
	probe::CounterSampler s;
	s.reset();
	auto out = s.sample({"nonexistent0"}, 1.0);
	CHECK_EQ((long long)out.size(), 0);
}

/* ---- iperf3 JSON parser ------------------------------------------------ */

static void test_iperf_json()
{
	// TCP: sum_received bits_per_second -> mbps
	auto tcp = probe::parse_iperf_json(
	    R"({"end":{"sum_received":{"bits_per_second":150000000}}})");
	CHECK(tcp.mbps.has_value());
	CHECK(tcp.mbps.value() > 149.9 && tcp.mbps.value() < 150.1);
	CHECK(!tcp.jitter_ms.has_value());

	// UDP: bits + jitter + loss
	auto udp = probe::parse_iperf_json(
	    R"({"end":{"sum_received":{"bits_per_second":20000000,"jitter_ms":1.25,"lost_packets":10,"packets":1000,"loss_pct":1.0}}})");
	CHECK(udp.mbps.has_value());
	CHECK(udp.mbps.value() > 19.9 && udp.mbps.value() < 20.1);
	CHECK(udp.jitter_ms.has_value() && udp.jitter_ms.value() > 1.2);

	// falls back to `sum` when sum_received absent
	auto fallback = probe::parse_iperf_json(
	    R"({"end":{"sum":{"bits_per_second":50000000}}})");
	CHECK(fallback.mbps.has_value() && fallback.mbps.value() > 49.9);

	// unparseable / empty -> no fields fabricated
	auto bad = probe::parse_iperf_json("not json");
	CHECK(!bad.mbps.has_value());
	auto empty = probe::parse_iperf_json("");
	CHECK(!empty.mbps.has_value());
}

/* ---- metrics renderer --------------------------------------------------- */

static void test_metrics()
{
	std::vector<probe::Snapshot> snapshots;
	probe::Snapshot s;
	s.interface = "awg_hub_a";
	s.have_rtt = true;
	s.rtt_ms = 12.5;
	s.have_jitter = true;
	s.jitter_ms = 1.5;
	s.loss_pct = 0.0;
	s.util_mbps = 3.25;
	s.ttl = 64;
	s.sent = 10;
	s.received = 10;
	s.errors = 0;
	snapshots.push_back(s);

	std::string text = probe::render_textfile(snapshots);

	CHECK(text.find("# TYPE mielofon_probe_rtt_ms gauge") != std::string::npos);
	CHECK(text.find("mielofon_probe_rtt_ms{interface=\"awg_hub_a\"} ") !=
	      std::string::npos);
	CHECK(text.find("mielofon_probe_rtt_ms{interface=\"awg_hub_a\"} 12.5") !=
	      std::string::npos);
	CHECK(text.find("mielofon_probe_jitter_ms{interface=\"awg_hub_a\"} 1.5") !=
	      std::string::npos);
	CHECK(text.find("mielofon_probe_loss_pct{interface=\"awg_hub_a\"} 0.0") !=
	      std::string::npos);
	CHECK(text.find("mielofon_probe_util_mbps{interface=\"awg_hub_a\"} 3.25") !=
	      std::string::npos);
	CHECK(text.find("mielofon_probe_pings_received_total{interface=\"awg_hub_a\"} 10") !=
	      std::string::npos);
	CHECK(text.find("mielofon_probe_build_info{") != std::string::npos);

	// label escaping: an interface with quotes/newlines must not break it
	probe::Snapshot q;
	q.interface = "awg_\"quoted\"\n";
	q.util_mbps = 1.0;
	q.sent = 1;
	q.received = 1;
	std::vector<probe::Snapshot> qs = { q };
	std::string qtext = probe::render_textfile(qs);
	CHECK(qtext.find("awg_\\\"quoted\\\"\\n") != std::string::npos);
}

int main()
{
	test_link_window();
	test_counters();
	test_iperf_json();
	test_metrics();

	if (g_failures != 0) {
		std::fprintf(stderr, "harness: %d failures\n", g_failures);
		return 1;
	}
	std::printf("harness: all checks passed\n");
	return 0;
}