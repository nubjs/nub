# Cold Start: where Node spends its 12–20 ms

Most of Node's warm startup is C++ that runs before any bootstrap JavaScript — dyld fixups, OpenSSL one-time init, V8 isolate construction and snapshot deserialize — which is what decides how much of the gap to Bun Nub can reclaim.

> Research target: source-grounded breakdown of why `node hello.js` takes ~15 ms warm on macOS arm64 while `bun hello.js` finishes in <5 ms. Locally measured on Node v24.14.0 / Bun 1.3.9. Goal: name the costs, name the upstream work, decide what Nub should do.

## Local measurements (Apple Silicon, macOS 15)

Hyperfine, `--shell=none`, 100 runs after a 10-run warmup:

| Invocation | Mean | Min … Max |
|---|---|---|
| `node -e ''`            | 26.7 ms | 25.3 … 28.9 |
| `node hello.cjs`        | 27.8 ms | 26.3 … 35.1 |
| `node hello.mjs`        | 29.4 ms | 28.1 … 32.4 |
| `node import-fs.mjs`    | 31.7 ms | 29.2 … 110.0 |
| `bun -e ''`             |  4.4 ms |  3.8 … 5.4  |
| `bun hello.cjs`         | 11.3 ms |  9.7 … 17.4 |
| `bun hello.mjs`         | 10.8 ms |  9.7 … 12.6 |
| `bun import-fs.mjs`     | 17.0 ms | 15.7 … 19.4 |

Cold first-run (fs cache evicted) is far worse — Node hit **137 ms** on the very first invocation before warmup — but the warm case dominates day-to-day developer experience.

The `node -e ''` case loads no script file; the ~1 ms it shaves vs `node hello.cjs` is the fopen+stat+read+parse of an empty script. Neither `--no-warnings` nor `--disable-warning` made a measurable difference, so the per-warning install cost is below noise.

## Where v26.7 spends it, measured in 2026-09

Three mechanisms explain most of Node's remaining gap to Deno 2.9 and Bun: V8's per-isolate search for three random primes on Node 25+ (every platform), and on macOS only, dyld weak-def coalescing before `main` plus two `atexit()` symbol-table scans.

The prime search exists because Node's build omits a define that V8's own build sets by default; the macOS costs come from ~2100 default-visibility weak-def symbols and a 204K-entry symbol table that Apple's `atexit` scans through `dladdr`.

Deno 2.9 halved Deno's own cold start (34.2 → 17.3 ms on Deno's x86_64 Linux box, per its release post), which is where a "Node is ~2.4× slower than Deno" figure comes from. Against Deno 2.8.1 the gap was ~1.2×. Re-measured here with Node v26.7.0 official binaries, Deno 2.9.0 and Bun 1.4 (hyperfine minimums; the Mac was under load ~40, the Linux box was idle):

| Invocation | macOS arm64 (M-series, loaded) | Linux x64 (n2-standard-64, idle) |
|---|---|---|
| `node hello.js` | 38.8 ms | 20.7 ms |
| `node --version` | 18.2 ms | 2.8 ms |
| `deno run hello.js` (2.9.0) | 17.3 ms | 12.2 ms |
| `deno run hello.js` (2.8.1) | 27.4 ms | 27.6 ms |
| `bun hello.js` | 18.0 ms | 3.4 ms |

`node --version` is the tell: it runs no JavaScript and creates no isolate, yet costs 18 ms on macOS and 3 ms on Linux. The macOS gap is outside Node's code; the Linux gap is entirely inside the isolate.

### Phase split on macOS

An interposer inserted with `DYLD_INSERT_LIBRARIES` timestamps its own initializer, `exit()` and `_exit()`; a `posix_spawn` harness reads them and keeps the minimum of 40 spawns.

That splits each launch into exec+dyld (spawn → first dylib initializer, before any of Node's static constructors), in-process, exit handlers, and kernel teardown:

| Runtime, `-e 0` / `eval 0` | exec+dyld | in-process | exit handlers | teardown |
|---|---|---|---|---|
| node v26.7.0 | 18.3 ms | 17.0 ms | 0.3 ms | 0.9 ms |
| deno 2.9.0 | 7.4 ms | 11.9 ms | 0.0 ms | 1.1 ms |
| bun 1.3.14 | 7.8 ms | 8.6 ms | 0.0 ms | 1.0 ms |
| a trivial C binary | 4.5 ms | 0.1 ms | | 0.3 ms |
| the same, linked to CoreFoundation + Security + libc++ | 5.6 ms | 0.7 ms | | 0.4 ms |

**dyld weak-def coalescing is the 10 ms.** The v26.7 Mach-O carries `WEAK_DEFINES` and `BINDS_TO_WEAK`: 2311 weak-def exports (inline and template instantiations compiled at default visibility: 1348 from `node::`, 417 `std::__1` instantiations, 265 abseil C symbols, 134 ICU, 60 abseil, 43 ada, 32 V8 header inlines) and 2125 of its 2840 unique dyld imports are `weak-def-coalesce` lookups, each a search across the loaded images at every launch. Two synthetic binaries isolate the cost: a C++ executable with 2500 default-visibility inline functions spends 14.6 ms in exec+dyld; the same source built with `-fvisibility-inlines-hidden` spends 4.8 ms. A binary with 130,000 rebases spends 4.5 ms, the same as a trivial one, so fixup count is not a factor and neither is binary size. Deno 2.9 and Bun carry no weak defines (Deno 2.9 also dropped its CoreFoundation, Security, Foundation and Metal links; it now depends only on libSystem, libobjc and libiconv). V8's part of this was fixed in [#56275][pr56275]; the rest of the binary is what [#65526][pr65526] (open, 2026-08) applies the same flags to, and `-fvisibility-inlines-hidden` alone removes the weak defs while leaving every non-inline symbol exported for addons and embedders.

**`atexit()` costs 1.1 ms per call on macOS.** Apple's `atexit` calls `dladdr` to find the image that owns the handler, and `dladdr` scans the executable's `nlist` symbol table linearly. The shipped binary keeps 204,315 entries (145,496 of them local `t` symbols), and Node registers two handlers, `ResetStdio` in `PlatformInit` and OpenSSL's `OPENSSL_cleanup` during static initialization; the interposer measures the pair at 2.2–2.4 ms per run, half of `node --version`'s 4.6 ms in-process time. Deno registers none; Bun registers one against a 1-symbol table (0.003 ms). `strip -x` on the shipped binary (204K → 39K symbols, 144 → 107 MB) cuts in-process time by 4.6 ms on `-e 0` with no source change. [#65549][pr65549] (open, 2026-08) replaces `atexit(ResetStdio)` with a static destructor and passes `OPENSSL_INIT_NO_ATEXIT`; its lazy cipher-table half landed separately in [#65484][pr65484].

Node's launch also runs 657 dyld initializers (365 in Network.framework, 253 its own, plus the Swift runtime, SkyLight and CoreDisplay reached through the CoreFoundation/Security dependency tree) and dlopens CoreFoundation, libswiftCore, ColorSync and QuartzCore from Foundation's `_NSInitializePlatform`; Deno 2.9 runs 20 initializers and Bun 6. Measured as the trivial-binary delta above, that tree costs ~1–2 ms.

### The isolate on Linux: a prime search on every start

With the dynamic loader at ~0.2 ms, a Linux `perf` profile of 200 runs of the official v26.7 `node -e 0` puts its single largest symbol at **12.1% of samples in `v8::internal::HashSeed::InitializeRoots`**.

Another 2.8% sits in `detail::sprp`, a Miller–Rabin strong-probable-prime test, and `LD_DEBUG=statistics` counts only 3.6K relocations, so the loader is not the cost. A sweep of official linux-x64 binaries dates it:

| Node | V8 | `-e 0` min | `HashSeed::InitializeRoots` share |
|---|---|---|---|
| v24.14.0 | 13.6 | 18.6 ms | absent |
| v25.9.0 | 14.1 | 20.6 ms | 13.4% |
| v26.0.0 | 14.6 | 20.5 ms | 12.6% |
| v26.3.0 | 14.6 | 20.7 ms | 11.9% |
| v26.7.0 | 14.6 | 19.6 ms | 12.1% |

The mechanism is in `deps/v8/src/numbers/hash-seed.cc`. Since V8 14.1 the string hash is rapidhash, whose secret is three 64-bit words that must each have balanced bytes, pairwise Hamming distance 32, and be prime ("a feeling to be perfect", per the wyhash author quoted in `third_party/rapidhash-v8/secret.h`). `HashSeed::InitializeRoots` either copies a compile-time default secret and stores a fresh random meta seed, when `V8_USE_DEFAULT_HASHER_SECRET` is defined, or calls `rapidhash_make_secret(seed, …)`, which rejection-samples the three primes with a 12-base Miller–Rabin test. V8's `BUILD.gn` sets `v8_use_default_hasher_secret = true`, so Chrome takes the first path; Node builds without the define, so every Node isolate, main thread and each `worker_threads` Worker alike, takes the second. The code's own comment estimates the generation at ~200 µs; measured, it is 2–3 ms of a 20 ms process.

The random secrets are not optional for Node. Its March-2026 HashDoS fix ([CVE-2026-21717][hashdos2026]) scrambles array-index string hashes with three multipliers derived from exactly these secrets (`DeriveSecretsForArrayIndexHash` in the same file, enabled by `v8_enable_seeded_array_index_hash` in `tools/v8_gypfiles/features.gypi`), and Node's write-up states the scheme "needs to be used together with `v8_use_default_hasher_secret = false` for HashDoS resistance"; the per-process meta seed does not enter that hash, so with the define the multipliers would be public constants. Deno builds V8 with the same `v8_use_default_hasher_secret = false` ("prevent hashdos" in `rusty_v8`'s `.gn`), so it pays the same generation on every isolate. The 2019 `v8_use_siphash = true` in `common.gypi` is the same posture: Node runs stronger flooding defenses than Chrome, where a renderer DoS is out of scope.

Building main (`53234233acb`, gcc 13, `./configure --ninja`) with `V8_USE_DEFAULT_HASHER_SECRET=1` therefore only prices the generation; it is not a shippable configuration. Paired in one hyperfine session with the unpatched build on the same box:

| `node -e 0`, Linux x64 | min | median |
|---|---|---|
| main | 21.7 ms | 25.4 ms |
| main with the generation compiled out | 19.3 ms | 20.5 ms |

`performance.nodeTiming`'s `v8Start → environment` span drops from 8.4 to 6.4 ms and `HashSeed::InitializeRoots` and `sprp` leave the profile (the top symbol becomes the snapshot deserializer at 8.4%). The cost lives in one helper, not in the randomness: `mul_mod` in `secret.h` is a 64-iteration shift-add loop with two 64-bit `%` per iteration, called ~9,000 times per generation inside the Miller–Rabin test (~1,560 candidates, ~99 `sprp` calls), while the file's own comment says the test "is fast as long as we have 64x64 -> 128 bit muls and modulos". A standalone copy of the generator with `mul_mod` rewritten as a 128-bit multiply and modulo (`__uint128_t`, the type V8's own `rapidhash.h` already uses for its multiply) produces bit-identical secrets for 50,000 seeds on x64 and arm64 Linux and macOS and for 3,000 seeds on s390x, ppc64le and riscv64 under QEMU, and runs 13–27× faster wherever it links: 2,569 → 148 µs on an Apple M1 Max, 7,353 → 339 µs on x64 macOS, 2,087 → 141 µs on arm64 Linux. It does not link on Windows: clang-cl defines `__SIZEOF_INT128__`, but its runtime ships no `__umodti3` (compiler-rt builds the 128-bit division helpers for LP64 targets only, the reason abseil refuses `__int128` on clang-for-Windows), and clang-cl declares neither `_udiv128` nor, on arm64, `_umul128`. So the fix is a six-line `#if defined(__SIZEOF_INT128__) && !defined(_WIN32)` fast path in V8's `secret.h`, with no change in what gets generated, and Windows keeps the loop; a loop variant with a conditional subtraction in place of the two `%` was measured as well, 2× on arm64 and nothing under clang-cl x64, and is not worth carrying. Built into Node main on the same Linux box and paired against the unpatched build (100 runs each, interleaved): `node -e 0` 19.75 → 18.09 ms min, `hello.js` 21.99 → 19.84 ms, the `v8Start → environment` span 8.06 → 6.20 ms; `sprp` leaves the profile, the eight `test/parallel/*hash*` tests pass, and the full `parallel`, `sequential`, `message` and `es-module` suites (5,228 tests) show no failure the unpatched build does not have. Both `-fvisibility` variants of [#65526][pr65526] were built on the same box for comparison and change nothing on Linux (19.9 / 20.5 / 21.4 ms are within noise), which is expected: `-fvisibility=hidden` there shrinks `.dynsym` from 208,643 to 186,791 entries and weak symbols from 14,135 to 869, but glibc's loader was never the cost.

### The three fixes built on macOS

Six builds of Node main (`53234233acb`) on macOS 15 arm64 runners, benchmarked interleaved on one Mac, confirm it: the visibility flags empty the dyld bucket, the `atexit` change saves 2 ms in-process, and secret generation is 1.2 ms of the isolate.

Every number below comes from the same session on a host under load ~40, so absolute values are inflated and only the differences matter (hyperfine minimum / median of 40 runs; the phase split is the interposer minimum of 40 spawns):

| Variant | weak-coalesce imports | exec+dyld | in-process | `--version` | `-e 0` | `hello.js` |
|---|---|---|---|---|---|---|
| main | 2305 | 17.6 ms | 14.3 ms | 18.6 / 20.8 | 30.6 / 34.5 | 32.5 / 35.7 |
| main + `atexit` change ([#65549][pr65549], `node.cc` half) | 2305 | 17.9 ms | 12.0 ms | 15.9 / 18.8 | 27.7 / 31.7 | 29.8 / 31.8 |
| main + `-fvisibility-inlines-hidden` only | 985 | 12.5 ms | 14.2 ms | 12.7 / 15.1 | 25.9 / 29.3 | 26.1 / 30.1 |
| main + [#65526][pr65526] (`-fvisibility=hidden` too) | 18 | 8.5 ms | 14.8 ms | 8.6 / 11.1 | 21.7 / 25.5 | 22.4 / 25.3 |
| main + #65526 + `atexit` change | 18 | 7.9 ms | 12.1 ms | 7.1 / 9.9 | 18.5 / 22.1 | 20.7 / 23.3 |
| main + `V8_USE_DEFAULT_HASHER_SECRET=1` | 2305 | 18.0 ms | 13.1 ms | 19.6 / 21.8 | 29.8 / 32.9 | 30.7 / 33.1 |

What the rows say:

- `-fvisibility-inlines-hidden` alone removes 57% of the weak-coalesce imports and ~5 ms of exec+dyld; the 985 that survive are non-inline weak definitions (415 libc++ template instantiations, 354 in `node::`, 196 abseil C symbols such as `AbslInternalPerThreadSemPost`, 134 ICU, 54 abseil, 34 ada), which only `-fvisibility=hidden` reaches. The full [#65526][pr65526] leaves 18 and brings exec+dyld to 8.5 ms, level with Deno 2.9 and Bun on the same box.
- The `atexit` change is worth 2.0–2.3 ms in-process in every pairing (the interposer counts the registrations at 0), independent of the visibility flags.
- Compiling the secret generation out (the define row, a measurement and not a shippable build, see above) shows the cost where it should be: the in-process bucket drops 1.2 ms and the runner's `performance.nodeTiming` `v8Start → environment` span goes from 5.2 to 3.7 ms, the macOS counterpart of the Linux 8.4 → 6.4.
- Together, [#65526][pr65526] plus the `atexit` change take `node --version` from 18.6 to 7.1 ms and `node -e 0` from 30.6 to 18.5 ms on this host, 40% off; the secret generation's ~1.2 ms lands in a phase neither of them touches. In the same session `deno run hello.js` (2.9.0) took 11.6 ms and `bun hello.js` 11.8 ms against the patched Node's 20.7 ms, so the remaining gap is the in-process 12 ms (isolate and snapshot deserialization, then the bootstrap chain), no longer anything before `main`.

What [#65526][pr65526] changes for addons and embedders, measured on the two binaries: the export table shrinks from 38,531 to 21,496 symbols. Gone are ICU (7,080 `icu_78::` C++ symbols, 1,081 `u*_78` C entry points, the converter data tables), 5,617 `node::` internals, abseil, ada and the Temporal crate's Rust symbols. Kept are every `napi_*` (161), every `uv_*` (318), OpenSSL (2,132), zlib and the `NODE_EXTERN` C++ API, which carry default visibility explicitly. Against real code: 37 popular native packages installed on macOS arm64 (sharp, better-sqlite3, bcrypt, node-pty, sqlite3, canvas, re2, @parcel/watcher, @swc/core, lightningcss, sodium-native, tree-sitter, isolated-vm, zeromq and the rest) contain 74 Mach-O `.node` files importing 1,734 distinct symbols (319 `v8::`, 124 `napi_*`, 25 `uv_*`, 16 `node::`), and none of them is among the symbols the change stops exporting. `isolated-vm`, which imports 304 V8 symbols, is unaffected because V8's public API is already built with explicit `V8_EXPORT` visibility.

The two Linux-verified fixes were then built on the same macOS 15 arm64 runner (main, then the `mul_mod` change, then the entropy change in its first form, as incremental rebuilds of one tree) and measured interleaved on one Mac: hyperfine min / median of 100 runs, the interposer's in-process bucket as a min of 40 spawns, `performance.nodeTiming` as a min of 20, and ten `worker_threads` Workers spawned in one process.

| Build | `-e 0` | `hello.js` | in-process (`-e 0`) | `v8Start → environment` | `nodeStart → v8Start` | per Worker spawn |
|---|---|---|---|---|---|---|
| main | 30.8 / 33.1 | 32.5 / 35.3 | 16.5 ms | 5.11 ms | 2.48 ms | 3.92 ms |
| + 128-bit `mul_mod` | 29.3 / 31.0 | 30.6 / 32.4 | 14.6 ms | 3.70 ms | 2.53 ms | 3.07 ms |
| + entropy from `uv_random()` | 29.0 / 30.3 | 30.5 / 32.3 | 13.9 ms | 3.68 ms | 1.90 ms | 3.05 ms |

The entropy change was then revised (the seeding check keys on `OSSL_PROVIDER_available`, which keeps the default provider activated at startup for `--openssl-legacy-provider`, and AIX stays on OpenSSL's CSPRNG). Rebuilt the same way and re-measured on the same Mac, the revised form moves `nodeStart → v8Start` from 2.63 to 2.13 ms (min of 20) and the in-process bucket from 16.20 to 15.50 ms, the same saving as the first form within noise.

On Apple Silicon the generator fix is 1.4 ms of every isolate, which includes 0.85 ms of every Worker spawn, and the entropy change is 0.6 ms of every process; `--version` does not move with either, since it exits before OpenSSL is initialized and never creates an isolate.

### The remaining 18 ms on Linux, and V8's entropy through OpenSSL

With the generator fix in, a call-graph `perf` profile of 300 runs of `node -e 0` on the idle Linux box splits the rest: snapshot deserialization is a third of the process, OpenSSL's startup a sixteenth, and the kernel's page-fault handling a fifth.

| Phase (inclusive share of samples) | Share |
|---|---|
| `Isolate::Init`, i.e. the V8 startup snapshot (`StartupDeserializer` 17.0%, read-only heap 1.3%) | 26.5% |
| Node's context snapshot (`ContextDeserializer`) | 16.6% |
| `InitializeOncePerProcessInternal` (OpenSSL config load 2.5%, CSPRNG seeding 3.7%, `V8::Initialize` 1.4%, option-parser tables 1.2%) | 12.6% |
| `LoadEnvironment` (running `internal/main/eval_string`) | 8.1% |
| Isolate and heap teardown at exit | ~5% |
| Kernel page faults (3,240 minor faults; `--use-largepages=on` recovers 0.3 ms) | 23% |

The CSPRNG line is avoidable. `InitializeOncePerProcessInternal` calls `ncrypto::CSPRNG(nullptr, 0)` to confirm OpenSSL's random source is seeded, and installs a V8 entropy source that also goes through OpenSSL, so the first `RAND_status()` of the process happens before V8 starts and instantiates the DRBG, which constructs the default provider's algorithm and name tables (`ossl_method_construct`, `ossl_namemap_stored`). V8 only uses that entropy for hash seeds, address-space randomization and `Math.random()`, none of them cryptographic; the 2022 change that let V8 fall back to its own source ([#44493][pr44493]) already records that V8's entropy is proper on every platform. A build that seeds V8 from `uv_random()` (the OS CSPRNG: `getrandom`, `getentropy`, `RtlGenRandom`; AIX stays on OpenSSL because libuv reads the blocking `/dev/random` there) and runs the eager seeding check only when `OSSL_PROVIDER_available(nullptr, "default")` is false or FIPS is in effect moves the `nodeStart → v8Start` span from 2.91 to 2.11 ms and `node -e 0` from 29.18 to 27.82 ms (min of 300 runs on a second n2-standard-64, a slower host than the one above), and removes `RAND_status` and the provider's table construction from the startup profile, 2.8% of samples before; the provider activation that remains is 0.05%, the OpenSSL config load stays as it must, and the first `crypto.randomBytes()` instantiates the DRBG lazily in 0.19 ms. Two constraints shape that predicate. `test-crypto-no-algorithm` pins that a configuration with no random provider (a `base`-only section) aborts at startup rather than hanging, and that configuration can arrive through `--openssl-config`, `OPENSSL_CONF`, or the `nodejs_conf` section of the file in OPENSSLDIR, which Node reads with no flag at all, so a check keyed on how the configuration was supplied misses the last case, and a check keyed on FIPS alone turns the abort into a normal start with `ERR_OSSL_EVP_UNSUPPORTED` at the first crypto call. And `--openssl-legacy-provider` loads its provider explicitly, which disables OpenSSL's provider fallback, so the default provider has to be active before that point; the eager check used to activate it as a side effect, and `OSSL_PROVIDER_available` activates it the same way. With that predicate the `parallel`, `sequential`, `message`, `es-module` and `addons` suites show no failure the unpatched build does not have. Running both builds over 59 OpenSSL configuration and flag cases on Linux (52 on macOS) found two further differences. A configuration whose `[random]` section names a DRBG that cannot be fetched used to abort at startup and now starts, with the first crypto call failing on `unable to fetch drbg`; the provider check cannot see that case without fetching the DRBG, which is the work being deferred. And with `--secure-heap`, the process DRBGs are now instantiated after the secure heap exists and take 512 bytes of it; the same laziness removes an abort in the unpatched build, where eight `worker_threads` Workers on a 1024-byte secure heap die in V8's entropy callback because each Worker's isolate fetched entropy through a per-thread DRBG allocated from the exhausted heap. Tests for the three cases ride with the change.

Two things that look like wins and are not, measured on the same box. Pre-loading `stream`, `net` and `tty` into a snapshot removes the 28-module load behind the first `console.log` (3.09 → 0.18 ms) but grows the snapshot by 660 KB and its deserialization by ~2 ms, so `hello.js` gains 0.5 ms while a script that never prints loses 2.1 ms; the built-in modules are already compiled from the embedded code cache, and having `net` import `internal/streams/duplex` instead of the `stream` index would save only the ~0.2 ms of the nine extra stream modules. And `-z pack-relative-relocs` (RELR) would trim the loader's 24% share of `node --version`, but it needs glibc 2.36 at run time while the official binaries target 2.28.

On macOS the last framework cost is attributable: a binary that links nothing but libSystem starts in 4.19 ms on this host, one that also links CoreFoundation in 5.48 ms (+0.6 ms of dyld work, +0.6 ms of initializers), and adding Security on top changes nothing further. Node imports 36 CoreFoundation and Security symbols: 34 from the system certificate store code in `src/crypto/crypto_context.cc`, which only runs under `--use-system-ca`, and `CFTimeZoneCopyDefault`/`CFTimeZoneGetName` from abseil's `cctz::local_time_zone()`. Deno removed the same dependency in 2.9 by linking dlopen-on-first-use shims for its 47 framework imports ([lzld][denolzld]); Node's own 2022 look at dropping CoreFoundation ([#44715][issue44715]) was closed because Instruments' heap-allocation recorder needed the framework loaded.

## TL;DR

A warm `node ./hello.js` on macOS arm64 today spends its ~15–27 ms roughly like this (apportioned from flamegraphs and `--no-node-snapshot` A/B numbers in [nodejs/performance#180][perf180]):

| Phase | Approx. share | Notes |
|---|---|---|
| `dyld` + image load + global ctors (incl. weak-symbol fixups) | 4–6 ms | macOS-specific; was 8 ms worse before [#56275][pr56275] (already landed in v24) |
| `node::InitializeOncePerProcess` (OpenSSL, cppgc, ncrypto, ICU register) | 3–4 ms | dominated by `OPENSSL_init_crypto` and `cppgc::InitializeProcess` |
| `NodeMainInstance` ctor (Isolate + 4 context snapshots deserialize) | 3–4 ms | this is the V8 startup snapshot fast path |
| Bootstrap JS (`internal/process/pre_execution.js`, ESM loader init, source-map cache, diagnostics_channel, etc.) | 2–3 ms | a lot of "setup X" calls that run even when unused |
| Compile + run user file, CJS loader, module resolution | 0.5–1 ms | the actual work |
| `__cxa_atexit` / teardown (counted by hyperfine) | 1–2 ms | non-trivial on macOS |

Takeaways:

1. **Most of the 15 ms is not "Node bootstrap" in the JS sense.** It's `dyld`, libc++ template fixups, OpenSSL one-time init, V8 isolate construction, and snapshot deserialization — all C++ that runs before a single line of `lib/internal/bootstrap/node.js` executes.
2. **The snapshot is doing its job.** Without it, empty-script startup on Linux is ~53 ms vs ~18 ms with it (the 2021 builtins-snapshot work, ~3× speedup, per the PR description on [#27321][pr27321]). The remaining 15 ms is what's left _after_ the biggest already-fixed lever.
3. **Bun's headline win is mostly that it links statically (no dyld weak-symbol fixups), uses JSC instead of V8 (smaller engine init, smaller snapshot footprint), and skips OpenSSL on macOS in favor of BoringSSL/Apple frameworks.** That is, the gap is mostly _under_ Node's main(), not above it.

## Sources of cost, itemized

Six items, ordered by cost: dyld and global C++ constructors, one-time process init, isolate construction plus snapshot deserialize, the bootstrap JS that survives the snapshot, module resolution and the user script, then teardown.

### 1. `dyld` and global C++ constructors (macOS-only tax)

The single biggest macOS-specific cost, and the one with a proper post-mortem in the PR description on [#56275][pr56275].

Between v20 and v23, macOS startup regressed from ~19 ms to ~30 ms. Root cause: V8's upgrade 11.3 → 11.8 added many templated `StaticCallInterfaceDescriptor` instantiations, and without `-fvisibility=hidden` on the V8 build those templates produced weak symbols that `dyld` resolved at process start. From the bench in [nodejs/performance#180][perf180]:

> "DYLD_PRINT_BINDINGS=1 ./node --version 2>&1 | grep 'looking for weak-def symbol' | wc -l: 7317" versus the fixed build: "1755"

Fix: add `-fvisibility=hidden` (plus `BUILDING_V8_SHARED`) to V8's gypfiles. Result: **2.33× faster startup on macOS arm64 (28.9 ms → 12.4 ms), binary 10 MB smaller (118 → 108 MB)**, landed in [#56275][pr56275] (Dec 2024, in v23.7/v22.13). V8's own `node-ci` fork did not have the regression because Chromium's build always sets `-fvisibility=hidden`; that contrast is how the regression was located.

**This fix is in v24, so the local 27 ms baseline already reflects it.**

Even with the fix in, `dyld` is still the largest single contributor on macOS. From `otool -L node`:

```
/System/Library/Frameworks/CoreFoundation.framework/.../CoreFoundation
/usr/lib/libSystem.B.dylib
/usr/lib/libc++.1.dylib
```

A proposal to remove CoreFoundation ([#44715][issue44715]) was closed in 2022 after screenshots on macOS 12.6 showed Instruments' Heap Allocations recorder no longer working without the framework loaded; nothing in that thread concerns ICU. From Daniel Lemire in [#180][perf180]:

> "One maybe significant difference between bun and node is that node depends on Core Foundation whereas bun does not appear to do so... So I am back at my theory: this is a case where dynamic linking and rebasing is expensive under macOS for some reason."

There is no equivalent post-mortem for Linux, where startup stayed roughly flat across versions (Lemire's measurements on Linux i5: node 17.9 → 21.8 ms across the same versions — actually _improving_).

### 2. `node::InitializeOncePerProcess` (OpenSSL, cppgc, ICU)

Source: [`src/node.cc`][nodecc] `InitializeOncePerProcessInternal`. The flamegraph diff in [#180][perf180] (billywhizz) put this at **~30% of v22 wall time** (6.5 ms of the 16 ms regression). Sub-costs:

- `OPENSSL_init_crypto` — ~6.5% of total. Building with `./configure --without-ssl` drops about 3.5 ms in A/B (35.4 → 26.9 ms). Node forks [quictls/openssl][quictls] and links it statically; OpenSSL v3's provider model is heavier than 1.1 was.
- `cppgc::InitializeProcess` — ~2.5%, added between v20 and v22 when V8's cppgc became a hard dependency. No tracking issue for removal.
- `ncrypto::CSPRNG` — ~2.5%, the seed gather for `crypto.randomUUID` etc. (Separately, [#59550][pr59550] moved _system CA_ loading off-thread — saved ~48 ms on first TLS context, 57 → 8.5 ms — but this is a TLS warmup fix, not a startup fix.)
- ICU registration — `--without-intl` saves ~30 MB binary but "didn't seem to make a difference" to startup ([#180][perf180]).

### 3. V8 isolate construction + snapshot deserialize

Source: `node::NodeMainInstance::NodeMainInstance` constructor. The flamegraph attributes ~26% of wall time to it in both fast and slow builds — i.e. constant, not regressed. So roughly **3.5 ms** at v23 macOS.

What gets deserialized is documented in [`tools/snapshot/README.md`][snapREADME]: one isolate snapshot plus **four** context snapshots:

> 1. The default context snapshot ... 2. The vm context snapshot ... 3. The base context snapshot ... 4. The main context snapshot ... captures initializations done by `node::CommonEnvironmentSetup::CreateForSnapshotting()`, most notably `node::CreateEnvironment()`, which runs the following scripts via `node::Realm::RunBootstrapping()` for the main context as a principal realm, so that at runtime, these scripts do not need to be run. Instead only the context initialized by them is deserialized at runtime. 1. `internal/bootstrap/realm` 2. `internal/bootstrap/node` 3. `internal/bootstrap/web/exposed-wildcard` 4. `internal/bootstrap/web/exposed-window-or-worker` 5. `internal/bootstrap/switches/is_main_thread` 6. `internal/bootstrap/switches/does_own_process_state`

That's the only JS that doesn't run at runtime in a default build. As of Feb 2026, the ESM loader joined them via [#61769][pr61769]; before that it was being initialized from scratch on every start.

Snapshot compression was disabled by default in [#45716][pr45716] — +2.7 MB binary in exchange for **9–18% faster startup**, validating that decompression was on the hot path.

### 4. Bootstrap JS that still runs on every start

The post-snapshot JS bootstrap lives in [`lib/internal/process/pre_execution.js`][preExec] (26 KB), a sequential `setup…()` chain:

```
patchProcessObject     setupTraceCategoryState     setupInspectorHooks
setupNetworkInspection setupNavigator              setupWarningHandler
setupFFI               setupSQLite                 setupStreamIter
setupQuic              setupWebStorage             setupWebsocket
setupEventsource       setupCodeCoverage           setupDebugEnv
initializeReport       setupDiagnosticsChannel     initializePermission
initializeSourceMapsHandlers                       initializeDeprecations
initializeConfigFileSupport                        initializeDns
setupStacktracePrinterOnSigint                     initializeReportSignalHandlers
initializeHeapSnapshotSignalHandlers               setupChildProcessIpcChannel
initializeClusterIPC                               initializeExtensionFormatMap
setupVmModules                                     initializeModuleLoaders
setupHttpProxy
```

Every one runs on every hello-world. [#45659][pr45659] ("bootstrap: lazy load non-essential modules", merged Dec 2022) was the big project to push this back. From the PR description:

> "It turns out that even with startup snapshots, there is a non-trivial overhead for loading internal modules. This patch makes the loading of the non-essential modules lazy again."

Result: **~17% faster basic startup, ~37% faster worker startup**, at the cost of 5–10% on apps that need every builtin. The PR primarily recovered a regression rather than going below v14.

Follow-ups still landing in 2025–26: [#59517][pr59517] lazy `internal/tty` in tests; [#56980][pr56980] lazy modules in test runner; [#57307][pr57307] `fs.getLazy`; [#59473][pr59473] simdjson for `--snapshot-config`. Pattern: each PR shaves micro/single-digit milliseconds; nobody has the lever to drop 5+ ms in one PR anymore. ([#62267][pr62267], a lazy source-map cache in the CJS loader, was closed by its author in April 2026 without landing, so that one is unclaimed.) The exception arrived on 2026-09-04: [#65336][pr65336] starts worker threads from the built-in snapshot and reports a Worker start of 21.1 → 10.6 ms in Node's own `misc/startup-core` benchmark; every per-Worker figure in this document predates it.

### 5. Module resolution and the actual user script

Almost free. From the discussion in [#180][perf180]:

> "in recent versions of Node.js no internal JS code is compiled at all when executing a CJS script, that's because we moved the internal JS code compilation into build time and serialized the bytecode into the snapshot."

For ESM (`hello.mjs`), the loader is now snapshotted as of [#61769][pr61769] (Feb 2026) but only just merged. From the PR description:

> "empty/minimal CJS startup is now slightly slower in worker but other metrics get a slight boost (because they all incur ESM loader initialization). In reality ESM loading is likely to happen at some point in the lifetime of an application especially with the growing adoption of ESM and `require(esm)`."

This explains the ~2 ms gap between `hello.cjs` (27.8 ms) and `hello.mjs` (29.4 ms) in our local bench — and that gap should narrow once #61769 propagates.

### 6. Teardown (counted by hyperfine)

Teardown is not startup, but every hyperfine number quoted in this doc includes it.

billywhizz, [#180][perf180]: "the small traces on left and right of the graph are system code being run when tearing down the process and take ~6% of time in the fast instance and ~12% of time in the slow instance" — roughly **1–2 ms** of `__cxa_atexit`/`Isolate::Delete`. Hyperfine includes this; it is not strictly "startup" but is in every benchmark quoted here.

## What Node has already done

Timeline of landmark startup work, all from [#35711][issue35711] ("Tracking issue: snapshot integration in Node.js core", open since 2020):

| When | PR | What |
|---|---|---|
| v12.5 (2019) | [#27321][pr27321] / [#28181][pr28181] | First V8 startup snapshot landed |
| 2021 | `tools/js2c.py` | Internal JS encoded into C++ arrays at build time, V8 code cache pre-compiled into `node_code_cache.cc`, bootstrap context serialized into `node_snapshot.cc`. "Node.js does not need to execute this part of the bootstrap at all." |
| v19.6 | [#42466][pr42466] | Build-time user-land snapshot via `--node-snapshot-main`; foundation for SEA |
| v19.6 | [#45659][pr45659] | Bootstrap lazy-loads non-essential modules — **+17% startup** |
| v19.6 | [#45716][pr45716] | Disable snapshot compression by default — **+9–18% startup, +2.7 MB binary** |
| v23.7 / v22.13 | [#56275][pr56275] | `-fvisibility=hidden` for V8 on macOS — **+2.33× macOS startup, –10 MB binary** |
| 2025 | [#59550][pr59550] | System-CA load off the main thread — first TLS context 57 → 8.5 ms |
| 2026-02 | [#61769][pr61769] | ESM loader baked into the built-in snapshot |
| ongoing | many | `getLazy()` retrofits across `fs`, source-map cache, test runner, internal/tty |

For runtime user-land snapshotting, [#44014][issue44014] is the open tracking issue, gated on V8 not supporting a long list of types in run-time-built snapshots. The build-time path is what powers SEA (Single Executable Applications), and the integration story for packagers is still cumbersome ([#42566][pr42566]).

## What's still on the table upstream

Ten items are open or unowned: V8's rapidhash secret generation, the macOS weak defs and `atexit` registrations, user-land snapshots, OpenSSL and cppgc init, config-file probing, CoreFoundation, the bootstrap chain, and options parsing.

- **rapidhash secret generation in V8** — required by Node's [CVE-2026-21717][hashdos2026] fix, so the define Chrome uses is not available; measured above at 2–3 ms of every isolate start on Node 25+ (Deno pays it too). The cost is the schoolbook `mul_mod` inside `third_party/rapidhash-v8/secret.h`; a 128-bit multiply gives identical secrets 15–22× faster everywhere but Windows, whose clang runtime lacks `__umodti3`, and, built into Node, 1.7 ms off `node -e 0` on Linux with the full suites unchanged. A floating patch on Node's V8 copy was proposed in [#65795][pr65795] and closed: Node takes V8 changes only once they have landed in V8, so the diff has to go through V8's own review, where bug 409717082 tracks the generator.
- **V8's entropy source routed through OpenSSL** — no issue; seeding V8 from `uv_random()` and running the eager CSPRNG check only when the default provider is unavailable or FIPS is in effect takes 0.8 ms off the `nodeStart → v8Start` span per process on Linux, measured above, with the full suites unchanged; proposed in [#65796][pr65796].
- **Default-visibility weak defs in the macOS binary** — [#65526][pr65526], open; the interposer numbers above put dyld's coalescing of them at ~10 ms of the 18 ms exec+dyld bucket, and the export-table and addon-import checks above show the public API and 74 real addons untouched.
- **`atexit()` on macOS** — [#65549][pr65549], open; 2.2 ms for the two registrations, because Apple's `atexit` scans the 204K-entry symbol table via `dladdr`.
- **Run-time snapshots for arbitrary user code** — [#44014][issue44014], open since 2022. Blocked on V8 supporting more embedder types outside build-time snapshots.
- **Macro-level OpenSSL init cost** — no tracking issue; the `OPENSSL_init_crypto` line is the largest single C++ frame still visible. Bun avoids it by using BoringSSL.
- **`cppgc::InitializeProcess`** — added by V8 upgrade; no Node-side fix being worked.
- **Config-file initialization** ([#53787][issue53787]) is adjacent: loading a config file by default "adds overhead to the startup to probe the file system." Same problem will face any package.json-based hooks config; the lean is on a field _inside package.json_ rather than a new dotfile.
- **CoreFoundation dependency on macOS** — [#44715][issue44715] proposed removal; closed in 2022 because Instruments' Heap Allocations recorder needed the framework loaded, an observation made on macOS 12.6 that nobody has repeated on a current macOS.
- **Single-pass bootstrap JS** — `pre_execution.js` is still a long imperative chain. Each `setup…` adds μs; together they're a couple of ms. There are `TODO: move this to vm.js?` markers in the source suggesting more lazification is wanted.
- **Run-time options parsing in JS** — `refreshRuntimeOptions()` runs every start. [#59473][pr59473] moved snapshot-config parsing to simdjson; the full options system is still JS.

## Why hasn't Node done this already?

Three reasons: part of the work already shipped, most of the rest is landing slowly behind compat guarantees, and what remains is held by distro-packaging, FIPS and embedder promises.

1. **Some they did.** `-fvisibility=hidden` shipped in [#56275][pr56275] (Dec 2024). The 16 ms reclaim is _already in the v24 baseline_.

2. **Most of the rest, Node is doing — slowly.** The `getLazy()` PR train ([#45659][pr45659] and follow-ups) has been clawing back `pre_execution.js` overhead for three years and is maybe halfway. Each `setup{Inspector, Permission, DiagnosticsChannel, …}` call has subtle ordering guarantees: `process.on('warning')` listeners installed by user code must fire if a warning is emitted by another setup; the permission model must be live before any fs/net access; diagnostics channels must precede async_hooks. Each step has to land behind tests and a release cycle, so they cannot take the cut in one swing. A from-scratch runtime can, in exchange for accepting compat risk on userland that introspects globals before touching them.

3. **Some they can't do without breaking promises.**
   - **Static linking**: Debian/Fedora packaging policy forbids it — distros want to swap OpenSSL for CVEs without rebuilding Node. Bun ships static because it's distributed direct from `bun.sh/install`.
   - **Narrower snapshot**: Node's snapshot is shared across `node`, `--eval`, `vm.Script`, workers. Specializing it means either binary bloat (multiple snapshots) or build-at-install — and the productized "build-at-install snapshot" already exists as SEA, which is opt-in. Changing the default breaks embedders.
   - **Lazy OpenSSL init**: tracked off and on, stalled on FIPS mode (must be configured pre-crypto), eager `globalThis.crypto` (Web Crypto is a spec-visible global), and OpenSSL thread callbacks needing to be installed before any worker spawns. Not impossible, but the Node team has chosen predictability over ~3 ms.

## Third-party analysis (quoted from nodejs/performance#180)

From Daniel Lemire (TSC, perf-focused), in [#180][perf180]:

> "10 ms is quite a large effect. Especially if you account for the fact that bun can print 'hello' in 5 ms."

> "Bun is 58 MB and it runs in about 7 ms on my mac with the same benchmark. One maybe significant difference between bun and node is that node depends on Core Foundation whereas bun does not appear to do so..."

> "So I am back at my theory: this is a case where dynamic linking and rebasing is expensive under macOS for some reason."

From billywhizz (independent runtime author of `lo` / `just-js`), [#180][perf180], after the M3 Max flamegraph diff:

> "most of the overhead in InitializeOncePerProcessInternal seems to be coming from OpenSSL initialization. Most of the overhead in NodeMainInstance::NodeMainInstance constructor seems to be coming from v8 snapshot initialization."

> "bun is crazy fast for a micro bench like this. i think is more to do with JSC than anything. i have tried building a minimal runtime on v8 for macos and best i can do is ~15 ms, which is roughly same as deno. while bun on same hardware is ~7 ms. on linux the situation is the opposite ime — bun/JSC is almost 2x slower than a minimal v8 runtime."

**JSC vs V8 is a big lever on macOS specifically**, not in general — JSC is not a free lunch cross-platform.

From Geoffrey Booth (ESM lead), on config-file proposals in [#53787][issue53787]:

> "The recent .env file support was meant as a bridge to this; that effort got us the ability to parse JSON files without needing to start V8."

From isaacs (npm originator), [#53787][issue53787]:

> "We all hate json. No comments, excessive quoting, no multi line strings, no trailing commas, etc. But: it's specified very clearly (unlike ini, which is not specified at all); it's built into the language; It's FAST, like, omg wow, much faster than yaml or toml, not even close."

## Sources

Every number above comes from the nodejs/performance startup thread, one of the landmark PRs listed in the timeline, or Node's own snapshot README; the link definitions below resolve those references.

[perf180]: https://github.com/nodejs/performance/issues/180
[pr56275]: https://github.com/nodejs/node/pull/56275
[pr65526]: https://github.com/nodejs/node/pull/65526
[pr65549]: https://github.com/nodejs/node/pull/65549
[pr65795]: https://github.com/nodejs/node/pull/65795
[pr65796]: https://github.com/nodejs/node/pull/65796
[pr65484]: https://github.com/nodejs/node/pull/65484
[hashdos2026]: https://nodejs.org/en/blog/vulnerability/march-2026-hashdos
[pr44493]: https://github.com/nodejs/node/pull/44493
[issue44715]: https://github.com/nodejs/node/issues/44715
[denolzld]: https://github.com/denoland/deno/pull/35341
[pr45659]: https://github.com/nodejs/node/pull/45659
[pr45716]: https://github.com/nodejs/node/pull/45716
[pr42466]: https://github.com/nodejs/node/pull/42466
[pr59550]: https://github.com/nodejs/node/pull/59550
[pr61769]: https://github.com/nodejs/node/pull/61769
[pr59473]: https://github.com/nodejs/node/pull/59473
[pr27321]: https://github.com/nodejs/node/pull/27321
[pr28181]: https://github.com/nodejs/node/pull/28181
[pr62267]: https://github.com/nodejs/node/pull/62267
[pr65336]: https://github.com/nodejs/node/pull/65336
[pr59517]: https://github.com/nodejs/node/pull/59517
[pr56980]: https://github.com/nodejs/node/pull/56980
[pr57307]: https://github.com/nodejs/node/pull/57307
[pr42566]: https://github.com/nodejs/node/issues/42566
[issue35711]: https://github.com/nodejs/node/issues/35711
[issue44014]: https://github.com/nodejs/node/issues/44014
[issue53787]: https://github.com/nodejs/node/issues/53787
[snapREADME]: https://github.com/nodejs/node/blob/main/tools/snapshot/README.md
[preExec]: https://github.com/nodejs/node/blob/main/lib/internal/process/pre_execution.js
[nodecc]: https://github.com/nodejs/node/blob/main/src/node.cc
[quictls]: https://github.com/quictls/openssl

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Initial publication.
- 2026-08-28 — Trimmed to the measured findings and current behavior.
- 2026-09-04 — Added the v26.7 decomposition against Deno 2.9 and Bun on macOS and Linux: dyld weak-def coalescing and `atexit`/`dladdr` on macOS, and V8's per-isolate rapidhash prime search on Node 25+ (absent from Node 24), with the `V8_USE_DEFAULT_HASHER_SECRET=1` build verified on Linux.
- 2026-09-04 — Added the macOS build verification: six variants of Node main built on macOS 15 arm64 runners and benchmarked interleaved on one host, confirming the weak-def, `atexit` and hasher attributions and putting the patched Node at 20.7 ms `hello.js` against Deno 2.9 at 11.6 ms.
- 2026-09-04 — **REVERSAL:** the doc previously recommended building Node with `V8_USE_DEFAULT_HASHER_SECRET=1`. It must not: Node's CVE-2026-21717 fix derives its array-index hash multipliers from the random rapidhash secrets, and Node's own write-up requires `v8_use_default_hasher_secret = false`. The define build now stands only as a measurement of the generation's cost. Added the actual fix: a 128-bit `mul_mod` in V8's `secret.h`, bit-identical secrets, 2,569 → 148 µs standalone. Noted that Deno builds with the same setting and pays the same cost.
- 2026-09-04 — Verified the `mul_mod` fix inside a Node build on Linux (`-e 0` 19.75 → 18.09 ms). Added the call-graph split of the remaining Linux start, the V8-entropy-through-OpenSSL finding with a measured fix (0.7 ms), the export-table and 74-addon import check for the visibility flags, the CoreFoundation attribution on macOS, and two measured non-wins (snapshotting `stream`/`net`, RELR).
- 2026-09-04 — Self-review of the two proposed fixes. The 128-bit `mul_mod` does not link under clang-cl on Windows (`__umodti3` is absent from clang's Windows runtime, measured on x64 and arm64 runners), so the V8 patch keeps the loop there and the fast path is guarded with `!defined(_WIN32)`. The entropy change keeps the eager CSPRNG check for user-supplied OpenSSL configurations, which `test-crypto-no-algorithm` pins; full `parallel`/`sequential`/`message`/`es-module` suite comparisons on Linux (5,228 tests) show no new failure for either change. Corrected libuv's Windows random source (`RtlGenRandom`, not `BCryptGenRandom`).
- 2026-09-04 — Added the macOS measurement of the two fixes built on a macOS 15 arm64 runner and benchmarked interleaved: 1.4 ms per isolate for the `mul_mod` change (0.85 ms per Worker spawn), 0.6 ms per process for the entropy change, `-e 0` 30.8 → 29.0 ms min.
- 2026-09-04 — Both fixes are now proposed upstream: the `mul_mod` change as [#65795][pr65795], the entropy change as [#65796][pr65796]. Later that day #65795 was closed: Node does not float a V8 patch that could land in V8 first, so that change is not proposed anywhere at present.
- 2026-09-04 — Entropy fix revised after review on [#65796][pr65796]: the seeding check is keyed on `OSSL_PROVIDER_available` instead of on how the OpenSSL configuration was supplied (the OPENSSLDIR file is read with no flag, and `--openssl-legacy-provider` needs the default provider active first), AIX keeps OpenSSL's CSPRNG for V8, and the `addons` suite joins the comparison; Linux and macOS numbers re-measured for the revised build.
- 2026-09-04 — Both builds run over 59 OpenSSL configuration and flag cases on Linux and 52 on macOS: two further behavior differences recorded (an unavailable `[random]` DRBG now fails at first use instead of aborting at startup; the DRBGs live in the secure heap and Workers no longer abort the process on an exhausted one), with tests.
- 2026-09-04 — `mul_mod` equivalence re-run at 50,000 seeds per toolchain (gcc and clang, x64 and arm64, Linux and macOS) and at 3,000 seeds on s390x, ppc64le, riscv64 and aarch64 under QEMU, with a UBSan and ASan pass; the earlier 2,000-seed figure and its Windows wording are replaced, since the 128-bit form does not build on Windows and the loop stays there.
- 2026-09-04 — Corrections: #44715 was closed over Instruments' heap recorder, not ICU; #62267 never landed; #65336 (worker threads from the built-in snapshot) landed today and supersedes the per-Worker figures here.
