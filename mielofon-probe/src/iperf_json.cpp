/*
 * mielofon-probe - iperf3 JSON result parser (iperf_json.cpp)
 *
 * Uses json-c to walk the iperf3 JSON output. Values are read defensively:
 * a missing/malformed field yields an unset optional, never a fabricated 0.
 */

#include "iperf_json.hpp"

#include <json-c/json.h>

#include <cstring>

namespace probe
{

namespace
{

/// Read a double from `obj` at `key`, or nullopt when absent/not a number.
std::optional<double> get_double(struct json_object *obj, const char *key)
{
	struct json_object *v = nullptr;
	if (!json_object_object_get_ex(obj, key, &v))
		return std::nullopt;
	if (json_object_get_type(v) != json_type_double &&
	    json_object_get_type(v) != json_type_int)
		return std::nullopt;
	return json_object_get_double(v);
}

/// Pull a sub-object by key, if present.
struct json_object *get_obj(struct json_object *obj, const char *key)
{
	struct json_object *v = nullptr;
	if (!json_object_object_get_ex(obj, key, &v))
		return nullptr;
	return v;
}

} // namespace

IperfResult parse_iperf_json(const std::string &json)
{
	IperfResult out;

	struct json_object *root = json_tokener_parse(json.c_str());
	if (root == nullptr)
		return out;

	struct json_object *end = get_obj(root, "end");
	if (end != nullptr) {
		// Bits/second at the `end` summary. Prefer `sum_received` (accurate
		// bitrate for both directions) falling back to `sum`.
		struct json_object *sum = get_obj(end, "sum_received");
		if (sum == nullptr)
			sum = get_obj(end, "sum");
		if (sum != nullptr) {
			auto bps = get_double(sum, "bits_per_second");
			if (bps)
				out.mbps = *bps / 1e6;
			auto jit = get_double(sum, "jitter_ms");
			if (jit)
				out.jitter_ms = *jit;
			auto loss = get_double(sum, "loss_pct");
			if (loss)
				out.loss_pct = *loss;
		}
	}

	json_object_put(root);
	return out;
}

} // namespace probe