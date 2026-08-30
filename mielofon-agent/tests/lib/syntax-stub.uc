/* mielofon-agent - ucode `-c` syntax-check stub header
 *
 * Loaded via `sed`/concatenation in CI before compiling each agent module to
 * bytecode (ucode -c, never executed). ucode resolves imports at compile time,
 * but the OpenWrt/site modules (uclient, uci, ubus, uloop, log, fs) are not
 * part of the upstream ucode tree, so the import lines are stripped first and
 * this header supplies the referenced names for the compile-only pass. The
 * module's own `export function`s are not stubbed here — after the export
 * prefix is removed they are real declarations in the module itself.
 */
const uc = {
	new: function() {},
};
const log = {
	NOTE: function() {}, INFO: function() {}, WARN: function() {}, ERR: function() {},
};
const uloop = {
	run: function() {}, timer: function() {}, interval: function() {}, process: function() {},
};
function cursor() {}
function connect() {}
function readfile() {}
function unlink() {}
function new_client() {}
function create() {}
function post_json() {}
function apply_cost() {}
function run_cmd() {}
function parse_ping() {}
function parse_transaction_rate() {}
function parse_iperf3() {}
function util_mbps() {}