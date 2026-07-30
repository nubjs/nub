---
**Status:** v1, 2026-05-28. Empirical — first actual run of Node's resolution test subset under nub. Harness lives at `crates/nub-cli/tests/resolution_compat.rs` and runs in CI as a normal `cargo test`.
**Question:** Does nub's thin resolver-over-Node (the ~100-LOC JS layer in `runtime/preload.mjs` that delegates to Node's native resolver and only adds TS-specific behavior) actually match Node's resolution across Node's own resolution test corpus? Nub's public compatibility claim previously described such a validation layer before one existed; this is that layer.
**Headline answer:** Yes. Across the 56-test resolution subset, nub matches Node on every test whose resolution behavior is a valid parity signal — 47 parity, 2 baseline-skipped, **0 genuine resolution divergences**. The 7 tests where augmented `nub` exits non-zero while bare Node exits zero were each traced — with evidence — to nub's default webstorage augmentation (the injected `--experimental-webstorage` flag plus nub's warning suppression), not to its resolver. The thin-layer-over-Node design is validated.
**Builds on:** [`node-test-suite-leverage.md`](node-test-suite-leverage.md) (the broad dual-mode harness design this is a focused instance of), [`module-resolution.md`](module-resolution.md). A resolution divergence is an additivity violation, so this doubles as an additivity check.
---

# Resolution conformance

## TL;DR

- nub's resolver is intentionally thin: it delegates to Node's native resolver and only adds TS-specific behavior (tsconfig `paths`, extensionless TS probing, `.js→.ts`/`.mjs→.mts` swaps). A34-ROOT asks whether those additions, and the seams where they meet Node's resolver, diverge from Node. The answer is no.
- The harness (`crates/nub-cli/tests/resolution_compat.rs`) discovers the resolution-relevant subset of `tests/node-suite/test/{es-module,parallel}`, runs each test under the exact Node binary nub resolves (`nub node which`, the passthrough baseline) and under augmented `nub`, and asserts nub never *fails a test Node passes* except for an explicitly documented, non-resolution reason.
- Result on 2026-05-28: **47 parity, 2 baseline-skipped, 0 nub-more-permissive, 7 known divergences, 0 undocumented** of 56 discovered. Green.
- All 7 known divergences are nub's webstorage augmentation, proven by a three-way comparison (below). None is a resolution difference.

## Methodology

**Apples-to-apples baseline.** The baseline is not "some Node on `$PATH`" — it is `nub node which`, the exact Node binary nub itself spawns. So the only variable between the two columns is nub's augmentation (resolution hooks + injected flags), never the Node version. This is the same dual-mode idea as [`node-test-suite-leverage.md`](node-test-suite-leverage.md) (`nub --node` passthrough vs augmented `nub`), narrowed to the resolution corpus and using `nub node which` as the passthrough because it is exactly the binary under augmentation.

**Discovery.** `is_resolution_test` keeps files whose names contain `resolve`, `specifier`, `extensionless`, `exports`, `imports`, `self-ref`, `legacymainresolve`, `module-resolution`, `esm-cjs`, or `cjs-esm`, and drops `hook`/`expose`/`loader-mock`/`permission` (the module-hooks API tests are not a resolution-correctness signal because nub *itself* uses those hooks; `--expose-*` tests need internal flags nub has no analogue for). Files carrying `--expose-internals` / `--allow-natives-syntax` / `--expose-externalize-string` / `--expose-gc` in their flag header are also skipped. This yields 56 tests.

**Pass/fail = exit code.** Each test is run with `NODE_TEST_KNOWN_GLOBALS=0` and stdin nulled; exit 0 is pass. That is the Node test suite's own contract (a test asserts internally and exits non-zero on any failure). The harness categorizes each test by the `(node_ok, nub_ok)` pair: `(true,true)` parity; `(false,false)` baseline-skipped (Node can't run it standalone in our setup, so it is not a valid signal); `(false,true)` nub-more-permissive (noted, not failed); `(true,false)` divergence (the case that matters — Node passes, nub fails, i.e. real Node code that breaks under nub).

**The gate.** The test asserts that every `(true,false)` divergence is in `KNOWN_DIVERGENCES` with a justification. An undocumented divergence fails CI. This is what makes the harness a *gate* for the resolver-fix items (A34, A35, D4, A26): land a fix, and any resolution regression it introduces shows up here as a new undocumented divergence.

## Result (2026-05-28)

```
47 parity, 2 baseline-skipped, 0 nub-more-permissive, 7 divergence(s) of 56 discovered
```

The 2 baseline-skipped are tests Node cannot run standalone in this harness (they need setup the suite's own runner provides) — not a parity signal either way.

## The 7 divergences are webstorage, not resolution

Each of the 7 tests where augmented `nub` fails and bare Node passes was run three ways: bare `node`, `node --import=<preload>` (nub's resolution **hooks** active, no injected flags), and `node --experimental-webstorage` (the augmentation **flag** nub injects, no hooks). The split is total and unambiguous:

| test | `node` | `node --import=preload` (hooks) | `node --experimental-webstorage` (flag) |
|---|---|---|---|
| test-esm-cjs-named-error.mjs | pass | **pass** | **fail** |
| test-esm-exports-deprecations.mjs | pass | **pass** | **fail** |
| test-esm-imports-deprecations.mjs | pass | **pass** | **fail** |
| test-esm-extensionless-esm-and-wasm.mjs | pass | **pass** | **fail** |
| test-require-module-cycle-esm-cjs-esm.js | pass | **pass** | **fail** |
| test-require-module-cycle-esm-cjs-esm-esm.js | pass | **pass** | **fail** |
| test-require-module-cycle-esm-esm-cjs-esm-esm.js | pass | **pass** | **fail** |

Every one **passes** with nub's resolution hooks loaded and **fails** with the webstorage flag set. The resolver is provably not the cause; the webstorage augmentation provably is. These tests assert on things the webstorage flag perturbs: exact deprecation-warning output, the precise set of warnings emitted (nub suppresses the webstorage `ExperimentalWarning`, which a test expecting a *different* warning can be sensitive to), and re-spawned child processes that inherit nub's `NODE_OPTIONS`. None of those assertions concern module resolution, so they are accepted (`KNOWN_DIVERGENCES` in the harness) rather than treated as bugs.

This is also why the naive idea of "make the baseline `node --experimental-webstorage` so the comparison is fair" does **not** work: nub suppresses the webstorage warning while bare `node --experimental-webstorage` does not, so that baseline fails ~47 tests nub passes (the warning trips Node's `common` leak/warning checker), inverting the signal. The honest design is the plain-Node baseline plus an explicit, evidence-backed allowlist.

## Follow-up: nub's webstorage augmentation is observable to warning-strict tests

Not a resolution issue, but surfaced here and worth recording. nub injects `--experimental-webstorage` (the whitepaper's storage-by-default promise) and suppresses its `ExperimentalWarning`. The combination is observable to the narrow class of programs that assert on their exact warning set or re-spawn themselves and introspect inherited flags — Node's own deprecation/cycle tests are exactly that class. Real application code essentially never asserts on the process's exact warning output, so the practical compat impact is ~nil, and there is no clean removal (webstorage-by-default *requires* injecting the flag). Logged as a known, accepted augmentation side-effect; if a future item revisits warning-suppression policy or per-feature opt-out, this is the prior art.

## Reproduce

```sh
cargo test -p nub-cli --test resolution_compat -- --nocapture
```

The `--nocapture` output prints the parity/divergence breakdown and labels each divergence with its `KNOWN_DIVERGENCES` reason. To re-derive the three-way table, run the listed tests under the `nub node which` baseline binary bare, with `--import=file://<repo>/runtime/preload.mjs`, and with `--experimental-webstorage`.

## Changelog

- 2026-06-04 — `--which-node` removed from the CLI surface; the passthrough baseline is now `nub node which` (path on stdout, `» resolved from <source>` explainer on stderr). `nub --version` went pure (`nub <ver>`, no `(node <ver>)` suffix); the resolved Node version + path now live under bare `nub node` (status) and `nub node which`. Harness (`baseline_node`) updated to invoke `nub node which`; behavior identical (same binary, stdout-captured).
- 2026-05-28 — Initial write-up. Harness stood up (`crates/nub-cli/tests/resolution_compat.rs`); 47 parity / 0 genuine resolution divergences across the 56-test subset; the 7 augmented-mode divergences traced by three-way comparison to nub's webstorage augmentation, not its resolver.
