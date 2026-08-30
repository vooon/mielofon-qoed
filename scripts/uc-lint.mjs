/*
 * uc-lint.mjs - lightweight ucode linter using node's ESM parser.
 *
 * ucode is ECMAScript-based, so node can syntax-check the `.uc` modules. This
 * also enforces a couple of ucode-specific rules that a plain node parse won't
 * catch:
 *   - `export function foo(){...}` must be terminated with `;`
 *     (this ucode parses the export as an expression statement)
 *   - array ops use the global form `push(arr, ...)`, not `arr.push(...)`
 *
 * Usage: node scripts/uc-lint.mjs
 */
import { readdirSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const dirs = [
	path.join(repoRoot, 'mielofon-agent/files/usr/share/ucode/mielofon'),
	path.join(repoRoot, 'mielofon-agent/tests/lib'),        /* mocklib + submodule mocks */
	path.join(repoRoot, 'mielofon-agent/tests/lib/mocklib'),
];

let failed = 0;

function err(file, msg) {
	console.error(`[uc-lint] ${file}: ${msg}`);
	failed = 1;
}

function warn(file, msg) {
	console.warn(`[uc-lint] ${file}: warning: ${msg}`);
}

for (const dir of dirs) {
const syntaxCheck = dir.endsWith('mielofon-agent/files/usr/share/ucode/mielofon');
for (const file of readdirSync(dir).filter((f) => f.endsWith('.uc'))) {
	const src = readFileSync(path.join(dir, file), 'utf8');

	// 1) ESM syntax via node's parser (cat f | node --input-type=module --check)
	// ucode's `function name;` / `export function name;` forward-declaration
	// (ucode docs §4.2) is not valid ECMAScript, so strip those lines first.
	const syntaxSrc = src.replace(/^(?:export\s+)?function\s+[A-Za-z_$][\w$]*\s*;\s*$/gm, '');
	const r = spawnSync(process.execPath, ['--input-type=module', '--check'], {
		input: syntaxSrc,
		encoding: 'utf8',
	});
	if (syntaxCheck && r.status !== 0)
		err(file, `syntax:\n${r.stderr}`);

	// 2) ucode-specific rules
	// 2a) `export function foo(){...}` must be terminated with `;`
	for (const m of src.matchAll(/export function\s+\w+\s*\([^)]*\)\s*\{/g)) {
		let i = m.index + m[0].length;   // just past the opening '{'
		let depth = 1;
		while (depth > 0 && i < src.length) {
			const c = src[i];
			if (c === '{') depth++;
			else if (c === '}') depth--;
			i++;
		}
		if (src[i] !== ';')
			err(file, `export function not terminated with ';': ${m[0].replace(/\s+/g, ' ')}`);
	}
	// 2b) arrays use the global form push(arr,...), not arr.push(...)
	for (const line of src.split('\n')) {
		if (/\.\s*(push|pop|map|filter|shift|unshift|join|slice)\s*\(/.test(line))
			err(file, `array method must be global (e.g. push(arr,...)): "${line.trim()}"`);
	}

	// 2c) strings are not []-indexable in ucode (use substr(s,i,1) / ord(s,i)).
	//     Static heuristic: a variable is "string-typed" if an initializer is a
	//     string literal / sprintf / substr / readfile / getenv / template
	//     literal, and it is never assigned an array/object. Only flag []-index
	//     on such string-typed vars, plus direct literal indexing.
	const stringVars = new Set();
	const arrayOrObjVars = new Set();
	const initRe = /^\s*(?:let|const)\s+([A-Za-z_$][\w$]*)\s*=\s*(.*)$/;
	for (const line of src.split('\n')) {
		const m = line.match(initRe);
		if (!m)
			continue;
		const [, id, rhs] = m;
		if (/^['"`]|^sprintf\s*\(|^substr\s*\(|^readfile\s*\(|^getenv\s*\)|^getenv\s*\(|^\`/.test(rhs))
			stringVars.add(id);
		if (/^\[|^\{|^ctx\.get\s*\(|^struct\.unpack\s*\(|^parse_key\s*\(|^parse_value\s*\(|^flows\s*\(|^filter\s*\(|^map\s*\(/.test(rhs))
			arrayOrObjVars.add(id);
	}
	src.split('\n').forEach((line, ln) => {
		if (line.match(/(?:'[^'\\]*(?:\\.[^'\\]*)*'|"[^"\\]*(?:\\.[^"\\]*)*")\s*\[/))
			err(file, `string literal is not []-indexable (line ${ln + 1})`);
		for (const id of stringVars) {
			if (arrayOrObjVars.has(id))
				continue;
			if (new RegExp(`\\b${id}\\s*\\[`).test(line))
				err(file, `'${id}' is a string and not []-indexable (line ${ln + 1}): "${line.trim()}"`);
		}
	});

	// 2d) forward-declared exports: warn that the target (OpenWrt) ucode does not
	//     support `export function name;`, and require a matching definition.
	const defnOf = (name) =>
		new RegExp(`export\\s+function\\s+${name}\\s*\\(`).test(src);
	for (const m of src.matchAll(/^export\s+function\s+([A-Za-z_$][\w$]*)\s*;\s*$/gm)) {
		warn(file, `forward-export declaration 'export function ${m[1]};' is not supported by the target (OpenWrt) ucode; prefer declare-before-use`);
		if (!defnOf(m[1]))
			err(file, `forward-declared export '${m[1]}' has no matching definition`);
	}
	// 2e) a plain `function name;` must not shadow a later `export function name`.
	for (const m of src.matchAll(/^function\s+([A-Za-z_$][\w$]*)\s*;\s*$/gm)) {
		if (new RegExp(`export\\s+function\\s+${m[1]}\\s*\\(`).test(src))
			err(file, `plain forward declaration 'function ${m[1]};' would shadow the exported '${m[1]}'`);
	}
}
}

if (failed)
	process.exit(1);
console.log('[uc-lint] all modules OK');
