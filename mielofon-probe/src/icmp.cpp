/*
 * mielofon-probe - ICMP latency/jitter/loss loop (icmp.cpp)
 *
 * One raw socket per managed link (ICMPv6 for the mesh, ICMPv4 for
 * completeness). The loop is a single uloop_timeout on the ping cadence that
 * sends one echo per link per tick, plus one uloop_fd per link socket that
 * reads replies. A reply is matched by its (id, seq); RTT is measured on the
 * monotonic clock between send and the matching reply. Each send that is still
 * in flight when the next tick fires is a timeout (loss).
 *
 * `Peer` and the uloop glue are local to this TU; the single daemon instance
 * (`g_loop`) and an fd->Peer map let the C callbacks reach the right Peer.
 */

#include "icmp.hpp"

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/icmp6.h>
#include <netinet/ip_icmp.h>
#include <netinet/ip.h>
#include <sys/socket.h>
#include <syslog.h>
#include <time.h>
#include <unistd.h>

// libubox typically lacks `extern "C"` guards; included from C++ the uloop
// functions would be C++-mangled and fail to link against the C libs.
extern "C" {
#include <libubox/uloop.h>
}

#include <algorithm>
#include <cstring>
#include <map>
#include <memory>
#include <set>

namespace probe
{

namespace
{

struct Peer {
	Link link;

	int fd = -1;
	int family = AF_INET6;
	sockaddr_storage dst = {};
	socklen_t dst_len = 0;

	uint16_t id = 0;
	uint16_t seq = 0;
	bool inflight = false;
	uint16_t inflight_seq = 0;
	uint64_t inflight_us = 0;

	struct uloop_fd ufd = {};
};

uint64_t mono_us()
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return static_cast<uint64_t>(ts.tv_sec) * 1000000ULL +
	       static_cast<uint64_t>(ts.tv_nsec / 1000);
}

uint64_t wall_secs()
{
	return static_cast<uint64_t>(time(nullptr));
}

uint16_t checksum(const uint8_t *buf, size_t len)
{
	uint32_t sum = 0;
	while (len > 1) {
		sum += (static_cast<uint16_t>(buf[0]) << 8) | buf[1];
		buf += 2;
		len -= 2;
	}
	if (len)
		sum += static_cast<uint16_t>(buf[0]) << 8;
	while (sum >> 16)
		sum = (sum & 0xffff) + (sum >> 16);
	return static_cast<uint16_t>(~sum & 0xffff);
}

} // namespace

/* TU-local: the single instance + fd->Peer table + uloop glue. */
namespace
{

IcmpLoop *g_loop = nullptr;
std::map<int, Peer *> g_fd_peers;
struct uloop_timeout g_tick;

void peer_fd_cb(struct uloop_fd *u, unsigned int events)
{
	(void)events;
	if (g_loop == nullptr)
		return;
	auto it = g_fd_peers.find(u->fd);
	if (it != g_fd_peers.end())
		g_loop->recv_probe(it->second);
}

void tick_cb(struct uloop_timeout *t)
{
	(void)t;
	if (g_loop == nullptr)
		return;
	g_loop->tick();
	// Rearm with the current ping interval (may change via configure).
	int ms = static_cast<int>(g_loop->params().ping_interval * 1000.0);
	if (ms < 100)
		ms = 100;
	uloop_timeout_set(&g_tick, ms);
}

bool resolve_target(const Link &link, int &family, sockaddr_storage &dst,
                    socklen_t &dst_len)
{
	std::memset(&dst, 0, sizeof(dst));
	if (inet_pton(AF_INET6, link.target.c_str(),
	              &reinterpret_cast<sockaddr_in6 *>(&dst)->sin6_addr) == 1) {
		reinterpret_cast<sockaddr_in6 *>(&dst)->sin6_family = AF_INET6;
		dst_len = sizeof(sockaddr_in6);
		family = AF_INET6;
		return true;
	}
	if (inet_pton(AF_INET, link.target.c_str(),
	              &reinterpret_cast<sockaddr_in *>(&dst)->sin_addr) == 1) {
		reinterpret_cast<sockaddr_in *>(&dst)->sin_family = AF_INET;
		dst_len = sizeof(sockaddr_in);
		family = AF_INET;
		return true;
	}
	return false;
}

bool bind_source(int fd, int family, const std::string &source)
{
	if (source.empty())
		return true;
	sockaddr_storage ss = {};
	socklen_t len = 0;
	if (family == AF_INET6 &&
	    inet_pton(AF_INET6, source.c_str(),
	              &reinterpret_cast<sockaddr_in6 *>(&ss)->sin6_addr) == 1) {
		reinterpret_cast<sockaddr_in6 *>(&ss)->sin6_family = AF_INET6;
		len = sizeof(sockaddr_in6);
	} else if (inet_pton(AF_INET, source.c_str(),
	                     &reinterpret_cast<sockaddr_in *>(&ss)->sin_addr) == 1) {
		reinterpret_cast<sockaddr_in *>(&ss)->sin_family = AF_INET;
		len = sizeof(sockaddr_in);
	} else {
		return false;
	}
	return bind(fd, reinterpret_cast<sockaddr *>(&ss), len) == 0;
}

int open_socket(const Link &l, int &family, sockaddr_storage &dst,
                socklen_t &dst_len)
{
	if (!resolve_target(l, family, dst, dst_len)) {
		syslog(LOG_WARNING, "open %s: cannot resolve target %s",
		       l.interface.c_str(), l.target.c_str());
		return -1;
	}
	int proto = (family == AF_INET6) ? static_cast<int>(IPPROTO_ICMPV6)
                                 : static_cast<int>(IPPROTO_ICMP);
	int fd = socket(family, SOCK_RAW | SOCK_NONBLOCK | SOCK_CLOEXEC, proto);
	if (fd < 0) {
		syslog(LOG_WARNING, "open %s: socket(%s, RAW) failed: %s",
		       l.interface.c_str(),
		       family == AF_INET6 ? "INET6" : "INET",
		       std::strerror(errno));
		return -1;
	}
	if (family == AF_INET6) {
		int off = 2; // checksum field offset in icmp6_hdr
		if (setsockopt(fd, IPPROTO_IPV6, IPV6_CHECKSUM, &off, sizeof(off)) < 0) {
			syslog(LOG_WARNING, "open %s: IPV6_CHECKSUM failed: %s",
			       l.interface.c_str(), std::strerror(errno));
			close(fd);
			return -1;
		}
	}
	if (!bind_source(fd, family, l.source)) {
		syslog(LOG_WARNING, "open %s: bind %s failed: %s",
		       l.interface.c_str(), l.source.c_str(), std::strerror(errno));
		close(fd);
		return -1;
	}
	syslog(LOG_NOTICE, "probe %s -> %s up (fd %d)",
	       l.interface.c_str(), l.target.c_str(), fd);
	return fd;
}

} // namespace

IcmpLoop::IcmpLoop() : throughput_(params_)
{
	g_loop = this;
}

IcmpLoop::~IcmpLoop()
{
	g_loop = nullptr;
	for (auto &it : peers_) {
		auto *p = static_cast<Peer *>(it.second);
		if (p->fd >= 0) {
			uloop_fd_delete(&p->ufd);
			g_fd_peers.erase(p->fd);
			close(p->fd);
		}
		delete p;
	}
	peers_.clear();
}

size_t IcmpLoop::configure(std::vector<Link> links, Params params)
{
	params_ = params;
	// Drop cached counter deltas so a reconfigured link set doesn't carry a
	// stale utilisation across the change.
	counters_.reset();

	std::set<std::string> wanted;
	for (const Link &l : links)
		wanted.insert(l.interface);

	// Close removed peers.
	for (auto it = peers_.begin(); it != peers_.end();) {
		if (wanted.count(it->first)) {
			++it;
		} else {
			auto *p = static_cast<Peer *>(it->second);
			uloop_fd_delete(&p->ufd);
			g_fd_peers.erase(p->fd);
			close(p->fd);
			delete p;
			it = peers_.erase(it);
		}
	}

	size_t failed = 0;

	for (const Link &l : links) {
		auto it = peers_.find(l.interface);
		if (it == peers_.end()) {
			auto p = std::make_unique<Peer>();
			p->link = l;
			int fd = open_socket(l, p->family, p->dst, p->dst_len);
			if (fd < 0) {
				++failed;
				continue;
			}
			p->fd = fd;
			p->id = static_cast<uint16_t>(0x4D50U + peers_.size());
			p->ufd.fd = fd;
			p->ufd.cb = peer_fd_cb;
			uloop_fd_add(&p->ufd, ULOOP_READ);
			g_fd_peers[p->fd] = p.get();
			peers_.emplace(l.interface, p.release());
		} else {
			// Refresh target/source if changed (reopen the socket).
			auto *p = static_cast<Peer *>(it->second);
			if (p->link.target != l.target || p->link.source != l.source) {
				uloop_fd_delete(&p->ufd);
				g_fd_peers.erase(p->fd);
				close(p->fd);
				p->inflight = false;
				p->link = l;
				int fd = open_socket(l, p->family, p->dst, p->dst_len);
				if (fd < 0) {
					p->fd = -1;
					++failed;
					continue;
				}
				p->fd = fd;
				p->ufd.fd = fd;
				uloop_fd_add(&p->ufd, ULOOP_READ);
				g_fd_peers[p->fd] = p;
			}
		}
	}

	if (!peers_.empty()) {
		g_tick.cb = tick_cb;
		uloop_timeout_add(&g_tick);
	}
	return failed;
}

void IcmpLoop::tick()
{
	// Sample interface utilisation once per tick, before probing, and sync it
	// into each link's snapshot/util field.
	std::vector<std::string> ifaces;
	ifaces.reserve(peers_.size());
	for (const auto &it : peers_)
		ifaces.push_back(it.first);
	auto util = counters_.sample(ifaces, params_.ping_interval);

	for (const auto &it : peers_) {
		Peer *p = static_cast<Peer *>(it.second);
		auto u = util.find(it.first);
		if (u != util.end())
			p->link.util_mbps = u->second;
		send_probe(p);
	}
}

void IcmpLoop::send_probe(void *pvoid)
{
	Peer &p = *static_cast<Peer *>(pvoid);
	uint64_t now = mono_us();

	// An in-flight probe that never got a reply before this tick is loss.
	if (p.inflight) {
		p.link.record_timeout();
		p.inflight = false;
	}

	uint8_t buf[64];
	size_t n;
	uint16_t pseq = p.seq++;

	if (p.family == AF_INET6) {
		auto *h = reinterpret_cast<icmp6_hdr *>(buf);
		h->icmp6_type = ICMP6_ECHO_REQUEST;
		h->icmp6_code = 0;
		h->icmp6_cksum = 0;
		h->icmp6_id = htons(p.id);
		h->icmp6_seq = htons(pseq);
		std::memcpy(buf + sizeof(*h), &now, sizeof(now));
		n = sizeof(*h) + sizeof(now);
	} else {
		auto *h = reinterpret_cast<icmphdr *>(buf);
		h->type = ICMP_ECHO;
		h->code = 0;
		h->checksum = 0;
		h->un.echo.id = htons(p.id);
		h->un.echo.sequence = htons(pseq);
		std::memcpy(buf + sizeof(*h), &now, sizeof(now));
		n = sizeof(*h) + sizeof(now);
		h->checksum = checksum(buf, n);
	}

	ssize_t sent = sendto(p.fd, buf, n, 0,
	                      reinterpret_cast<sockaddr *>(&p.dst), p.dst_len);
	if (sent < 0) {
		p.link.errors++;
		return;
	}

	p.inflight = true;
	p.inflight_seq = pseq;
	p.inflight_us = now;
	p.link.record_sent(wall_secs());
}

void IcmpLoop::recv_probe(void *pvoid)
{
	Peer &p = *static_cast<Peer *>(pvoid);
	uint8_t buf[512];
	uint64_t now = mono_us();

	for (;;) {
		ssize_t r = recv(p.fd, buf, sizeof(buf), 0);
		if (r < 0)
			break;

		if (p.family == AF_INET) {
			if (static_cast<size_t>(r) < sizeof(iphdr))
				continue;
			const iphdr *ip = reinterpret_cast<const iphdr *>(buf);
			size_t off = static_cast<size_t>(ip->ihl) * 4;
			if (r < static_cast<ssize_t>(off + sizeof(icmphdr)))
				continue;
			const icmphdr *h = reinterpret_cast<const icmphdr *>(buf + off);
			if (h->type != ICMP_ECHOREPLY || ntohs(h->un.echo.id) != p.id)
				continue;
			if (!p.inflight || ntohs(h->un.echo.sequence) != p.inflight_seq)
				continue;
			uint64_t sent_us;
			if (off + sizeof(*h) + sizeof(sent_us) > static_cast<size_t>(r))
				continue;
			std::memcpy(&sent_us, buf + off + sizeof(*h), sizeof(sent_us));
			double rtt_ms = static_cast<double>(now - sent_us) / 1000.0;
			p.link.record_reply(rtt_ms, ip->ttl, wall_secs());
			p.inflight = false;
			break;
		} else {
			if (static_cast<size_t>(r) < sizeof(icmp6_hdr))
				continue;
			const icmp6_hdr *h = reinterpret_cast<const icmp6_hdr *>(buf);
			if (h->icmp6_type != ICMP6_ECHO_REPLY ||
			    ntohs(h->icmp6_id) != p.id)
				continue;
			if (!p.inflight || ntohs(h->icmp6_seq) != p.inflight_seq)
				continue;
			uint64_t sent_us;
			if (sizeof(*h) + sizeof(sent_us) > static_cast<size_t>(r))
				continue;
			std::memcpy(&sent_us, buf + sizeof(*h), sizeof(sent_us));
			double rtt_ms = static_cast<double>(now - sent_us) / 1000.0;
			p.link.record_reply(rtt_ms, 0, wall_secs());
			p.inflight = false;
			break;
		}
	}
}

std::vector<Snapshot> IcmpLoop::snapshot() const
{
	std::vector<Snapshot> out;
	for (const auto &it : peers_) {
		const Peer *p = static_cast<const Peer *>(it.second);
		const Link &l = p->link;
		Snapshot s;
		s.interface = l.interface;
		s.target = l.target;
		s.rtt_ms = 0.0;
		s.have_rtt = l.rtt(s.rtt_ms);
		s.jitter_ms = 0.0;
		s.have_jitter = l.jitter(s.jitter_ms);
		s.loss_pct = l.loss();
		s.util_mbps = l.util_mbps;
		s.ttl = l.last_ttl;
		s.ts = l.last_ts;
		s.sent = l.sent;
		s.received = l.received;
		s.errors = l.errors;
		out.push_back(std::move(s));
	}
	std::sort(out.begin(), out.end(),
	          [](const Snapshot &a, const Snapshot &b) {
		          return a.interface < b.interface;
	          });
	return out;
}

void *IcmpLoop::find_peer(const std::string &interface) const
{
	auto it = peers_.find(interface);
	return it == peers_.end() ? nullptr : it->second;
}

ThroughputOutcome IcmpLoop::run_throughput(const std::string &interface,
                                           int duration)
{
	ThroughputOutcome out;
	void *pv = find_peer(interface);
	if (pv == nullptr)
		return out; // unknown link: treat as no result

	Peer &p = *static_cast<Peer *>(pv);
	out.util_mbps = p.link.util_mbps;

	// Quiet gate: do not run a load test while the link is carrying traffic.
	if (out.util_mbps > params_.quiet_max_mbps) {
		out.busy = true;
		return out;
	}

	// TCP then UDP, in-process (serialised by the caller / fence).
	out.tcp = throughput_.run_tcp(p.link, duration);
	out.udp = throughput_.run_udp(p.link, duration);
	return out;
}

} // namespace probe