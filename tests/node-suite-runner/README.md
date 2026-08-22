# Node test-suite runner

Executes Node's own test corpus against an arbitrary runtime — `node`, `nub`,
`nub --node` — and reports a pass rate. Two jobs:

1. Measure nub's Node compatibility on the corpus that upstream Node itself
   gates on, in the dual modes [`wiki/research/node-test-suite-leverage.md`](../../wiki/research/node-test-suite-leverage.md) specifies:
   `nub --node` (passthrough integrity) and `nub` (additivity).
2. Reconcile [`../node-compat-config.jsonc`](../node-compat-config.jsonc)
   against a newer Node release, so the allowlist tracks upstream instead of
   drifting.

The corpus is the `tests/node-suite` submodule (`nodejs/node`), pinned to a
**release tag** rather than a `main` commit: a floating `main` pin is not
reproducible and its tests target unreleased behavior.

## Running

```sh
git submodule update --init --depth 1 tests/node-suite

node tests/node-suite-runner/run-suite.mjs \
  --runtime "$(command -v node)" --label node \
  --corpus tests/node-suite --list <list-file> \
  --out /tmp/node.json --jobs 4 --timeout 60
```

`--list` is a newline-delimited file of test names. A bare name
(`test-assert.js`) resolves against `parallel/` then `sequential/`; a
dir-prefixed name (`es-module/test-esm-basic-imports.mjs`) resolves literally.

Useful flags: `--runtime-args` (e.g. `--node`), `--node-options` (sets
`NODE_OPTIONS` for the child, to isolate a single flag's effect), `--resume`
(continue from the `.jsonl` checkpoint), `--limit`.

**Pin the Node version when measuring nub.** nub resolves its Node from `PATH`,
so an unpinned run scores nub against whatever Node happens to be installed and
attributes that version's skew to nub:

```sh
PATH="/path/to/node-v26.7.0/bin:$PATH" node tests/node-suite-runner/run-suite.mjs \
  --runtime "$(command -v nub)" --runtime-args --node --label nub-node ...
```

## Fidelity

The runner reproduces `tools/test.py` + `test/testpy/__init__.py` semantics.
Each decision below is grounded in that source, not guessed:

| Behavior | Why |
|---|---|
| cwd = corpus root | `test.py` never `chdir`s; it is invoked from the repo root and passes an absolute test path |
| `NODE_SKIP_FLAG_CHECK` deliberately **unset** | `test.py` sets it because it parses `// Flags:`/`// Env:` itself. Leaving it unset lets `test/common/index.js` do the parse-and-respawn instead — same semantics, no reimplemented parser |
| `TEST_THREAD_ID` / `TEST_SERIAL_ID` per worker | `common/tmpdir.js` derives `.tmp.<id>` from these; sharing one would make concurrent tests collide |
| `sequential/` serial, everything else concurrent | matches upstream; note only `sequential` is forced serial |
| pass ⇔ exit code 0 | upstream's own criterion |
| `detached: true` on each child | so the timeout path can `kill(-pid)` the child's whole tree. Without it `kill(-pid)` signals whatever group owns that id — which can be the runner's own, and did kill a run |

Results stream to a `.jsonl` checkpoint as they complete, so a crash costs only
the in-flight test and `--resume` picks up where it stopped.

## Interpreting a number

**Always run the matching upstream Node as a control.** Some tests cannot pass
outside a built Node checkout (they want `out/Release`, a fixture the tarball
omits, or privileges a container lacks). That is a property of the environment,
not of the runtime under test — so the control run, not 100%, is the ceiling a
runtime's score should be read against. Measured here, Node 26.3.0 scores
98.67% (4,459/4,519) on its own `parallel` + `sequential` tests.

Two corpus caveats when comparing to a published figure:

- `js-native-api/` and `node-api/` tests need addons compiled per ABI, so they
  are not runnable here without a build step.
- `test-eslint-*` and `test-corepack-version.js` exist in Node's git tree but
  are stripped from the release tarball. They test Node's own lint rules, not
  runtime behavior.

## Why published pass-rates are not comparable

The two public trackers score different test sets by different rules, so their
percentages cannot be read against each other or against ours without
restating them on a common corpus.

| | Bun tracker | Deno `node-test-viewer` |
|---|---|---|
| Node version | 26.3.0 | 26.5.1 |
| directories | 4, exhaustively | 17, selectively |
| set size | 4,608 | 5,482 run |
| how a test "passes" | the file exists in Bun's repo | the test is executed |
| ignored tests | counted in the denominator | **removed** from it (5,482 − 442 = 5,040) |

The two sets share 3,375 members — a Jaccard overlap of **67.3%**. Bun's set
adds 1,233 the Deno config never lists (1,129 of them `parallel/`, plus the 59
napi directories); Deno's adds 409 from directories Bun does not touch at all
(`es-module`, `module-hooks`, `pummel`, `test-runner`, `client-proxy`, `sea`,
`internet`, `wasi`, `pseudo-tty`, `abort`, `system-ca`, `wasm-allocation`).

Restated on the 5,041 tests this config covers, measured 2026-08-21/22 against
the same Node 26.5.1 corpus:

| runtime | pass | rate |
|---|---:|---:|
| Node 26.5.1 (control) | 4,973 | 98.65% |
| `nub --node` | 4,973 | 98.65% |
| `nub` augmented | 4,876 | 96.73% |
| Deno 2.9.5 | 3,489 | 69.21% |

Deno's own published figure is 74.64%, not 69.21%, because it drops its 442
ignore-listed tests from the denominator; applying that same convention to
these four directories gives 75.29%. Neither number is wrong — they answer
different questions, which is the point. Ours counts every test in the corpus.

## Measurement integrity

A compatibility percentage is easy to move without improving compatibility.
These four rules are what make ours mean something, and each exists because
the opposite was found in a shipping tracker (including, on rule 3, in this
repo's own runner).

1. **Test files are never modified.** The corpus is the pinned submodule,
   verbatim. No commented-out assertion, no injected `common.skip()`, no
   expectation rewritten to match what nub happens to do. If a test fails, it
   fails.
2. **Exemptions are declared, not embedded.** A known divergence goes in
   `node-compat-config.jsonc` with a reason, where it is countable and
   reviewable. An exemption living inside a test file reads as a pass.
3. **Both runtimes get the same environment.** An exemption applied to nub but
   not to node measures the exemption. This runner previously set
   `NODE_TEST_KNOWN_GLOBALS=0` for nub only, which disabled `test/common`'s
   global-leak check for exactly the runtime whose preload could trip it.
4. **The headline rate counts every test in the corpus.** Moving a test to the
   ignore list must not be able to raise the number. The curated rate, which
   excludes declared divergences, is reported second and never quoted alone.

A corollary for reading anyone's figure, ours included: a pass-rate is only
comparable to another when the corpus, the ignore convention, and whether the
tests were executed at all are the same. They usually are not.

## Reconciling the allowlist

```sh
node tests/node-suite-runner/reconcile-config.mjs \
  tests/node-compat-config.jsonc /tmp/node267.json /tmp/nub.json \
  tests/node-compat-config.jsonc /tmp/reconcile-report.md
```

Three rules keep this from being a blind regeneration:

1. **Existing entries are preserved verbatim.** Their `reason` strings are
   hand-written curation; a generated run must not clobber them.
2. **A test is added only if upstream Node passes it here.** A test real Node
   fails measures the environment, not nub — adding it as `ignore` would pad
   the file with noise.
3. **Disagreements are reported, never silently flipped.** An active entry that
   now fails is a regression for a human to look at; an ignored entry that now
   passes is an un-ignore candidate. Both land in the report.
