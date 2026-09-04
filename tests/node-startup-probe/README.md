# node-startup-probe

Builds Node.js at a pinned `main` SHA on a real macOS arm64 runner in several build-flag variants and measures cold-start cost per variant. It exists to verify, on hardware nobody else is loading, two claims that local instrumentation of the shipped v26.7.0 binary produced:

1. Most of Node's launch-time gap to Deno and Bun on macOS is paid *before Node's first initializer runs*: dyld coalesces ~2100 weak-def symbols (inline/template instantiations exported at default visibility) on every launch, ~4 µs each. A synthetic binary with 2500 such symbols costs ~10 ms of exec+dyld; the same binary built with `-fvisibility-inlines-hidden` costs none.
2. Node's two `atexit()` registrations cost ~2.2 ms in-process, because Apple's `atexit` calls `dladdr`, which linearly scans the binary's 204K-entry symbol table.

## Variants

| variant | what changes |
| --- | --- |
| `baseline` | pinned `main`, default `./configure --ninja` |
| `inlines-hidden` | `-fvisibility-inlines-hidden` only, for node/ada/icu/abseil (every non-inline symbol stays exported) |
| `pr65526` | nodejs/node#65526 as-is: `-fvisibility=hidden` plus `-fvisibility-inlines-hidden` |
| `hasher` | `V8_USE_DEFAULT_HASHER_SECRET=1` in `tools/v8_gypfiles/features.gypi`: V8's own default, which Node's gyp build omits, so every isolate start runs a Miller-Rabin search for three random 64-bit primes (`rapidhash_make_secret`), the top symbol in a Linux `perf` profile of Node 25 and 26 at 12-14% of samples |
| `+atexit` | the `src/node.cc` half of nodejs/node#65549 applied on top as an incremental rebuild (the lazy cipher-table half already landed on main) |

## Running it

Push to `probe/node-startup`; the workflow runs one job per variant (~2-3 h each on the 3-core runner) and uploads `results-<variant>` (text) and `bins-<variant>` (the built binaries) as artifacts.

Locally, `measure-macos.sh <node-binary> <label> harness/` runs the same measurement against any binary.

## Harness

- `harness/interpose2.c` — a `DYLD_INSERT_LIBRARIES` library that timestamps its own initializer, `exit()` and `_exit()`, and counts `atexit()` calls.
- `harness/spawnbench.c` — spawns a command N times with the interposer inserted and reports per-phase minimums: exec+dyld, in-process, exit handlers, kernel teardown.
- `measure-macos.sh` — hyperfine plus the decomposition plus the dyld/symbol facts that explain the first bucket.
