# Research: Node version floor for Nub's extensibility mechanisms

**Status:** v1, 2026-05-18. Commissioned to scrutinize the "support any active LTS" stance in `runtime/target-version.md` and `runtime/auto-flag-injection.md` in light of the actual Node-version availability of the mechanisms Nub depends on as a CLI-on-top-of-Node augmenter (`architecture/augmenter-not-fork.md`). **Informs:** `runtime/target-version.md`, `runtime/auto-flag-injection.md`, `runtime/ts-transpilation.md`. **Adjacent:** `node-extensibility-headroom.md`, [`augmentation-layers.md`](augmentation-layers.md).

## Question

Nub is a Rust CLI orchestrating the user's installed Node via Node's public extension surfaces. The framing question is which floor to commit to before users depend on the answer, because **setting a higher Node floor now is one-way-easy; dropping support later is one-way-hard.** What floor maximizes mechanism quality without unnecessarily excluding users? Three candidates:

- **Node 20+**: matches the existing target-version doc; covers every still-receiving-security-patches LTS *at the moment*; Node 20 went EOL April 30 2026 (~18 days before this doc).
- **Node 22+**: drops EOL Node 20; gives us sync `registerHooks()` with the 22.15 backport; still requires async-register fallback for 22.0–22.14.
- **Node 24+**: gives us sync `registerHooks()` natively, fewer flags to inject, fewer code paths to test.

## TL;DR

**Set the floor at Node 24.** Justification follows but the short version: Node 20 is EOL as of 18 days ago, Node 22 has sync `registerHooks()` only from 22.15 onward (which most v22 users on distro packages won't have), and the *one* mechanism that defines Nub's TS-transpilation hot path — sync `module.registerHooks()` — shipped to Node 24 first (23.5 actually, but 24 is the LTS landing zone). Supporting Node 22 means maintaining a fallback path (`module.register()` async, with its ~400ms startup tax) for an ever-shrinking population of users on a release line that itself EOLs April 2027. Supporting Node 20 means maintaining that fallback for users on a runtime Node has stopped patching. The headline value-prop ("Nub makes Node fast") *inverts* on async-register: a Node 22.0 user with Nub running async-register loads ~400ms slower than the same user invoking Node directly. **The floor that lets Nub keep its promise is Node 24+.**

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

- **`module.register()` (async)**: Landed in Node 20.6.0 (Aug 31, 2023). The mechanism uses a worker thread + Atomics handshake to give the application-thread a sync `import.meta.resolve` against async user hooks. The ~400ms wall-clock startup penalty ([nodejs/discussions#51661](https://github.com/orgs/nodejs/discussions/51661)) is the cost of spawning that worker. *Backported to 18.19* but Node 18 is out of scope here.

- **`--import`**: Added in Node 18.18 / 18.19 timeframe, stable in 20+. Universally available on every version we'd consider.

- **`--require`**: Added in Node 1.6 (2015). Universally available.

- **`--enable-source-maps`**: Added in Node 12.12.0. Has had quality improvements since but the flag and the basic source-map → stack-trace remap is universally available.

- **`--experimental-vm-modules`**: Still experimental and flagged as of Node 26.x. Not a candidate for "unflag for free" via auto-flag-injection unless we accept the experimental label; not load-bearing for v0.

- **N-API**: ABI 8 stable in Node 18+. No concerns at any candidate floor; napi-rs binaries built against ABI 8 run on 20/22/24/26.

- **`argv[0]` basename detection**: A trivial property of any POSIX-spawned process; available on every Node version since the beginning. Useful for Nub detecting when invoked as `nubx` vs `nub`.

## Section 2 — Why sync `registerHooks()` is load-bearing

The entire TS-transpilation pipeline (`runtime/ts-transpilation.md`) is built on sync `registerHooks()`. The design decision (2026-05-16) locks this in: *"Sync `module.registerHooks()`, not async `register()`."* That decision was made for one reason — the async path's startup penalty.

### Quantifying the async-register penalty

From [nodejs/discussions#51661](https://github.com/orgs/nodejs/discussions/51661) and the linked-from-augmentation-layers benchmarks:

- **Cold-start cost: ~400ms wall-clock, single-shot.** This is the cost of spawning the loader worker thread, initializing the V8 isolate inside it, running the loader's own ESM-load chain to bootstrap user-registered hooks, then completing the first Atomics handshake. It's paid **once at process startup**, not per-resolve.

- **Per-resolve cost: microseconds.** Once the worker is warm, each `import` traverses the Atomics handshake in single-digit µs. Negligible.

- **Where the penalty bites:** Every short-lived `node` invocation — exactly the CLI-script-runner workload Nub is positioned for. `nub script.ts` paying +400ms is the difference between feeling fast and feeling like `ts-node`.

- **Worse: the penalty exists even when no CJS file is transformed by hooks.** Per [#51661](https://github.com/orgs/nodejs/discussions/51661), the CJS→CJS `require()` path still does the worker round-trip for communication bookkeeping even though hooks aren't invoked. So even users with no `.ts` files in scope pay it.

- **Cannot be skipped lazily.** Once `module.register()` is called, the worker is up. We could try to defer registration to the first TS import, but `--import` preload runs *before* user code, so the hook needs to be registered before user code can import anything. Hence: worker spawns at preload time, on every invocation, on every Node where sync hooks aren't available.

- **Compare to sync `registerHooks()`:** in-thread, in-realm, single digit ms one-time install, ~µs per resolve. The PR description for [#55698](https://github.com/nodejs/node/pull/55698) makes the pitch explicit: *"easier for CJS monkey-patchers to migrate to"* — i.e. specifically targets the workload Nub cares about.

**Net:** any Node version that lacks sync `registerHooks()` forces Nub into the async path, which costs +400ms per script invocation. That number is larger than Nub's entire startup budget (~80–130ms target per `node-extensibility-headroom.md`).

### Does async-register cover the same surface as sync?

Mostly — but with important asymmetries Nub would hit:

- **Sync hooks intercept `require()` calls in the same chain as `import`.** Designed for it.
- **Async hooks intercept ESM `import` cleanly.** Same as before.
- **Async hooks intercept CJS `require()` ambiguously.** Per the Node 22 docs: "When `require()` calls inside CommonJS modules are customized by asynchronous hooks, Node.js may need to load the source code of the CommonJS module multiple times to maintain compatibility with existing CommonJS monkey-patching." Double-loading. Side effects fire twice. Source-map indices diverge. This is fixable for our specific case (TS transpile is deterministic) but it's a sharp edge that doesn't exist on the sync path.
- **`node:*` interception via `require('node:zlib')` in sync hooks required the [`2d560e4`](https://github.com/nodejs/node/commit/2d560e42fa) fix.** Present in 22.15+ and 24+, not present in earlier 22.x. The async path doesn't have this gap, but it does have its own: `require('node:x')` resolution against async hooks is mediated by a sync shim and a bunch of caching that the documentation warns about explicitly.

So the matrix narrows to: **on Node 22.15+ and 24+, sync hooks are strictly better than async hooks** for everything Nub needs. On Node 22.0–22.14 and Node 20.x, async hooks are the only option, and they bring the ~400ms tax and the double-load asymmetry.

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

1. **Node 20 is EOL.** Not "going EOL" — it ended April 30 2026, 18 days before this doc. Security patches stop. Distros that ship Node 20 in 2026 are shipping a runtime upstream no longer patches.

2. **Node 22 is in Maintenance LTS, not Active LTS.** Active LTS moved to Node 24 in October 2025. Per Node's own guidance — "*Production applications should only use Active LTS or Maintenance LTS releases*" — both 22 and 24 are still valid, but Active is the recommendation. New deployments standing up in mid-2026 should land on 24 by default.

### Download-share data

Best public numbers (Node's own metrics endpoint plus aggregators like radixweb and codeless, May 2026):

- **Node 24**: ~150M+ monthly downloads, growing. Active LTS now.
- **Node 22**: ~120M monthly downloads, declining slowly as workloads migrate to 24.
- **Node 20**: ~100M monthly downloads but **dropping fast post-EOL**. Distros and CI runners are mid-migration.
- **Node 18 and older**: ~30% of total downloads (per radixweb). Distros shipping ancient Node, unsupported workloads. Not our problem.

The composition: as of May 2026, **a developer standing up a new project today, on a recent macOS/Linux/Windows install, gets Node 24** from nodejs.org's default download, from nvm's `nvm install --lts` (which now points to 24), and from most package-manager-provided Node versions (Homebrew Node, Volta default, mise default).

Existing projects on Node 22 will be there for a while (Node 22's own EOL is April 2027). Existing projects on Node 20 are running on an EOL runtime; if they're not migrating, they're not in a position to adopt new tooling either.

### "Who would we exclude with Node 24+?"

Most charitable read of the data: requiring Node 24+ excludes
- Users on Node 22 who haven't migrated to 24 yet (significant — Node 22 is still Maintenance LTS).
- Users on Node 20 who are running an EOL runtime (small and shrinking).
- Users on Node 18 or older (not our problem; explicitly out of scope per existing target-version.md).

The Node 22 cohort is the real cost. It's not trivial — Node 22 has ~120M monthly downloads. But here's the framing the user's strategic question asks us to consider: **the alternative is not "support Node 22 forever and never break them." It's "support Node 22 for a year, then drop it when 22 EOLs in April 2027, and break them then."** The window of usefulness for Node 22 support is one calendar year. The ongoing maintenance cost (a second hot path through async-register, doubled CI matrix, perf regressions only visible on the slow path) lasts forever — or at least until we drop 22 a year from now, having spent that year supporting it.

## Section 4 — The async-register degradation path: cost analysis

If we required Node 22+ (not 24+) and fell back to async-register on 22.0–22.14, here's what we'd inherit:

### Maintenance burden

- **Two hook-registration code paths** in our preload. Sync via `registerHooks` on 22.15+/24+. Async via `register` on 22.0– 22.14. Different APIs, different lifecycle, different worker semantics.
- **Two CI matrices.** Every PR runs the full suite against both paths.
- **Performance regressions invisible on the fast path.** A contributor lands a fix that's clean on sync hooks; on async hooks it triggers the double-load CJS surface and breaks someone's transpile. We find out from a bug report.
- **The "Nub is fast" pitch breaks** on Node 22.0–22.14. The +400ms async tax is larger than our entire cold-start budget. Users on those versions experience Nub as *slower than `tsx`*, which they could be using instead.

### User experience burden

- A user on Node 22.14 gets Nub running async-register. Cold start is +400ms vs. our advertised number. They benchmark. They conclude Nub is slow. They don't know the version cutoff matters.
- A user on Node 22.15 gets the fast path. Same Nub version, same binary, different experience. **The product feels inconsistent** in a way that's invisible to the user.
- We'd need to surface this. Probably a startup warning: "Nub is running in compatibility mode on Node 22.14; upgrade to Node 22.15+ for full performance." Which is exactly the kind of nag that erodes trust.

### Counter-argument: the degradation path exists in research

[`augmentation-layers.md`](augmentation-layers.md) §B describes sync hooks as the default and notes the async-register alternative. It does not commit us to *supporting* async-register; it characterizes it as "the historical path tsx is migrating away from." Our planning doc framing has always been "sync hooks first"; the question is whether we engineer a fallback or just set a floor.

## Section 5 — Recommendation

**Set the floor at Node 24+.**

Justifications, in priority order:

1. **Mechanism quality.** Sync `registerHooks()` is the load-bearing primitive for TS transpilation, package replacement, and synthetic-module surfaces. It's available cleanly and uniformly on Node 24+ (and 23.5+, but 23 is not LTS). Node 22's support is partial (22.15+) and the partial-coverage version range is messy to detect, test, and document.

2. **The async-register tax inverts our value-prop.** Nub on async-register costs +400ms per invocation vs. Node alone. The product slogan ("Nub makes Node faster, on whatever Node you have") becomes false on the degraded path. Better to refuse the degraded path than to ship a slower product to that population.

3. **The supported-Node-22 window is small.** Node 22 EOLs April 2027, ~11 months out. Supporting Node 22 means: design a fallback, test it, ship it, maintain it, then drop it in 11 months — at which point we break the same population we set out to support. **The strategic question's framing applies directly: "set a higher floor now to avoid breaking people later."**

4. **Node 20 is EOL.** Supporting an EOL runtime is supporting users who aren't taking security patches. We don't gain trust by serving them; we incur liability.

5. **Active LTS in 2026 is Node 24.** New projects land on 24. Existing projects on 22 will migrate to 24 before 22's April 2027 EOL. Setting the floor at 24 means **the floor moves with the ecosystem's center of gravity, not behind it**.

6. **Fewer flags to inject.** Per `runtime/auto-flag-injection.md`: on Node 24, the relevant injections are essentially `--no-warnings --enable-source-maps`. No `--experimental-sqlite` (stable), no `--experimental-import-meta-resolve` (stable), no `--experimental-strip-types` decision (we own that surface anyway). Smaller flag table. Smaller matrix.

7. **Compat-mode (`nub node`) still works on the user's vanilla Node.** This is important: even at floor=24, a user running `nub node script.js` on Node 22 still gets Node 22 behavior (Nub's flag injection and hooks are bypassed in compat mode). The "minimum Node" is for Nub's *augmented* mode, not for Node-compat. We don't break their existing scripts; we just require 24 for the Nub-flavored experience.

### What we update

- **`runtime/target-version.md`:** Change the minimum from Node 20 to Node 24. Add a "Why not 22?" section pointing here.
- **`runtime/auto-flag-injection.md`:** Remove the Node 20.x and 22.x rows from the flag table. Keep 23, 24, 26 (and future) rows.
- **`runtime/ts-transpilation.md`:** Reference to "Node 24.13.1+" can be relaxed to "any Node 24.x" since we're floor=24, but the exact patch-version mention is fine to keep as the conservative tested minimum.
- **Startup error path:** If `node --version` returns <24.0.0, Nub exits with a clear error: *"Nub requires Node 24 or newer. Detected Node X.Y.Z at /path/to/node. Upgrade via your version manager (nvm, mise, fnm, Volta) or download from https://nodejs.org."* No silent fallback, no degraded mode.

### Counter-recommendation considered: Node 22+

Tempting because Node 22 is still in Maintenance LTS and supporting it covers a real population. But:

- The 22.0–22.14 sub-range forces async-register, which costs more than our entire perf budget.
- We could declare "Node 22.15+" as the floor, which gives sync hooks. But that's a weird floor — most users don't know what patch version they're on, and 22.15-as-floor is not a story users recognize ("Node 22" yes; "Node 22.15" not really).
- Node 22 EOLs April 2027. We'd drop it then anyway.
- Compat-mode covers vanilla-Node-22 users who need a Node-22-shaped surface.

Verdict: **the 11-month window of usefulness for Node 22 support is not worth the matrix expansion, perf-inversion risk, and the inevitable Node-22 dropoff a year from now.**

### Counter-recommendation considered: Node 20+

The current target-version.md stance. Was reasonable when written (Node 20 was still Active LTS). As of 2026-05-18 (Node 20 EOL'd 18 days ago), it's no longer reasonable. We'd be supporting an EOL runtime, and on top of that paying the async-register tax on every 20.x user. Not viable.

## Section 6 — Implementation notes

- **Version detection runs before spawn** per `runtime/auto-flag-injection.md`. We already do this. Adding the floor check is a single comparison on the parsed version tuple.

- **Error message wording matters.** Anti-pattern: "Unsupported Node version." Good pattern: "Nub requires Node 24+ (you have 20.18.1). Upgrade: `nvm install --lts` then `nvm use --lts`." Make the next action obvious.

- **`--node`/compat mode still works.** A user on Node 22 can run `nub node script.js` and gets vanilla Node 22 behavior, no hooks, no flag injection. The floor only gates Nub's augmented features.

- **The flag table simplification is real.** Removing Node 20.x and 22.x rows removes per-version flag-conditional code in the Rust spawn pipeline. Fewer cases to keep correct as Node evolves.

- **Future-proofing.** When Node 26 enters LTS (October 2026), the floor stays at 24 — we don't move it eagerly. Floor moves only when the runtime our hot path depends on changes (i.e. when a new sync-hooks-equivalent landing forces a re-evaluation, which isn't on the horizon).

## Open questions

- **Should we error or warn on Node <24?** Lean: error. Soft failures invite "it worked once, why doesn't it work now" confusion. Compat mode handles the "I need Node 22 behavior" use case.
- **How loudly to surface the floor in docs / install?** Probably: README first paragraph, install script checks before download, and the spawn-time error is the safety net.
- **Do we offer a "use vanilla Node-X regardless" escape hatch beyond `--node`?** Maybe — `NUB_DISABLE_FLOOR_CHECK=1` for testing — but document it as not-for-production and don't advertise.
- **When does the floor move next?** Two triggers: (a) Node 24 EOL in April 2028 (move to 26 or whatever's Active LTS then), or (b) a new mechanism we want to depend on lands only in a newer version. No current candidates.

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
