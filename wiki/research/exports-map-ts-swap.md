# The `.js → .ts` exports-map swap controversy

- **Status:** v1, 2026-05-18. Bottom-line recommendation: **do not ship in v0.**
- **Scope:** the "tsx trick" — silently rewriting `.js` to `.ts` when the `.js` does not exist, including inside a package's `exports` map. Distinct from the candidate-list probing in [[research/ts-extension-precedence]].
- **Builds on:** [[research/module-resolution]], which already declares this a non-goal (this doc is the longer justification), and [[research/tsx-architecture]], where tsx's exports-map swap lives in source.

When a `.ts` file writes `import "./foo.js"` and `./foo.js` does not exist but `./foo.ts` does, the TypeScript ecosystem has converged on *usually* resolving to `./foo.ts`. That much is uncontroversial.

The controversy is the next step out. When `import "some-package"` hits the package's `exports` map, the map points to `./dist/foo.js`, and `./dist/foo.js` is not on disk (because the package shipped unbuilt TS sources alongside its `package.json`), should the runtime probe `./dist/foo.ts` and resolve to that?

tsx says yes. Bun says no. The TS compiler says yes for type-checking but not for runtime. The TS team's guidance for package authors is to declare the `.ts` honestly in the exports map instead. Nothing about this is settled.

## The two layers

Two adjacent behaviors that are easy to confuse:

**Layer 1: relative-path `.js → .ts` rewriting.** Inside a TS-family file, the user wrote `import "./foo.js"` and `./foo.js` does not exist, so the runtime tries `./foo.ts` and resolves to that. This is the TypeScript-blessed pattern that `allowImportingTsExtensions` + `rewriteRelativeImportExtensions` emit *into*: the user writes the `.js` extension that will exist post-build, and the runtime (or tsc itself, running in-source) sees through it to the `.ts` source.

Status across tools:

- **tsx**: yes (via candidate-list swap table — `src/utils/map-ts-extensions.ts:20`).
- **Bun**: yes (resolver `.js → .ts` rewrite at `src/resolver/resolver.rs:5686-5739`, except where `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` overrides).
- **ts-node / Vite / Rolldown / esbuild**: yes (built into their candidate orders or extension lists).
- **Plain Node (--experimental-strip-types)**: no. `import "./foo.js"` requires `./foo.js` to exist.

Layer 1 is **not** the controversy. It extends "`.ts` beats `.js`" ([[research/ts-extension-precedence]]) from extensionless to explicit-extension imports. Nub should ship it: same code path as the candidate list, no extra cost. The Rust-side `candidates_for(...)` in [[research/module-resolution]] already generates `.ts` candidates for `.js` specifiers in TS-family parents.

**Layer 2: exports-map `.js → .ts` swap.** The package author wrote `"exports": { ".": "./dist/index.js" }`. Node's resolver evaluates that, gets `./dist/index.js`, finds it missing on disk, and throws `ERR_MODULE_NOT_FOUND`. tsx catches that error, pulls the missing path out of it, swaps `.js → .ts`, and retries (`src/esm/hook/resolve.ts:215-222`). If `./dist/index.ts` exists, the import resolves to it.

This is the controversy. **Nub should not ship Layer 2 in v0.**

## What problem does Layer 2 solve?

The intended user story is a workspace with packages A and B, where B imports A and A's build has not run. Under tsx, B's import of A works anyway — tsx silently resolves to A's `./src/index.ts`. No build step in the inner loop.

A's `package.json` declares `"exports": { ".": "./dist/index.js" }`, because that is what ships to npm, so during development `./dist/index.js` does not exist.

The same shape applies to:

- A monorepo where every package's `exports` field points to `dist/` but the workspace dev environment hasn't run any builds.
- A package author who ships both `.ts` source and `.js` build artifacts in the same npm publish, and a downstream tool wants to consume the `.ts` directly for some reason (Bun's "bun" condition use case).
- "Just-in-time monorepo development" where running any package's tests pulls in dependencies' source rather than dependencies' build output.

The value is real in a narrow set of cases, all of them **monorepo / workspace cases**. It is never useful in a single repo, where there is no other package's exports map to navigate, and never useful against properly-published third-party deps in `node_modules`, where the `.js` files exist.

## In what scenarios is it useful?

The population where Layer 2 changes outcomes:

1. **Workspace packages** whose `exports` points at `dist/*.js` that doesn't exist locally because the user hasn't run the build.
2. **Hand-cloned source dependencies** where the user dropped a git checkout into `node_modules` (rare; `pnpm install --link` / workspace `*` protocol is the supported path).
3. **Packages published to npm with `.ts` sources alongside `.js` builds** *and* a tool deliberately wanting the `.ts`. Bun's solution to this is the explicit `"bun"` export condition (`"exports": { "bun": "./src/index.ts", "default": "./dist/index.js" }`). The condition is the *honest* way to expose `.ts` — the package author declares it.

Of these, only **(1)** is a non-trivial population. (2) is a historical curiosity; (3) is solved by conditional exports without a runtime swap.

The question reduces to: *should Nub's resolve hook silently ignore the package's authored `exports` map and substitute source files, to save monorepo users from running their build step?*

## Where Layer 2 causes problems

The cost of swapping `.js` for `.ts` after the exports map resolved:

**1. The package's `exports` becomes non-authoritative.** The author *declared* `./dist/index.js`; the swap returns `./src/index.ts`, which may or may not be byte-equivalent to the build output. If the build transforms beyond type-stripping — `tsc` with `module` set, swc with a target that polyfills async, esbuild with `--target`, any tsconfig path rewriting — the `.ts` source is **not** what the package shipped, and the runtime silently runs a different program. Reversibility breaks: behavior under `nub` diverges from `node` on the same package.

**2. The error is asymmetric.** When it works, it works invisibly. When the `.ts` source carries a stray decorator or enum (non-erasable syntax), the user gets a strip-types failure from a file they did not write and do not know they are running. The stack trace names `node_modules/some-pkg/src/foo.ts`, which the user has never heard of — they thought they were running `some-pkg/dist/foo.js`.

**3. The `exports` map is a public contract.** It declares which paths are reachable and which are not. Layer 2 lets the runtime pick a *different* path the author never exposed, undermining subpath privacy.

**4. Semantics of `node_modules` differ between Nub and Node.** A package publishing both `.ts` and `.js` behaves one way on Node (`.js`) and another on Nub (`.ts`), making bugs possible that reproduce on only one. The compatibility trust contract exists to prevent this class of divergence.

**5. Catching `ERR_PACKAGE_PATH_NOT_EXPORTED` is fragile.** tsx string-matches the error message (`getMissingPathFromNotFound` in its source), so a future Node version changing the error format breaks the swap — a maintenance liability that scales with how invasive the intercept is.

## Is this a tsc-only convention that runtime tools mirrored?

Partly. The chain:

- **tsc**, with `moduleResolution: "nodenext"` and similar, has always done extension substitution for `.js → .ts` (it has to — it sees `.ts` source files but emits `.js` paths). This is purely a type-checking-time behavior; the emitted `.js` files have `.js` imports and the runtime is expected to find `.js`.
- **`allowImportingTsExtensions` + `rewriteRelativeImportExtensions`** (TS 5.7) made the type-time/runtime split explicit: write `.ts` in source, get `.js` in output. Both are runtime-honest.
- **The TS team's documented position** ([microsoft/TypeScript#61991](https://github.com/microsoft/TypeScript/issues/61991), [#61050](https://github.com/microsoft/TypeScript/issues/61050)): rewriting only applies to relative paths, not to paths resolved through `exports`/`imports` maps. They explicitly chose not to do Layer 2 inside the compiler.
- **tsx** lifted the "swap `.js` for `.ts` if missing" pattern from the type-checker analogy and applied it to the runtime *including* the exports-map layer, which the compiler itself does not do.
- **Bun** went halfway: relative-path swap yes, exports-map swap no. Inside `node_modules`, Bun has the `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` feature flag that explicitly suppresses the rewrite, citing exactly the authoritative-exports problem above.

The accurate framing: **the type-checker has a "`.ts` source backs the `.js` emit" model that makes sense at compile time; tsx generalized it to runtime, including the exports-map layer; Bun, plain Node and Deno declined the generalization, because the type-time/runtime distinction matters at runtime in a way it does not at type-check time.**

This is not a settled de-facto standard. tsx is the outlier; the weight of the field sits on the not-Layer-2 side.

## Why Bun has not implemented Layer 2

From Bun's resolver source:

- `load_as_file_or_directory` at `src/resolver/resolver.rs:5292-5310` does the relative `.js → .ts` rewrite (Layer 1) for the candidate list.
- The exports-map evaluation in `Resolver::resolve_package_imports_or_exports` returns `ModuleNotFound` when the exports-mapped path is missing on disk, with no retry pass and no equivalent of tsx's `getMissingPathFromNotFound` rescue.
- `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES` exists as an explicit feature flag suppressing the relative rewrite inside `node_modules` — even the conservative Layer 1 has cases where Bun says this goes too far.

Bun's pattern: **the package's declared shape is authoritative.** When a package says "import me at `./dist/index.js`", that is the contract, and a missing `./dist/index.js` means a broken package that should error.

No public Bun design doc spells out the rationale, but the code, the feature flag, and the `"bun"` condition (the honest way to expose `.ts`) are consistent: **opt in via export condition, not out via silent runtime swap.**

## Should Nub ship Layer 2 in v0?

**No.** The reasons compound.

**Compatibility.** The reversibility filter is the load-bearing principle for every Nub resolution decision, and Layer 2 violates it directly: a package behaves differently under Nub than under Node, in a way the package author can neither opt into nor out of. That is the Nub-only ecosystem fork the filter exists to prevent.

**The user problem is solved another way.** Workspaces wanting an inner loop without build steps have:

- pnpm/yarn/npm workspaces with `"exports": "./src/index.ts"` declared honestly. The author means "consume the source" and exposes it; Nub runs that, and so does plain Node with strip-types.
- The `"bun"` / `"node"` / `"development"` conditional-export pattern, exposing the source path only under specific conditions. Nub supports it transparently because Node's resolver does.
- Build-on-demand tooling (Turborepo, Nx) where the inner-loop build is incremental and near-free.

What Layer 2 uniquely solves is **monorepo packages whose authors declared `dist/*.js` in exports, did not build, and expect downstream consumers to find the source anyway** — a configuration mistake, not an unmet need. The fix is to build, or to declare exports honestly.

**Implementation cost is non-trivial.** Layer 2 means catching Node's resolver errors, parsing them, and retrying with a mutated specifier. tsx string-matches the error message, which is fragile across Node versions. A cleaner implementation would re-walk the exports map, which means re-implementing exports evaluation — the thing [[research/module-resolution]] lists as explicitly out of scope for the Rust side.

**Bun is the right reference point.** It is what TS-runtime users benchmark against, it does not ship Layer 2, and it is the most popular TS runtime in 2026. The absence of pressure for Layer 2 in Bun's issue tracker suggests the missing feature is not hurting adoption.

**The permissive reading holds.** When a `.js` file resolves — it exists on disk and the exports map points to it — run it. Second-guessing the author's declared shape needs a reason there is not.

## What to ship instead

Layer 1 only, with `exports` maps passed through to Node untouched, source-consuming monorepos told to declare `.ts` in `exports`, and the divergence from tsx documented.

1. **Layer 1 (relative `.js → .ts`) inside TS-family parents.** When the user wrote `import "./foo.js"`, `./foo.js` is missing and `./foo.ts` exists, resolve to `./foo.ts`. This is the `allowImportingTsExtensions + rewriteRelativeImportExtensions` workflow, runs identically on plain Node post-build, and is already in the [[research/module-resolution]] candidate list.
2. **Pass-through for `exports` maps.** Use whatever Node's resolver returns; when the file is missing, surface the error Node would.
3. **Honest exports for `.ts` source.** A Nub-using monorepo that wants to consume a sibling's `.ts` has the sibling declare `"exports": "./src/index.ts"`, which Nub resolves and transpiles like any other `.ts`.
4. **Document the choice.** A short loader-docs note: "Nub does not silently substitute `.ts` for `.js` inside package exports maps. Declare `.ts` honestly in `exports`." Predictable behavior, and an upgrade path for migrators from tsx.

## What to revisit later

If adoption shows monorepos hitting Layer 2 cases often enough to be a friction point — measurable via support requests or direct feedback — two less-invasive middle grounds exist:

- **A `--package-source` flag** (or config field) opting into Layer 2 for *the consuming repo* rather than the package ecosystem at large: user-controlled, deliberate, and no pollution of the published-package contract.
- **An `exports` condition** Nub honors (`"source"`, say), giving package authors a clean way to expose `.ts` source to source-aware consumers. Node's resolver already supports the pattern, so it needs no Nub-specific work.

Neither is needed for v0. **The design space stays open for a less-invasive answer if pressure materializes**, and shipping Layer 2 now closes it.

## Cross-references

Two docs carry the parts this one only rules on: the Layer-1 candidate table, and the broader non-goal of reimplementing Node's resolver.

- Layer 1, the unobjectionable half, lives in [[research/ts-extension-precedence]] §Nub recommendation, encoded as `.js` rows in the candidate table.
- The "don't reimplement Node's resolver" non-goal in [[research/module-resolution#Non-goals|`module-resolution.md`]] covers this case at a higher level; this doc is the longer justification for the Layer-2 subcase.

## Sources

Line-level locations in the tsx and Bun resolvers, plus the TypeScript issue threads and release notes behind the compiler's position.

- tsx exports-map rescue: `tsx/src/esm/hook/resolve.ts:215-228, 278-291` (`getMissingPathFromNotFound` then retry).
- Bun relative-only swap, no exports-map retry: `bun/src/resolver/resolver.rs:5292-5310` (load_as_file_or_directory), no equivalent in the exports-map path.
- Bun `DISABLE_AUTO_JS_TO_TS_IN_NODE_MODULES`: `bun/src/bun_core/feature_flags.rs:109-110`.
- Bun `"bun"` condition for honest `.ts` exposure: [bun.sh/docs/runtime/modules](https://bun.sh/docs/runtime/modules).
- TS team position on exports/paths rewriting: [microsoft/TypeScript#61991](https://github.com/microsoft/TypeScript/issues/61991), [microsoft/TypeScript#61050](https://github.com/microsoft/TypeScript/issues/61050).
- TS 5.7 release notes for `rewriteRelativeImportExtensions`/`allowImportingTsExtensions`: [typescriptlang.org/docs/handbook/release-notes/typescript-5-7](https://www.typescriptlang.org/docs/handbook/release-notes/typescript-5-7.html).
- tsx surprising behavior issue: [privatenumber/tsx#442](https://github.com/privatenumber/tsx/issues/442).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
