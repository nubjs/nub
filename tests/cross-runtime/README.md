# Cross-runtime Node-compatibility benchmark

This harness runs Node's own test suite — the whole `test/` tree of a Node release, the same corpus Deno vendors as [`denoland/node_test`](https://github.com/denoland/node_test) — identically against `node`, `nub`, `bun`, and `deno`, and reports pass rates per runtime under several explicit scoring lenses. Nobody curates the file list: every runtime runs the same files, with the same flags, under the same pass criterion, and every runtime's failures are published by name in `results.json`.

## What's pinned (so it reproduces forever)

- **Corpus:** a full checkout of Node **v26.7.0** (tag commit `b4f23d3619c98bed09af93a21192f6080197a8c6`) — the `tests/node-suite` submodule. The tests are Node's `test/` tree, byte-for-byte what Deno's [`vendor.ts`](https://github.com/denoland/node_test/blob/main/vendor.ts) vendors for a version; the rest of the checkout matters because Node's own runner assumes it: tests read `doc/api/*.md`, `deps/npm` and `benchmark/` relative to the root, and `test/ffi/` needs its fixture library compiled in place (`npx node-gyp rebuild` in `test/ffi/fixture_library`). Deno's own corpus tracked 26.5.1 at the time of measurement. The harness reads the version from `src/node_version.h` (or `node_version.ts` in a node_test-shaped tree).
- **Deno's skip list + per-test expected-failure config:** [`config.jsonc`](./config.jsonc), vendored verbatim from `denoland/deno` `tests/node_compat/config.jsonc` at `main` commit `98f9507a` (2026-08-21; written against the 26.5.1 corpus). It only affects the `deno` lens. (MIT.)
- **The engine-specific class:** [`engine-specific.txt`](./engine-specific.txt), 718 corpus paths selected by the seven rules below. It only affects the `*NoEngine` lenses.
- **The runner:** [`run.mjs`](./run.mjs) — a reimplementation of Deno's Rust runner (`tests/node_compat/runner/mod.rs`): same pass criterion (child exit 0 = pass; a Deno expected-failure entry passes only when it fails in exactly the configured way), same env (`NODE_TEST_KNOWN_GLOBALS=0`, `NODE_SKIP_FLAG_CHECK=1`, `NO_COLOR=1`), the test's own `// Flags:` directive passed on the command line to node, nub and bun alike (bun ignores `NODE_OPTIONS` but accepts Node flags as arguments), Deno's own flag translation for deno, same cwd/path model (cwd = corpus root, path = `test/<dir>/<file>`), 20 s timeout on macOS / 10 s elsewhere with process-group kill (5 min for a `wpt/` wrapper, which runs hundreds of WPT files in one process), and one symmetric retry of every failure at low parallelism. `test/pseudo-tty/` runs the way Node's runner runs it: inside a pseudo-terminal ([`pty-spawn.py`](./pty-spawn.py), a port of Node's `tools/pseudo-tty.py`), with the test's `// Env:` line applied and its sibling `.in` file as stdin, judged by matching the output line-by-line against the sibling `.out` file rather than by exit code — for every runtime alike.

## Runtime versions we measured

| Runtime | Version |
|---------|---------|
| node    | v26.7.0 |
| nub     | v0.7.5 (release build of `main` at `18a7fd2124`), augmented default mode, on Node v26.7.0 |
| bun     | 1.4.0 |
| deno    | 2.9.5 |
| node25  | v25.9.0 — the latest Node 25, run on the Node 26 corpus to size version skew |

macOS arm64, 2026-08-22. The retry pass flipped 1 node, 1 nub, 4 bun, 3 deno and 0 node25 verdicts, which bounds the load effect.

## Reproduce it yourself

```sh
# 1. Node 26.7.0 first on PATH (nub augments whatever Node it resolves).
export PATH="$HOME/.nvm/versions/node/v26.7.0/bin:$PATH"; node --version   # v26.7.0

# 2. The corpus: a full Node checkout at v26.7.0 (this repo's tests/node-suite submodule is one),
#    with the ffi fixture library compiled in place.
git clone --depth 1 --branch v26.7.0 https://github.com/nodejs/node /tmp/node-26.7.0
(cd /tmp/node-26.7.0/test/ffi/fixture_library && npx node-gyp rebuild)

# 3. Run. --include-excluded runs Deno's skipped tests too, so one pass yields every lens.
node tests/cross-runtime/run.mjs --corpus /tmp/node-26.7.0 --include-excluded \
  --bin nub=target/release/nub --bin bun=$(which bun) --bin deno=$(which deno) \
  --runtimes node,nub,bun,deno,node25 --bin node25=$HOME/.nvm/versions/node/v25.9.0/bin/node
```

Useful flags: `--runtimes node,nub` for a subset; `--only <substring>` / `--dirs parallel,sequential` / `--files <list>` to restrict the file list; `--no-retry` to skip the retry pass; `--parallelism N`; `--out <file>`; `--files <list> --merge results.json` re-runs a subset and folds the fresh verdicts into an existing results file, recomputing every score (the merge refuses a runtime that lacks a verdict for some file).

## The lenses

Every lens is computed from one run over one fixed file list; a lens only changes which files are *counted*, never which are *run*, and it applies to every runtime alike.

| Lens | Files | What it is |
|------|-------|------------|
| `denoExclusions` | 5,078 | Deno's directory set (its `IGNORED_TEST_DIRS`, nothing added or removed — `ffi/` and `test426/` included) minus the tests Deno's `config.jsonc` marks `ignore: true` or `darwin: false`. This is how Deno scores itself on its viewer, minus its `flaky` retries. |
| `bunUniverse` | 4,760 | `parallel/` + `sequential/`, nothing skipped. The universe bun.com's tracker draws its dots from, minus the 59 `js-native-api`/`node-api` addon tests that need a compiled addon per test. |
| `fullCorpus` | 5,664 | Every directory Deno collects plus `async-hooks/`, `report/` and `wpt/` (Node's 25 wrappers over its in-tree Web Platform Tests subset, with Node's expected-failure lists), nothing skipped. One caveat on `wpt/`: a wrapper drives Node's own WPT runner (`test/common/wpt.js`, worker threads and `vm` contexts), so a runtime that cannot host that runner fails the wrapper without its web APIs being exercised — Deno is in that position. |
| `fullCorpusNoEngine` / `bunUniverseNoEngine` | 4,946 / 4,111 | The same two minus the engine-specific class. |
| `engineSpecificOnly` | 718 | The engine-specific class alone. |
| `perDirectory` | — | `fullCorpus` broken out per top-level directory. |

Each lens reports two numbers per runtime. **Node-relative** (`pass / nodePass`): of the tests real Node 26.7.0 passes on this machine, how many does the runtime pass — numerator and denominator from the same set, so tests Node itself fails here (no pty, no network, Linux-only) cancel out. **Raw** (`rawPass / files`): passes over every file in the lens, which is how bun.com's tracker and Deno's viewer count. Node-relative is the headline; raw is there so the other trackers' numbers can be compared like for like.

### The engine-specific class

Tests that exercise the V8 engine or Node's private internals rather than Node's public API. A different engine cannot be expected to reproduce them, so a fair cross-runtime comparison drops them — symmetrically, for every runtime. The rules, applied to each test's source and its `// Flags:` line:

1. **Node-private internals** — the body imports `internal/…`, or calls `internalBinding(` / `process.binding(`.
2. **V8 natives syntax and V8-only flags** — `--allow-natives-syntax`, `--expose-externalize-string`, `--jitless`, `--harmony-*`, `--js-*`, `--stack-size`, `--max-semi-space-size`, `--predictable`, `--no-opt`, `--always-turbofan`, `--single-threaded`, sweeping flags; or `%Fn(` natives / `externalizeString` in the body.
3. **`node:v8`, heap dumps, GC timing** — imports `node:v8` or is a `test-v8-*`; `measureMemory`; heap-snapshot / heap-dump / heap-prof / cpu-prof tests; worker `resourceLimits` on a V8 heap space; `gc()` in a gc/leak/memory test.
4. **V8 startup snapshot** — `startupSnapshot`, `--build-snapshot`, `--snapshot-blob`, `test-snapshot-*`.
5. **V8 Inspector protocol** — inspector tests, `node:inspector`, `--inspect*`.
6. **V8 platform tracing** — `test-trace-events-*`, `--trace-gc|events|turbo|opt|deopt|ic|maps`, `node:trace_events`.
7. **V8-authored error text and stack limits** — stack-overflow / stack-size tests, or an assertion on one of V8's own message literals (`Maximum call stack size exceeded`, `Unexpected token`, `Cannot read properties of undefined`, `Invalid array length`, …).

Deliberately **not** excluded, because they are Node's public surface: the permission model, SEA, `node:sqlite`, WASI, QUIC, the VFS, `module.registerHooks`, `node:test`, the experimental CLI flags. A runtime that lacks them is less Node-compatible, and the lens says so.

## Results (2026-08-22, Node 26.7.0 corpus)

Node-relative pass rate (raw in parentheses):

| Lens | files / node passes | nub | deno 2.9.5 | bun 1.4.0 | node 25.9.0 |
|------|---------------------|-----|------------|-----------|-------------|
| `denoExclusions` | 5,078 / 5,044 | **98.14%** (97.52) | 74.19% (73.89) | 68.12% (67.84) | 90.17% (89.60) |
| `bunUniverse` | 4,760 / 4,734 | **97.89%** (97.35) | 71.78% (71.49) | 69.81% (69.60) | 89.56% (89.10) |
| `fullCorpus` | 5,664 / 5,614 | **97.08%** (96.26) | 68.10% (67.67) | 63.80% (63.47) | 89.97% (89.21) |
| `fullCorpusNoEngine` | 4,946 / 4,902 | **97.04%** (96.22) | 71.69% (71.25) | 69.50% (69.15) | 90.04% (89.28) |
| `bunUniverseNoEngine` | 4,111 / 4,089 | **97.92%** (97.40) | 76.08% (75.80) | 76.82% (76.60) | 89.56% (89.10) |
| `engineSpecificOnly` | 718 / 712 | 97.33% (96.52) | 43.40% (43.04) | 24.58% (24.37) | 89.47% (88.72) |

Per directory, the three that only run properly with the full checkout, the pty and the compiled fixture (node-relative passes / Node's passes): `pseudo-tty/` nub 28 / 31, deno 15, bun 12; `wpt/` nub 24 / 25, bun 6, deno 0 (see the caveat above); `ffi/` nub 11 / 13, bun 13, deno 13 (both skip every `ffi` test — `common.skip()` exits 0 — which counts as a pass under Node's own convention).

**Node itself** fails 50 of the 5,664 files on this host: six `system-ca/` (need Node's test CA in the login keychain), four `internet/`, eight `test-runner/` reporter snapshots, five timeouts under load, two ShadowRealm GC crashes, and a tail of single cases. They drop out of every node-relative figure for every runtime alike.

`nubVsNode.nubRegressions` — the honest nub number — is **164** tests real Node 26.7.0 passes and nub fails, by name in `results.json`, with the tail of each failure's output (machine paths replaced by `<corpus>`, `<repo>`, `<nub>` and `~` before the tail is cut, so a cut cannot bisect a path). 51 are the permission model (nub refuses `--permission` without `--allow-addons` because its transpiler is a native addon), 17 are `module-hooks/` chain tests that see nub's own hooks, 14 are `process.report` (nub adds `process.versions.nub`, so `componentVersions` no longer equals `process.versions`), 3 are Node 26's `--enable-source-maps` assert regression ([nodejs/node#63169](https://github.com/nodejs/node/issues/63169)) by the classifier's narrow rule (11 of the 164 flip to pass on a build that withholds the flag, measured with the build from nubjs/nub#784), 4 are output snapshots under a pty or the WPT runner where nub's preload adds stack frames, 2 are the checkout's `tsconfig.json` `paths` (which nub honours for `require()`) mapping `internal/*` onto `lib/`, the rest are stack-snapshot, compile-cache and loader-interaction divergences. `tests/node-compat-config.jsonc` carries the classification; the entries it could not classify are marked `untriaged` rather than `ignore`, and the gate reports them apart.

**Version skew, measured.** The `node25` column is Node 25.9.0 on the Node 26.7.0 corpus: 89.6% of the tests Node 26.7.0 passes under the `bunUniverse` lens (89.1% raw). On the Node 26.3.0 corpus (the version bun.com's tracker lists) the same Node 25.9.0 scores 94.7% node-relative / 93.8% raw (4,271 of 4,552 matched tests; run `--corpus` against a 26.3.0 tree to reproduce). A corpus bump of four minor versions costs the previous major ~5 points; that is the scale of "version skew" any comparison across corpus versions carries.

## How the other trackers count

Stated so the numbers above can be compared with them, not to score them: Deno's viewer reports passes over the collected tests minus its `ignore`/platform-`false` entries (442 entries at 26.5.1), and counts a `flaky` retry as a pass; that is the `denoExclusions` lens here. bun.com's tracker lists every Node 26.3.0 `parallel`/`sequential`/`js-native-api`/`node-api` test and marks a test passing when a file of that name exists in Bun's repository under `test/js/node/test/`; it runs nothing, and Bun's staging script adds a file only once it exits 0 under Bun, so the count is of tests Bun has landed rather than of a verbatim run. The `bunUniverse` lens here runs the same files unmodified.

## How to read the results

`results.json` carries:

- `meta` — corpus version, each runtime's binary and version (and, for nub, the Node it resolved), parallelism, timeout, and how many verdicts the retry pass flipped per runtime. Paths are scrubbed (`<corpus>`, `<repo>`, `<nub>` for the checkout the nub binary came from, `~`) before any output is truncated, so the file carries nothing machine-specific; the check is an unanchored grep for the username and the checkout directory names, not for the leading `/Users/`.
- `perRuntime` — raw pass / fail / timeout over the full file list.
- `scores` — every lens above, node-relative and raw.
- `nubVsNode` — `nubRegressions` (node passes, nub fails) and `nubFixesVsNode` (the inverse).
- `fails` — every runtime's failures by filename. Publishing our own failures by name is the anti-cherry-pick proof.
- `results` — the per-file, per-runtime verdict, with the tail of the output for every failure.

Don't report a single headline percentage as "nub's compatibility" without naming the lens and the Node version; the raw pass rate is capped by corpus-vs-binary alignment and invites denominator games. The defensible statement is the delta: *"on Node 26.7.0, nub passes every test Node passes except the N listed"*, shown next to the lens table.

[`results-prior-versions.json`](./results-prior-versions.json) is a historical run on the Node **25.8.1** corpus (bun 1.3.14, deno 2.8.1, 2026-08-20), kept for the before/after comparison recorded in git history; it is not comparable with the table above.

A regenerated `results.json` is only half the update. The published figures are hand-copied into the `COMPAT` array in `site/src/app/(home)/page.tsx` and into the compatibility sentence in `site/content/blog/introducing-nub.mdx`; nothing reads this file at build time, so both drift silently until someone copies them across.

## Attribution

The corpus is Node's (MIT, Node.js contributors); `config.jsonc` is Deno's (MIT), redistributed for reproducible measurement.
