# Cross-runtime Node-compatibility benchmark

This harness runs Node's own test suite — the whole `test/` tree of a Node release, the same corpus Deno vendors as [`denoland/node_test`](https://github.com/denoland/node_test) — identically against `node`, `nub`, `bun`, and `deno`, and reports pass rates per runtime under several explicit scoring lenses. Nobody curates the file list: every runtime runs the same files, with the same flags, under the same pass criterion, and every runtime's failures are published by name in `results.json`. One accommodation is symmetric by design: a `node:test`-based file runs under each runtime's own test mode where it needs one (`deno test`, `bun test`) — Node runs those files as plain scripts, Deno and Bun register their tests only inside their runners, and both runtimes' own compat suites make the same switch.

## What's pinned (so it reproduces forever)

- **Corpus:** a full checkout of Node **v26.7.0** (tag commit `b4f23d3619c98bed09af93a21192f6080197a8c6`) — the `tests/node-suite` submodule. The tests are Node's `test/` tree, byte-for-byte what Deno's [`vendor.ts`](https://github.com/denoland/node_test/blob/main/vendor.ts) vendors for a version; the rest of the checkout matters because Node's own runner assumes it: tests read `doc/api/*.md`, `deps/npm` and `benchmark/` relative to the root, and `test/ffi/` needs its fixture library compiled in place (`npx node-gyp rebuild` in `test/ffi/fixture_library`). Deno's own corpus tracked 26.5.1 at the time of measurement. The harness reads the version from `src/node_version.h` (or `node_version.ts` in a node_test-shaped tree).
- **Deno's skip list + per-test expected-failure config:** [`config.jsonc`](./config.jsonc), vendored verbatim from `denoland/deno` `tests/node_compat/config.jsonc` at `main` commit `98f9507a` (2026-08-21; written against the 26.5.1 corpus). It only affects the `deno` lens. (MIT.)
- **The engine-specific class:** [`engine-specific.txt`](./engine-specific.txt), 718 corpus paths selected by the seven rules below. It only affects the `*NoEngine` lenses.
- **The runner:** [`run.mjs`](./run.mjs) — a reimplementation of Deno's Rust runner (`tests/node_compat/runner/mod.rs`): same pass criterion (child exit 0 = pass; a Deno expected-failure entry passes only when it fails in exactly the configured way), same env (`NODE_TEST_KNOWN_GLOBALS=0`, `NODE_SKIP_FLAG_CHECK=1`, `NO_COLOR=1`), the test's own `// Flags:` directive passed on the command line to node, nub and bun alike (bun ignores `NODE_OPTIONS` but accepts Node flags as arguments), Deno's own flag translation for deno, same cwd/path model (cwd = corpus root, path = `test/<dir>/<file>`), 20 s timeout on macOS / 10 s elsewhere with process-group kill (5 min for a `wpt/` wrapper, which runs hundreds of WPT files in one process), and one symmetric retry of every failure at low parallelism. `test/pseudo-tty/` runs the way Node's runner runs it: inside a pseudo-terminal ([`pty-spawn.py`](./pty-spawn.py), a port of Node's `tools/pseudo-tty.py`), with the test's `// Env:` line applied and its sibling `.in` file as stdin, judged by matching the output line-by-line against the sibling `.out` file rather than by exit code — for every runtime alike.

## Runtime versions we measured

<!-- versions-table -->
| Runtime | Version |
|---------|---------|
| node    | v26.7.0 |
| nub     | v0.8.0, augmented default mode, on Node v26.7.0 |
| bun     | 1.4.0 |
| deno    | 2.9.5 |
| node25  | v25.9.0 — the latest Node 25, run on the Node 26 corpus to size version skew |
<!-- /versions-table -->

The nub binary is a release build of `main` at `e78a6701dd` plus the `NODE_OPTIONS` coverage-exclude change committed beside this results file. This table and the results table below are generated from `results.json` by [`readme-table.mjs`](./readme-table.mjs).

macOS arm64, 2026-08-22. The retry pass flipped 1 node, 1 nub, 4 bun, 2 deno and 0 node25 verdicts, which bounds the load effect. Bun's verdicts were re-measured 2026-08-30 with the `bun test` accommodation and `BUN_TEST_DRAIN_EVENT_LOOP=1` (see `buildPlainCommand` in `run.mjs`), one full bun pass over the same v26.7.0 checkout: 99 files flipped to pass and none flipped to fail.

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

After a run, `node tests/cross-runtime/readme-table.mjs --write` refreshes the results table below from `results.json` (the pre-push hook runs `--check`). Files under `sequential/` run one at a time after the parallel pool drains, as Node's own runner orders them, for every runtime alike. Useful flags: `--runtimes node,nub` for a subset; `--only <substring>` / `--dirs parallel,sequential` / `--files <list>` to restrict the file list; `--no-retry` to skip the retry pass; `--parallelism N`; `--out <file>`; `--files <list> --merge results.json` re-runs a subset and folds the fresh verdicts into an existing results file, recomputing every score (the merge refuses a runtime that lacks a verdict for some file).

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

## Results (2026-08-28, Node 26.7.0 corpus)

Node-relative pass rate (raw in parentheses). The rows are generated from `results.json` by [`readme-table.mjs`](./readme-table.mjs) (`--write` rewrites them, `--check` fails if they drifted); do not retype them.

<!-- results-table -->
| Lens | files / node passes | nub | deno 2.9.5 | bun 1.4.0 | node 25.9.0 |
|------|---------------------|-----|------------|-----------|-------------|
| `denoExclusions` | 5,078 / 5,046 | **98.45%** (97.87) | 74.16% (73.89) | 70.06% (69.75) | 90.15% (89.62) |
| `bunUniverse` | 4,760 / 4,736 | **98.16%** (97.67) | 71.75% (71.49) | 71.79% (71.55) | 89.55% (89.12) |
| `fullCorpus` | 5,664 / 5,616 | **97.40%** (96.61) | 68.07% (67.67) | 65.58% (65.22) | 89.96% (89.23) |
| `fullCorpusNoEngine` | 4,946 / 4,904 | **97.37%** (96.58) | 71.66% (71.25) | 71.41% (71.03) | 90.03% (89.30) |
| `bunUniverseNoEngine` | 4,111 / 4,091 | **98.22%** (97.74) | 76.04% (75.80) | 78.98% (78.74) | 89.54% (89.13) |
| `engineSpecificOnly` | 718 / 712 | **97.61%** (96.80) | 43.40% (43.04) | 25.42% (25.21) | 89.47% (88.72) |
<!-- /results-table -->

Per directory, the three that only run properly with the full checkout, the pty and the compiled fixture (node-relative passes / Node's passes): `pseudo-tty/` nub 28 / 31, deno 15, bun 12; `wpt/` nub 24 / 25, bun 6, deno 0 (see the caveat above); `ffi/` nub 11 / 13, bun 13, deno 13 (both skip every `ffi` test — `common.skip()` exits 0 — which counts as a pass under Node's own convention).

**Node itself** fails 48 of the 5,664 files on this host: six `system-ca/` (need Node's test CA in the login keychain), four `internet/`, eight `test-runner/` reporter snapshots, five timeouts under load, two `wasm-allocation/`, and a tail of single cases. They drop out of every node-relative figure for every runtime alike.

`nubVsNode.nubRegressions` — the honest nub number — is **146** tests real Node 26.7.0 passes and nub fails, by name in `results.json`, with the tail of each failure's output (machine paths replaced by `<corpus>`, `<repo>`, `<nub>` and `~` before the tail is cut, so a cut cannot bisect a path). 50 are the permission model (nub refuses `--permission` without `--allow-addons` because its transpiler is a native addon), 17 are `module-hooks/` chain tests that see nub's own hooks, 14 are `process.report` (nub adds `process.versions.nub`, so `componentVersions` no longer equals `process.versions`), 18 are stack traces or module graphs that run through nub's preload, 12 are features nub enables by default (`--experimental-import-text`, `--experimental-eventsource`, its own TypeScript loader) that a test expects to be absent, 8 are output snapshots where nub's preload adds frames, 6 are Node's compile cache superseded by nub's transpile cache, 3 are test-runner coverage tests reproduced by `node --enable-source-maps` alone (which nub injects), 3 are the checkout's `tsconfig.json` `paths` (which nub honours for `require()`) mapping `internal/*` onto `lib/`, 3 are `pseudo-tty/` fatal-error traces, 2 are `ffi/` permission tests, and the remaining 10 are single cases: `process.versions.nub`, an extension-probed bare subpath, a grandchild's coverage exclude, the WPT `idlharness`, two `zlib` kMaxLength errors under `--experimental-webstorage`, two warning-order tests and two transpiler source-position tests. `tests/node-compat-config.jsonc` carries the classification, and every entry in it is classified; a divergence the classifier cannot name would be marked `untriaged` rather than `ignore`, and the gate reports those apart.

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

The committed file is a `--merge` composite, not a single run: the bulk of the verdicts come from one full pass, and subsets were re-judged afterwards (the files a harness fix touched, the pairs whose recorded output needed re-scrubbing). `meta.generatedAt` names the last merge; every score is recomputed from the merged record, so the lenses are self-consistent, but a verdict pair may come from different moments of host load — and, for a test whose behaviour depends on absolute path length, from corpus checkouts at different depths (the Unix-socket test above flipped for three runtimes at once between generations for that reason). A passing verdict carries no generation marker; only a failing tail that embeds a `.tmp.<id>` path is auditable after the fact.

Don't report a single headline percentage as "nub's compatibility" without naming the lens and the Node version; the raw pass rate is capped by corpus-vs-binary alignment and invites denominator games. The defensible statement is the delta: *"on Node 26.7.0, nub passes every test Node passes except the N listed"*, shown next to the lens table.

[`results-prior-versions.json`](./results-prior-versions.json) is a historical run on the Node **25.8.1** corpus (bun 1.3.14, deno 2.8.1, 2026-08-20), kept for the before/after comparison recorded in git history; it is not comparable with the table above.

A regenerated `results.json` is only half the update. The published figures are hand-copied into the `COMPAT` array in `site/src/app/(home)/page.tsx` and into the compatibility sentence in `site/content/blog/introducing-nub.mdx`; nothing reads this file at build time. `readme-table.mjs --check` (run by the pre-push hook, including for pushes that touch only those files) fails on a mismatch in any hand-copied figure: both site surfaces' rates, counts, runtime version labels, Node corpus labels and the miss-count prose against `scores.denoExclusions` and `meta`, plus the wiki research doc's lens figures and this README's own retry-flip sentence. The copy step can no longer be skipped silently.

## Wording differences vs behavior differences

Not every failure is a defect a user would feel. A runtime can throw the right error at the right moment and word it differently — JavaScriptCore and V8 phrase the same brand check differently, so an `assert.throws({ message })` fails on text alone. That is worth measuring rather than asserting, so we did: re-run the corpus with failure output retained (one change to `run.mjs` — keep `raw.out` in `judge()`; the pass criterion is untouched), then bucket each failure by which keys differ inside `node:assert`'s `Comparison { }` block.

Forgiving every failure whose *only* difference is the message text moves bun 1.4.0 from 66.8% to 68.1% and deno 2.9.5 from 78.4% to 78.8% (measured on the 2026-08-29 verdicts, before the `bun test` accommodation shifted bun's baseline; the classification of individual failures is unaffected). nub does not move at all: it executes on the stock Node binary, so its error messages *are* Node's, and the count is zero rather than small. Also forgiving a differing or absent `code`/`name` reaches 69.1% and 80.3%.

**The published figures forgive neither, and the second is the reason.** Message text is cosmetic; error identity is not. bun frequently throws a raw JavaScriptCore `TypeError` carrying no `code` property at all, which breaks any program branching on `err.code === 'ERR_INVALID_ARG_TYPE'` — a real incompatibility that a text-only classifier reports as a wording nit.

The message-only failures were read individually rather than sampled — 60 for bun, 21 for deno. Most are pure phrasing: `Maximum call stack size exceeded.` against `…exceeded`, `0x7fffffff` against `2147483647`, eleven `URLSearchParams` tests where the two engines word the same receiver check differently. The rest hide something real behind a message, such as validating a different argument first, or deno's `test-urlpattern.js`, which fails because `URLPattern` does not exist.

## Attribution

The corpus is Node's (MIT, Node.js contributors); `config.jsonc` is Deno's (MIT), redistributed for reproducible measurement.
