// Aube's Bun security scanner bridge.
//
// Spawned by aube via `node --experimental-strip-types -e '<this file>'`,
// with `AUBE_SCANNER_SPEC` and `AUBE_BRIDGE_DIR` set in env:
//
// - `AUBE_SCANNER_SPEC`: npm package name or path of the user's
//   scanner module (the same string they'd put in Bun's bunfig).
// - `AUBE_BRIDGE_DIR`: a temp dir aube prepopulates with
//   `bun_shim.mjs` and `loader_hook.mjs`. We register the hook
//   from there so the user's `import Bun from 'bun'` resolves
//   to the shim.
//
// The bridge reads `{ packages: [{name, version}] }` on stdin,
// calls `scanner.scan(payload)`, and writes the resulting
// `Advisory[]` (or `{ advisories: [...] }`) on stdout as JSON.

import { register } from 'node:module';
import { pathToFileURL } from 'node:url';
import { resolve as pathResolve, join } from 'node:path';

const spec = process.env.AUBE_SCANNER_SPEC;
const bridgeDir = process.env.AUBE_BRIDGE_DIR;
if (!spec) {
  process.stderr.write('AUBE_SCANNER_SPEC env not set\n');
  process.exit(2);
}
if (!bridgeDir) {
  process.stderr.write('AUBE_BRIDGE_DIR env not set\n');
  process.exit(2);
}

const shimPath = join(bridgeDir, 'bun_shim.mjs');
const hookPath = join(bridgeDir, 'loader_hook.mjs');

// Register the loader hook before the first user import. The
// hook intercepts `'bun'` and serves our shim. Pass the shim
// path via `data` so the hook (which runs in a worker thread)
// can `readFile` it on demand.
register(pathToFileURL(hookPath).href, {
  parentURL: import.meta.url,
  data: { shimPath },
});

// Eagerly load the shim so `globalThis.Bun` is populated before
// the user scanner runs. Scanners often reference `Bun.env` /
// `Bun.semver` as globals without importing them (e.g. when the
// type-only `import type Bun from 'bun'` is the only "import"
// in the source, which strips to nothing at runtime).
try {
  const shim = (await import('bun')).default;
  globalThis.Bun = shim;
} catch (e) {
  process.stderr.write(`failed to install Bun shim: ${e?.message ?? e}\n`);
  process.exit(2);
}

async function loadScanner(spec) {
  // Path-like specs (`./foo`, `/foo`, `C:\\foo`) → resolve to a
  // file URL so dynamic import sees an unambiguous target.
  if (
    spec.startsWith('.') ||
    spec.startsWith('/') ||
    /^[a-zA-Z]:[/\\]/.test(spec)
  ) {
    const abs = pathResolve(process.cwd(), spec);
    return import(pathToFileURL(abs).href);
  }
  // Bare npm package name → node resolves from cwd's
  // `node_modules`. Try ESM first; fall back to CJS via
  // `createRequire` for older scanner packages.
  try {
    return await import(spec);
  } catch (e) {
    try {
      const { createRequire } = await import('node:module');
      const require = createRequire(pathResolve(process.cwd(), 'package.json'));
      return require(spec);
    } catch {
      throw e;
    }
  }
}

let mod;
try {
  mod = await loadScanner(spec);
} catch (e) {
  process.stderr.write(`failed to load scanner ${spec}: ${e?.message ?? e}\n`);
  process.exit(3);
}

// Accept the canonical Bun shape (`export const scanner = {...}`)
// plus common variants — `export default scanner`, default-
// export-is-the-scanner, or default-export-has-a-scanner-
// property. Keeps the bridge from breaking when scanner authors
// rearrange their entry points.
const scanner = mod?.scanner ?? mod?.default?.scanner ?? mod?.default ?? mod;
if (!scanner || typeof scanner.scan !== 'function') {
  process.stderr.write(`scanner ${spec} does not export a 'scan' function\n`);
  process.exit(4);
}

let buf = '';
for await (const chunk of process.stdin) buf += chunk;
const payload = JSON.parse(buf);

let result;
try {
  result = await scanner.scan(payload);
} catch (e) {
  process.stderr.write(`scanner.scan() threw: ${e?.message ?? e}\n`);
  process.exit(5);
}

const advisories = Array.isArray(result)
  ? result
  : (result?.advisories ?? []);
process.stdout.write(JSON.stringify(advisories));
