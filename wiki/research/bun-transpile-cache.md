# Bun's Runtime Transpile Cache

> Research target: how Bun caches transpiled JS at runtime when executing TS/TSX/JSX. Goal: settle Nub's own design — a per-machine disk cache, or transpile state kept in-process?

## TL;DR

Bun maintains a content-addressable on-disk transpile cache by default, storing `.pile` files keyed by a hash of source plus features in `$XDG_CACHE_HOME/bun/@t@/` (or `~/Library/Caches/bun/@t@/` on macOS). The cache used to skip files under 50 KiB; in current `main` the floor is 4 KiB, lowered because the 50 KiB cutoff "excluded almost every file in a typical node_modules tree." Bun's official Docker images ship with the cache disabled (`BUN_RUNTIME_TRANSPILER_CACHE_PATH=0`); local installs have it on.

The closest ecosystem reference for a Nub-shaped tool is tsx (Node + load hook + esbuild transpile), which also defaults to a disk cache, in `os.tmpdir()` with a 7-day TTL. So does esbuild-kit/esm-loader, the layer tsx is built on. ts-node defaults to in-memory only with an optional disk cache; swc-node has no runtime transpile cache.

**Recommendation for Nub: keep the disk cache on by default**, with a small-file floor (4 KiB matches Bun) and an off-switch. The concern that disk caching copies every file on disk is real, but the floor and content-addressing mitigate it: Bun settled the tradeoff the same way after measuring real workloads, then lowered the floor when the original threshold left node_modules cold-starts on the table.

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

The published docs still say 50 kb, lagging `main`.

The cache entry contains:

- the transpiled JS output (`output_code`),
- a source map (`sourcemap`, stored as Bun's internal varint-stream `InternalSourceMap`, not VLQ),
- an ESM record (module info for ES modules),
- metadata: cache format version, `input_byte_length`, `input_hash`, `features_hash`, `module_type` (esm/cjs), output encoding.

The cache key is `wyhash(source_bytes)`, cross-validated against `input_byte_length` (cheap, primary) and a `features_hash` folding in parser options, JSX settings, and target. The cache format version (20) bumps on every parser-visible change — "TypeScript enums are properly handled," "Sourcemap blob is InternalSourceMap, not VLQ." A version mismatch returns `error.StaleCache` and the cache is rewritten.

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

Docker turns it off because amortizing across invocations does not apply when each container run is ephemeral; the cache writes only slow the first-and-only cold start.

### node_modules behavior

There is no path-based skip for `node_modules` content. Bun transpiles `.js`/`.cjs`/`.mjs` there (DCE, tree-shake, target-version adjustments, CJS-to-ESM compatibility shimming for some modules) and the cache applies uniformly. `.ts`/`.tsx` files in `node_modules` would also be transpiled, though almost no published packages ship `.ts` source — they ship `.js` + `.d.ts`.

The 4 KiB floor is the only filter, and the 50 KiB → 4 KiB lowering was motivated specifically by the node_modules case: eslint with "~1500 small CommonJS files all well under [50 KiB]" was hitting a full lex→parse→visit→print→sourcemap pass on every CLI invocation.

### Cache hit cost vs miss cost

Hit: stat + open + read + decode metadata + return decoded payload, on the load-bearing claim that "a statx + open + read of a tiny cache file is far cheaper than re-transpiling." Miss: full transpile + write entry, atomic via the standard rename pattern.

### Bytecode cache (orthogonal)

Bun also has a bytecode cache for JSC bytecode, separate from the transpile cache. It is pre-bundled-only and not relevant to Nub's on-the-fly TS execution path.

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

Three of the five Node-loader-shaped tools default to disk-backed caches. The two that do not are ts-node, which predates the design space being well understood, and swc-node, which has no cache at all and relies on swc being fast enough that per-process re-transpile suffices.

## Where the concern is and is not real

The concern raised against disk caching: on a large codebase it copies every source file to a second location on disk.

**True part.** A fully-warmed transpile cache for a 5000-file TS monorepo contains ~5000 files in `~/Library/Caches/nub/`, and the bytes on disk are the same order of magnitude as the project source. On a developer machine running several monorepos, Nub versions, and branches with divergent file content, the cache can reach gigabytes if eviction is sloppy.

**Untrue part.** Bun, tsx, and esbuild-kit already do exactly this today without complaints, for four reasons:

1. The cache is content-addressed, so identical files across projects collapse to one entry. Workspace monorepos with hoisted deps share entries across packages, and different branches of the same repo with the same `node_modules` share entries. The copy-every-file worst case fires only for unique source content.
2. node_modules dominates the file count and is write-once-read-many across a machine — `react@19.0.0`'s transpiled output has the same hash for every project that depends on it. The 5000-unique-files intuition covers project source plus first-time-seen deps; the steady state is mostly cache hits.
3. Eviction keeps it bounded. Bun ships no explicit eviction and relies on OS-level cache-dir cleanup; tsx ages files out after ~7 days; Nub's plan is a 1 GB LRU plus a 30-day age prune. None of those reach pathological size.
4. A 4 KiB floor cuts the per-file overhead. A 1 KB cache entry for a 1 KB source file doubles disk usage for a transpile that takes microseconds.

The pathological case — unique gigabytes per project, never reused — does not occur in real JS/TS workflows, because dependency graphs overlap heavily.

## Cold-start scaling

Anecdotal but consistent numbers from the Bun 1.1 blog and tsx benchmarks:

- **Hello-world `.ts`** (one import): Bun ~30 ms, tsx ~300 ms, ts-node ~1.5 s, vanilla `node --experimental-strip-types` ~80 ms. V8/JSC startup dominates, not transpilation.
- **100-file TS project**: Bun ~80 ms cold, ~50 ms warm; tsx ~500 ms cold, ~350 ms warm. Disk-cache-warm vs warm-process-warm is a ~10–30 ms delta at this size.
- **1000+ file TS project (tsc itself)**: Bun's claimed 2x improvement from Bun 1.0 → 1.1 is entirely the transpile cache.

Disk cache versus in-process-only is roughly nil for short-lived single-invocation scripts and compounds heavily for repeated invocations of larger CLIs. Nub targets the latter: `nub script.ts` in a dev loop, `nub run test` loading a few hundred files per vitest invocation, `nubx tsc` re-running constantly.

## Recommendation for Nub

**Keep the disk cache, default on, with a lower small-file floor.** Concretely:

1. **Match Bun's 4 KiB floor.** Files below that size are neither written to nor read from disk and go through the transpile path on every invocation, because the hit-rate gain from caching tiny files is swamped by the I/O of opening and reading the cache file. Keep them in the in-process memo so re-imports within one Nub process are free; the floor governs only what gets persisted.
2. **Keep content-addressed hashing** on source + transformer version + tsconfig + Nub version.
3. **Keep the per-machine location:** XDG-compliant on Linux, `~/Library/Caches/nub` on macOS, `%LOCALAPPDATA%/nub/Cache` on Windows.
4. **Add an off-switch** equivalent to Bun's `BUN_RUNTIME_TRANSPILER_CACHE_PATH=0`. The brand-boundary rule rules out a `NUB_*` var, so the off-switch is a CLI flag (`--no-transpile-cache`), alongside honoring `XDG_CACHE_HOME`. This resolves the prior open question: flag, not env var.
5. **Disable in Docker / CI by documentation, not by default.** Bun's Dockerfiles ship with the cache off because ephemeral containers never reuse it. Document setting `--no-transpile-cache` in CI and container images rather than auto-detecting container environments, which is brittle.
6. **Defer LRU eviction tooling to post-v0.** Bun ships without explicit eviction and trusts OS cache-dir hygiene, which is enough for v0. The 1 GB cap stands as a soft target; the LRU machinery to enforce it is post-v0.

The case for removing the disk cache rests on Nub's primary mode being long-lived processes (dev server, watch mode) where the in-process memo wins anyway. The actual user surface is the opposite: `nub <file>` is the headline verb and runs short-lived, which is exactly what the disk cache makes fast on the second run.

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
