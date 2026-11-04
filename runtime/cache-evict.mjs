// Bounded-size eviction for the transpile cache (A16). Kept in its own module
// so it loads lazily — only when a sweep is actually due (see preload.mjs's
// throttled maybeSweepCache) — and so it can be tested in isolation.
//
// LRU by mtime: when the cache exceeds `maxBytes`, delete oldest-written entries
// until at or below `lowWater`. Entries are content-addressed, so evicting one
// only costs a re-transpile on next use. mtime reflects write time, not last
// read — true read-LRU would need an mtime touch on every cache hit, defeating
// the read-only fast path; oldest-written is the right, cheap proxy for a cache
// whose entries are written once per source version.

// NO STATIC `node:` IMPORTS. This module is reached by an UNAWAITED dynamic
// `import()` from the deferred sweep, so its import graph is in flight while the
// user's entry is still loading. On Node 20.10–20.18 a PENDING builtin job in the
// ESM loadCache makes a transpiled CommonJS entry's synchronous `require()` of the
// same builtin take the ESM translator's sync path (getModuleJobSync → runSync) and
// trip `assert(this.module instanceof ModuleWrap)` — ERR_INTERNAL_ASSERTION, ~25% of
// runs (#706). Fetching the builtins synchronously never enters the ESM loader, so
// no job exists for user code to collide with.
let readdirSync, rmdirSync, statSync, unlinkSync, join;
let _getBuiltin = null;

/**
 * Thread in a synchronous builtin getter. Required below Node 22.3, where
 * `process.getBuiltinModule` does not exist — transform-core hands over the same
 * getter it uses itself (createRequire-backed on the floor).
 */
export function setBuiltinGetter(fn) {
  _getBuiltin = fn;
}

function ensureBuiltins() {
  if (readdirSync) return;
  const get = _getBuiltin || ((id) => process.getBuiltinModule(id));
  ({ readdirSync, rmdirSync, statSync, unlinkSync } = get("node:fs"));
  ({ join } = get("node:path"));
}

// A valid cache entry is a 64-char lowercase-hex sha256 (see cacheKey in
// preload.mjs). Everything else — the `.sweep` sentinel, `*.tmp` in-flight
// writes from cacheSet — is skipped: never counted toward the cap, never
// evicted.
const ENTRY_RE = /^[0-9a-f]{64}$/;

// What one entry actually costs the filesystem. `stat.size` is the LOGICAL
// length, but the disk hands out whole blocks, and this cache is mostly small
// files — measured on a real 32k-entry cache: 45% of entries sat under one 4 KiB
// block, so 492.6 MB of logical bytes occupied 571.0 MB of disk, and a nominal
// 512 MiB cap silently permitted ~594 MB. `blocks` is POSIX (512-byte units);
// Windows does not report it, so fall back to the logical size there.
function diskBytes(s) {
  return typeof s.blocks === "number" && s.blocks > 0 ? s.blocks * 512 : s.size;
}

/**
 * Evict oldest entries from `dir` until total entry bytes ≤ `lowWater`, if they
 * exceed `maxBytes`. Sizes are the entries' real on-disk footprint, so the cap
 * bounds what the user's disk actually loses. Returns `{ scanned, deleted, freed }`.
 * Best-effort: a stat or unlink that fails (concurrent reader/evictor, Windows
 * open handle) is skipped, and the next sweep retries. Never throws.
 */
export function sweepCache(dir, maxBytes, lowWater = Math.floor(maxBytes * 0.75)) {
  ensureBuiltins();
  let names;
  try {
    names = readdirSync(dir);
  } catch {
    return { scanned: 0, deleted: 0, freed: 0 };
  }

  const entries = [];
  let total = 0;
  for (const name of names) {
    if (!ENTRY_RE.test(name)) continue;
    const path = join(dir, name);
    const s = statSync(path, { throwIfNoEntry: false });
    if (!s || !s.isFile()) continue;
    const size = diskBytes(s);
    entries.push({ path, size, mtime: s.mtimeMs });
    total += size;
  }

  if (total <= maxBytes) {
    return { scanned: entries.length, deleted: 0, freed: 0 };
  }

  // Oldest first; delete down to the low-water mark so we don't sweep again on
  // the very next write.
  entries.sort((a, b) => a.mtime - b.mtime);
  let deleted = 0;
  let freed = 0;
  for (const e of entries) {
    if (total <= lowWater) break;
    try {
      unlinkSync(e.path);
      total -= e.size;
      freed += e.size;
      deleted++;
    } catch {
      // Already removed by a concurrent sweep, or held open (Windows): skip.
      // `total` is unchanged so we keep trying to reach lowWater via other
      // entries; the loop is bounded by `entries`, so it still terminates.
    }
  }
  return { scanned: entries.length, deleted, freed };
}

/**
 * Evict from nub's OWN default V8 compile-cache dir. The layout there is Node's,
 * not ours — `<dir>/<version>-<arch>-<hash>-<uid>/<entry>` — so this walks one
 * level down, treats every regular file as an entry (V8 picks the names; there
 * is no pattern of ours to match), and prunes a version directory once eviction
 * has emptied it. Those version directories are the quiet half of the problem:
 * one accumulates per Node build ever run under nub and nothing removed it, so a
 * working machine had 96 of them totalling 6.9 GB across ~594k files.
 *
 * Deleting a live entry is safe: a missing or unreadable code-cache entry only
 * makes V8 recompile that module. Same oldest-first, delete-to-`lowWater` policy
 * and the same never-throws contract as `sweepCache`.
 */
export function sweepCompileCache(dir, maxBytes, lowWater = Math.floor(maxBytes * 0.75)) {
  ensureBuiltins();
  let versionDirs;
  try {
    versionDirs = readdirSync(dir, { withFileTypes: true }).filter((d) => d.isDirectory());
  } catch {
    return { scanned: 0, deleted: 0, freed: 0 };
  }

  const entries = [];
  let total = 0;
  for (const vd of versionDirs) {
    const vpath = join(dir, vd.name);
    let names;
    try {
      names = readdirSync(vpath);
    } catch {
      continue;
    }
    for (const name of names) {
      const path = join(vpath, name);
      const s = statSync(path, { throwIfNoEntry: false });
      if (!s || !s.isFile()) continue;
      const size = diskBytes(s);
      entries.push({ path, size, mtime: s.mtimeMs });
      total += size;
    }
  }

  if (total <= maxBytes) {
    return { scanned: entries.length, deleted: 0, freed: 0 };
  }

  entries.sort((a, b) => a.mtime - b.mtime);
  let deleted = 0;
  let freed = 0;
  for (const e of entries) {
    if (total <= lowWater) break;
    try {
      unlinkSync(e.path);
      total -= e.size;
      freed += e.size;
      deleted++;
    } catch {
      // Concurrent sweep or an open handle (Windows): skip, retried next sweep.
    }
  }

  // `rmdir` refuses a non-empty directory, so this can only ever remove one the
  // eviction above actually emptied — never a version dir still holding entries.
  for (const vd of versionDirs) {
    try {
      rmdirSync(join(dir, vd.name));
    } catch {
      // Still populated, or in use by a concurrent run.
    }
  }

  return { scanned: entries.length, deleted, freed };
}
