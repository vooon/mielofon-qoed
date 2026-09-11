/*
 * mielofon-probe - Prometheus textfile writer (metrics.hpp)
 *
 * Writes a node-exporter textfile with per-link gauges/counters. Rewritten
 * atomically (temp + rename) on a cadence from the CLI-provided `--textfile`
 * path. Mirrors obserwrt's metrics.cpp: no HTTP server; doubles supported
 * (rtt/jitter/loss/util are fractional, unlike obserwrt's integer-only
 * exposition).
 */

#pragma once

#include "snapshot.hpp"

#include <string>
#include <vector>

namespace probe
{

/// Emit the full textfile body for the given snapshots + build info.
std::string render_textfile(const std::vector<Snapshot> &snapshots);

/// Rewrite `path` atomically (temp + rename). Returns false on error.
bool write_textfile(const std::string &path, const std::string &body);

} // namespace probe