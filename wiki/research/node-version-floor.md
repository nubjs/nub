# Research: Node version floor for Nub's extensibility mechanisms

**Status:** v1, 2026-05-18. Scrutinizes the "support any active LTS" stance against the actual Node-version availability of the mechanisms Nub depends on as a CLI-on-top-of-Node augmenter. **Adjacent:** [`augmentation-layers.md`](augmentation-layers.md).

## Question

Nub is a Rust CLI orchestrating the user's installed Node via Node's public extension surfaces. Which floor to commit to before users depend on the answer, given that **setting a higher Node floor now is one-way-easy and dropping support later is one-way-hard**? Three candidates:

- **Node 20+**: matches the existing target-version doc; covers every still-receiving-security-patches LTS *at the moment*; Node 20 went EOL April 30 2026 (~18 days before this doc).
- **Node 22+**: drops EOL Node 20; gives us sync `registerHooks()` with the 22.15 backport; still requires async-register fallback for 22.0–22.14.
- **Node 24+**: gives us sync `registerHooks()` natively, fewer flags to inject, fewer code paths to test.

## TL;DR

**Set the floor at Node 24.** Node 20 went EOL 18 days ago; Node 22 has sync `registerHooks()` only from 22.15 onward, which most v22 users on distro packages will not have; and the one mechanism that defines Nub's TS-transpilation hot path — sync `module.registerHooks()` — reached an LTS line in Node 24 (23.5 first, but 23 is not LTS). Supporting Node 22 means maintaining a fallback path (async `module.register()`, with its ~400 ms startup tax) for a shrinking population on a release line that EOLs April 2027. Supporting Node 20 means maintaining that fallback for users on a runtime Node has stopped patching. The headline value-prop ("Nub makes Node fast") *inverts* on async-register: a Node 22.0 user loads ~400 ms slower than the same user invoking Node directly. **The floor that lets Nub keep its promise is Node 24+.**

## Section 1 — Mechanism × Node-version matrix

Versions verified against Node's changelogs, release-blog posts, and the PRs that landed each feature. "✓" = present and stable; "⚠" = behind a flag or with caveats; "✗" = unavailable.

| Mechanism                                            | 18.x   | 20.x   | 22.0–22.14 | 22.15+ | 23.x | 24.x  | 26.x |
|------------------------------------------------------|--------|--------|------------|--------|------|-------|------|
| `module.registerHooks()` (**sync**)                  | ✗      | ✗      | ✗          | ✓ (22.15) | ✓ (23.5) | ✓ | ✓ |
| `module.register()` (**async**)                      | ✗      | ✓ (20.6) | ✓        | ✓      | ✓    | ✓     | ✓    |
| `--import` (ESM preload)                             | ✓ (18.19) | ✓     | ✓        | ✓      | ✓    | ✓     | ✓    |
| `--require` (CJS preload)                            | ✓ (1.6) | ✓      | ✓          | ✓      | ✓    | ✓     | ✓    |
| `--enable-source-maps`                               | ✓      | ✓      | ✓          | ✓      | ✓    | ✓     | ✓    |
| `--experimental-vm-modules`                          | ⚠      | ⚠      | ⚠          | ⚠      | ⚠    | ⚠     | ⚠ (still flagged) |
| N-API stable (ABI 8+)                                | ✓      | ✓      | ✓          | ✓      | ✓    | ✓     | ✓    |
| `argv[0]` basename detection                         | ✓      | ✓      | ✓          | ✓      | ✓    | ✓     | ✓    |
| `module.enableCompileCache()`                        | ✗      | ✗      | ✓ (22.8)  | ✓      | ✓    | ✓     | ✓    |
| `require(esm)` (sync ESM load from CJS)              | ✗      | ✗ (flagged) | ⚠ (flagged 22.0–22.11) | ✓ (22.12 stable) | ✓ | ✓ | ✓ |
| Native `.ts` strip-types                             | ✗      | ✗      | ⚠ (flagged 22.6+) | ⚠   | ✓ (23.6 unflagged) | ✓ | ✓ |
| Sync `registerHooks()` intercepts `node:*` via `require()` | ✗ | ✗ | ✗      | ✓ (post-fix `2d560e4`) | ✓ | ✓ | ✓ |
| Synthetic-module `nub:*` namespace via resolve hook  | (with async) | (with async) | (with async) | ✓ (sync) | ✓ | ✓ | ✓ |

### Exact version sources

- **`module.registerHooks()` (sync)**: Landed in Node 23.5.0 (Dec 19, 2024) via [PR #55698](https://github.com/nodejs/node/pull/55698). Backported to v22.x via [PR #57130](https://github.com/nodejs/node/pull/57130), merged March 31 2025, **shipped in Node 22.15.0** (per the 22.15.0 release blog). Also gets a hotfix in 22.15 enforcing sync-callback semantics. **Not present in Node 22.0– 22.14, not present in any Node 20.x, not present in any Node 18.x.**

- **`module.register()` (async)**: Landed in Node 20.6.0 (Aug 31, 2023). Uses a worker thread plus an Atomics handshake to give the application thread a sync `import.meta.resolve` against async user hooks. The ~400 ms wall-clock startup penalty ([nodejs/discussions#51661](https://github.com/orgs/nodejs/discussions/51661)) is the cost of spawning that worker. Backported to 18.19, but Node 18 is out of scope here.

- **`--import`**: Added in the Node 18.18 / 18.19 timeframe, stable in 20+. Available on every candidate version.

- **`--require`**: Added in Node 1.6 (2015). Universally available.

- **`--enable-source-maps`**: Added in Node 12.12.0. Quality has improved since, but the flag and the basic source-map → stack-trace remap are universally available.

- **`--experimental-vm-modules`**: Still experimental and flagged as of Node 26.x. Not a candidate for unflagging via auto-flag-injection without accepting the experimental label; not load-bearing for v0.

- **N-API**: ABI 8 stable in Node 18+. No concerns at any candidate floor; napi-rs binaries built against ABI 8 run on 20/22/24/26.

- **`argv[0]` basename detection**: A property of any POSIX-spawned process, available on every Node version. Useful for Nub detecting whether it was invoked as `nubx` or `nub`.

## Section 2 — Why sync `registerHooks()` is load-bearing

The TS-transpilation pipeline is built on sync `registerHooks()`. The design decision of 2026-05-16 locks it in — *"Sync `module.registerHooks()`, not async `register()`"* — for one reason: the async path's startup penalty.

### Quantifying the async-register penalty

From [nodejs/discussions#51661](https://github.com/orgs/nodejs/discussions/51661) and the benchmarks linked from [`augmentation-layers.md`](augmentation-layers.md):

- **Cold-start cost: ~400 ms wall-clock, single-shot.** Spawning the loader worker thread, initializing the V8 isolate inside it, running the loader's own ESM-load chain to bootstrap user-registered hooks, then completing the first Atomics handshake. Paid **once at process startup**, not per-resolve.

- **Per-resolve cost: microseconds.** Once the worker is warm, each `import` traverses the Atomics handshake in single-digit µs.

- **Where the penalty bites:** every short-lived `node` invocation — the CLI-script-runner workload Nub is positioned for. `nub script.ts` paying +400 ms is the difference between feeling fast and feeling like `ts-node`.

- **The penalty exists even when hooks transform no CJS file.** Per [#51661](https://github.com/orgs/nodejs/discussions/51661), the CJS→CJS `require()` path still does the worker round-trip for communication bookkeeping even though hooks are not invoked. Users with no `.ts` files in scope pay it too.

- **Cannot be skipped lazily.** Once `module.register()` is called the worker is up. Deferring registration to the first TS import does not work, because `--import` preload runs *before* user code and the hook must be registered before user code can import anything. So the worker spawns at preload time, on every invocation, on every Node without sync hooks.

- **Sync `registerHooks()` by comparison:** in-thread, in-realm, single-digit-ms one-time install, ~µs per resolve. The PR description for [#55698](https://github.com/nodejs/node/pull/55698) states the pitch — *"easier for CJS monkey-patchers to migrate to"* — which is the workload Nub cares about.

**Net:** any Node version lacking sync `registerHooks()` forces Nub onto the async path at +400 ms per script invocation, larger than Nub's entire startup budget (~80–130 ms target).

### Does async-register cover the same surface as sync?

Mostly, with asymmetries Nub would hit:

- **Sync hooks intercept `require()` calls in the same chain as `import`.** Designed for it.
- **Async hooks intercept ESM `import` cleanly.**
- **Async hooks intercept CJS `require()` ambiguously.** Per the Node 22 docs: "When `require()` calls inside CommonJS modules are customized by asynchronous hooks, Node.js may need to load the source code of the CommonJS module multiple times to maintain compatibility with existing CommonJS monkey-patching." Double-loading, side effects firing twice, source-map indices diverging. Fixable for this specific case, since TS transpile is deterministic, but a sharp edge that does not exist on the sync path.
- **Interception of `node:*` via `require('node:zlib')` in sync hooks required the [`2d560e4`](https://github.com/nodejs/node/commit/2d560e42fa) fix.** Present in 22.15+ and 24+, absent in earlier 22.x. The async path lacks this gap but has its own: `require('node:x')` resolution against async hooks is mediated by a sync shim and caching that the documentation warns about explicitly.

The matrix narrows to: **on Node 22.15+ and 24+, sync hooks are strictly better than async hooks** for everything Nub needs. On Node 22.0–22.14 and Node 20.x, async hooks are the only option, and they bring the ~400 ms tax and the double-load asymmetry.

## Section 3 — Ecosystem usage data (May 2026)

### LTS status snapshot

Per [endoflife.date](https://endoflife.date/nodejs) and Node's own [release schedule](https://nodejs.org/en/about/previous-releases), as of 2026-05-18:

| Line | Status         | Active LTS until | EOL            |
|------|----------------|------------------|----------------|
| 18   | EOL            | (gone 2024-10)   | 2025-04-30     |
| 20   | **EOL (18 days ago)** | (was 2024-10–2025-10) | **2026-04-30** |
| 22   | Maintenance LTS | (was 2024-10–2025-10) | 2027-04-30   |
| 24   | Active LTS     | 2025-10–2026-10  | 2028-04-30     |
| 26   | Current         | (LTS 2026-10)    | 2029-04-30 (proj.) |

Two things matter here:

1. **Node 20 is EOL** — it ended April 30 2026, 18 days before this doc. Security patches stop. Distros shipping Node 20 in 2026 ship a runtime upstream no longer patches.

2. **Node 22 is in Maintenance LTS, not Active LTS.** Active LTS moved to Node 24 in October 2025. Per Node's own guidance — "*Production applications should only use Active LTS or Maintenance LTS releases*" — both 22 and 24 are valid, but Active is the recommendation. New deployments standing up in mid-2026 should land on 24 by default.

### Download-share data

Best public numbers (Node's own metrics endpoint plus aggregators like radixweb and codeless, May 2026):

- **Node 24**: ~150M+ monthly downloads, growing. Active LTS now.
- **Node 22**: ~120M monthly downloads, declining slowly as workloads migrate to 24.
- **Node 20**: ~100M monthly downloads but **dropping fast post-EOL**. Distros and CI runners are mid-migration.
- **Node 18 and older**: ~30% of total downloads (per radixweb). Distros shipping ancient Node, unsupported workloads. Not our problem.

Composition as of May 2026: **a developer standing up a new project on a recent macOS/Linux/Windows install gets Node 24** — from nodejs.org's default download, from `nvm install --lts` (now pointing at 24), and from most package-manager-provided Node versions (Homebrew Node, Volta default, mise default).

Existing projects on Node 22 will be there for a while, since Node 22's own EOL is April 2027. Existing projects on Node 20 run an EOL runtime; a team not migrating off that is not in a position to adopt new tooling either.

### Who a Node 24+ floor excludes

- Users on Node 22 who have not migrated to 24 yet — significant, since Node 22 is still Maintenance LTS.
- Users on Node 20, running an EOL runtime. Small and shrinking.
- Users on Node 18 or older, already out of scope.

The Node 22 cohort is the real cost, at ~120M monthly downloads. But the alternative is not "support Node 22 forever and never break them"; it is "support Node 22 for a year, then drop it when 22 EOLs in April 2027, and break them then." The window of usefulness is one calendar year, while the maintenance cost — a second hot path through async-register, a doubled CI matrix, perf regressions visible only on the slow path — runs for that whole year and buys nothing after it.

## Section 4 — The async-register degradation path: cost analysis

A Node 22+ floor with an async-register fallback on 22.0–22.14 inherits the following.

### Maintenance burden

- **Two hook-registration code paths** in the preload: sync `registerHooks` on 22.15+/24+, async `register` on 22.0–22.14. Different APIs, lifecycle, and worker semantics.
- **Two CI matrices.** Every PR runs the full suite against both paths.
- **Performance regressions invisible on the fast path.** A fix that is clean on sync hooks triggers the double-load CJS surface on async hooks and breaks someone's transpile, surfacing only as a bug report.
- **The "Nub is fast" pitch breaks** on Node 22.0–22.14. The +400 ms async tax exceeds the entire cold-start budget, so users on those versions experience Nub as slower than `tsx`.

### User experience burden

- A user on Node 22.14 gets async-register, so cold start is +400 ms against the advertised number. They benchmark, conclude Nub is slow, and never learn that the version cutoff mattered.
- A user on Node 22.15 gets the fast path. Same Nub version, same binary, different experience — **an inconsistency invisible to the user**.
- Surfacing it needs a startup warning ("Nub is running in compatibility mode on Node 22.14; upgrade to Node 22.15+ for full performance"), which is the kind of nag that erodes trust.

### Counter-argument: the degradation path exists in research

[`augmentation-layers.md`](augmentation-layers.md) §B describes sync hooks as the default and notes the async-register alternative, characterizing it as "the historical path tsx is migrating away from." It does not commit Nub to *supporting* async-register. The framing has always been sync-hooks-first; the open question is whether to engineer a fallback or set a floor.

## Section 5 — Recommendation

**Set the floor at Node 24+.**

Justifications, in priority order:

1. **Mechanism quality.** Sync `registerHooks()` is the load-bearing primitive for TS transpilation, package replacement, and synthetic-module surfaces. It is available uniformly on Node 24+ (and 23.5+, but 23 is not LTS). Node 22's support starts at 22.15, and that partial-coverage range is messy to detect, test, and document.

2. **The async-register tax inverts the value-prop.** Nub on async-register costs +400 ms per invocation against Node alone, so "Nub makes Node faster, on whatever Node you have" becomes false on the degraded path. Refusing the degraded path beats shipping a slower product to that population.

3. **The supported-Node-22 window is small.** Node 22 EOLs April 2027, ~11 months out. Supporting it means designing, testing, shipping and maintaining a fallback, then dropping it in 11 months and breaking the same population it was built for. Setting a higher floor now avoids breaking people later.

4. **Node 20 is EOL.** Supporting an EOL runtime means serving users who are not taking security patches — liability rather than trust.

5. **Active LTS in 2026 is Node 24.** New projects land on 24, and existing projects on 22 will migrate before its April 2027 EOL. A floor at 24 **moves with the ecosystem's center of gravity rather than behind it**.

6. **Fewer flags to inject.** On Node 24 the relevant injections are essentially `--no-warnings --enable-source-maps`: no `--experimental-sqlite` (stable), no `--experimental-import-meta-resolve` (stable), no `--experimental-strip-types` decision, since Nub owns that surface. Smaller flag table, smaller matrix.

7. **Compat mode (`nub node`) still works on the user's vanilla Node.** Even at floor 24, `nub node script.js` on Node 22 gets Node 22 behavior, with Nub's flag injection and hooks bypassed. The minimum applies to Nub's *augmented* mode only, so existing scripts keep working; 24 is required for the Nub-flavored experience.

### Downstream edits

- **Target version:** change the minimum from Node 20 to Node 24, with a "Why not 22?" section pointing here.
- **Auto-flag injection:** remove the Node 20.x and 22.x rows from the flag table; keep 23, 24, 26 and later.
- **TS transpilation:** the "Node 24.13.1+" reference can relax to "any Node 24.x" under floor 24, though keeping the exact patch version as the conservative tested minimum is fine.
- **Startup error path:** if `node --version` returns <24.0.0, Nub exits with *"Nub requires Node 24 or newer. Detected Node X.Y.Z at /path/to/node. Upgrade via your version manager (nvm, mise, fnm, Volta) or download from https://nodejs.org."* No silent fallback, no degraded mode.

### Counter-recommendation considered: Node 22+

Tempting, because Node 22 is still in Maintenance LTS and covers a real population. Against it:

- The 22.0–22.14 sub-range forces async-register, which costs more than the entire perf budget.
- A "Node 22.15+" floor would give sync hooks, but most users do not know their patch version, and 22.15-as-floor is not a recognizable story.
- Node 22 EOLs April 2027, so it gets dropped then anyway.
- Compat mode covers vanilla-Node-22 users who need a Node-22-shaped surface.

Verdict: **the 11-month window is not worth the matrix expansion, the perf-inversion risk, and the inevitable Node-22 dropoff a year from now.**

### Counter-recommendation considered: Node 20+

The prior stance, reasonable when written, since Node 20 was still Active LTS. As of 2026-05-18, with Node 20 EOL 18 days earlier, it means supporting an unpatched runtime and paying the async-register tax on every 20.x user. Not viable.

## Section 6 — Implementation notes

- **Version detection already runs before spawn.** Adding the floor check is a single comparison on the parsed version tuple.

- **Error message wording matters.** Anti-pattern: "Unsupported Node version." Good pattern: "Nub requires Node 24+ (you have 20.18.1). Upgrade: `nvm install --lts` then `nvm use --lts`." Make the next action obvious.

- **Compat mode (`--node`) still works.** A user on Node 22 runs `nub node script.js` and gets vanilla Node 22 behavior — no hooks, no flag injection. The floor gates Nub's augmented features only.

- **The flag-table simplification is real.** Dropping the Node 20.x and 22.x rows removes per-version flag-conditional code from the Rust spawn pipeline.

- **Future-proofing.** When Node 26 enters LTS (October 2026) the floor stays at 24. It moves only when the mechanism the hot path depends on changes — no candidate is on the horizon.

## Open questions

- **Error or warn on Node <24?** Lean: error. Soft failures invite "it worked once, why doesn't it work now" confusion, and compat mode covers the "I need Node 22 behavior" case.
- **How loudly to surface the floor in docs and install?** Probably the README first paragraph, an install-script check before download, and the spawn-time error as the safety net.
- **An escape hatch beyond `--node`?** Possibly `NUB_DISABLE_FLOOR_CHECK=1` for testing, documented as not-for-production and unadvertised.
- **When does the floor move next?** Two triggers: Node 24's EOL in April 2028 (move to whatever is Active LTS then), or a new mechanism worth depending on that lands only in a newer version. No current candidates.

## Sources

- [Node 22.15.0 release blog (module.registerHooks backport)](https://nodejs.org/en/blog/release/v22.15.0)
- [PR #55698 — module.registerHooks implementation](https://github.com/nodejs/node/pull/55698)
- [PR #57130 — 22.x backport of registerHooks](https://github.com/nodejs/node/pull/57130)
- [Node 20.6.0 release blog (module.register added)](https://nodejs.org/en/blog/release/v20.6.0)
- [nodejs/discussions#51661 — async loader 400ms overhead](https://github.com/orgs/nodejs/discussions/51661)
- [PR #44710 — move ESM loaders off-thread](https://github.com/nodejs/node/pull/44710)
- [endoflife.date Node.js status (May 2026)](https://endoflife.date/nodejs)
- [Node.js previous-releases / LTS schedule](https://nodejs.org/en/about/previous-releases)
- [commit `2d560e4` — sync hooks intercept node:* via require](https://github.com/nodejs/node/commit/2d560e42fa)
- [Node CLI options docs — `--import`, `--require`, `--enable-source-maps`](https://nodejs.org/api/cli.html)
- [Node module hooks docs (registerHooks, register)](https://nodejs.org/api/module.html#customization-hooks)

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
