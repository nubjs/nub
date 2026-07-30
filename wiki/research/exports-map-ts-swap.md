---
**Status:** v1, 2026-05-18. Bottom-line recommendation: **do not
ship in v0.** Reasoning below.
**Scope:** The "tsx trick" — silently rewriting `.js` to `.ts` when
the `.js` doesn't exist, including inside a package's `exports`
map. Distinct from the candidate-list probing in
[`ts-extension-precedence.md`](ts-extension-precedence.md).
**Builds on:** [`module-resolution.md`](module-resolution.md)
(already declares this a non-goal; this doc is the longer
justification), [`tsx-architecture.md`](tsx-architecture.md) (where
tsx's exports-map swap lives in source).
**Informs:** Resolve-hook behavior in `lib/internal/nub/`. The
reversibility filter
applied to a specific concrete case.
---

# The `.js → .ts` exports-map swap controversy

When a `.ts` file writes `import "./foo.js"` and `./foo.js` doesn't exist but `./foo.ts` does, the TypeScript ecosystem has converged on *usually* resolving to `./foo.ts`. That much is uncontroversial.

The controversy is the next step out. When `import "some-package"` hits the package's `exports` map, the map points to `./dist/foo.js`, and `./dist/foo.js` doesn't exist on disk (because the package shipped unbuilt TS sources alongside its `package.json`), should the runtime probe `./dist/foo.ts` and resolve to that?

tsx says yes. Bun says no. The TS compiler itself says yes-but-only-for-typechecking-not-runtime. The TS team's official guidance for package authors is "don't do this — declare the `.ts` honestly in the exports map." Nothing about this is settled.

This doc walks the question to a recommendation for Nub.

## The two layers

It's easy to confuse two adjacent behaviors. They're not the same:

**Layer 1: relative-path `.js → .ts` rewriting.** Inside a TS-family file, the user wrote `import "./foo.js"` and `./foo.js` doesn't exist. The runtime tries `./foo.ts` and resolves to that. This is the TypeScript-officially-blessed pattern that `allowImportingTsExtensions` + `rewriteRelativeImportExtensions` emit *into*: the user writes the `.js` extension that will exist post-build, and the runtime (or tsc itself, when running in-source) sees through it to the `.ts` source.

Status across tools:

- **tsx**: yes (via candidate-list swap table — `src/utils/map-ts-extensions.ts:20`).
- **Bun**: yes (resolver `.js → .ts` rewrite at `src/resolver/resolver.rs:5686-5739`, except where `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` overrides).
- **ts-node / Vite / Rolldown / esbuild**: yes (built into their candidate orders or extension lists).
- **Plain Node (--experimental-strip-types)**: no. `import "./foo.js"` requires `./foo.js` to exist.

Layer 1 is **not** the controversy. It's the natural extension of "`.ts` beats `.js`" ([`ts-extension-precedence.md`](ts-extension-precedence.md)) from extensionless to explicit-extension imports. Nub should ship it; same code path as the candidate list, costs nothing extra. The [`module-resolution.md`](module-resolution.md) Rust-side `candidates_for(...)` already generates `.ts` candidates for `.js` specifiers in TS-family parents.

**Layer 2: exports-map `.js → .ts` swap.** The package author wrote `"exports": { ".": "./dist/index.js" }`. Node's resolver evaluates that, gets `./dist/index.js`, finds it doesn't exist on disk, and throws `ERR_MODULE_NOT_FOUND`. tsx catches that error, pulls the missing path out of it, swaps `.js → .ts`, retries (`src/esm/hook/resolve.ts:215-222`). If `./dist/index.ts` exists, the import resolves to it.

This is the controversy. **Nub should not ship Layer 2 in v0.**

## What problem does Layer 2 solve?

The intended user story: "I have a workspace with packages A and B. B imports A. A's `package.json` declares `"exports": { ".": "./dist/index.js" }` because that's what will be shipped to npm. But during development I haven't run A's build, so `./dist/index.js` doesn't exist. With tsx, B's import of A just works — tsx silently resolves to A's `./src/index.ts` source. No build step in the inner loop."

The same shape applies to:

- A monorepo where every package's `exports` field points to `dist/` but the workspace dev environment hasn't run any builds.
- A package author who ships both `.ts` source and `.js` build artifacts in the same npm publish, and a downstream tool wants to consume the `.ts` directly for some reason (Bun's "bun" condition use case).
- "Just-in-time monorepo development" where running any package's tests pulls in dependencies' source rather than dependencies' build output.

So the value is real, in a narrow set of cases, all of which are **monorepo / workspace use cases.** It's never useful in a single repo (no other package's exports map to navigate). It's never useful when consuming third-party deps from `node_modules` that were properly published (the `.js` files exist).

## In what scenarios is it useful?

Concretely, the population where Layer 2 changes outcomes is:

1. **Workspace packages** whose `exports` points at `dist/*.js` that doesn't exist locally because the user hasn't run the build.
2. **Hand-cloned source dependencies** where the user dropped a git checkout into `node_modules` (rare; `pnpm install --link` / workspace `*` protocol is the supported path).
3. **Packages published to npm with `.ts` sources alongside `.js` builds** *and* a tool deliberately wanting the `.ts`. Bun's solution to this is the explicit `"bun"` export condition (`"exports": { "bun": "./src/index.ts", "default": "./dist/index.js" }`). The condition is the *honest* way to expose `.ts` — the package author declares it.

Of these, only **(1)** is a non-trivial population. (2) is a historical curiosity; (3) is solved by the conditional-exports mechanism without needing a runtime swap.

So the question reduces to: *should Nub's resolve hook silently ignore the package's authored `exports` map and substitute source files, to save monorepo users from running their build step?*

## Where Layer 2 causes problems

The honest cost of swapping `.js` for `.ts` after the exports map resolved:

**1. The package's `exports` is now non-authoritative.** The package author *declared* `./dist/index.js`. We're returning `./src/index.ts`. The `.ts` may or may not be byte-equivalent to what the build would produce. If the build does any transformation beyond type-stripping — `tsc` with `module` set, swc with a target that polyfills async, esbuild with `--target`, any tsconfig path rewriting — the `.ts` source is **not** what the package shipped. The runtime is silently running a different program. Reversibility breaks: behavior under `nub` diverges from behavior under `node` running the same package.

**2. The error is asymmetric.** When it works, it works invisibly. When the `.ts` source has a stray decorator or enum (non-erasable syntax), the user gets a strip-types failure from a file they didn't write and don't know they're running. The stack trace will say `node_modules/some-pkg/src/foo.ts`, which the user has never heard of — they thought they were running `some-pkg/dist/foo.js`.

**3. The `exports` map is supposed to be a public contract.** A package's `exports` declares which paths are reachable and which aren't. Layer 2 says: actually, the runtime can pick a *different* path that the author didn't expose. Subpath privacy (deliberately narrow `exports`) is undermined.

**4. `node_modules` semantics differ between Nub and Node.** A package that publishes both `.ts` and `.js` will behave one way on Node (`.js`) and a different way on Nub (`.ts`). Bugs that only reproduce on Nub (or only on Node) become possible. The compatibility trust contract exists specifically to prevent this class of divergence.

**5. Catching `ERR_PACKAGE_PATH_NOT_EXPORTED` is fragile.** tsx does this by error-message string-matching (`getMissingPathFromNotFound` in tsx's source). Future Node versions that change the error format break the swap. This is a maintenance liability that scales with how invasive the intercept is.

## Is this a tsc-only convention that runtime tools mirrored?

Partly yes. Here's the chain:

- **tsc**, with `moduleResolution: "nodenext"` and similar, has always done extension substitution for `.js → .ts` (it has to — it sees `.ts` source files but emits `.js` paths). This is purely a type-checking-time behavior; the emitted `.js` files have `.js` imports and the runtime is expected to find `.js`.
- **`allowImportingTsExtensions` + `rewriteRelativeImportExtensions`** (TS 5.7) made the type-time/runtime split explicit: write `.ts` in source, get `.js` in output. Both are runtime-honest.
- **The TS team's documented position** ([microsoft/TypeScript#61991](https://github.com/microsoft/TypeScript/issues/61991), [#61050](https://github.com/microsoft/TypeScript/issues/61050)): rewriting only applies to relative paths, not to paths resolved through `exports`/`imports` maps. They explicitly chose not to do Layer 2 inside the compiler.
- **tsx** lifted the "swap `.js` for `.ts` if missing" pattern from the type-checker analogy and applied it to the runtime *including* the exports-map layer, which the compiler itself does not do.
- **Bun** went halfway: relative-path swap yes, exports-map swap no. Inside `node_modules`, Bun has the `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` feature flag that explicitly suppresses the rewrite, citing exactly the authoritative-exports problem above.

So the most accurate framing is: **the type-checker has a "`.ts` source backs the `.js` emit" model that makes sense at compile time. tsx generalized that to runtime, including the exports-map layer. Other runtimes (Bun, plain Node, deno) chose not to follow the generalization, because the type-time/runtime distinction matters at runtime in a way it doesn't at type-check time.**

This is not a settled de-facto standard. tsx is the outlier; the weight of the field is on the not-Layer-2 side.

## Why Bun explicitly hasn't implemented Layer 2

From reading Bun's resolver source:

- `load_as_file_or_directory` at `src/resolver/resolver.rs:5292-5310` does the relative `.js → .ts` rewrite (Layer 1) for the candidate list.
- The exports-map evaluation in `Resolver::resolve_package_imports_or_exports` returns a `ModuleNotFound` result when the exports-mapped path doesn't exist on disk — *without* a retry pass. There's no equivalent of tsx's `getMissingPathFromNotFound` rescue.
- `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` exists as an explicit feature flag to suppress the relative rewrite inside `node_modules` — i.e., even the conservative Layer 1 has cases where Bun says "this is going too far."

The pattern in Bun is **the package's declared shape is authoritative.** When a package says "import me at `./dist/index.js`", that's the contract. If `./dist/index.js` is missing, the package is broken — let it error.

There's no public Bun design doc spelling out the rationale, but the consistency between the code, the feature flag, and the `"bun"` condition (the explicit way to expose `.ts` honestly) is the position. Bun's stance: **opt-in via export condition, not opt-out via silent runtime swap.**

## Should Nub ship Layer 2 in v0?

**No.** The reasons compound:

**Compatibility.** The reversibility filter is the load-bearing principle for every Nub resolution decision. Layer 2 violates it directly — a package's behavior under Nub diverges from its behavior under Node, in a way the package author cannot opt into or out of. This is exactly the kind of "Nub-only ecosystem fork" we're trying to avoid.

**The user problem is solved another way.** Workspaces that want inner-loop development without build steps have:

- pnpm/yarn/npm workspaces with `"exports": "./src/index.ts"` declared honestly. The package author *means* "consume the source" and exposes it. Nub runs that just fine. So does plain Node with strip-types.
- The `"bun"` / `"node"` / `"development"` conditional-export pattern, where the source path is exposed only under specific conditions. Nub supports this transparently because Node's resolver does.
- Build-on-demand tooling (Turborepo, Nx) where the inner-loop build step is incremental and ~free.

The set of cases Layer 2 uniquely solves is **monorepo packages whose authors chose to declare `dist/*.js` in exports but didn't build, *and* expect downstream consumers to magically find the source.** This is a configuration mistake, not an unmet need. The fix is for the package author to either build, or declare exports honestly.

**Implementation cost is non-trivial.** Layer 2 means catching Node's resolver errors, parsing them, and retrying with a mutated specifier. tsx does this by string-matching on the error message — fragile across Node versions. A cleaner implementation would re-walk the exports map ourselves, which means re-implementing exports evaluation — exactly the thing [`module-resolution.md`](module-resolution.md) §"Things we explicitly don't move to Rust" says we shouldn't do.

**Bun is the right reference point.** Bun is what TS-runtime users benchmark against. Bun doesn't ship Layer 2 and it's the most popular TS runtime in 2026. The lack of pressure for Layer 2 in Bun's issue tracker is informative: the missing feature isn't visibly hurting adoption.

**The permissive reading holds.** If we can resolve a `.js` file (it exists on disk, the exports map points to it), we just run it. There's no compelling reason to second-guess the package author's declared shape.

## What we should ship instead

1. **Layer 1 (relative `.js → .ts`) inside TS-family parents.** When the user wrote `import "./foo.js"` and `./foo.js` doesn't exist but `./foo.ts` does, resolve to `./foo.ts`. This is the `allowImportingTsExtensions + rewriteRelativeImportExtensions` workflow, runs identically on plain Node post-build, and is in the [`module-resolution.md`](module-resolution.md) candidate list already.
2. **Pass-through for `exports` maps.** Whatever Node's resolver returns from an exports-map lookup is what we use. If the file doesn't exist, we surface the same error Node would.
3. **Honest exports for `.ts` source.** If a Nub-using monorepo wants to consume `.ts` source from a sibling package, the sibling declares `"exports": "./src/index.ts"`. We resolve and transpile it like any other `.ts`. No magic.
4. **Document the choice.** A short note in the loader docs explaining: "Nub does not silently substitute `.ts` for `.js` inside package exports maps. Declare `.ts` honestly in `exports`." This makes the behavior predictable and gives migrators from tsx an upgrade path.

## What we should revisit later

If real-world adoption shows that monorepos hit Layer 2 cases often enough to be a friction point — measurable via support requests or direct user feedback — there's a middle ground worth considering:

- **A `--package-source` flag** (or `nub.json` config) that opts into Layer 2 for *the consuming repo*, not for the package ecosystem at large. User-controlled, deliberate, scoped to the user's own project. Doesn't pollute the published-package contract.
- **An `exports` condition** that Nub honors (e.g. `"source"`) giving package authors a clean way to expose `.ts` source for source-aware consumers. This is the conditional-export pattern; it's already supported by Node's resolver and doesn't need any Nub-specific work.

Neither is needed for v0. The point is: **the design space is open for a less-invasive answer if pressure materializes.** Shipping Layer 2 now closes that space.

## Cross-references

- Layer 1 (the unobjectionable half) lives in [`ts-extension-precedence.md`](ts-extension-precedence.md) §Nub recommendation, encoded as `.js` rows in the candidate table.
- The "we don't reimplement Node's resolver" non-goal in [`module-resolution.md`](module-resolution.md#non-goals) covers this case at a higher level; this doc is the longer justification for the specific Layer-2 subcase.
- The reversibility filter is the architectural reason the recommendation lands where it does.

## Sources

- tsx exports-map rescue: `tsx/src/esm/hook/resolve.ts:215-228, 278-291` (`getMissingPathFromNotFound` then retry).
- Bun relative-only swap, no exports-map retry: `bun/src/resolver/resolver.rs:5292-5310` (load_as_file_or_directory), no equivalent in the exports-map path.
- Bun `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES`: `bun/src/bun_core/feature_flags.rs:109-110`.
- Bun `"bun"` condition for honest `.ts` exposure: [bun.sh/docs/runtime/modules](https://bun.sh/docs/runtime/modules).
- TS team position on exports/paths rewriting: [microsoft/TypeScript#61991](https://github.com/microsoft/TypeScript/issues/61991), [microsoft/TypeScript#61050](https://github.com/microsoft/TypeScript/issues/61050).
- TS 5.7 release notes for `rewriteRelativeImportExtensions`/`allowImportingTsExtensions`: [typescriptlang.org/docs/handbook/release-notes/typescript-5-7](https://www.typescriptlang.org/docs/handbook/release-notes/typescript-5-7.html).
- tsx surprising behavior issue: [privatenumber/tsx#442](https://github.com/privatenumber/tsx/issues/442).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
