/*
 * mielofon-probe - resident measurement probe daemon (main.cpp)
 *
 * A config-free C++23 daemon: the ucode agent pushes the link set and probe
 * parameters over ubus (`configure`), reads results back (`status`), and
 * triggers gated throughput tests (`throughput`). Only the Prometheus textfile
 * path and the log level come from the command line (procd).
 *
 * Measurement engine: `icmp.cpp` runs the continuous ICMP latency/jitter/loss
 * loop over the configured links; `status` returns its per-link snapshot.
 * Later stages add the interface-counter util sampler, the Prometheus textfile
 * writer and the libiperf3 TCP+UDP throughput path.
 */

#include <getopt.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <syslog.h>

// libubox typically lacks `extern "C"` guards; included from C++ the blobmsg/
// uloop functions would be C++-mangled and fail to link against the C libs.
extern "C" {
#include <libubox/blobmsg.h>
#include <libubox/uloop.h>
#include <libubox/utils.h>
#include <libubus.h>
}

#include <cstdio>
#include <map>
#include <memory>
#include <string>
#include <vector>

#include "icmp.hpp"
#include "metrics.hpp"

static const char *g_textfile = nullptr;
static int g_log_level = LOG_INFO;
static std::unique_ptr<probe::IcmpLoop> g_loop;

static const int kMetricsIntervalMs = 20000; // textfile rewrite cadence

/* Periodically rewrite the Prometheus textfile (atomic temp+rename). */
static struct uloop_timeout g_metrics_timer;

static void metrics_timer_cb(struct uloop_timeout *t)
{
	if (g_textfile != nullptr && g_loop != nullptr) {
		if (!probe::write_textfile(g_textfile,
		                           probe::render_textfile(g_loop->snapshot())))
			syslog(LOG_WARNING, "textfile write failed: %s", g_textfile);
	}
	uloop_timeout_set(t, kMetricsIntervalMs);
}

/* Map a syslog level name (emerg..debug) to its priority; default info. */
static int syslog_level_from_str(const char *name)
{
	static const std::map<std::string, int, std::less<>> levels = {
		{ "emerg", LOG_EMERG },   { "alert", LOG_ALERT },
		{ "crit", LOG_CRIT },     { "err", LOG_ERR },
		{ "warning", LOG_WARNING }, { "notice", LOG_NOTICE },
		{ "info", LOG_INFO },     { "debug", LOG_DEBUG },
	};
	auto it = levels.find(name);
	return it != levels.end() ? it->second : LOG_INFO;
}

/* ---- build info (injected at configure time) --------------------------- */

#ifndef MIELOFON_PROBE_VERSION
#define MIELOFON_PROBE_VERSION "dev"
#endif
#ifndef MIELOFON_PROBE_OS
#define MIELOFON_PROBE_OS "linux"
#endif
#ifndef MIELOFON_PROBE_ARCH
#define MIELOFON_PROBE_ARCH "unknown"
#endif

/* ---- ubus object `mielofon-probe` -------------------------------------- */

static int method_status(struct ubus_context *ctx, struct ubus_object *obj,
                         struct ubus_request_data *req, const char *method,
                         struct blob_attr *msg);
static int method_configure(struct ubus_context *ctx, struct ubus_object *obj,
                            struct ubus_request_data *req, const char *method,
                            struct blob_attr *msg);
static int method_throughput(struct ubus_context *ctx, struct ubus_object *obj,
                             struct ubus_request_data *req, const char *method,
                             struct blob_attr *msg);

enum {
	CONF_LINKS,
	CONF_QUIET_MAX,
	CONF_IPERF_PORT,
	CONF_UDP_RATE,
	CONF_PING_INTERVAL,
	__CONF_MAX,
};

static const struct blobmsg_policy conf_policy[__CONF_MAX] = {
	[CONF_LINKS] = { "links", BLOBMSG_TYPE_ARRAY },
	// Numeric params use BLOBMSG_TYPE_UNSPEC: a JSON integer literal arrives
	// as INT32 and a float as DOUBLE, and blobmsg_parse would skip a policy
	// type mismatch. Read them with the type-agnostic helpers below.
	[CONF_QUIET_MAX] = { "quiet_max_mbps", BLOBMSG_TYPE_UNSPEC },
	[CONF_IPERF_PORT] = { "iperf_port", BLOBMSG_TYPE_UNSPEC },
	[CONF_UDP_RATE] = { "udp_rate_mbps", BLOBMSG_TYPE_UNSPEC },
	[CONF_PING_INTERVAL] = { "ping_interval", BLOBMSG_TYPE_UNSPEC },
};

enum {
	THR_IFACE,
	THR_DURATION,
	__THR_MAX,
};

static const struct blobmsg_policy thr_policy[__THR_MAX] = {
	[THR_IFACE] = { "interface", BLOBMSG_TYPE_STRING },
	[THR_DURATION] = { "duration", BLOBMSG_TYPE_INT32 },
};

static struct ubus_method probe_methods[] = {
	UBUS_METHOD_NOARG("status", method_status),
	UBUS_METHOD("configure", method_configure, conf_policy),
	UBUS_METHOD("throughput", method_throughput, thr_policy),
};

static struct ubus_object_type probe_object_type =
	UBUS_OBJECT_TYPE("mielofon-probe", probe_methods);

static struct ubus_object probe_object = {
	.name = "mielofon-probe",
	.type = &probe_object_type,
	.methods = probe_methods,
	.n_methods = ARRAY_SIZE(probe_methods),
};

/* Emit one Snapshot as a blobmsg object. */
static void blobmsg_add_snapshot(struct blob_buf *b, const probe::Snapshot &s)
{
	void *tab = blobmsg_open_table(b, s.interface.c_str());
	if (s.have_rtt)
		blobmsg_add_double(b, "rtt_ms", s.rtt_ms);
	if (s.have_jitter)
		blobmsg_add_double(b, "jitter_ms", s.jitter_ms);
	if (s.loss_pct >= 0.0)
		blobmsg_add_double(b, "loss_pct", s.loss_pct);
	blobmsg_add_double(b, "util_mbps", s.util_mbps);
	blobmsg_add_u64(b, "ts", s.ts);
	blobmsg_add_u64(b, "sent", s.sent);
	blobmsg_add_u64(b, "received", s.received);
	blobmsg_add_u64(b, "errors", s.errors);
	blobmsg_close_table(b, tab);
}

/* Read a numeric attr as a double regardless of wire type: JSON integer
 * literals arrive as INT32/INT64, floats as DOUBLE. */
static double blobmsg_get_num(struct blob_attr *attr)
{
	switch (blobmsg_type(attr)) {
	case BLOBMSG_TYPE_DOUBLE:
		return blobmsg_get_double(attr);
	case BLOBMSG_TYPE_INT64:
	case BLOBMSG_TYPE_INT32:
	case BLOBMSG_TYPE_INT16:
	case BLOBMSG_TYPE_INT8:
		return static_cast<double>(blobmsg_cast_s64(attr));
	default:
		return 0.0;
	}
}

/* Read a numeric attr as an integer regardless of wire type. */
static int blobmsg_get_num_int(struct blob_attr *attr)
{
	return static_cast<int>(blobmsg_get_num(attr));
}

/* `status` — return the current per-link measurement snapshot. */
static int method_status(struct ubus_context *ctx, struct ubus_object *obj,
                         struct ubus_request_data *req, const char *method,
                         struct blob_attr *msg)
{
	(void)obj;
	(void)method;
	(void)msg;

	if (g_loop == nullptr)
		return UBUS_STATUS_NOT_FOUND;

	struct blob_buf b = {};
	blob_buf_init(&b, 0);
	for (const auto &s : g_loop->snapshot())
		blobmsg_add_snapshot(&b, s);
	ubus_send_reply(ctx, req, b.head);
	blob_buf_free(&b);
	return 0;
}

/* `configure` — replace the managed links + probe params (pushed by agent). */
static int method_configure(struct ubus_context *ctx, struct ubus_object *obj,
                            struct ubus_request_data *req, const char *method,
                            struct blob_attr *msg)
{
	(void)obj;
	(void)method;

	// Parse params (all optional, default kept).
	probe::Params params;
	if (g_loop)
		params = g_loop->params();
	struct blob_attr *tb[__CONF_MAX] = {};
	blobmsg_parse(conf_policy, ARRAY_SIZE(conf_policy), tb, blob_data(msg),
	              blob_len(msg));
	if (tb[CONF_QUIET_MAX])
		params.quiet_max_mbps = blobmsg_get_num(tb[CONF_QUIET_MAX]);
	if (tb[CONF_IPERF_PORT])
		params.iperf_port = blobmsg_get_num_int(tb[CONF_IPERF_PORT]);
	if (tb[CONF_UDP_RATE])
		params.udp_rate_mbps = blobmsg_get_num(tb[CONF_UDP_RATE]);
	if (tb[CONF_PING_INTERVAL])
		params.ping_interval = blobmsg_get_num(tb[CONF_PING_INTERVAL]);

	// Parse the links array.
	std::vector<probe::Link> links;
	static const struct blobmsg_policy link_policy[4] = {
		{ "interface", BLOBMSG_TYPE_STRING },
		{ "target", BLOBMSG_TYPE_STRING },
		{ "source", BLOBMSG_TYPE_STRING },
		{ "window", BLOBMSG_TYPE_INT32 },
	};
	if (tb[CONF_LINKS]) {
		struct blob_attr *link;
		int rem;
		blobmsg_for_each_attr(link, tb[CONF_LINKS], rem) {
			probe::Link l;
			struct blob_attr *tb2[4] = {};
			blobmsg_parse(link_policy, 4, tb2, blobmsg_data(link),
			              blobmsg_len(link));
			if (tb2[0])
				l.interface = blobmsg_get_string(tb2[0]);
			if (tb2[1])
				l.target = blobmsg_get_string(tb2[1]);
			if (tb2[2])
				l.source = blobmsg_get_string(tb2[2]);
			if (tb2[3])
				l.window = static_cast<size_t>(blobmsg_get_u32(tb2[3]));
			links.push_back(std::move(l));
		}
	}

	if (g_loop)
		g_loop->configure(std::move(links), params);

	syslog(LOG_NOTICE, "configure: %d links, quiet=%.1f iperf_port=%d",
	       (int)links.size(), params.quiet_max_mbps, params.iperf_port);

	struct blob_buf b = {};
	blob_buf_init(&b, 0);
	blobmsg_add_u32(&b, "ok", 1);
	ubus_send_reply(ctx, req, b.head);
	blob_buf_free(&b);
	return 0;
}

/* `throughput` — run a gated TCP+UDP test on a link (quiet gate + fence). */
static int method_throughput(struct ubus_context *ctx, struct ubus_object *obj,
                             struct ubus_request_data *req, const char *method,
                             struct blob_attr *msg)
{
	(void)obj;
	(void)method;

	if (g_loop == nullptr)
		return UBUS_STATUS_NOT_FOUND;

	struct blob_attr *tb[__THR_MAX] = {};
	blobmsg_parse(thr_policy, ARRAY_SIZE(thr_policy), tb, blob_data(msg),
	              blob_len(msg));
	if (tb[THR_IFACE] == nullptr)
		return UBUS_STATUS_INVALID_ARGUMENT;

	std::string iface = blobmsg_get_string(tb[THR_IFACE]);
	int duration = 4;
	if (tb[THR_DURATION])
		duration = blobmsg_get_u32(tb[THR_DURATION]);

	// Runs TCP then UDP in-process; blocks for ~2*duration seconds. The
	// controller holds the fence, so only one such test runs cluster-wide.
	probe::ThroughputOutcome out = g_loop->run_throughput(iface, duration);

	struct blob_buf b = {};
	blob_buf_init(&b, 0);
	blobmsg_add_string(&b, "interface", iface.c_str());
	blobmsg_add_u8(&b, "busy", out.busy ? 1 : 0);
	blobmsg_add_double(&b, "util_mbps", out.util_mbps);
	if (out.tcp && out.tcp->mbps)
		blobmsg_add_double(&b, "tcp_mbps", *out.tcp->mbps);
	if (out.udp) {
		if (out.udp->mbps)
			blobmsg_add_double(&b, "udp_mbps", *out.udp->mbps);
		if (out.udp->udp_jitter_ms)
			blobmsg_add_double(&b, "udp_jitter_ms", *out.udp->udp_jitter_ms);
		if (out.udp->udp_loss_pct)
			blobmsg_add_double(&b, "udp_loss_pct", *out.udp->udp_loss_pct);
	}
	ubus_send_reply(ctx, req, b.head);
	blob_buf_free(&b);
	return 0;
}

/* ---- CLI ---------------------------------------------------------------- */

static void usage(FILE *out)
{
	fprintf(out,
	        "usage: mielofon-probe [options]\n"
	        "\n"
	        "  --textfile PATH   Prometheus textfile to write (default: none)\n"
	        "  --log-level LVL   syslog level: emerg..debug (default: info)\n"
	        "  --version         print version and exit\n"
	        "  --help            this help\n");
}

/* ---- main --------------------------------------------------------------- */

int main(int argc, char **argv)
{
	static const struct option longopts[] = {
		{ "textfile", required_argument, nullptr, 't' },
		{ "log-level", required_argument, nullptr, 'l' },
		{ "version", no_argument, nullptr, 'V' },
		{ "help", no_argument, nullptr, 'h' },
		{ nullptr, 0, nullptr, 0 },
	};

	int c;
	while ((c = getopt_long(argc, argv, "t:l:Vh", longopts, nullptr)) != -1) {
		switch (c) {
		case 't':
			g_textfile = optarg;
			break;
		case 'l':
			g_log_level = syslog_level_from_str(optarg);
			break;
		case 'V':
			printf("mielofon-probe %s (%s/%s)\n", MIELOFON_PROBE_VERSION,
			       MIELOFON_PROBE_OS, MIELOFON_PROBE_ARCH);
			return 0;
		case 'h':
			usage(stdout);
			return 0;
		default:
			usage(stderr);
			return 2;
		}
	}

	openlog("mielofon-probe", LOG_PID | LOG_NDELAY, LOG_DAEMON);
	setlogmask(LOG_UPTO(static_cast<int>(g_log_level)));
	syslog(LOG_INFO, "mielofon-probe %s starting (%s/%s)",
	       MIELOFON_PROBE_VERSION, MIELOFON_PROBE_OS, MIELOFON_PROBE_ARCH);
	if (g_textfile != nullptr)
		syslog(LOG_INFO, "textfile output: %s", g_textfile);

	struct ubus_context *ctx = ubus_connect(nullptr);
	if (ctx == nullptr) {
		syslog(LOG_ERR, "failed to connect to ubus");
		return 1;
	}
	ubus_add_uloop(ctx);

	// The measurement engine. Its tick timer is armed on the first
	// `configure` (a link set), which only happens during uloop_run() when
	// uloop is initialised.
	g_loop = std::make_unique<probe::IcmpLoop>();

	int rc = ubus_add_object(ctx, &probe_object);
	if (rc != 0) {
		syslog(LOG_ERR, "failed to add ubus object: %s", ubus_strerror(rc));
		ubus_free(ctx);
		return 1;
	}
	syslog(LOG_INFO, "registered ubus object 'mielofon-probe'");

	// Periodically rewrite the Prometheus textfile when one is configured.
	if (g_textfile != nullptr) {
		g_metrics_timer.cb = metrics_timer_cb;
		uloop_timeout_set(&g_metrics_timer, kMetricsIntervalMs);
	}

	signal(SIGPIPE, SIG_IGN);

	uloop_run();

	ubus_remove_object(ctx, &probe_object);
	ubus_free(ctx);
	uloop_done();
	g_loop.reset();
	closelog();
	return 0;
}