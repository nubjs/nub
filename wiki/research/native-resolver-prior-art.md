# Prior art: native module resolver in Node.js core

Research compiled 2026-05-17. Scope: what Node TSC members, Modules WG, and core contributors have said, done, attempted, or rejected about porting the JS module resolver to native code.

Sibling refs: [[research/module-resolution]], [[research/rust-resolution-feasibility]].

## TL;DR

Five findings: the C++ port is already under way in tree, the monkey-patch surface is an upstream deprecation target, off-thread loader hooks are being walked back, nobody has proposed a single-PR rewrite, and Rust landed in tree in April 2026.

1. **The C++ port is already happening in tree, incrementally** — started by Yagiz Nizipli (TSC) and continued by Joyee Cheung, H4ad, Marco Ippolito. `readPackageJSON`, `getPackageScopeConfig`, `getNearestParentPackageJSON`, `legacyMainResolve`, and `fileURLToPath`-adjacent paths are native today. The remaining JS parts (`Module._findPath`, `packageResolve`, `packageExportsResolve`, `resolvePackageTarget`, `finalizeResolution`) are the *intentionally-deferred next slice*, not a green field. Nub is in well-trodden territory.
2. **Monkey-patching of `Module._findPath` / `Module._resolveFilename` / `Module._cache` is officially "convoluted compatibility burden" the core team wants to deprecate** (Joyee's [#52219], landed as `module.registerHooks()`; `module.register()` itself is now doc-deprecated and runtime-deprecated in 26+: [#62395], [#62401]). Nub should preserve the monkey-patch surface in `NODE_COMPAT=1` only — upstream is moving the same direction.
3. **Off-thread loader hooks are being walked back.** Loaders WG [#201] concluded that synchronous, in-thread hooks (Option 3) are preferred over the off-thread design. `module.registerHooks()` is the implementation. A native resolver that punts back to JS for user hooks via in-thread call-up is now the upstream-aligned shape.
4. **No one has ever proposed a full resolver rewrite as a single PR** — the consensus is "slice by slice, behind benchmarks". The reason: ESM spec corners (exports/imports/conditions/patterns) are moving targets, and core wants byte-parity tests before flipping. Nub's strategy (both halves prototyped together, parity-gated by `NODE_COMPAT=1`) is *more* aggressive than anything upstream has shipped; the prototype value is real but expect spec churn during implementation.
5. **Rust is no longer hypothetically in scope — it landed in tree in April 2026.** [#63015] (richardlau, merged) adds a Rust build target for macOS cross compiles; [#62565] (Yagiz, April 2026) rewrites `js2c.cc` in Rust as an explicit experiment. Joyee's pushback on [#62565] is "vendoring is the real cost, not the language choice." Marco's: "the problem of adding rust dependencies is vendoring." The Rust-vs-C++ question for Nub is open; for v0 staying in C++ avoids the vendoring fight.

## Key citations

Every upstream PR, issue, and working-group thread this document rests on, with the author, the date, and the position taken.

| Topic | Who | Where | When | Position |
|---|---|---|---|---|
| Move `readPackageJSON` cache + parser to C++ | Yagiz Nizipli (TSC) | [PR 50322] | 2023-10 | Merged. ~5% speedup on `vite --version`. Used `simdjson`. Notable change. Accepted a small `ERR_INVALID_PACKAGE_CONFIG` regression (no JSON.Parse message). |
| Native `FSLegacyMainResolve` + `fileURLToPath` rewrite | H4ad (collaborator), reviewed by aduh95, GeoffreyBooth, anonrig | [PR 48325] | 2023-06 | Merged. 34–35% speedup on `legacyMainResolve` micro-bench. Decided NOT to expose native `fileURLToPath` after measuring it was 96% *slower* for common URL-object inputs (FFI overhead). |
| Performance issue: improve legacy CJS resolve | Yagiz | [perf 73] | 2023-04 | Drove the PR above. |
| Performance issue: module resolution (top-level) | sheplu, Marvin Hagemeister | [perf 39] | 2023-01 | Identifies stat/throwIfNoEntry, file extension trial, stat caching as hot spots. Joyee comments fast-call `internalModuleStat` is the next lever. **Still open as of 2025-08.** RafaelGSS (TSC release lead) reopened it asking for fresh benchmarks. |
| Performance issue: `fs.isFile`/`isDirectory` fast path | Marvin Hagemeister | [perf 46] | 2023-01 | Still open. Yagiz engaged. Inspired by `enhanced-resolve` and `browserify/resolve` perf. |
| Cache negative `package.json` results | David Michon (MS, contributor) | [PR 56834] | 2025-01 | Restores Node ≤20 behavior lost during the C++ migration. ljharb raised invalidation; dmichon-msft and bnoordhuis confirmed cache is process-scoped, "shut down to invalidate." jasnell asked about runtime cache clearing for hot-reload — flagged as follow-up. **Confirms: process-scoped no-invalidation stat-cache stance is already core policy.** |
| Add JS-side cache to `getNearestParentPackageJSON` | IlyasShabi | [PR 59086] | 2025-07 | Merged. JS cache reduced run time from 4.5s → 0.5s. Yagiz: "C++ cache is still useful since it eliminates certain calls not exposed to JS." **Real finding: FFI cost is significant; cache JS-side too, not only native-side.** |
| Universal sync in-thread hooks API (`module.addHooks` / `module.registerHooks`) | Joyee Cheung (TSC) | [issue 52219], landed as registerHooks | 2024-03 → 2025/26 | Explicit motivation: "Monkey-patching … leads to very convoluted code in the CJS loader and also spreads to the ESM loader. It also makes refactoring of the loaders for any readability or performance improvements difficult." |
| Loaders WG: hooks thread direction | Geoffrey Booth (regular TSC), with Qard, Flarna, mcollina, JakobJingleheimer, giltayar, arcanis, ShogunPanda, dygabo | [loaders 201] | 2024-06 | Consensus: prefer **Option 3 (synchronous main-thread hooks)** over off-thread. mcollina: "3, 2, 1." arcanis: "current async off-thread model has been revealed relatively fragile." Flarna: "I see not that much of a gain we got from moving loaders off-thread." |
| `require.resolve()` through registerHooks | Joyee Cheung | [PR 62028] | 2026-02 | Merged. `require.resolve` now flows through the same hook chain as resolution. |
| Doc-deprecate `module.register()` | Geoffrey Booth | [PR 62395] | 2026-03 | Merged. |
| Runtime-deprecate `module.register()` (DEP0205) | Geoffrey Booth | [PR 62401] | 2026-03 | Merged for v26. mcollina: "this should land for v26." |
| Sharing stat cache between CJS/ESM loaders | Guy Bedford (Modules WG, collaborator) | [issue 30674] | 2019-11 | Long-standing. Some unification has happened with the C++ migration (the package.json cache *is* now shared). Stat-cache sharing is still incomplete. |
| Off-thread loader perf cost | cspotcode | [loaders 129] | 2023-02 | Documents the message-passing dance and event-loop tick cost of off-thread hooks. |
| Maintain hook module registration for on-thread hooks | Guy Bedford | [loaders 203] | 2024-06 | Long thread on `Worker.setDefaultHooks` etc; mcollina, joyeecheung, Qard active. Confirms in-thread hooks are the future. |
| Virtual File System (huge new fs/resolver-touching API) | Matteo Collina (TSC) | [PR 61478] | 2026-01 | Open. Introduces `loaderStat`, `loaderReadPackageJSON`, etc. as JS-side wrappers above the native bindings — gives a clean hook point that a native resolver must also respect. Joyee: API should reflect the SEA VFS requirements doc. |
| Module hooks: synchronous module-evaluate hooks | Joyee Cheung | [PR 57139] | 2025-02 | Open. Adds an `evaluate` hook on top of `registerHooks` (replacement for `Module.prototype._compile` patching). |
| Runtime-deprecate `require.extensions` | Marco Ippolito (TSC) | [PR 58642] | 2025 | Open. Same pattern: kill the monkey-patch surface, point users at hooks. |
| `module.clearCache` for CJS and ESM | Yagiz | [PR 61767] | 2026 | Open. Sanctioned cache-clearing path — relevant to Nub's stat-cache invalidation question. |
| Rewrite `js2c.cc` in Rust | Yagiz | [PR 62565] | 2026-04 | Open. "No specific reason for this rewrite other than the benefits of Rust over C++." Joyee: "moving to rust would be a fun exercise though most of the Rust over C++ benefit doesn't really apply for something that's just a build tool." Marco: "the problem of adding rust dependencies is vendoring." |
| Rust build target for macOS cross compiles | Richard Lau (TSC) | [PR 63015] | 2026-04 | Merged. Concrete infra step. |
| Temporal Rust crate already in tree | various | various, see [PR 60897] | 2025-11 | Existing precedent, linked into V8, not exposed via `internalBinding`. |
| Joyee `--run` C++ implementation precedent | Joyee | [PR 52190] | 2024-03 | Merged. Whole feature implemented C++-side first; informative pattern for "land C++ implementation that the JS surface delegates to." |
| Marvin Hagemeister's article (cited motivator) | Marvin Hagemeister | [marvinh.dev part-2] | 2023-01 | Documents `fs.statSync` slowness from `captureLargerStackTrace`. The article that spawned [perf 39] / [perf 46]. |

[PR 50322]: https://github.com/nodejs/node/pull/50322 [PR 48325]: https://github.com/nodejs/node/pull/48325 [PR 56834]: https://github.com/nodejs/node/pull/56834 [PR 59086]: https://github.com/nodejs/node/pull/59086 [PR 62028]: https://github.com/nodejs/node/pull/62028 [PR 62395]: https://github.com/nodejs/node/pull/62395 [PR 62401]: https://github.com/nodejs/node/pull/62401 [PR 61478]: https://github.com/nodejs/node/pull/61478 [PR 57139]: https://github.com/nodejs/node/pull/57139 [PR 61767]: https://github.com/nodejs/node/pull/61767 [PR 62565]: https://github.com/nodejs/node/pull/62565 [PR 63015]: https://github.com/nodejs/node/pull/63015 [PR 60897]: https://github.com/nodejs/node/pull/60897 [PR 52190]: https://github.com/nodejs/node/pull/52190 [PR 58642]: https://github.com/nodejs/node/pull/58642 [issue 52219]: https://github.com/nodejs/node/issues/52219 [issue 30674]: https://github.com/nodejs/node/issues/30674 [perf 39]: https://github.com/nodejs/performance/issues/39 [perf 46]: https://github.com/nodejs/performance/issues/46 [perf 73]: https://github.com/nodejs/performance/issues/73 [loaders 201]: https://github.com/nodejs/loaders/issues/201 [loaders 203]: https://github.com/nodejs/loaders/issues/203 [loaders 129]: https://github.com/nodejs/loaders/issues/129 [loaders 168]: https://github.com/nodejs/loaders/issues/168 [marvinh.dev part-2]: https://marvinh.dev/blog/speeding-up-javascript-ecosystem-part-2/

## What to weight heavily

Positions from people on the resolver hot path who are still active in TSC / Modules / Loaders / Performance:

- **Joyee Cheung (TSC voting)** — owns most of the recent loader internals refactoring and the `registerHooks` proposal. Her stated long-term direction *is* what Nub is building. Quote ([issue 52219]): "I think the design of ESM (the specification, not the Node.js ESM loader) provides a lot of room for an implementation with optimal performance. It doesn't need to unconditionally do more things." Direct alignment with Nub's premise.
- **Yagiz Nizipli (TSC voting, "anonrig")** — drove the C++ migration of `readPackageJSON`, `getNearestParentPackageJSON`, `legacyMainResolve` (as reviewer/champion), the ada URL parser landing, and now the Rust experiment ([PR 62565]). The most directly relevant ally and the technical pattern-setter.
- **Matteo Collina (TSC voting)** — Driving VFS [PR 61478], which introduces `loaderStat` / `loaderReadPackageJSON` / etc. as the *canonical* JS-side hook boundary above the native bindings. Any Nub native resolver MUST call through these same wrappers (or its own equivalent) or it breaks VFS / test-runner mocking.
- **Antoine du Hamel (TSC voting, "aduh95")** — Loaders WG, primary reviewer of [PR 48325] and many resolver PRs. Conservative on expanding native surface; insisted `fileURLToPath` rewrite be a separate PR and got it dropped when numbers didn't pan out. Read: expect every Nub native function to be benchmarked individually.
- **Geoffrey Booth (TSC regular, ex-Modules WG chair)** — primary voice for "ESM loader becomes the only loader, eventually". On [issue 52219] pushed back hard on the parallel-hooks-API approach ("UX complexity of essentially two APIs for doing the same thing") but lost that argument; `registerHooks` shipped anyway. Nub should ship a *path* that doesn't strand Geoffrey's vision either.
- **Guy Bedford (Modules WG, collaborator, not currently TSC)** — Designed conditional exports. Long history of asking for shared CJS/ESM stat cache ([issue 30674]). Will care that Nub's native resolver preserves exports/imports/conditions semantics. The hardest spec-corner critic.
- **Ben Noordhuis (TSC regular, "bnoordhuis")** — Wrote the *original* C++ package.json reader in Nov 2017. On [PR 56834] he confirms: "I'm somewhat surprised to learn that has changed because I remember bug reports from people trying to install npm packages at runtime." The "no stat-cache invalidation" stance has bitten before. Take seriously.
- **Marco Ippolito (TSC voting)** — Pushed TypeScript stripping into core, drove `require(esm)` rollout. Most likely to care about the resolver as integration surface with TS / strip-types.

## What to weight lightly

Sources that look relevant but carry no upstream weight: benchmark-free perf issues, the userland-resolver blog thread, competitor performance claims, the 2017–2020 rewrite debates, and V8 string-interning folklore.

- **Random "Node should be faster" issues with no benchmark** — there are dozens. They consistently die from "show me the numbers."
- **Marvin Hagemeister's blog and follow-up issues** — directionally correct, but mostly about `enhanced-resolve` / userland resolvers, not the in-tree resolver. The TSC has not picked up his suggestions beyond keeping [perf 39]/[perf 46] technically open.
- **External tools' performance claims (Bun, Deno)** — almost never cited in nodejs/* threads as motivating evidence. The Node conversation is internally driven; appealing to Bun is not productive in upstream debate (and is irrelevant for Nub, since Nub is upstream-adjacent).
- **The 2017–2020 NodeRealm / `process.binding` rewrite debates** — historical, mostly closed, not load-bearing for Nub.
- **Speculation about V8 string-interning costs** — frequently mentioned in performance threads but no one has shipped a fix citing it. Treat as folklore until measured.

## Specific plan-impacting findings (Q1–Q10)

Ten questions answered from the archives: prior native-rewrite proposals, the C++ migration's rationale, the benchmarks that exist, the monkey-patch and loader-hook debates, spec churn, snapshots, the stat cache, and Rust in core.

### Q1. Has anyone proposed rewriting the resolver in native code?

**Partially, incrementally — never as a single PR.** What has happened instead is a multi-year, slice-by-slice migration, driven by Yagiz Nizipli (TSC) and a small cluster (H4ad, Joyee, IlyasShabi, dmichon-msft, Marco, lemire). Landed slices:

- [PR 50322] (2023-10): native `readPackageJSON`, `getPackageScopeConfig`, with `simdjson` and snapshot-safe `BindingData`. **This is the precedent Nub is extending.**
- [PR 48325] (2023-06): native `LegacyMainResolve`.
- [PR 59086] (2025-07): JS-side cache *re-added on top of* the C++ cache to win back lost perf — direct evidence that "FFI hops at every resolve" is a real cost. Read this PR before Phase 1.

No one has proposed `Module._findPath` or `packageResolve` in C++ yet. The closest serious proposal is what Joyee gestures at in [issue 52219]: *"a fast happy path that can be hit by maybe >80% of the pure ESM packages."* No PR. **Nub is the prototype.**

### Q2. Rationale for moving `readPackageJSON` / `FSLegacyMainResolve` to C++?

[PR 50322] description and [perf 73]:

- Avoid V8 allocation during JSON parsing → use `simdjson` (lemire is the upstream author and was co-author on the PR).
- Cache lifecycle alignment with `BindingData` to make snapshot serialization clean.
- Joyee added a startup benchmark in [PR 50684] specifically to measure these changes; she required benchmarks before review.
- The authors said *nothing* explicit about whether the rest of the resolver should follow. The closest statement is Yagiz's "C++ cache is still useful since it eliminates certain calls that are not exposed to JS" on [PR 59086] — implying further pull-down is desirable when the JS↔C++ boundary becomes the bottleneck. That's Nub's premise.

[PR 50684]: https://github.com/nodejs/node/pull/50684

### Q3. Has anyone benchmarked the resolver?

Yes, scattered:

- Yagiz's `vite --version` `hyperfine` runs: 5% faster after [PR 50322].
- H4ad's `esm/esm-legacyMainResolve.js` micro-bench: 34–35% faster on the function itself ([PR 48325]).
- IlyasShabi's `getNearestParentPackageJSON` measurement: ~102k calls on a typical run; JS-side cache went 4.5s → 0.5s ([PR 59086]). **This is the single biggest perf datapoint anywhere in the archives** — it dwarfs anything FFI-internal.
- `benchmark/esm/esm-loader-defaultResolve.js` exists but H4ad documented it doesn't reflect granular changes; he added the legacy-main-specific bench.
- Marvin's blog identified `fs.statSync` + `captureLargerStackTrace` as deopt-driver source ([perf 39]).

**No one has benchmarked end-to-end resolver throughput for a Vite or Next.js cold start.** That data does not exist in the nodejs/* repos. Nub should produce it (and contribute it back).

### Q4. TSC concerns about the monkey-patch surface?

**The TSC has explicitly signaled that the monkey-patch surface is the deprecation target, not a contract.** Direct quotes from Joyee on [issue 52219]:

- "[Patching] leads to very convoluted code in the CJS loader and also spreads to the ESM loader. It also makes refactoring of the loaders for any readability or performance improvements difficult."
- "When the dependencies on `require()` monkey patching drop enough in the ecosystem, start emitting runtime warnings when the internal properties of `Module` are patched."
- "After that we will no longer maintain compatibility hacks."

`module.registerHooks()` shipped, `module.register()` got runtime-deprecated for v26 ([PR 62401]), and `require.extensions` is on the runtime-deprecation queue ([PR 58642]). The TSC is actively unwinding the patch surface.

**Implication for Nub**: not preserving `Module._findPath` / `Module._cache` byte-for-byte in default mode is *more* defensible than Nub's current plan assumes. The "compat mode" gate is correct; the rest of the planet is moving Nub's direction.

### Q5. Loader hooks perf state of debate

[loaders 201] is the canonical thread. The conclusion (June 2024) was unanimous-or-close among active participants:

- Qard, mcollina, Flarna, arcanis, giltayar, dygabo, JakobJingleheimer all said "**Option 3** (sync, on-thread hooks)" or "3, then 2."
- Joyee already had the implementation in flight via [PR 51977] → registerHooks line of PRs.
- Off-thread hooks remain available via `module.register()` but are being deprecated.

**Implication for Nub**: a native resolver that synchronously calls back up to JS for user hooks is now the *upstream-canonical* architecture, not a compromise. Worth saying explicitly in the plan.

### Q6. ESM resolution spec corners — ongoing churn?

Modest, not destabilizing. Recent landed items:

- `module-sync` exports condition ([PR 54648], merged 2024-08).
- `require(esm)` whole feature (Joyee, 2024–25; major resolver caller-side change but resolver itself is unchanged).
- `findPackageJSON` util ([PR 55412], JakobJingleheimer, 2024-10).
- Import maps: open work item, [loaders 168] (wesleytodd, JakobJingleheimer). Geoffrey explicitly noted that import maps should sit *above* `defaultResolve` like the policy manifest redirects do, not in the resolver. This matches Nub's import-map layering.

[PR 54648]: https://github.com/nodejs/node/pull/54648 [PR 55412]: https://github.com/nodejs/node/pull/55412

**No active spec churn that would invalidate a C++ port.** The pattern-trailer / null-target / "default" sentinel semantics are stable. The cost is in carefully mirroring `resolvePackageTarget` recursion, not in shooting at a moving target.

### Q7. The `nodejs/loaders` "module-hooks" effort — status, direction

- Implementation: `module.registerHooks()`, **shipped**, stable in recent versions.
- Direction: replace `module.register()` (off-thread) and `require()` monkey-patching as the universal customization API. Includes resolve / load / evaluate / `findPackageJSON` hooks.
- Doc-deprecation merged ([PR 62395]); runtime-deprecation merged for v26 ([PR 62401]).
- Open: `evaluate` hook ([PR 57139]), `clearCache` ([PR 61767]).

**Implication for Nub**: integrating `registerHooks` cleanly is the forward-compat play. The native resolver should bottom out into the same JS dispatch point that `registerHooks` chains into.

### Q8. Snapshot interaction — gotchas for new C++ bindings

`src/node_modules.cc` is already snapshot-aware: `BindingData` inherits `SnapshotableObject` and implements `PrepareForSerialization` / `Deserialize` / `MemoryInfo` / `SerializeInternalFields`. Concrete gotchas from [PR 50322]:

- The `package.json` cache itself must be serialized or cleared before snapshot. The merged PR clears it (no preserved entries across snapshot).
- `simdjson::parser` is not snapshot-safe state — instance must live per-Realm, not in static.
- Yagiz had to make `node_modules.cc` snapshottable as part of [PR 50322] — that's the reference work to copy.

**Nub's `NODE_COMPAT` env read in the dispatcher**: per Joyee's patterns, *runtime* reads of `process.env.NODE_COMPAT` are fine; the snapshot-unsafe pattern is *capturing* the env at module-top-level into a const. Nub's plan already does the right thing (read at every call). The risk is in any helper that caches `isCompat` as a class member at construction time. Audit for this.

### Q9. Stat cache — has anyone tried?

Yes, repeatedly:

- [issue 30674] (Guy Bedford, 2019) requested shared CJS/ESM stat cache. Partial implementation, never finished for `stat` specifically.
- [issue 26926] (closed): "Module stat cache keeps broken results."
- [issue 31803] (closed): "Clear the internal require statCache on unsuccessful module load."
- [PR 56834] (David Michon, 2025): added negative-result caching to the C++ `GetPackageJSON`. ljharb pushed back on invalidation ("how does one clear the cache after a change?"). dmichon-msft and bnoordhuis confirmed the answer is "restart the process." jasnell raised a clearing API as a follow-up.
- [PR 61767] (Yagiz, 2026): `module.clearCache` for CJS and ESM — open.

What bit people:

- Apps installing/changing `node_modules` at runtime (bnoordhuis on [PR 56834]).
- Test fixtures that mutate the filesystem mid-test.
- `node --watch` not invalidating the cache when a `package.json` changes (dmichon-msft's follow-up note).

**Implication for Nub**: the "process-scoped, no invalidation" default is core-consistent. The `NODE_NO_RESOLVER_CACHE=1` knob is necessary for tests. Plan to expose an `invalidateStatCache(path?)` binding early — `--watch` integration is a known follow-up that core has explicitly punted on, and Nub can be the place where it lands first.

### Q10. Rust in core — pushback?

State as of May 2026:

- Temporal Rust crate already linked (via V8, not `internalBinding`) — [PR 60897].
- [PR 63015] (richardlau, merged 2026-04): macOS Rust cross-compile target. **Infra is in.**
- [PR 62565] (Yagiz, 2026-04): experimental `js2c.cc` → Rust rewrite. Open. Pushback is *not* "no Rust"; it's:
  - Joyee: most Rust-over-C++ benefits don't apply to a build tool; extra build complexity is the cost.
  - Marco: "the problem of adding rust dependencies is vendoring."
- No TSC-level statement against Rust in core. The vendoring concern is real and concrete (Node bundles its deps; cargo's transitive-dep model is in tension with that).

**Implication for Nub**: C++ for v0 of the native resolver is the right call *for upstream-alignment reasons* (lowest friction with existing patterns), but Rust is no longer disqualified at v1. The framing should be "we're staying in C++ to match `src/node_modules.cc` and avoid the vendoring debate Marco/Joyee flagged" — that's a direct citation, not a vibe.

## Implications for a Nub native resolver

Eight consequences: frame the work as the next migration slice, treat monkey-patch fidelity as a compat-mode obligation, bottom out in the `registerHooks` chain, cache on both sides of the FFI boundary, and stay in C++ for v0.

1. **Frame the work as the next slice of an existing core migration, not as net-new** — [PR 50322], [PR 48325], [PR 59086], [PR 56834], and Joyee's [issue 52219] are the prior art. The C++ resolver slices land one PR at a time upstream; a Nub prototype commits to the rest of the slice in one coordinated step instead of waiting 2–3 years of incremental PRs.
2. **Monkey-patch fidelity is a v0 compat-mode obligation only.** Upstream runtime-deprecated `module.register()` in v26 ([PR 62401]) and shipped `module.registerHooks()` as the replacement, so a default-native path inherits that direction rather than diverging from it.
3. **The native resolver bottoms out by calling back up to the same JS hook chain** that `registerHooks` dispatches ([loaders 201], [PR 62028]). Addressing `defaultResolve` JS-side is not enough; the contract for synchronous in-thread hooks has to be explicit.
4. **Stat cache:** negative results are already cached on the C++ side ([PR 56834]); mirror the upstream `module.clearCache` name from [PR 61767] rather than inventing one; and jasnell's "runtime clearing for hot reload" is the existing follow-up to ship into.
5. **Cache JS-side as well as native-side.** The IlyasShabi benchmark went 4.5s → 0.5s from a JS cache atop the native one ([PR 59086]), so a dispatcher should reuse `Module._pathCache` and the package.json cache JS-side rather than pushing every lookup through the FFI boundary.
6. **Risk: mismatch with the VFS wrappers from [PR 61478].** If VFS merges, every native resolver stat / package.json read must call through `loaderStat` / `loaderReadPackageJSON` JS-side rather than straight to libuv. Mitigation: monitor the VFS landing and provide a native-side override hook that JS can point at the VFS wrappers.
7. **Rust is no longer blocked in core** — infrastructure landed April 2026 ([PR 63015]), with the experiment at [PR 62565] — but vendoring (per Marco and Joyee on that PR) would dominate v0 effort. Stay C++ for v0 and revisit at v1.
8. **Open shape:** should a native resolver expose a public binding (similar to `internalBinding`) that VFS, the test runner, and `--watch` can hook to invalidate cache entries? "No invalidation for the prototype" is correct, but the follow-up shape needs nailing down so the v1 API is not reinvented.

## Open questions still open

Five unresolved items: whether the TSC has ever refused a full rewrite, what upstream counts as stable enough to land, the `evaluate` hook's interaction, a Rust `node_modules.cc`, and the permission-model contract.

- **Has anyone in TSC said no to a full-resolver-rewrite PR specifically?** I could not find such a statement. Closest is Geoffrey on [issue 52219] arguing for fewer APIs / less divergence — not a veto on native code, but pressure toward consolidation. A Nub prototype that *stays additive* (compat mode preserves JS) threads this needle.

- **What's the upstream definition of "stable enough" for a resolver port to land?** I could not find a written bar. Empirically: micro-benchmarks for any new C++ function (per aduh95's pattern on [PR 48325]), startup macro-benchmark (Joyee's [PR 50684]), no regression on `pummel` policy tests. Nub's "black-box test bucket" is in the spirit of this but not a literal match.

- **Joyee's `evaluate` hook ([PR 57139]) interaction.** The hook is proposed but not merged. A native resolver does not interact with evaluation, so probably out of scope, but worth tracking — if it lands and changes the loader's call-into-resolver pattern, Nub's dispatcher contract may shift.

- **TSC voting on `node_modules.cc` Rust replacement.** Hypothetical. Yagiz's [PR 62565] is a pure experiment on a build tool. The resolver is much higher stakes. If Nub ships a v1 Rust version, no prior decision is binding — it's a fresh debate. Worth a separate research doc when v1 starts.

- **Permission-model integration.** Resolver in C++ + permission model in C++ → integration should be mechanical, but I did not find a documented contract for "native subsystems consult `permission::PermissionScope` at this layer." Confirm in source during Phase 1 ([`src/permission/permission.h`](https://github.com/nodejs/node/blob/main/src/permission/permission.h)).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
