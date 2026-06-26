# Worker WPT conformance harness

Runs a pinned, vendored slice of [web-platform-tests](https://github.com/web-platform-tests/wpt)
`.any.js` tests — the `webmessaging` battery, the structured-clone battery over a
`MessageChannel`, and the worker-scope event tests — **through nub's runtime**, so the
globals under test are nub's polyfilled `Worker`, `MessageChannel`, `MessagePort`,
`MessageEvent`, and `structuredClone` (`runtime/worker-polyfill.mjs`). It is the
conformance gate for nub's browser-shape `Worker`.

The design follows Node's `test/common/wpt.js` model: a vendored WPT subset pinned to a
commit, a per-module **status file** (skip / expected-fail / pass), and a
subprocess-per-file driver that loads the **real** `testharness.js` (not a hand-rolled
shim — the shim is what inflated an earlier prototype's fail count with `node:vm`
free-identifier artifacts).

## Layout

```
tests/worker-wpt/
  wpt/                      vendored WPT subset, pinned (see wpt/WPT_COMMIT)
    WPT_COMMIT              the upstream WPT commit this slice is pinned to
    LICENSE.md             WPT's 3-Clause BSD license (required for redistribution)
    resources/testharness.js
    webmessaging/…         MessageChannel / MessagePort / MessageEvent / postMessage
    html/…/messagechannel.any.js   the structured-clone battery driver
    workers/Worker-*.any.js        worker-scope event tests
  status.json              per-file skip / expected-fail expectations
  harness/
    run-wpt.mjs            orchestrator: runs each file, compares to status.json
    run-file.mjs           per-file driver (loads real testharness.js under nub)
```

## Run

```sh
nub tests/worker-wpt/harness/run-wpt.mjs                  # whole matrix
nub tests/worker-wpt/harness/run-wpt.mjs --filter webmessaging
WPT_NUB=/path/to/nub node tests/worker-wpt/harness/run-wpt.mjs
```

The runner discovers the nub binary from `$WPT_NUB`, else the dev `target/fast/nub` /
`target/release/nub` beside the repo, else `nub` on `PATH`. **The binary must sit next
to a sibling `runtime/` dir** so nub can locate its preload (the worktree's own
`target/fast/nub` does; a bare `/tmp` copy does not).

The run is **green** iff every subtest's outcome matches the status file: a `skip` file
is not run, an `expected-fail` subtest must fail, every other subtest must pass. An
*unexpected pass* on an expected-fail (a divergence got fixed) is reported so the status
file can be tightened, but does not fail the run; an *unexpected fail* (a regression)
fails it.

A `fail` entry may carry a `versioned` list of `{ minMajor?, maxMajor?, expected, note }`
for divergences that exist only on a specific Node line — those `expected` names are
treated as expected-fail only when the running Node's major is in `[minMajor, maxMajor]`
(either bound optional/inclusive), and pass-as-normal on every other version. This is for
behavior inherited from a particular Node release, e.g. `structuredClone(File)` losing its
File-ness on Node 22 (fixed in Node 24) — see the battery entry in `status.json`.

## How a file runs

`run-file.mjs` parses the `// META:` block, sets `self` + `GLOBAL.isWindow()`, loads the
real `testharness.js` via `vm.runInThisContext`, then the `META: script=` includes and
the test body — all in one realm under nub. Results are harvested through the harness's
own `add_result_callback` / `add_completion_callback` and emitted as one
`__WPT_RESULT__`-framed JSON line the orchestrator parses.

- **window-scope files** (`global=window,…`) run in the driver's main realm.
- **worker-scope files** (`global=worker` only) run the harness + body **inside a real
  nub `Worker`** (a `data:` module), so the worker-side globals
  (`self.addEventListener` / `dispatchEvent` / `onmessage`) are genuinely exercised — the
  thing the earlier prototype could not reach.

## Node-version tiers

nub's polyfill takes different paths per tier — the **fast tier** (Node 22.15+, sync
`module.registerHooks`) and the **compat tier** (18.19–22.14, async loader-worker) — so a
green run on one modern Node masks tier defects. The `wpt-worker` CI leg
(`.github/workflows/wpt-worker.yml`) runs the matrix across both tiers (Node 18.19, 20,
22.15, and the latest major). Run locally across versions with
`PATH="$HOME/.nvm/versions/node/v20.19.0/bin:$PATH" nub …`.

## Updating the pinned WPT subset

Re-copy the relevant files from a fresh WPT checkout, update `wpt/WPT_COMMIT`, then re-run
the matrix and reconcile `status.json` against any new/changed subtests. Keep the slice
small and messaging/clone-focused — DOM, `SharedWorker`, and blob-URL-document tests are
out of scope (no DOM substrate on Node) and stay un-vendored.
