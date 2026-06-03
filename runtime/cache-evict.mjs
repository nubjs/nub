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

import { readdirSync, statSync, unlinkSync } from "node:fs";
import { join } from "node:path";

// A valid cache entry is a 64-char lowercase-hex sha256 (see cacheKey in
// preload.mjs). Everything else — the `.sweep` sentinel, `*.tmp` in-flight
// writes from cacheSet — is skipped: never counted toward the cap, never
// evicted.
const ENTRY_RE = /^[0-9a-f]{64}$/;

/**
 * Evict oldest entries from `dir` until total entry bytes ≤ `lowWater`, if they
 * exceed `maxBytes`. Returns `{ scanned, deleted, freed }`. Best-effort: a stat
 * or unlink that fails (concurrent reader/evictor, Windows open handle) is
 * skipped, and the next sweep retries. Never throws.
 */
export function sweepCache(dir, maxBytes, lowWater = Math.floor(maxBytes * 0.75)) {
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
    entries.push({ path, size: s.size, mtime: s.mtimeMs });
    total += s.size;
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
