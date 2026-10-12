/*
 * mielofon-probe - unit test harness (tests/harness.cpp)
 *
 * Host-runnable (plain C++ toolchain, json-c) unit tests for the pure,
 * dependency-free parts of the probe daemon: the per-link window math
 * (link.hpp), the byte-counter rate sampler (counters.cpp), the Prometheus
 * textfile renderer (metrics.cpp), the iperf3 JSON result parser
 * (iperf_json.cpp), the RTT source selection (link.hpp) and the throughput
 * child's result pipe format (throughput.hpp). These are deliberately isolated from libubus/libiperf3 so
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
#include "throughput.hpp"

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

/* ---- RTT source selection --------------------------------------------- */

static void test_select_rtt()
{
	// Regression: a 20 ms reply read 9 s late (event loop stalled by a
	// throughput test) must keep its real RTT from the kernel RX stamp
	// instead of becoming a 9000 ms sample that wrecks the jitter window.
	double rtt = probe::select_rtt_ms(20000000LL, 9000000ULL);
	CHECK(rtt > 19.99 && rtt < 20.01);

	// No kernel stamp: monotonic send->read span.
	rtt = probe::select_rtt_ms(std::nullopt, 15000ULL);
	CHECK(rtt > 14.99 && rtt < 15.01);

	// Realtime clock stepped: kernel RTT negative or longer than the
	// monotonic span is impossible, fall back to monotonic.
	rtt = probe::select_rtt_ms(-5000000LL, 15000ULL);
	CHECK(rtt > 14.99 && rtt < 15.01);
	rtt = probe::select_rtt_ms(3600000000000LL, 15000ULL);
	CHECK(rtt > 14.99 && rtt < 15.01);

	// The stall scenario end to end on the window: 59 x 20 ms plus one
	// late-read reply. With the kernel stamp jitter stays ~0, with the old
	// read-time RTT it explodes past any sane threshold.
	probe::Link good, bad;
	good.window = bad.window = 60;
	for (int i = 0; i < 59; i++) {
		good.record_reply(20.0, 64, 1);
		bad.record_reply(20.0, 64, 1);
	}
	good.record_reply(probe::select_rtt_ms(20000000LL, 9000000ULL), 64, 1);
	bad.record_reply(9000.0, 64, 1);
	double jit = 0;
	CHECK(good.jitter(jit) && jit < 0.01);
	CHECK(bad.jitter(jit) && jit > 1000.0);
}

/* ---- throughput child result pipe -------------------------------------- */

static void test_result_codec()
{
	probe::ThroughputResult tcp;
	tcp.mbps = 87.5;
	probe::ThroughputResult udp;
	udp.mbps = 19.75;
	udp.udp_jitter_ms = 1.25;
	udp.udp_loss_pct = 0.5;

	std::optional<probe::ThroughputResult> t, u;
	probe::decode_results(probe::encode_results(tcp, udp), t, u);
	CHECK(t && t->mbps && *t->mbps > 87.49 && *t->mbps < 87.51);
	CHECK(t && !t->udp_jitter_ms && !t->udp_loss_pct);
	CHECK(u && u->mbps && *u->mbps > 19.74 && *u->mbps < 19.76);
	CHECK(u && u->udp_jitter_ms && *u->udp_jitter_ms > 1.24);
	CHECK(u && u->udp_loss_pct && *u->udp_loss_pct > 0.49);

	// A failed leg stays unset, never a fabricated 0.
	probe::decode_results(probe::encode_results(tcp, std::nullopt), t, u);
	CHECK(t.has_value());
	CHECK(!u.has_value());

	// Child killed by the watchdog: nothing or a torn line on the pipe.
	probe::decode_results("", t, u);
	CHECK(!t && !u);
	probe::decode_results("tcp 87.5 - -\nudp 19", t, u);
	CHECK(t.has_value());
	CHECK(!u.has_value());
	probe::decode_results("tcp garbage\n", t, u);
	CHECK(!t.has_value());
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
	test_select_rtt();
	test_result_codec();
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