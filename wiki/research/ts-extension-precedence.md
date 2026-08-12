---
**Status:** v1, 2026-05-18.
**Scope:** Extensionless ESM imports inside `.ts`/`.tsx`/`.mts`/`.cts` files. What extension wins when `./foo` could match `./foo.ts`, `./foo.tsx`, `./foo.js`, `./foo/index.ts`, etc.
**Builds on:** [`module-resolution.md`](module-resolution.md) (parent-extension-aware probing), [`tsx-architecture.md`](tsx-architecture.md) (candidate-list pattern).
**Sibling:** [`exports-map-ts-swap.md`](exports-map-ts-swap.md) — the related `.js → .ts` exports-map controversy.
**Informs:** the resolve hook, and the Rust-side `candidates_for(parent_ext, …)` candidate-list generation.
---

# Extension precedence in extensionless ESM imports

A TypeScript codebase writing `import "./foo"` from a `.ts` file has, in the worst case, eight files on disk that all want to be `./foo`:

```
./foo.ts   ./foo.tsx   ./foo.js   ./foo.jsx
./foo.mts  ./foo.mjs   ./foo.cts  ./foo.cjs
./foo.json
./foo/index.{ts,tsx,js,jsx,mts,mjs,cts,cjs,json}
```

Which wins? Every tool that runs TypeScript natively has had to answer this. The set of answers turns out to be remarkably consistent for `.ts ↔ .js` and surprisingly divergent at the edges.

## TL;DR

- **De-facto standard for extensionless probing inside a TS-family parent: `.ts` before `.js`, and `.tsx` before `.jsx`.** Every serious TS runtime agrees.
- **`.mts`/`.cts` are uniformly probed alongside (or before) their `.mjs`/`.cjs` counterparts in TS-family parents.** Same logic, same direction.
- **Where tools disagree is the rest of the candidate list** — ordering of `.tsx` vs `.ts`, `.jsx` vs `.js`, the position of `.json`, and whether `node_modules` gets a different (`.js`-first) list.
- **Nub's parent-extension-aware ordering** ([`module-resolution.md`](module-resolution.md) §Candidate probing) is well inside the consensus and cheaper at the hot first-probe than the fixed orders Bun/tsx use.

## Concrete behavior matrix

Local-file probe order for extensionless `import "./foo"` from a `.ts`-family parent. (`node_modules` probe order is listed separately where it diverges.)

| Tool                          | Probe order (TS parent, local file)                         | `node_modules` override                                                  |
| ----------------------------- | ----------------------------------------------------------- | ------------------------------------------------------------------------ |
| **Bun** (`MODULE_EXTENSION_ORDER`)  | `.tsx, .jsx, .mts, .ts, .mjs, .js, .cts, .cjs, .json` | `.mjs, .jsx, .js, .mts, .tsx, .ts, .cjs, .cts, .json` (deps prefer `.js`) |
| **tsx**                       | `.ts, .tsx, .jsx, .js, .json`                               | `.js, .json, .ts, .tsx, .jsx` (deps prefer `.js`)                        |
| **ts-node** (`esm` loader)    | `.ts, .tsx, .js, .jsx` (then `.json`)                       | none                                                                     |
| **Vite** (`resolve.extensions`) | `.mjs, .js, .ts, .jsx, .tsx, .json` (default)             | none                                                                     |
| **Rolldown** (default)        | `.tsx, .ts, .jsx, .js, .json`                               | none by default; configurable                                            |
| **Rspack** (default)          | `.js, .json` (TS adds `.ts, .tsx, .js, .json` via preset)   | none                                                                     |
| **esbuild** (`resolveExtensions`) | `.tsx, .ts, .jsx, .js, .css, .json` (default; *bundler* only) | none built-in                                                       |
| **oxc-resolver**              | `.js, .json, .node` (base default; TS users must add)       | none                                                                     |
| **Deno**                      | does not probe — explicit extensions required               | n/a                                                                      |

(Bun source: `src/resolver/options.rs:177-190`. tsx source: `src/utils/map-ts-extensions.ts:4-16`. esbuild content-types page documents the default `resolveExtensions`. Vite `DEFAULT_EXTENSIONS` is the canonical export of `resolve.ts`. Rolldown's docs page lists the default. Rspack ships `.js, .json` and the TS preset adds the TS extensions.)

A few oddities to call out:

- **Bun's local-default and ESM-mode lists differ.** The `MODULE_EXTENSION_ORDER` quoted above is the ESM path; Bun's generic default (`EXTENSION_ORDER`) is `.tsx, .ts, .jsx, .cts, .cjs, .js, .mjs, .mts, .json`. The ESM list deliberately interleaves `.mts/.mjs` higher than the CJS variants, and the CJS list pushes `.cts/.cjs` higher. In practice, both lists agree that `.ts` beats `.js` in the local case.
- **`node_modules` flips the priority** in both Bun and tsx. Both cite the esbuild v0.20.0 release notes: a published package's `.js` is more likely to be correct (because the author ran the build) than the `.ts` source they may have shipped alongside. This is a real design decision, not an oversight.
- **Rspack** ships `.js, .json` as the literal default, but every TS preset / starter (`@rsbuild/plugin-typescript`, `create-rspack-app`) extends it to `.ts, .tsx, .js, .json` before user code sees it. The "default" in the docs and the "default" in practice are different things.
- **esbuild's default is bundler-only.** esbuild's `transform` API (the one tsx uses) does no resolution at all — tsx supplies the probing logic above it. So esbuild's order matters only for `nub build`-class consumers, not for runtime `import` semantics.
- **oxc-resolver's "default" is conservative on purpose.** It mirrors Node's runtime resolver, which doesn't know about TS. Anyone embedding oxc-resolver in a TS-aware tool sets the extension list themselves. So oxc-resolver's "default" doesn't really count as a prior-art datapoint for the TS question.
- **Deno doesn't play.** Deno's no-magic policy means `import "./foo"` is an error; the user writes `import "./foo.ts"`. Deno cleanly opts out of this entire question.

## The de-facto consensus

Strip out the bundler-only and not-really-a-TS-tool entries (esbuild, oxc-resolver, Rspack-without-preset, Deno) and the TS-runtime field shrinks to: Bun, tsx, ts-node, Vite, Rolldown. Within that set:

- **`.ts` before `.js`** in TS-family parents — *unanimous*.
- **`.tsx` before `.jsx`** — *unanimous*.
- **`.mts` before `.mjs`, `.cts` before `.cjs`** — *unanimous* where the tool considers them at all (tsx and ts-node don't probe `.mts`/`.cts` in their extensionless path; Bun and Rolldown do).
- **`.tsx` vs `.ts` ordering** — *split*. tsx, esbuild, Rolldown put `.tsx` first; Bun's MODULE list also puts `.tsx` first; ts-node puts `.ts` first; Vite puts `.ts` before `.tsx` after `.js`.
- **`.json` last** — *unanimous*.

So the load-bearing claim is narrow but solid: **TS extensions beat their JS counterparts**. The internal ordering among the TS extensions is contested.

## Why `.tsx` is usually before `.ts`

The argument for `.tsx`-first (esbuild, tsx, Rolldown, Bun ESM list): a `.tsx` file is a superset of `.ts` parseable as TS-with-JSX, and a `.tsx` file is the rarer case — when it exists, the user definitely wants it. False positives (parsing a plain `.ts` as `.tsx` and choking on a `<` operator) don't happen because the lookup is by filename, not by content.

The argument for `.ts`-first (ts-node, Vite, our [parent-aware ordering](#nub-recommendation)): in any given import, the relative likelihood of `./foo.ts` existing is much higher than `./foo.tsx` existing, so probing `.ts` first hits earlier on the average import. This matters if you're counting per-import stat syscalls — but only for the case where `./foo.tsx` is present, which is rare.

Practical impact is zero in cache-warm steady state and negligible otherwise: **the cost difference is below the measurement floor.** Both choices are defensible, so we pick on what makes the algorithm cleanest rather than on perf.

## Same logic for `.mts ↔ .mjs` and `.cts ↔ .cjs`?

Yes. Every TS runtime that handles `.mts`/`.cts` at all probes **`.mts` before `.mjs`** and **`.cts` before `.cjs`** inside TS-family parents. The rationale is identical: the user's source-of-truth is the `.ts`-family file, not the `.js`-family file. Bun's `MODULE_EXTENSION_ORDER` is explicit:

```
.tsx, .jsx, .mts, .ts, .mjs, .js, .cts, .cjs, .json
       ^^^         ^^^
       .mts before .mjs    (and .cts before .cjs, further down)
```

tsx's `mapTsExtensions` is more conservative — it only probes `.cts`/`.mts` when the *parent's extension* is `.cjs`/`.mjs` respectively (`src/utils/map-ts-extensions.ts:22-23`). Same principle in the other direction: keep the TS variant adjacent to its JS variant for the case that needs it.

The asymmetry to remember: **`.cts`/`.mts` files are uncommon enough in real codebases that getting them slightly wrong has minimal blast radius.** The TS team itself only added them under the package-`type`-aware module system, and most real-world TS code is still plain `.ts` with `package.json` `"type": "module"`. Designing the candidate list around `.ts`/`.tsx` ↔ `.js`/`.jsx` is what matters.

## What about `./foo/index.*`?

Every TS runtime tries both `./foo.<ext>` and `./foo/index.<ext>`, in that order. The directory case uses the same extension list, just with `/index` appended. No tool inverts this. Cost: one extra stat per import in the directory case (~1 μs warm).

## Nub recommendation

Adopt the **parent-extension-aware ordering** already in [`module-resolution.md`](module-resolution.md) §Candidate probing — which is *slightly* novel relative to the field but in the same spirit as the de-facto consensus, and measurably cheaper at the first probe:

| Parent | Probe order                                  |
| ------ | -------------------------------------------- |
| `.ts`  | `.ts, .tsx, .js, .jsx, .json`                |
| `.tsx` | `.tsx, .ts, .jsx, .js, .json`                |
| `.mts` | `.mts, .ts, .mjs, .js, .json`                |
| `.cts` | `.cts, .ts, .cjs, .js, .json`                |
| `.jsx` | `.jsx, .js, .tsx, .ts, .json`                |
| `.mjs` | `.mjs, .js, .mts, .ts, .json`                |
| `.cjs` | `.cjs, .js, .cts, .ts, .json`                |
| `.js`  | (gate doesn't fire — Node's resolver wins)   |

(`.js` parents in ESM mode are gated out by the [scope rule in `module-resolution.md`](module-resolution.md#scope-looser-but-no-looser-than-needed): extensionless is a TS-file concession, not a JS-ESM relaxation.)

Rationale:

1. **First-probe hit rate is the dominant cost on warm caches.** A `.ts → .ts` import (the overwhelming majority of imports inside a TS app) resolves on probe 1. A `.tsx → .tsx` (component → sibling component) likewise. Bun and tsx pay an extra probe in the `.tsx`-parent case because their lists are static and assume the parent is `.ts`.
2. **`.mts/.cts` parents get the right answer too** without forcing the rest of the list to interleave `.mts/.mjs`. Bun's interleaved ESM list reflects the same intuition; ours just keys it explicitly on parent.
3. **The list is short on purpose.** Five entries per parent. Bun's 9-entry list (`.tsx, .jsx, .mts, .ts, .mjs, .js, .cts, .cjs, .json`) probes `.cts/.cjs` on every ESM import — wasted probes for the ~99% of imports that have no `.cts`/`.cjs` involvement. We include them only in the `.cts`-parent row.
4. **`node_modules` is out of scope.** Per [`module-resolution.md`](module-resolution.md#non-goals), Nub doesn't probe extensions inside published dependencies; the shipped shape is what they shipped. Bun and tsx flip the priority for `node_modules`; we don't probe at all.

### Worst-case probe count

5 (the `.ts` row tries `.ts, .tsx, .js, .jsx, .json` before giving up). Same worst case as Bun's local lists, less than Bun's `node_modules` lists (9). In practice the warm-cache average is ~1.05 probes per import for TS-heavy apps.

### What about `.tsx`-first?

Considered. The argument: when `./foo.tsx` exists, it's almost certainly what the user wanted. Counter-argument: our parent-aware ordering already puts `.tsx` first when the parent is `.tsx` — the case where `./foo.tsx` is most likely to exist. The remaining cases (plain-`.ts` parent reaching for a `.tsx` sibling) are rare enough that the second probe is fine. Stick with `.ts`-first for `.ts` parents.

### Tsconfig `allowImportingTsExtensions`

Out of scope for this doc. When the user writes `import "./foo.ts"` the extension is already present and the candidate list doesn't fire. The only interaction is: if `./foo.ts` doesn't exist, do we also probe `./foo.tsx` / `./foo.js`? Recommendation: no — the user wrote `.ts`, they get the `.ts` lookup or an error. Matches Node's own behavior with explicit extensions.

### What `--node` mode does

Skips the gate entirely. With `--node`, Nub's vanilla-Node-faithful mode, the resolve hook returns `null` for extensionless imports and lets Node's stricter ESM resolver throw `ERR_MODULE_NOT_FOUND`. The probing is a Nub-mode-only relaxation.

## What we explicitly don't standardize on

- **`node_modules` extension flipping.** We don't probe inside `node_modules` at all, so the question doesn't arise. If we ever do — e.g. for workspace-symlinked TS packages — match Bun/tsx and prefer `.js` over `.ts` for the unbuilt-package case.
- **`.json` early.** Some configs (Webpack-style) put `.json` earlier. Our list keeps it last on every row. JSON imports are rare enough relative to JS/TS that probing for them ahead of `.tsx`/`.jsx` is paying a stat-cost on every miss for the benefit of a small minority.
- **`.css`, `.svg`, `.wasm`** in the extensionless list. Nub's extension-loader surface is `.ts`/`.tsx`/`.jsx` only, so asset extensions don't get probed.

## Cross-link: the `.js → .ts` swap

This doc covers what to do when the user wrote `import "./foo"` (extensionless). The neighboring question — what to do when they wrote `import "./foo.js"` and a `./foo.ts` exists — is the subject of [`exports-map-ts-swap.md`](exports-map-ts-swap.md). The two are often confused but solve different problems.

## Sources

- Bun extension order: `bun/src/resolver/options.rs:177-190`.
- tsx extension map: `tsx/src/utils/map-ts-extensions.ts:4-16`.
- esbuild default `resolveExtensions`: [esbuild.github.io/api/#resolve-extensions](https://esbuild.github.io/api/#resolve-extensions), [esbuild content-types page](https://esbuild.github.io/content-types/).
- Vite `DEFAULT_EXTENSIONS`: [vite.dev/config/shared-options](https://vite.dev/config/shared-options#resolve-extensions).
- Rolldown defaults: [rolldown.rs/options/resolve](https://rolldown.rs/options/resolve).
- Rspack defaults: [rspack.rs/config/resolve](https://rspack.rs/config/resolve).
- oxc-resolver defaults: [oxc-project/oxc-resolver](https://github.com/oxc-project/oxc-resolver).
- ts-node ESM behavior: [TypeStrong/ts-node Discussion #1781](https://github.com/TypeStrong/ts-node/discussions/1781).
- Deno no-magic-resolution: [docs.deno.com/runtime/reference/ts_config_migration](https://docs.deno.com/runtime/reference/ts_config_migration/).
- esbuild v0.20.0 release notes (cited by tsx for the deps-prefer-`.js` rationale): [github.com/evanw/esbuild/releases/tag/v0.20.0](https://github.com/evanw/esbuild/releases/tag/v0.20.0).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
