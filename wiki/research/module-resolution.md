---
**Status:** v3, 2026-05-16. v2 was rewritten after a Node-TS-state
audit; v3 incorporates a Bun-vs-tsx prior-art pass and reframes
candidate probing around dynamic ordering rather than worst-case
probe count.
**Builds on:** [`tsx-architecture.md`](tsx-architecture.md)
(candidate-list pattern), [`augmentation-layers.md`](augmentation-layers.md)
(resolve-hook positioning), [`rust-from-js.md`](rust-from-js.md)
(N-API call-cost rules).
**Sibling:** [`tsconfig-paths.md`](tsconfig-paths.md) — TS path-alias
resolution split out because the design surface is large enough to
warrant its own page.
**Informs:** `runtime/pre-processing.md`,
`runtime/package-replacement.md`,
`PLAN.md` — Pre-processing model.
---

# Research: module resolution — extensionless ESM in TS, and how close we can get to Bun

Working write-up. Conclusions are current best read; future agents should feel free to revisit any of them as facts change.

## Question

TypeScript codebases overwhelmingly write `import { foo } from "./foo"` without an extension, inside `.ts` files that emit ESM. Node's ESM resolver refuses to resolve these. That refusal is the single biggest reason a `tsc`-clean TypeScript repo *still* can't be run by plain `node`, even though native type-stripping is now stable.

This doc nails down:

1. What Node's current TS support actually does and doesn't do (so we fix the right gap).
2. How candidate probing should work, with dynamic ordering keyed on the parent file's extension.
3. **A differential analysis vs Bun**: where can we plausibly match Bun, and where are we structurally unable to compete because we sit outside the runtime?

tsconfig path-aliases are a sibling concern with its own surface and have moved to [`tsconfig-paths.md`](tsconfig-paths.md). The extensions-swap-style "let unbuilt packages import each other's TS sources" trick from tsx is *out of scope* — Bun doesn't fully do it either; see [Non-goals](#non-goals).

## Current state of Node TS support (mid-2026)

To avoid the v1 error of treating type-stripping as still experimental, the precise state as of Node 26.x current / 24.x LTS:

- **Type-stripping is stable.** Added behind `--experimental-strip-types` in v22.6.0, unflagged (on by default) in v23.6.0, marked stable in v24.12.0 LTS and v25.2.0 ([nodejs/node#60600](https://github.com/nodejs/node/pull/60600)). Opt-out is `--no-strip-types`. (Source: [nodejs.org/api/typescript.html](https://nodejs.org/api/typescript.html).)
- **`.ts` extensions in import specifiers are supported and mandatory.** `import "./foo.ts"` works with no flag; `import "./foo"` does not. Design from v22.6.0 onward.
- **Extensionless imports are refused.** No flag enables them. No accepted proposal in core; Node TS WG's position is explicit consistency with JS ESM resolution.
- **Directory / index resolution is refused.** `import "./foo"` does not resolve to `./foo/index.ts` (or `./foo/index.js`). Same reasoning.
- **Only erasable syntax is supported.** Enums, namespaces with runtime emit, parameter properties, `import =` / `export =`, and `emitDecoratorMetadata` throw `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`.
- **`--experimental-transform-types` was removed in v26.0.0.** It was the only built-in path for non-erasable syntax; current Node has no in-runtime answer at all.
- **`node_modules` is never type-stripped** — Node won't apply TS handling to dependencies.

### Why the gap matters

"Extensions are mandatory" + "TypeScript codebases write extensionless" is the wedge. tsc has never synthesized extensions on extensionless imports (long-standing TS-team position) and still doesn't. TS 5.7's `--rewriteRelativeImportExtensions` only rewrites *existing* `.ts` extensions in the emit (`./foo.ts` → `./foo.js`); it leaves extensionless imports extensionless. So the workflow "write extensions, let tsc rewrite, let Node run" is real but helps only the small population of codebases that already author with explicit extensions.

Layer in: **transform-types is gone in v26.** Real-world TS codebases that use enums, decorators, parameter properties, or `emitDecoratorMetadata` (NestJS, TypeORM, class-validator, class-transformer, the Angular/Nx world) can no longer run on plain Node at all. Native Node TS now covers *less* ground than at peak experimental. The wedge widens for any tool doing the full transform — swc handles all of the above.

## Scope: looser, but no looser than needed

Current thinking: **extensionless is a TS-file concession, not a JS-ESM relaxation.** Node's ESM resolver is the strict baseline; we relax in one place only.

- **Inside `.ts` / `.tsx` / `.cts` / `.mts` / `.jsx` files**: extensionless imports resolve via candidate probing, dynamically ordered by parent extension (next section).
- **Inside `.cjs` files and CJS-mode `.js` files**: Node's built-in `require()` already permits extensionless. Our hook covers this path only because the unified sync hook also intercepts `require()`; same probing logic applies.
- **Inside ESM `.js` files (`.js` with `"type": "module"` or `.mjs`)**: leave Node's resolver alone. Extensionless ESM in plain JS fails as it does on plain Node. No migration story to protect, no muscle memory to preserve; silently relaxing it would put us further from Node identity for no upside.

Smallest possible departure from Node semantics that still makes TS work. Clean answer to "what does `--node` disable?" — the entire candidate probing is gated on file extension; in `--node` mode we skip our hook and Node's stricter resolver wins.

The gate is on the **parent URL's extension**, not the specifier. `./util` from `app.ts` is probed; `./util` from `app.js` (ESM) is not.

## Candidate probing: dynamic ordering by parent

Both tsx and Bun use a fixed candidate order regardless of the parent file's extension (Bun: `[.tsx, .ts, .jsx, .cts, .cjs, .js, .mjs, .mts, .json]` for local CJS; tsx: `[.ts, .tsx, .jsx, .js, .json]` for local). Both happen to put `.tsx` or `.ts` first, which works well for TS codebases but means a `.ts` file importing another `.ts` file pays for whatever isn't `.ts` to be probed first if the parent happens to be `.tsx`.

Nub can do slightly better cheaply: **order the candidate list by the parent file's own extension first**, then the natural family fallback. The probe order becomes a function of parent extension:

| Parent  | Probe order                                              |
| ------- | -------------------------------------------------------- |
| `.ts`   | `.ts, .tsx, .js, .jsx, .json`                            |
| `.tsx`  | `.tsx, .ts, .jsx, .js, .json`                            |
| `.mts`  | `.mts, .ts, .mjs, .js, .json`                            |
| `.cts`  | `.cts, .ts, .cjs, .js, .json`                            |
| `.jsx`  | `.jsx, .js, .tsx, .ts, .json`                            |
| `.mjs`  | `.mjs, .js, .mts, .ts, .json`                            |
| `.cjs`  | `.cjs, .js, .cts, .ts, .json`                            |
| `.js`   | `.js, .ts, .jsx, .tsx, .json` (only via CJS path; ESM-`.js` parents are gated out per [Scope](#scope-looser-but-no-looser-than-needed)) |

Then for directory resolution, append `/index` to the specifier and re-run the same list.

### Expected probe count

The first probe hits when the import matches the parent's family. Concretely:

- `.ts` → `.ts` (the overwhelming majority of TS app imports): hit on first probe.
- `.tsx` → `.tsx` (component → component): first probe.
- `.tsx` → `.ts` (component → hook/util, the canonical React case): second probe.
- `.ts` → `.tsx` (test → component, less common but real): second probe.
- `.mts` ↔ `.cts` etc.: rare; covered by fallback.

In practice the expected probe count is ~1.05 for TS-heavy apps, ~1.3 for React apps with a `.ts` / `.tsx` mix. Worst-case 5 probes exists but is reached only by something like a `.cjs` file importing `./foo` where only `foo.tsx` exists — not a real-world pattern. **Optimizing the design around 5-probe worst case is not useful**; the design is fast because the hit rate on the first probe is near 100%.

### Index resolution

Same candidate list, but with `/index` appended to the specifier before extension probing. tsx and Bun both work this way and it's unobjectionable. Adds one stat per import in the directory case; again, in cache-warm steady state it's a hashmap lookup.

## The Rust / JS split

The architectural rule from [`rust-from-js.md`](rust-from-js.md): **every napi call has a ~26 ns floor, ~230 ns if it returns objects.** Resolution gets called once per import; a typical TS startup involves 50–500 imports. At 500 × one napi call each, the floor is ~12–115 μs total — well inside cold-start budget. The N-API tax isn't the constraint.

What *is* the constraint is per-import fs latency. With dynamic ordering, average ~1 stat call (warm OS cache: ~1 μs; cold: ~3–5 μs). For 500 imports = 0.5–2.5 ms cold OS cache, much less warm. Worth pushing down via a resolution cache; not catastrophic without one.

### Layer-by-layer current thinking

**JS-side hook entry point:**

- Receive `(specifier, context, nextResolve)`.
- Call `nubResolve(specifier, parentURL) -> ResolvedEntry | null` via a single napi entry.
- If Rust returns a result, return it to Node with `shortCircuit: true`. Done.
- If Rust returns `null` (bare-specifier, defer to Node), call `nextResolve(specifier, context)` and return its result.

**Rust-side (the hot path):**

- Parent-URL gate (suffix check). Non-TS parent → return `null`.
- tsconfig path-alias lookup — details in [`tsconfig-paths.md`](tsconfig-paths.md).
- Candidate-list generation, ordered by parent extension per the table above.
- Candidate fs probing via `std::fs::metadata` — direct syscalls, no JS callback hops.
- Resolution cache: `HashMap<(specifier, parent_dir), ResolvedEntry>`, populated on success. Per-process. Turns 500 imports × ~1 stat into 500 hashmap lookups on warm startup.
- Persistent on-disk resolution cache (later): same shape, keyed on `(parent_dir, specifier, project_signature)` where the project signature is mtime/inode of tsconfig + relevant package.json files.
- Format detection (string-suffix; for ambiguous `.ts`, swc parser).
- Bare specifiers: return `null`, defer to Node.

**Things we explicitly don't move to Rust:**

- Bare-specifier resolution itself (exports, conditions, subpath imports). Node's resolver is canonical and the spec moves under us; reimplementing it is the [bundle-then-exec sharp edge](augmentation-layers.md#augmentation-layer-a-bundle-then-exec-rolldown-statically-linked) in miniature. The compat trust contract argues for Node's resolver, even at a perf cost (see [differential analysis](#differential-analysis-vs-bun)).
- `nextResolve` in a loop. We call it zero times for resolved relative paths and once for bare specifiers. tsx's per-candidate `nextResolve` is an artifact of being pure JS; we don't have to pay it.

## Differential analysis vs Bun

The goal is competitive parity with Bun on cold and warm TS-app startup, accepting that some gap is structural because Nub is outside the runtime. Itemizing what each side pays per import.

### What Bun pays per import

1. Stat syscall(s) for candidate probing — same count as us with warm caches.
2. Hashmap lookup in the resolver state.
3. Resolution result handed to JSC's module loader as a native pointer.
4. For bare specifiers: walk `node_modules`, parse `package.json`, evaluate `exports` conditions — all in Rust with the resolver's own caches.

### What Nub pays per import

1. **Node's `module.registerHooks()` invocation.** Node calls our JS hook for every resolve and every load. The dispatch itself (allocating the context object, invoking the callback) is a few hundred nanoseconds per import in Node's machinery. **Structural — only removable by forking Node.**
2. **JS → napi → JS round-trip.** ~230 ns per call when returning a small object. For 500 imports: ~115 μs. Tiny.
3. **JS → Rust → fs → Rust → JS for resolved relatives.** This is the path we have the most control over. Roughly the same fs cost as Bun, plus the napi cost above.
4. **JS → Rust → `null` → JS → `nextResolve(...)` for bare specifiers.** This is the load-bearing structural disadvantage. `nextResolve` runs Node's own ESM resolver, which is implemented in JavaScript (`lib/internal/modules/esm/`). It walks `package.json`s, parses `exports`, evaluates conditions — all in JS. Bun's equivalent is Rust with tight data structures. Per bare specifier the delta is probably **5–50 μs depending on the depth of the package**. For an app with 50 bare specifiers at cold cache, that's ~250 μs–2.5 ms of pure overhead Bun doesn't pay.
5. **Hook prelude bootstrap.** Process spawn → `--import nub-hooks.mjs` runs the prelude before user code. Adds ~1–5 ms of startup. Bun has zero of this; its hooks are built in. **Structural.**

### Where we can close the gap

- (3) and (4) — the per-import fs and bare-specifier costs — both collapse to hashmap lookups with a per-process resolution cache. Warm process: roughly Bun-parity per import.
- A **persistent on-disk resolution cache**, keyed on `(parent_dir, specifier, project_signature)`, eliminates the cold-cache bare-specifier cost on second-and-subsequent runs of the same project. With a populated cache, `nub script.ts` should resolve in close to the same wall-time as `bun script.ts`, modulo (1) and (5).

### Where we cannot close the gap

- (1) **Per-import hook invocation overhead.** Even an empty resolve hook costs Node a few hundred ns of dispatch. With 500 imports, ~150 μs we can't avoid. Bun pays zero.
- (5) **Hook prelude bootstrap.** Spawning Node + loading the prelude before user code is a fixed cost. Bun's loader is the runtime; nothing to bootstrap.
- **Node's own cold start.** Stock Node 24 starts in ~30 ms; measured Bun is ~10 ms. The ~20 ms delta exists before any resolution happens. **Structural — only removable by forking Node or embedding it.**

### Honest summary

For a typical TS app cold start, our floor is roughly:

```
Bun:  ~10 ms (runtime start) + resolution
Nub:  ~30 ms (Node start) + ~1–5 ms (prelude) + ~150 μs (hook
              dispatch) + resolution
```

The resolution cost itself we can drive within ~5% of Bun via the cache. The ~20–25 ms Node-startup-and-prelude gap is what we're stuck with. **For the kind of TS apps Nub targets (apps that take 100 ms+ to bootstrap their own logic), this is in the noise.** For microbenchmarks of "how fast does hello world run", we lose by a factor of 2–3x on cold start, and we should be honest about that when discussing perf.

## Non-goals

- **Reimplementing Node's resolver.** See above.
- **Extending extensionless to plain JS ESM.** Per [Scope](#scope-looser-but-no-looser-than-needed).
- **`.js → .ts` swap in package `exports`/`imports` maps.** tsx does this — when a bare-specifier import resolves to `./dist/foo.js` via a package's `exports` field and that file doesn't exist on disk, tsx retries with `.ts` and resolves to the source. The effect is "packages that ship unbuilt TS sources just work."

**Nub deliberately does not do this.** Reasons:
  1. **Bun doesn't fully do it either.** Bun's resolver applies a `.js → .ts` rewrite for relative-path file probes (`src/resolver/resolver.rs:5686–5739`) but **not** for exact exports-map hits — that branch returns `ModuleNotFound` without retry, and there's an explicit `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` flag for some cases. So the "unbuilt-package-import" behavior is a tsx-specific affordance, not a general TS-runtime expectation. Matching Bun here is the predictable choice.
  2. **It's invasive.** It requires intercepting Node's resolver errors and reinterpreting them, which makes the package author's `exports` declaration not authoritative. Packages would behave differently under Nub than under Node-with-a-compiled-package, which violates the reversibility filter.
  3. **Real-world dependence is unclear.** No high-profile packages are known to require this. The intended workflow for ship-TS-source packages is to author `"exports": ".": "./src/index.ts"` honestly.
- **Extensionless imports inside `node_modules`.** We don't probe for extensions in third-party dependencies; their published shape is whatever they shipped.
- **Caching resolution results across processes (v1).** Per-process cache lands first; persistent on-disk cache is a follow-on once cold-cache measurements justify it.
- **swc's own resolver.** swc ships `swc_ecma_loader`; not tracked against Node's resolver and has its own quirks. Don't reuse it.

## Concrete shape

```js
// nub-resolve.mjs — installed via module.registerHooks() from the prelude
import { resolve as nativeResolve } from "@nub/native";

const hook = {
  resolve(specifier, context, nextResolve) {
    const result = nativeResolve(specifier, context.parentURL);
    if (result !== null) {
      return {
        url: result.url,
        format: result.format,
        shortCircuit: true,
      };
    }
    return nextResolve(specifier, context);
  },
  load: /* unchanged from pre-processing plan */,
};

module.registerHooks(hook);
```

```rust
// crates/nub-resolve/src/lib.rs (shape, not literal API)
#[napi]
pub fn resolve(specifier: String, parent_url: Option<String>) -> Option<ResolvedEntry> {
    let parent_url = parent_url?;
    let parent_ext = ts_family_ext(&parent_url)?; // None → defer
    let cache_key = (specifier.clone(), parent_dir(&parent_url));
    if let Some(cached) = CACHE.get(&cache_key) {
        return cached.clone();
    }

    let rewritten = apply_tsconfig_paths(&specifier, &parent_url);
    if !is_relative_or_absolute(&rewritten) {
        return None; // bare specifier, defer to Node
    }

    let candidates = candidates_for(parent_ext, &rewritten, &parent_url);
    for candidate in candidates {
        if let Ok(meta) = std::fs::metadata(&candidate.path) {
            if meta.is_file() {
                let entry = ResolvedEntry { url: candidate.url, format: candidate.format };
                CACHE.insert(cache_key, Some(entry.clone()));
                return Some(entry);
            }
        }
    }
    CACHE.insert(cache_key, None);
    None
}
```

## Performance budget

Targets, not measurements. Validate during prototype.

| Step                                              | Budget    |
| ------------------------------------------------- | --------- |
| Hook invocation (Node dispatch)                   | ~300 ns   |
| JS → napi → JS round-trip                         | ~230 ns   |
| Parent-URL gate (suffix check)                    | < 100 ns  |
| Candidate-list generation                         | < 1 μs    |
| Single fs::metadata (warm OS cache)               | ~1 μs     |
| Resolution cache hit                              | < 200 ns  |
| First-probe extensionless resolve (typical)       | < 5 μs    |
| Bare specifier via Node's resolver (cold)         | 5–50 μs   |
| Bare specifier via on-disk resolution cache (hit) | < 2 μs    |

Cold-start ballpark for a 500-import TS app:
- ~450 relative imports × ~5 μs = ~2 ms
- ~50 bare specifiers × ~25 μs (cold Node resolver) = ~1.25 ms
- ~150 μs cumulative hook dispatch

Total resolution: ~3.5 ms. Both numbers well inside the < 50 ms cold-start ambition; the dominant cost is Node startup + prelude (see [differential analysis](#differential-analysis-vs-bun)).

## Open questions

- **Does swc's parser give us a cheap "is this an ambiguous TS file" answer?** Format detection cost scales with file size; if swc exposes a cheap-mode check, we skip pulling in es-module-lexer.
- **Persistent on-disk resolution cache shape.** Same content-addressed store as the transform cache, or a separate store? Folding in is probably right for atomic eviction; unverified.
- **Interaction with [nodejs/node#62720](https://github.com/nodejs/node/issues/62720) (the new `vm/modules` API).** If the new high-level loader API lands in 2026 and exposes a different resolution-customization surface, this design may migrate.
- **Non-erasable TS coverage as a wedge.** With v26 removing `--experimental-transform-types`, the population of "real TS codebases that can't run on plain Node at all" got bigger. Worth measuring against the NestJS / TypeORM / decorator-heavy worlds specifically; might want a dedicated benchmark/demo for that case.
- **Bare-specifier resolution speed.** Is the "Node's JS resolver is ~5–50 μs per bare specifier" estimate borne out? If it's at the high end of that range, the persistent resolution cache becomes more important; if low, less. Microbench needed.

## Sources

- Node TS docs: [nodejs.org/api/typescript.html](https://nodejs.org/api/typescript.html).
- Type-stripping stable: [nodejs/node#60600](https://github.com/nodejs/node/pull/60600) (Marco Ippolito), released v24.12.0 LTS and v25.2.0.
- Type-stripping unflagged: [nodejs/node#56350](https://github.com/nodejs/node/pull/56350) (v23.6.0).
- `--experimental-transform-types` removed in v26.0.0 — confirmed in current API docs; PR not pulled in this pass.
- TypeScript 5.7 / 5.8 `rewriteRelativeImportExtensions` and `erasableSyntaxOnly`: TS release notes.
- tsx resolve hook: `tsx/src/esm/hook/resolve.ts`, `src/utils/map-ts-extensions.ts`. Full breakdown in [`tsx-architecture.md`](tsx-architecture.md).
- Bun resolver: `bun/src/resolver/resolver.rs` (`load_as_file` at 5657–5671, `.js→.ts` rewrite at 5686–5739), `bun/src/resolver/options.rs` (`ExtensionOrder` at 130–193), `bun/src/bun_core/feature_flags.rs:109–110` (`DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES`).
- Node's ESM resolution algorithm: [nodejs.org/api/esm.html#resolution-algorithm](https://nodejs.org/api/esm.html#resolution-algorithm).
- `module.registerHooks()` sync hook API: [nodejs.org/api/module.html](https://nodejs.org/api/module.html).
- napi-rs call-cost floor: [`rust-from-js.md`](rust-from-js.md).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
