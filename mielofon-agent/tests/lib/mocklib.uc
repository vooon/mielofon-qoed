/* obserwrt - unit-test mock library (mocklib.uc)
 *
 * Loaded with `ucode -l mocklib`, this module makes ucode resolve
 * `import ... from '<mod>'` to the `.uc` fixtures under tests/lib/mocklib/
 * (e.g. `import * as socket from 'socket'` -> mocklib/socket.uc), so tests can
 * stub out the OS-facing modules without a real OpenWrt target.
 *
 * Default searches REQUIRE_SEARCH_PATH for a `*.uc` pattern, derives the
 * mocklib directory from it and prepends `mocklib/*.uc`. Data fixtures (uci
 * JSON, etc.) are looked up through MOCK_SEARCH_PATH (set via -D, defaulting to
 * the test dir/files + ./tests/mocks).
 */

/* strict mode compliance: ensure that globals are defined */
if (!exists(global, 'REQUIRE_SEARCH_PATH'))
	global.MOCK_SEARCH_PATH = null;

if (!exists(global, 'MOCK_SEARCH_PATH'))
	global.MOCK_SEARCH_PATH = null;

if (!exists(global, 'TRACE_CALLS'))
	global.TRACE_CALLS = null;

let _fs = require("fs");

/* Force reloading fs module on next require */
delete global.modules.fs;

let _log = (level, fmt, ...args) => {
	let color, prefix;

	switch (level) {
	case 'info':
		color = 34;
		prefix = '!';
		break;

	case 'warn':
		color = 33;
		prefix = 'W';
		break;

	case 'error':
		color = 31;
		prefix = 'E';
		break;

	default:
		color = 0;
		prefix = 'I';
	}

	let f = sprintf("\u001b[%d;1m[%s] %s\u001b[0m", color, prefix, fmt);
	warn(replace(sprintf(f, ...args), "\n", "\n    "), "\n");
};

let read_data_file = (path) => {
	for (let dir in MOCK_SEARCH_PATH) {
		let fd = _fs.open(dir + '/' + path, "r");

		if (fd) {
			let data = fd.read("all");
			fd.close();

			return data;
		}
	}

	return null;
};

let read_json_file = (path) => {
	let data = read_data_file(path);

	if (data != null)  {
		try {
			return json(data);
		}
		catch (e) {
			_log('error', "Unable to parse JSON data in %s: %s", path, e);

			return NaN;
		}
	}

	return null;
};

let format_json = (data) => {
	let rv;

	let format_value = (value) => {
		switch (type(value)) {
		case "object":
			return sprintf("{ /* %d keys */ }", length(value));

		case "array":
			return sprintf("[ /* %d items */ ]", length(value));

		case "string":
			if (length(value) > 64)
				value = substr(value, 0, 64) + "...";

			/* fall through */
			return sprintf("%J", value);

		default:
			return sprintf("%J", value);
		}
	};

	switch (type(data)) {
	case "object":
		rv = "{";

		let k = sort(keys(data));

		for (let i, n in k)
			rv += sprintf("%s %J: %s", i ? "," : "", n, format_value(data[n]));

		rv += " }";
		break;

	case "array":
		rv = "[";

		for (let i, v in data)
			rv += (i ? "," : "") + " " + format_value(v);

		rv += " ]";
		break;

	default:
		rv = format_value(data);
	}

	return rv;
};

let trace_call = (ns, func, args) => {
	let msg = "[call] " +
		(ns ? ns + "." : "") +
		func;

	for (let k, v in args) {
		msg += ' ' + k + ' <';

		switch (type(v)) {
		case "array":
		case "object":
			msg += format_json(v);
			break;

		default:
			msg += v;
		}

		msg += '>';
	}

	switch (TRACE_CALLS) {
	case '1':
	case 'stdout':
		_fs.stdout.write(msg + "\n");
		break;

	case 'stderr':
		_fs.stderr.write(msg + "\n");
		break;
	}
};

/* Prepend mocklib to REQUIRE_SEARCH_PATH */
for (let pattern in REQUIRE_SEARCH_PATH) {
	/* Only consider ucode includes */
	if (!match(pattern, /\*\.uc$/))
		continue;

	let path = replace(pattern, /\*/, 'mocklib'),
	    stat = _fs.stat(path);

	if (!stat || stat.type != 'file')
		continue;

	if (type(MOCK_SEARCH_PATH) != 'array' || length(MOCK_SEARCH_PATH) == 0)
		MOCK_SEARCH_PATH = [ replace(path, /mocklib\.uc$/, '../mocks') ];

	unshift(REQUIRE_SEARCH_PATH, replace(path, /mocklib\.uc$/, 'mocklib/*.uc'));
	break;
}

if (type(MOCK_SEARCH_PATH) != 'array' || length(MOCK_SEARCH_PATH) == 0)
	MOCK_SEARCH_PATH = [ './mocks' ];

let _print = global.print;

/* Register global mocklib namespace */
global.mocklib = {
	require: function(module) {
		let path, res, ex;

		if (type(REQUIRE_SEARCH_PATH) == "array" && index(REQUIRE_SEARCH_PATH[0], 'mocklib/*.uc') != -1)
			path = shift(REQUIRE_SEARCH_PATH);

		try {
			res = require(module);
		}
		catch (e) {
			ex = e;
		}

		if (path)
			unshift(REQUIRE_SEARCH_PATH, path);

		if (ex)
			die(ex);

		return res;
	},

	I: (...args) => _log('info', ...args),
	N: (...args) => _log('notice', ...args),
	W: (...args) => _log('warn', ...args),
	E: (...args) => _log('error', ...args),

	format_json,
	read_data_file,
	read_json_file,
	trace_call
};

/* Override stdlib functions */
global.system = function(argv, timeout) {
	trace_call(null, "system", { command: argv, timeout });

	return 0;
};

global.time = function() {
	trace_call(null, "time");

	return 1615382640;
};

/* Deterministic monotonic clock so the exporter's monotonic->epoch offset and
 * the emitted flowStart/flowEnd millisecond fields are reproducible. Around
 * t ~ 1615382640 s the real CLOCK_MONOTONIC is a few seconds uptime; any fixed
 * epoch/clock pair keeps offset_ms (time()*1000 - mono_ms()) constant. */
global.clock = function(clockid) {
	trace_call(null, "clock", { clockid });

	return [ 1615383, 520000 ];
};

global.print = function(...args) {
	if (length(args) == 1 && type(args[0]) in ["array", "object"])
		printf("%s\n", format_json(args[0]));
	else
		_print(...args);
};

return global.mocklib;