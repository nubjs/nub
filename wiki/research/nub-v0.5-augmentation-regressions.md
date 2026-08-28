# nub v0.5.0 augmentation regressions — the 70 vs Node 25.8.1, root causes + provenance

The 70 confirmed nub-vs-Node-25.8.1 regressions measured by running Deno's corpus against nub v0.5.0 collapse into **9 root causes**. This doc is the diagnosis: what each cause is, **when it landed**, whether it is a real bug, and the recommendation.

## Read this first — three things that were getting conflated

Three separable facts: the benchmark changed no runtime code, the count grew because feature development added augmentation surface between the measurements, and the discarded fix of 2026-07-23 was an editor discard rather than a regression.

1. **The benchmark introduced nothing.** Running the corpus against nub changed zero runtime code. Verified: no `runtime/`, `crates/nub-core/src/node/`, or `crates/nub-native/` commit landed during the benchmark window (2026-07-22/23); the nearest code commits are the v0.5.0 release train of 2026-07-21, before the benchmark ran.

2. **"Regression" here means grew relative to the June measurement, not caused by the benchmark.** The June benchmark (nub v0.0.49, ~2026-06-16) counted ~53–57 nub-vs-node regressions; the July benchmark (nub v0.5.0) counts 70. The delta is **normal v0.1→v0.5 feature development** adding augmentation surface between the two measurements — every offending line below landed between 2026-06-03 and 2026-07-09, during ordinary development. Re-measuring is what surfaced them.

3. **The "revert" is a separate event from any regression.** On 2026-07-23 an applied fix to `runtime/polyfills.cjs` (and, later, the process-group guard in `crates/nub-cli/tests/node_compat.rs`) was discarded from the working tree — no git-reflog entry, not in any stash, sibling edit `flags.rs` untouched. That signature is a **per-file `git restore` / editor "Discard Changes"**, not automation and not a code regression: it removed a fix in progress rather than introducing a bug.

## Timeline anchors

| date | event |
|---|---|
| 2026-06-03 | repo's first commit |
| ~2026-06-16 | **June benchmark**, nub v0.0.49 — ~53–57 regressions |
| 2026-07-21 | nub **v0.5.0** released |
| 2026-07-22/23 | **July benchmark** (this effort), nub v0.5.0 — 70 regressions |

## The 9 root causes

Severity key: **High** = silent wrong result or every-invocation cost · **Med** = observable divergence, bounded blast radius · **Low** = niche. "Landed" is the git-`-S` first-touch of the offending behavior.

---

### Cause 1 — Eager preload materializes undici (the big one)

One `typeof` probe against a lazy undici-backed global pulls 227 modules into every startup where Node loads 110, which makes this the largest lever in the set.

- **Tests:** ~25 (module-hooks ×11, heap-prof ×10, es-module-tla, tls ×2, worker-memory, process-finalization, trace-events).
- **What breaks:** the line `runtime/polyfills.cjs:145` reads `typeof globalThis.MessageEvent` to install a spec ports-freeze. `MessageEvent` is a lazy undici-backed global, so the *read* synchronously loads undici plus its http/http2/tls/crypto/zlib/worker closure — **227 modules at startup vs Node's 110**. Consequences: startup cost on every invocation; workers accumulate it (60 workers → 5.5× RSS, over the test's 5× cap — a real memory regression); heap/cpu profilers capture it; and several tests get slow enough to blow the 25s corpus budget (the "hangs" — none are real deadlocks, all exit 0 under a 300s budget, verified).
- **Landed:** 2026-06-25 (#163), between the two benchmarks — part of the 53→70 growth.
- **Severity:** High — the single biggest lever; fixing it closes or reduces 4 clusters and cuts startup on every run.
- **Recommendation:** a previously-written then discarded fix — probe with `"MessageEvent" in globalThis` (the `in` op never fires the lazy getter) and version-gate the freeze off on the fast tier (Node froze `.ports` natively at 22.3.0; our floor is 22.15). Verified: 227→114 modules, ports still frozen, workers fine. **Re-apply.** Three independent investigations root-caused this same line.

### Cause 2 — `--enable-source-maps` always injected

Injecting the flag on every run puts each invocation on Node's source-map error path, which changes what a message-less assertion throws. Gated off across the affected Node 26 band (see the reversed verdict below).

- **Tests:** ~5 (assert ×2, source-map-enable, es-module-cjs-named-error; also the 26.x TypeError).
- **What breaks:** nub injects `--enable-source-maps` on every run (`flags.rs`, `ALWAYS_INJECT`). Node's error path bails when a file has no source map, so (a) on Node 26 a no-message `assert.ok(false)` throws **`TypeError` instead of `AssertionError`** — breaks `catch (e) { e instanceof assert.AssertionError }`; (b) on Node 24.9–25.x the assert *message* degrades from the source expression to `false == true`; (c) a child spawned without the flag still gets remapped stacks.
- **Landed:** source-maps injection predates the June benchmark — the original 26.2-only gate for it is from **2026-06-11**, so the behavior is **old / pre-existing**, not part of recent growth. The 26.x TypeError only became visible as Node 26 shipped and became the recommended line.
- **VERDICT — REVERSED 2026-08-21: gate the whole released 26.x band.** The 2026-07-24 call below accepted the divergence and left `source_maps_safe` at 26.2-only. Two facts overturned it. First, the band was mis-scoped: the regression is [nodejs/node#63169](https://github.com/nodejs/node/issues/63169) and it affects **every released 26.x** — reproduced on 26.0.0, 26.3.0, 26.5.1 and 26.7.0 — so a 26.2-only gate protected almost nobody while the docs claimed 26.1 and 26.3+ were clean. Second, the failure is a wrong error TYPE, not a degraded message, so it breaks `node:test` and any `instanceof assert.AssertionError` check from outside the assertion, where "pass a message" is not a workaround the caller owns. Upstream fixed it on `main` in b5d37cd4 ([nodejs/node#63215](https://github.com/nodejs/node/pull/63215), 2026-08-20), unreleased as of v26.7.0. `source_maps_safe` now withholds the flag for `26.0.0 <= v < 26.8.0` as a stopgap; re-verify when 26.8.0 ships. 25.x is clean (25.9.0 throws an `AssertionError` with the degraded `false == true` message — right type, worse text).
- **Superseded verdict — accept, no fix (2026-07-24).** The bug was judged narrow: only a bare `assert.ok(false)`/`assert(false)` with **no message** (asserts with a message, `strictEqual`, and plain throws are all fine). A 26.x-wide gate was briefly landed then **dropped**, on the reasoning that disabling TS stack-trace remapping for every Node-26 user to fix one edge case was the wrong trade. That reasoning rested on the mis-scoped band and on treating the type corruption as cosmetic.

### Cause 3 — Default `NODE_COMPILE_CACHE` lacks provenance

The default cache dir defers to a user-set value correctly; what is missing is a marker recording whose dir it is, and that is what a coverage child inherits wrongly.

- **Tests:** ~8 (compile-cache-api ×2, coverage-width ×4, test-runner-coverage, v8-coverage).
- **What breaks:** nub sets a default `NODE_COMPILE_CACHE`. It defers correctly to a user-set value (verified; an earlier "clobber" guess was wrong), but the sentinel doesn't record whether the dir is nub's default or the user's, so under coverage a child inherits nub's warm cache. Effects: a warm V8 code cache collapses block-coverage ranges (**branch % inflates, e.g. 42→100**), and `module.enableCompileCache()` — a public API — silently no-ops because nub claimed the one-shot slot first.
- **Landed:** **2026-06-11** (#88d75dde87) — **pre-existing**, predates the June benchmark.
- **Severity:** High — coverage tooling silently reports wrong numbers.
- **Recommendation:** add a `user`/`default` provenance marker to the compile-cache sentinel so the preload re-exports `NODE_COMPILE_CACHE` only for user-set values. Touches the Rust + JS halves of the sentinel protocol; needs a cache-version bump. Real fix, not a one-liner.

### Cause 4 — `Module._load` / `_resolveFilename` monkeypatch frames leak into stacks

Wrapping two anonymously-declared Node internals leaves an extra frame in every user error stack and a null-named one beside it.

- **Tests:** ~5 (repl ×2, test-output-abort ×2, util-inspect).
- **What breaks:** nub wraps `Module._load` (to lazily detect `child_process`) and `_resolveFilename`. Both are declared anonymously in Node, so nub's `.call()` wrapper leaves a null-named frame and an extra `at module_._load (…/.cache/nub/…/preload-common.cjs:1064)` line **in every user error stack**, and breaks the REPL's error-frame slicing (`Uncaught [Error:` instead of `Uncaught Error:`).
- **Landed:** `_resolveFilename` 2026-06-10; `_load` 2026-06-14 (the `-S` hit is a revert of a *different* `_load` deferral). Around the June benchmark — **old/pre-existing**.
- **Severity:** Med — cosmetic but user-visible in every trace.
- **Recommendation:** fold the `child_process` detection into the existing `registerHooks` resolve hook, which is not on the stack during user code, rather than wrapping `_load`; restore `.name` on the displaced `_resolveFilename`. Two independent investigations converged; the resolve-hook approach supersedes a bare rename.

### Cause 5 — Injected flags copied into child argv (`process.execArgv`)

Injected flags reach a child twice, through `NODE_OPTIONS` and through argv, and the argv copy is the one user code can read back.

- **Tests:** ~3 (vm-main-context ×2, vm-dynamic-import-missing-flag).
- **What breaks:** injected experimental flags are copied to child **argv** as well as `NODE_OPTIONS`, so `process.execArgv` contains flags the user never passed — breaks argv-introspecting code and tests asserting a flag is absent.
- **Landed:** partially fixed 2026-07-01 (#271 routed the *preload* flag via NODE_OPTIONS on the file-run path); the residual copy for the experimental flag set remains. Mid-period.
- **Severity:** Med.
- **Recommendation:** inject via NODE_OPTIONS only, never argv. `integration.rs:1312` already enforces this invariant for the preload flag — extend it to the experimental set. Clean fix.

### Cause 6 — `--disable-warning=ExperimentalWarning` leaks to children

Muting nub's own experimental warnings with a blunt flag also mutes the warnings a child the user spawned earns on its own.

- **Tests:** ~4 (experimental-warnings, import-assertion-warning, wasm-module-instances-warning, process-warnings).
- **What breaks:** nub injects `--disable-warning=ExperimentalWarning` to mute warnings for the flags *it* injects, but the flag rides `NODE_OPTIONS` into every child, so a `node` child the user spawns has its own legitimate experimental warnings suppressed too. (Verified: the failing subtest expects a child's `--experimental-loader` warning and gets empty stderr.)
- **Landed:** ~2026-07-09 (#398, the flag-intersect refactor). Between benchmarks.
- **Severity:** Med — and a judgment call: the same flag legitimately mutes nub's own noise.
- **Recommendation:** replace the blunt flag with a selective preload-side `warning` listener that drops only warnings for features **nub itself unflagged**, delegating the rest. Two config ignore-entries (`test-process-warnings`, `test-domain-multi`) currently blame "webstorage ExperimentalWarning"; both reasons are factually wrong and should be corrected.

### Cause 7 — Transpiled TypeScript gets a bare-path `sourceURL`

A `sourceURL` written as a filesystem path rather than a `file://` URL makes Node's coverage collector skip the file, so TypeScript coverage reports empty.

- **Tests:** ~2 (typescript-coverage; contributes to test-runner-coverage).
- **What breaks:** the emitter at `crates/nub-native/src/cache.rs:135` writes `//# sourceURL=<path>` (a filesystem path) instead of a `file://` URL. Node's coverage collector skips any URL not starting with `file:`, so **`nub --test --experimental-test-coverage` on any TypeScript project reports an empty coverage table and a fake 100%** (verified end-to-end: node shows `lib.ts | 100 | 100 | 50.00`, nub drops the row entirely).
- **Landed:** 2026-06-26 (#181). Between benchmarks.
- **Severity:** High — silent wrong coverage on the exact workflow nub targets (TS).
- **Recommendation:** emit the module's `file://` URL as `sourceURL` (the URL is already at the load boundary). Needs a transpile-cache version bump — the bad URL is baked into on-disk entries.

### Cause 8 — Explicit `--env-file` values get `$`-expanded  (FIXED: Node-parity restored)

Expansion was removed on 2026-08-24. An explicit `--env-file` now delivers every value verbatim, matching Node; the automatic `.env*` cascade still expands.

- **Tests:** ~2 (dotenv; watch-mode subtests).
- **What happened:** the CLI at `crates/nub-cli/src/cli.rs` ran `expand_env_map` on the explicit `--env-file` map. Node keeps values literal; nub expanded `$VAR`, so `PW="p@ss$WORD{x}"` → `p@ss{x}`.
- **Landed:** 2026-06-27 (#207/#214). A deliberate DX choice.
- **VERDICT — intentional divergence, keep it (2026-07-24).** *(Superseded — see the reversal below.)* Measured: node keeps `--env-file` literal, but **bun expands identically** (`p@ss{x}`), and nub's compat bar for this DX surface is bun-parity, not Node-parity. Mark `test-dotenv` `ignore` in `node-compat-config.jsonc` with that reason.
- **VERDICT — REVERSED 2026-08-24: expansion removed, Node-parity restored.** Three grounds. (1) `AGENTS.md` is the canonical tracked contract and outranks a dated internal verdict; two of its rules bind here — "Compatibility is paramount… byte-for-byte" for code targeting Node, and "Never implement a Bun-specific non-spec behavior for parity's sake." `--env-file` is **Node's own flag**, not a nub DX surface, so the bun-parity framing was applied to the wrong surface. (2) The 2026-07-24 premise is only half-true: re-measured against bun 1.3.14, nub did **not** match bun — on `A="{ port: $MISSING_VAR}"` nub produced `{ port: }` and bun produced `{ port: `, so the divergence bought neither Node- nor bun-parity. (3) Concrete user harm: any value holding a literal `$` — passwords, connection strings — was silently corrupted, the exact footgun the Node team cited when rejecting expansion. This also restores agreement with the two older records that the 07-24 verdict had silently contradicted, [[research/env-file-loading]] and [[research/env-expansion-and-test-skip]], both of which always specified no expansion for `--env-file=`. The prescribed `node-compat-config.jsonc` ignore entry was never added, so `test-dotenv` had stayed red the whole time; it now passes.
- **Correction:** an earlier revision of this doc rated this "High — silent secret corruption, fix recommended," which the 07-24 revision then downgraded to a deliberate bun-matching DX feature. The 2026-08-24 reversal restores the original reading: it was silent corruption, and it is now fixed.
- **Verified NOT a `--node` auto-load bug:** nub does **not** auto-load `.env` in a plain file run — `nub script.js`, `nub --node script.js`, and plain `node script.js` all leave a cwd `.env` unread; only an explicit `--env-file` loads it. Bun does auto-load `.env`, so nub is the more conservative of the two.

### Cause 9 — `URL.revokeObjectURL()` arity + domain-sweep DEP0097
Two low-severity, cleanly-fixable items:
- **9a `revokeObjectURL()`** (`runtime/worker-blob-url.cjs:109`, landed **2026-06-24** #99): the wrapper always forwards one argument, defeating Node's arity check, so a no-arg call silently no-ops instead of throwing `ERR_MISSING_ARGS` (verified). Fix: preserve arity.
- **9b domain DEP0097** (`runtime/preload.cjs`, the deferred `setImmediate(maybeSweepCache)`, from the **2026-06-03** initial commit — the oldest cause): the sweep callback runs under a user's active domain and trips DEP0097 in code using a long-lived entered domain, a deprecated pattern. Niche. Fix: run the sweep outside any active domain.

## What's intentional (not bugs)

Four divergences are correct as they stand, each for a stated reason; only the last carries an open call.

- `test-bootstrap-modules` — asserts a pristine builtin set; nub loads builtins for flags it needs. Accept.
- `test-typescript-commonjs` / the TS compile-cache tests — nub's oxc transpiler supersedes Node's native type-stripping. Accept (config reason is accurate).
- `test-repl-permission-model` — nub refuses `--permission` without `--allow-addons` because its transform is an N-API addon; a protective refusal with an actionable message. Accept.
- The 12 tests gated on `--experimental-eventsource` (module-hooks + http-parser) — plain Node plus the flag fails identically, so this is within the augmenter contract, and `EventSource` is non-enumerable so real app code never trips it. Open call: accept as a divergence (ignore-list), or author a lazy `EventSource` constructor.

## The durable guard (independent of every fix above)
nub already has a compat gate — `crates/nub-cli/tests/node_compat.rs` over `tests/node-compat-config.jsonc` — but its **CI job was deleted 2026-06-03 and never restored**.

That left zero automated compat signal for the whole June→July window in which these grew. It was pulled because the harness killed the CI runner by orphaning grandchildren of timed-out tests. The fix (process-group spawn + reap, ported from `run.mjs`) is written and verified; restoring the CI job on top of it prevents future regressions more broadly than any single adopted test.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-08-24 — **REVERSAL: Cause 8 (`--env-file` `$`-expansion) is a bug again, and is fixed.** The 07-24 revision reclassified it as an intentional bun-parity divergence and prescribed an ignore-list entry. Both halves fail on re-examination: `AGENTS.md` forbids implementing a Bun-specific non-spec behavior for parity's sake and holds byte-for-byte Node compatibility as paramount — and `--env-file` is Node's own flag, so bun-parity was never the governing bar — while a re-measurement against bun 1.3.14 showed nub did not match bun either (`{ port: }` vs `{ port: ` on the same input). Expansion is removed from the explicit `--env-file` path; the automatic `.env*` cascade is untouched and still expands. Node's `test/parallel/test-dotenv.js` goes from failing to passing. No other cause changed.
- 2026-07-24 — Initial write-up. 70 regressions → 9 root causes with git provenance. Key finding: none introduced by/since the benchmark; all landed 2026-06-03…07-09 during v0.1→v0.5 development; causes 2/3/4/9b predate the June benchmark, the rest are the 53→70 growth. Disambiguated the 07-23 fix "revert" (an editor discard) from code regressions.
- 2026-08-21 — **REVERSAL** on Cause 2. The `--enable-source-maps` regression band was mis-scoped as 26.2-only; it is [nodejs/node#63169](https://github.com/nodejs/node/issues/63169) and covers every released 26.x (reproduced on 26.0.0 / 26.3.0 / 26.5.1 / 26.7.0). The 2026-07-24 "accept, no fix" verdict is superseded: `source_maps_safe` now withholds the flag for `26.0.0 <= v < 26.8.0`, pending the upstream fix b5d37cd4 ([nodejs/node#63215](https://github.com/nodejs/node/pull/63215)) reaching a release.
