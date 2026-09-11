/*
 * mielofon-probe - interface byte-counter sampler (counters.cpp)
 *
 * Reads /sys/class/net/<iface>/statistics/{rx,tx}_bytes (same source the ucode
 * agent used), keeps the prior reading per interface, and derives the rate in
 * Mbps over the sampling interval. Two samples are required for a rate, so the
 * first call per interface returns no value.
 */

#include "counters.hpp"

#include <cstdio>
#include <cstring>
#include <string>

namespace probe
{

void CounterSampler::reset()
{
	prev_.clear();
}

bool CounterSampler::read_counters(const std::string &iface, uint64_t &rx,
                                   uint64_t &tx)
{
	const std::string base = "/sys/class/net/" + iface + "/statistics/";

	FILE *f = fopen((base + "rx_bytes").c_str(), "r");
	if (f == nullptr)
		return false;
	if (fscanf(f, "%llu", reinterpret_cast<unsigned long long *>(&rx)) != 1) {
		fclose(f);
		return false;
	}
	fclose(f);

	f = fopen((base + "tx_bytes").c_str(), "r");
	if (f == nullptr)
		return false;
	if (fscanf(f, "%llu", reinterpret_cast<unsigned long long *>(&tx)) != 1) {
		fclose(f);
		return false;
	}
	fclose(f);

	return true;
}

std::unordered_map<std::string, double> CounterSampler::sample(
    const std::vector<std::string> &ifaces, double interval_secs)
{
	std::unordered_map<std::string, double> out;
	if (interval_secs <= 0.0)
		return out;

	for (const std::string &iface : ifaces) {
		uint64_t rx = 0, tx = 0;
		if (!read_counters(iface, rx, tx))
			continue;

		auto it = prev_.find(iface);
		if (it == prev_.end()) {
			// First observation: store, no rate yet.
			prev_[iface] = Counters{rx, tx, true};
			continue;
		}

		Counters &p = it->second;
		uint64_t drx = rx >= p.rx ? rx - p.rx : 0;
		uint64_t dtx = tx >= p.tx ? tx - p.tx : 0;
		// bits / interval_secs -> Mbps = bytes*8/1e6/interval.
		double mbps = static_cast<double>((drx + dtx) * 8) /
		              (1000000.0 * interval_secs);
		p.rx = rx;
		p.tx = tx;
		p.valid = true;

		out[iface] = mbps;
	}
	return out;
}

} // namespace probe