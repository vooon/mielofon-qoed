/* obserwrt - log module mock
 *
 * Replaces the OpenWrt log module. INFO / WARN / ERR / NOTE calls are captured
 * into global.MOCK_LOG (instead of syslog) so stderr stays deterministic and
 * call sites can be asserted.
 */
"use strict";

if (!exists(global, 'MOCK_LOG'))
	global.MOCK_LOG = [];

function record(level, fmt, ...args) {
	push(global.MOCK_LOG, level + ': ' + sprintf(fmt, ...args));
}

function INFO(...args)  { record('INFO', ...args); }
function WARN(...args)  { record('WARN', ...args); }
function ERR(...args)   { record('ERR', ...args); }
function NOTE(...args)  { return null; }

function ulog_open() { return null; }
function ulog(priority, format, ...args) { record('ULOG', format, ...args); }
function ulog_threshold() { return null; }

export const ULOG_SYSLOG = 0;
export const LOG_DAEMON  = 0;
export const LOG_DEBUG   = 0;
export const LOG_INFO    = 0;
export { INFO, WARN, ERR, NOTE, ulog_open, ulog };
