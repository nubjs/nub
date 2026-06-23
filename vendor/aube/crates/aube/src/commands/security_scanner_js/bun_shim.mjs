// Aube's virtual `bun` module. Loaded via a module-loader hook
// when a scanner does `import Bun from 'bun'`. Also installed on
// `globalThis.Bun` (by the bridge runner) so scanners that
// reference the global without importing it still work.
//
// Scope: just enough of Bun's runtime API surface to run public
// scanner packages. Scanners that reach beyond this — file
// watchers, `Bun.spawn`, `Bun.password`, Bun's web framework —
// will throw a TypeError at runtime; the bridge surfaces that
// as `WARN_AUBE_SECURITY_SCANNER_FAILED` and the install fails
// open. Operators can fix by switching scanners or running Bun
// itself for those installs.
//
// Implemented APIs:
//
// - `Bun.env`                   → `process.env`
// - `Bun.file(path)`            → BunFile with .exists() / .text() / .json() / .arrayBuffer()
// - `Bun.semver.satisfies(v,r)` → delegates to project-installed `semver` package;
//                                 falls back to exact-equality with a one-time warning if
//                                 `semver` isn't resolvable.
// - `Bun.write(path, data)`     → writes file (used by some auth-cache scanners)

import { readFile, writeFile, access } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { resolve as pathResolve } from 'node:path';

// Try to load the project's `semver` package once, lazily, the
// first time `Bun.semver.satisfies` is called. Most projects
// have semver installed transitively via npm-tooling deps, so
// this resolves in practice; if it doesn't, we fall back to a
// naive impl and emit a single stderr warning so the operator
// can install `semver` explicitly.
let semverImpl = null;
let semverWarned = false;
function loadSemver() {
  if (semverImpl !== null) return semverImpl;
  try {
    const require = createRequire(pathResolve(process.cwd(), 'package.json'));
    semverImpl = require('semver');
  } catch {
    semverImpl = false; // sentinel for "tried and failed"
  }
  return semverImpl;
}

class BunFile {
  constructor(path) {
    this.path = path;
  }
  async exists() {
    try {
      await access(this.path);
      return true;
    } catch {
      return false;
    }
  }
  async text() {
    return readFile(this.path, 'utf8');
  }
  async json() {
    return JSON.parse(await this.text());
  }
  async arrayBuffer() {
    const buf = await readFile(this.path);
    return buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
  }
  async bytes() {
    return new Uint8Array(await readFile(this.path));
  }
}

function naiveSatisfies(version, range) {
  if (range === '*' || range === '' || range === 'latest') return true;
  // Trim common prefixes so `^1.2.3` against `1.2.3` reads as a match
  // in the trivial case. Real ranges (`>=1.2 <2`, unions, hyphens) need
  // real semver; this is a placeholder that warns once and degrades
  // gracefully rather than crashing.
  const trimmed = range.replace(/^[\^~>=<]+\s*/, '').trim();
  return version === trimmed;
}

const Bun = {
  env: process.env,
  file(path) {
    return new BunFile(path);
  },
  async write(path, data) {
    let payload;
    if (typeof data === 'string') {
      payload = data;
    } else if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
      payload = Buffer.from(data instanceof ArrayBuffer ? new Uint8Array(data) : data);
    } else if (data && typeof data.text === 'function') {
      // BunFile-like
      payload = await data.text();
    } else {
      payload = JSON.stringify(data);
    }
    await writeFile(path, payload);
    return typeof payload === 'string' ? Buffer.byteLength(payload) : payload.length;
  },
  semver: {
    satisfies(version, range) {
      const lib = loadSemver();
      if (lib && typeof lib.satisfies === 'function') {
        return lib.satisfies(version, range);
      }
      if (!semverWarned) {
        process.stderr.write(
          "aube: scanner called Bun.semver.satisfies but the project has no `semver` package installed; falling back to exact-equality comparison. Install `semver` as a dev dep for full Bun.semver compatibility.\n",
        );
        semverWarned = true;
      }
      return naiveSatisfies(version, range);
    },
  },
};

// Install on the global *if not already set* — lets the bridge
// runner set this once before the user scanner loads, and avoids
// double-installing when the user scanner does both
// `import Bun from 'bun'` and references `globalThis.Bun`.
if (typeof globalThis.Bun === 'undefined') {
  globalThis.Bun = Bun;
}

export default Bun;
export const env = Bun.env;
export const file = Bun.file;
export const semver = Bun.semver;
export const write = Bun.write;
