/*
 * mielofon-probe - interface byte-counter sampler (counters.hpp)
 *
 * Reads rx/tx byte counters for a set of interfaces and computes an
 * instantaneous utilisation (Mbps) over a short sampling window. Backed by
 * /sys/class/net/<iface>/statistics/{rx,tx}_bytes, which requires no extra
 * library (the ucode agent reads the same files). The sampler keeps the
 * previous reading per interface and derives the rate on the second sample,
 * so the first call per interface yields no value (need two points).
 */

#pragma once

#include <cstdint>
#include <string>
#include <unordered_map>
#include <vector>

namespace probe
{

class CounterSampler
{
public:
	/// Reset the sampler (drop cached readings) — call when the link set
	/// changes so stale deltas are not carried across a reconfigure.
	void reset();

	/// Read the current counters for every interface in `ifaces`, computing
	/// the Mbps rate since the previous sample where a prior reading exists.
	/// Returns a map iface -> Mbps (only interfaces with a prior reading and a
	/// non-negative delta; a counter reset yields 0).
	std::unordered_map<std::string, double> sample(
	    const std::vector<std::string> &ifaces, double interval_secs);

private:
	struct Counters {
		uint64_t rx = 0;
		uint64_t tx = 0;
		bool valid = false;
	};

	/// Read rx+tx bytes for one interface; false if unreadable.
	static bool read_counters(const std::string &iface, uint64_t &rx,
	                          uint64_t &tx);

	std::unordered_map<std::string, Counters> prev_;
};

} // namespace probe