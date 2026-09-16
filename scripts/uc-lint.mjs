/*
 * uc-lint.mjs - ucode linter built on ucode-lsp (https://github.com/NoahBPeterson/ucode-lsp).
 *
 * Runs `ucode-lsp`'s CLI checker (type inference, flow analysis, null-safety,
 * unused imports, forward-declarations) against the agent's `.uc` modules,
 * gated to the oldest supported OpenWrt release via `--target-version`.
 *
 * All the agent's `.uc` files live in one directory
 * (`mielofon-agent/files/usr/share/ucode/mielofon`) and only import sibling
 * modules relatively (`./client.uc`, `./utils.uc`, ...) or native OpenWrt
 * modules (`uci`, `fs`, `log`, `ubus`). ucode-lsp resolves relative imports
 * from the importing file's directory, so there is no need to stage anything.
 *
 * ucode-lsp does NOT enforce every ucode-only rule, so we also keep the one
 * that real ucode hard-requires and that the LSP does not flag:
 *   - `export function foo(){...}` must be terminated with `;`
 *     (ucode parses the export as an expression statement)
 *
 * Usage: node scripts/uc-lint.mjs
 */
import { readdirSync, readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const modulesDir = path.join(repoRoot, 'mielofon-agent/files/usr/share/ucode/mielofon');
const TARGET_VERSION = '25.12';

let failed = 0;

function err(msg) {
	console.error(`[uc-lint] ${msg}`);
	failed = 1;
}

function findUcFiles(dir) {
	return readdirSync(dir)
		.filter((f) => f.endsWith('.uc'))
		.map((f) => path.join(dir, f));
}

// 1) ucode-lsp CLI checker (type/flow/null-safety/version-gated).
// Pin the ucode-lsp version for reproducibility (supply-chain safety in the
// pre-commit hook). Bump deliberately.
const UCODE_LSP_VERSION = '0.8.11';

function runUcodeLsp() {
	const r = spawnSync('npx', ['-y', `ucode-lsp@${UCODE_LSP_VERSION}`, modulesDir, '--target-version', TARGET_VERSION], {
		encoding: 'utf8',
		cwd: repoRoot,
	});
	if (r.status !== 0)
		err(`ucode-lsp (target ${TARGET_VERSION}) found issues:\n${r.stdout || r.stderr}`);
}

// 2) ucode-only rule: `export function foo(){...}` must be terminated with `;`.
function checkExportSemicolons(dir) {
	for (const file of findUcFiles(dir)) {
		const src = readFileSync(file, 'utf8');
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
				err(`${path.relative(repoRoot, file)}: export function not terminated with ';': ${m[0].replace(/\s+/g, ' ')}`);
		}
	}
}

runUcodeLsp();
checkExportSemicolons(modulesDir);

if (failed)
	process.exit(1);
console.log('[uc-lint] all modules OK');
