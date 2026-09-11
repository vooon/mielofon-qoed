/*
 * mielofon-probe - resident measurement probe daemon (main.cpp)
 *
 * A config-free C++23 daemon: the ucode agent pushes the link set and probe
 * parameters over ubus (`configure`), reads results back (`status`), and
 * triggers gated throughput tests (`throughput`). Only the Prometheus textfile
 * path and the log level come from the command line (procd).
 *
 * Skeleton: getopt CLI + ubus object registration + uloop. Later stages add
 * the ICMP latency/jitter/loss loop, the interface-counter sampler, the
 * Prometheus textfile writer and the libiperf3 TCP+UDP throughput path.
 */

#include <getopt.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <syslog.h>

#include <libubox/blobmsg.h>
#include <libubox/uloop.h>
#include <libubox/utils.h>
#include <libubus.h>

#include <cstdio>
#include <string>

static const char *g_textfile = nullptr;
static int g_log_level = LOG_INFO;

/* Map a syslog level name (emerg..debug) to its priority; default info. */
static int syslog_level_from_str(const char *name)
{
	struct {
		const char *name;
		int level;
	} levels[] = {
		{ "emerg", LOG_EMERG }, { "alert", LOG_ALERT },
		{ "crit", LOG_CRIT },   { "err", LOG_ERR },
		{ "warning", LOG_WARNING }, { "notice", LOG_NOTICE },
		{ "info", LOG_INFO },   { "debug", LOG_DEBUG },
	};
	for (const auto &l : levels)
		if (strcmp(name, l.name) == 0)
			return l.level;
	return LOG_INFO;
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

static struct ubus_method probe_methods[] = {
	UBUS_METHOD_NOARG("status", method_status),
};

static struct ubus_object_type probe_object_type =
	UBUS_OBJECT_TYPE("mielofon-probe", probe_methods);

static struct ubus_object probe_object = {
	.name = "mielofon-probe",
	.type = &probe_object_type,
	.methods = probe_methods,
	.n_methods = ARRAY_SIZE(probe_methods),
};

/* Stub: later stages return the per-link measurement window. For now a plain
 * `{}` reply so the agent->daemon ubus round-trip can be smoke-tested under
 * the jail during the scaffold stage. */
static int method_status(struct ubus_context *ctx, struct ubus_object *obj,
                         struct ubus_request_data *req, const char *method,
                         struct blob_attr *msg)
{
	(void)obj;
	(void)method;
	(void)msg;

	struct blob_buf b = {};
	blob_buf_init(&b, 0);
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

	int rc = ubus_add_object(ctx, &probe_object);
	if (rc != 0) {
		syslog(LOG_ERR, "failed to add ubus object: %s", ubus_strerror(rc));
		ubus_free(ctx);
		return 1;
	}
	syslog(LOG_INFO, "registered ubus object 'mielofon-probe'");

	signal(SIGPIPE, SIG_IGN);

	uloop_run();

	ubus_remove_object(ctx, &probe_object);
	ubus_free(ctx);
	uloop_done();
	closelog();
	return 0;
}