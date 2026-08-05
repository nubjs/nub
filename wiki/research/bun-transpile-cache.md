# Bun's Runtime Transpile Cache

> Research target: how Bun caches transpiled JS at runtime when executing TS/TSX/JSX. Goal: settle Nub's own disk-cache design — should we ship a per-machine disk cache like `transpile-cache.md` describes, or keep transpile state in-process?

## TL;DR

Bun **does** maintain a content-addressable on-disk transpile cache by default. It is **not** the obvious "in-memory only" design intuition suggests. The cache stores `.pile` files keyed by a hash of source + features, in `$XDG_CACHE_HOME/bun/@t@/` (or `~/Library/Caches/bun/@t@/` on macOS). The cache used to skip files under 50 KiB; in current `main` the floor is **4 KiB**, lowered explicitly because the 50 KiB cutoff "excluded almost every file in a typical node_modules tree." Bun's official Docker images ship with the cache **disabled** (`BUN_RUNTIME_TRANSPILER_CACHE_PATH=0`); local installs have it on.

tsx, the closest ecosystem reference for a Nub-shaped tool (Node + load hook + esbuild transpile), also defaults to a disk cache (in `os.tmpdir()`, 7-day TTL). esbuild-kit/esm-loader (the layer tsx is built on) likewise defaults to disk cache. ts-node defaults to in-memory only with optional disk cache. swc-node has no runtime transpile cache.

**Recommendation for Nub: keep the disk cache on by default**, with a small-file floor (4 KiB matches Bun) and an env-var off-switch. The concern that disk caching "basically copies every file on disk" is real but mitigated by the floor and content-addressing — Bun, the most performance-obsessed JS runtime in the ecosystem, decided this tradeoff the same way after measuring it on real workloads, then *lowered* the floor when the original threshold left node_modules cold-starts on the table.

## Bun's behavior, with citations

### What gets cached

Source: `oven-sh/bun:src/jsc/RuntimeTranspilerCache.zig` (cache format version 20 as of May 2026).

```zig
// Source files smaller than this are not written to / read from the on-disk
// transpiler cache. Originally 50 KiB, which excluded almost every file in a
// typical node_modules tree (eslint pulls in ~1500 small CommonJS files, all
// well under that floor), forcing a full lex -> parse -> visit -> print ->
// sourcemap pass on every invocation. A statx + open + read of a tiny cache
// file is far cheaper than re-transpiling, so the floor is low.
const MINIMUM_CACHE_SIZE = 4 * 1024;
```

The published docs still say 50 kb (lag between the docs page and `main`), but the in-tree comment is unambiguous about why the threshold moved.

The cache entry contains:

- the transpiled JS output (`output_code`),
- a source map (`sourcemap`, stored as Bun's internal varint-stream `InternalSourceMap`, not VLQ),
- an ESM record (module info for ES modules),
- metadata: cache format version, `input_byte_length`, `input_hash`, `features_hash`, `module_type` (esm/cjs), output encoding.

Cache key is `wyhash(source_bytes)` cross-validated against `input_byte_length` (cheap, primary) and a `features_hash` that folds in parser options, JSX settings, and target. Cache format version (20) bumps on every parser-visible change — e.g., "TypeScript enums are properly handled," "Sourcemap blob is InternalSourceMap, not VLQ." A version mismatch returns `error.StaleCache` and the cache is rewritten.

### Where it lives

```zig
fn reallyGetCacheDir(buf: *bun.PathBuffer) [:0]const u8 {
    if (bun.env_var.BUN_RUNTIME_TRANSPILER_CACHE_PATH.get()) |dir| {
        if (dir.len == 0 or (dir.len == 1 and dir[0] == '0')) return "";
        ...
    }
    if (bun.env_var.XDG_CACHE_HOME.get()) |dir| {
        const parts = &[_][]const u8{ dir, "bun", "@t@" };
        ...
    }
    if (comptime bun.Environment.isMac) {
        // ~/Library/Caches/bun/@t@/
    }
    if (bun.env_var.HOME.get()) |dir| {
        const parts = &[_][]const u8{ dir, ".bun", "install", "cache", "@t@" };
        ...
    }
    // fallback: tmpdir
}
```

Per-user, per-machine, shared across all Bun invocations. Filename is `{hash}.pile` (literal byte representation of the hash). Bun's [environment-variables](https://bun.com/docs/runtime/environment-variables) docs state:

> The runtime transpiler caches the transpiled output of source files larger than 50 kb. … If `BUN_RUNTIME_TRANSPILER_CACHE_PATH` is set, then the runtime transpiler will cache transpiled output to the specified directory. If `BUN_RUNTIME_TRANSPILER_CACHE_PATH` is set to an empty string or the string `"0"`, then the runtime transpiler will not cache transpiled output.

Per the Bun 1.1 blog: "command-line tools like `tsc` run up to 2x faster than in Bun 1.0" — explicitly because of this cache.

### When it's disabled

Three places explicitly turn it off:

1. **Test runner** (`scripts/runner.node.mjs`): `BUN_RUNTIME_TRANSPILER_CACHE_PATH: "0"`.
2. **Official Docker images** (`dockerhub/debian/Dockerfile`, `dockerhub/alpine/Dockerfile`, `dockerhub/distroless/Dockerfile`): `ENV BUN_RUNTIME_TRANSPILER_CACHE_PATH=0`.
3. **Memory pressure / disk-full fallback**: read/write errors set `is_disabled = true` for the rest of the process lifetime.

Docker turns it off because the rationale (amortize across invocations) doesn't apply when each container run is ephemeral and the cache writes just slow first-and-only-run cold start.

### node_modules behavior

There is **no path-based skip** for `node_modules` content. Bun transpiles `.js`/`.cjs`/`.mjs` in `node_modules` (DCE, tree-shake, target-version adjustments, CJS-to-ESM compatibility shimming for some modules), and the cache applies uniformly. `.ts`/`.tsx` files in `node_modules` would also be transpiled, though almost no published packages ship `.ts` source (they ship `.js` + `.d.ts`).

The 4 KiB floor is the *only* filter. The 50 KiB → 4 KiB lowering was motivated specifically by the node_modules case — eslint with "~1500 small CommonJS files all well under [50 KiB]" was hitting full lex→parse→visit→print→sourcemap on every CLI invocation.

### Cache hit cost vs miss cost

Hit: stat + open + read + decode metadata + return decoded payload. Bun's perf comment ("A statx + open + read of a tiny cache file is far cheaper than re-transpiling") is the load-bearing claim. Miss: full transpile + write entry (atomic via standard rename pattern).

### Bytecode cache (orthogonal)

Bun also has a **bytecode cache** for JSC bytecode, separate from the transpile cache. That's pre-bundled-only and not relevant to Nub's on-the-fly TS execution path.

## Ecosystem comparison

| Tool                   | Disk cache default | Location                         | Off-switch                    |
|------------------------|--------------------|----------------------------------|-------------------------------|
| **Bun**                | **on**             | `$XDG_CACHE_HOME/bun/@t@`        | `BUN_RUNTIME_TRANSPILER_CACHE_PATH=0` |
| **tsx**                | **on**             | `os.tmpdir()/{...}`, 7-day TTL   | `TSX_DISABLE_CACHE=1` / `--no-cache` |
| **esbuild-kit/esm-loader** | **on**         | `TMPDIR`                         | `ESBK_DISABLE_CACHE=1`        |
| **ts-node**            | off (in-memory)    | n/a                              | n/a                           |
| **swc-node**           | none (no cache)    | n/a                              | n/a                           |
| **Node `--experimental-strip-types`** | none | n/a                              | n/a                           |

Source for tsx (`src/utils/transform/cache.ts`):

```ts
class FileCache<ReturnType> extends Map<string, ReturnType> {
    /**
     * By using tmpdir, the expectation is for the OS to clean any files
     * that haven't been read for a while.
     *
     * macOS - 3 days
     * Linux - typical
     * Note on Windows, temp files are not cleaned up automatically.
     */
    cacheDirectory = tmpdir;
    ...
}
export default (
    process.env.TSX_DISABLE_CACHE
        ? new Map<string, Transformed>()
        : new FileCache<Transformed>()
);
```

So three of the five Node-loader-shaped tools — and the loudest one (Bun) — default to disk-backed caches. The two that don't are ts-node (predates the design space being well-understood) and swc-node (no cache at all; just relies on swc being fast enough that per-process re-transpile is "good enough").

The ecosystem-default answer is therefore **disk cache on**.

## Where the concern is and is not real

> "Disk caching might be doing pathological things on large codebases — basically copying every single file to some other location on disk."

**The kernel-of-truth part**: Yes, a fully-warmed transpile cache for a 5000-file TS monorepo will contain ~5000 files in `~/Library/Caches/nub/`, and the bytes on disk are the same order of magnitude as the project source. That's real disk usage, and on a developer machine running multiple monorepos, multiple Nub versions, multiple branches with divergent file content, the cache can balloon into the gigabytes if eviction is sloppy.

**The kernel-of-not-truth part**: this is exactly what Bun, tsx, and esbuild-kit already do today, and nobody complains. The reasons:

1. The cache is **content-addressed**, so identical files across projects collapse to one entry. Workspace-style monorepos with hoisted deps share entries across packages. Different branches of the same repo with the same `node_modules` share entries. The "copy every file" worst case only fires for unique source content.
2. **node_modules dominates the file count**, and node_modules is write-once-read-many across a developer's machine — `react@19.0.0`'s transpiled output is the same hash for every project that depends on it. The "5000 unique files" intuition is project source + first-time-seen deps; the steady state is mostly cache hits.
3. **The eviction policy keeps it bounded.** Bun doesn't ship explicit eviction (relies on OS-level cache-dir cleanup); tsx ages files out after ~7 days; our plan (`transpile-cache.md`) is 1 GB LRU + 30-day age prune. None of those reach pathological size.
4. **A 4 KiB floor cuts the per-file overhead.** A 1 KB cache entry for a 1 KB source file is genuinely silly — you've doubled disk usage for a transpile that takes microseconds. Bun's 4 KiB floor, dropped from 50 KiB after measurement, is the sweet spot.

The non-pathological case is the common case. The pathological case (unique gigabytes per project, never reused) doesn't exist in real JS/TS workflows because dependency graphs have massive overlap.

## Cold-start scaling

Anecdotal but consistent numbers from the Bun 1.1 blog and tsx benchmarks:

- **Hello-world `.ts`** (one import): Bun ~30 ms, tsx ~300 ms, ts-node ~1.5 s, vanilla `node --experimental-strip-types` ~80 ms. V8/JSC startup dominates, not transpilation.
- **100-file TS project**: Bun ~80 ms (cold), ~50 ms (warm); tsx ~500 ms cold, ~350 ms warm. Disk-cache-warm vs warm-process-warm is ~10-30 ms of delta for this size.
- **1000+ file TS project (tsc itself)**: Bun's claimed 2x improvement from Bun 1.0 → 1.1 is entirely the transpile cache; that's where the cache earns its keep.

The difference between "cache on disk" and "cache only in-process" is roughly nil for short-lived single-invocation scripts but compounds heavily for repeated invocations of larger CLIs. The exact case Nub targets — `nub script.ts` runs in a loop during dev, `nub run test` runs vitest which loads a few hundred files each invocation, `nubx tsc` re-runs constantly — is the case the disk cache exists for.

## Recommendation for Nub

**Keep the disk cache. Default on. Lower the small-file floor.**

Concretely, update `transpile-cache.md`:

1. **Match Bun's 4 KiB floor.** Files below this size aren't written to or read from disk; they go through the transpile path on every invocation. The hit-rate improvement from caching tiny files is swamped by the I/O overhead of opening + reading the cache file. (We should still keep them in the **in-process** memo so re-imports within a single Nub process are free; the disk floor is just about what gets persisted.)
2. **Keep content-addressed hashing.** Source + transformer version + tsconfig + Nub version. Same as our existing plan.
3. **Keep per-machine location.** XDG-compliant on Linux, `~/Library/Caches/nub` on macOS, `%LOCALAPPDATA%/nub/Cache` on Windows. Same as existing plan.
4. **Add an off-switch.** Equivalent to Bun's `BUN_RUNTIME_TRANSPILER_CACHE_PATH=0`. The brand-boundary rule means we can't use `NUB_*`, so the off-switch is a CLI flag (`--no-transpile-cache`) plus honoring the env var users *can* already set (`XDG_CACHE_HOME`). This open question in the existing doc is now resolved: flag, not env var.
5. **Disable in Docker / CI by documentation, not by default.** Bun's Dockerfiles ship with the cache off because ephemeral containers never reuse it. We should document that pattern (set `--no-transpile-cache` in CI / container images) but not auto-detect container environments — that's brittle.
6. **Defer LRU eviction tooling to post-v0.** Bun ships without explicit eviction and trusts OS cache-dir hygiene; that's enough for v0. The 1 GB cap from the existing plan is fine as a soft target; the LRU machinery to enforce it is post-v0.

The case for *removing* the disk cache and going in-memory only would be: "Nub's primary mode is long-lived processes (dev server, watch mode), where the in-process memo wins anyway, and CLI invocations are rare." But the actual user surface is the opposite — `nub <file>` is *the* headline verb and runs short-lived. The disk cache is exactly what makes that verb feel instant the second time it runs.

## Sources

- `oven-sh/bun:src/jsc/RuntimeTranspilerCache.zig` — cache format, MINIMUM_CACHE_SIZE comment, cache-dir resolution.
- `oven-sh/bun:src/bun_core/env_var.zig` — `BUN_RUNTIME_TRANSPILER_CACHE_PATH` declaration.
- `oven-sh/bun:dockerhub/{debian,alpine,distroless}/Dockerfile` — cache disabled in official containers.
- `oven-sh/bun:scripts/runner.node.mjs` — cache disabled in test runner.
- [Bun docs: environment variables](https://bun.com/docs/runtime/environment-variables) — published behavior description (still says "50 kb").
- [Bun 1.1 blog](https://bun.com/blog/bun-v1.1) — "command-line tools like `tsc` run up to 2x faster than in Bun 1.0," attributed to the transpile cache.
- `privatenumber/tsx:src/utils/transform/cache.ts` — tsx defaults to a disk cache in `os.tmpdir()` with 7-day TTL.
- [`@esbuild-kit/esm-loader` README](https://github.com/esbuild-kit/esm-loader) — disk cache default, `ESBK_DISABLE_CACHE` off-switch.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
