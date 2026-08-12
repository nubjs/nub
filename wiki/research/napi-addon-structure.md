---
**Status:** v1, 2026-05-25. Canonical living doc.

**Question:** Should Nub ship a single monolithic N-API addon (oxc transpiler + data-format parsers in one `.node` file) or multiple per-concern addons (`nub-transpile.node`, `nub-data-loaders.node`, …)?

**Headline answer:** **Single monolithic addon for v0.1.** Nub is the only consumer of these bindings, and they ride inside the same `@nubjs/nub-<platform>` package as the Rust binary (per the brand-boundary exception in [`AGENTS.md`](../../AGENTS.md)). Every split argument — lazy load, independent versioning, size win — collapses against Nub's distribution shape.

**Builds on:** [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md).
---

# N-API addon structure: monolithic vs per-concern

The choice turns on who consumes the bindings. This document surveys what comparable napi-rs projects ship, then weighs a split against Nub's distribution, runtime and maintenance costs.

## 1. TL;DR

One addon, because Nub is its only consumer and the lazy-load win sits inside Node's startup noise floor.

- **Recommendation: (A) single monolithic addon — `nub-native.node` — exposing both the oxc transpile entry point and the YAML/TOML/JSON5/JSONC parsers.** One cargo crate, one napi-rs build, one platform matrix slot.
- **Headline reason: Nub is the only consumer.** The ecosystem pattern of "one addon per logical library" (oxc-parser / oxc-transform / oxc-resolver each their own npm package) exists because those crates are independently consumable by *other people*. Our addon is internal plumbing distributed inside `@nubjs/nub-<platform>`; nobody is ever going to `npm install nub-transpile` standalone.
- **Most surprising finding:** the "lazy-load data parsers until first `.yaml` import" trick is technically achievable with multiple addons but saves on the order of **1–2 ms of cold start and ~1 MB of resident memory for projects that never import `.yaml`/`.toml`/`.json5`/`.jsonc`** — well inside the noise floor of Node's own ~27 ms startup. Not worth a doubled CI matrix.

## 2. Ecosystem survey: what major napi-rs projects ship

The dominant pattern is **one addon per logical library**. Different libraries get different packages; sub-features of the same library do not.

### oxc — per-library split

Three independent npm packages come out of oxc, each with its own per-platform binding fan-out: the parser, the TS/JSX transformer, and the resolver.

[`oxc-parser`](https://www.npmjs.com/package/oxc-parser) 0.132.0 (parsing), [`oxc-transform`](https://www.npmjs.com/package/oxc-transform) 0.132.0 (TS/JSX transform), and [`oxc-resolver`](https://www.npmjs.com/package/oxc-resolver) 11.19.1 (resolver, with 20 `@oxc-resolver/binding-<platform>` packages). Each is its own crate, napi-rs wrapper, and publish cadence. These are separate libraries with separate third-party consumers — Vite/Rolldown want the resolver, tsdown wants the transformer, many tools want only the parser AST. Splitting is justified by external consumption, not internal preference.

### swc — single `@swc/core` for the core compiler

[`@swc/core`](https://www.npmjs.com/package/@swc/core) 1.15.40 ships parse + transform + codegen in **one addon** across 12 per-platform optional dependencies.

The separate `@swc/html` and `@swc/css` packages are different libraries; the core JS/TS compiler — parser, transformer, minifier, codegen — is one binding.

### Rolldown — single bundler binding

[`@rolldown/binding-darwin-arm64`](https://www.npmjs.com/package/@rolldown/binding-darwin-arm64) 1.0.2 ships **one 17.8 MB `.node` per platform**. Bundling is one pipeline, so it's one addon. With 594 versions published since March 2024, a per-concern split would 5× the publish surface.

### Lightning CSS — single addon

[`lightningcss`](https://www.npmjs.com/package/lightningcss) 1.32.0 ships **one addon** across 11 platforms. Parser + transformer + minifier + vendor-prefixer + CSS modules all in one binding.

### Biome — single CLI per platform

[`@biomejs/biome`](https://www.npmjs.com/package/@biomejs/biome) 2.4.15 distributes the whole Rust toolchain as one bin per platform (`@biomejs/cli-*`) — formatter, linter, JS/TS/CSS/GraphQL/JSON support all in one executable. Not a napi addon, but the closest distribution-shape analog to Nub.

### Pattern: per-library, not per-feature

The ecosystem rule is **"one addon per independently consumable Rust library."**

One project shipped together gets one addon; separate projects with separate consumers (oxc's three crates) ship separately. Nub's transpile binding plus data-format parsers are a single internal library — the "one addon" side of the line.

## 3. Trade-off analysis

Distribution, runtime, and maintenance, each scored for one addon against several.

### Distribution

Splitting multiplies the CI matrix by the number of addons and wins nothing on bundle size, install time, or publish surface.

- **Platform matrix.** Nub's platforms are macOS arm64/x64, Linux x64/arm64, Windows x64 — five. One addon = 5 CI build slots; two = 10; three = 15. The failure-mode surface scales linearly: a flaky Windows linker doubles release-blocking risk under a split.
- **Bundle size.** oxc-transform native is ~3.5 MB per platform; YAML/TOML/JSON5/JSONC parsers combined are <1 MB statically linked. Combined `.node` ≈ 4–5 MB. Two split `.node` files total roughly the same plus ~50–150 KB of duplicated headers / dynamic-symbol-table per file. No size win.
- **Install time.** Both shapes install inside `@nubjs/nub-<platform>` (a single optional dependency, one tarball fetch). Splitting changes nothing.
- **Publish surface.** Splitting means N crates and N npm packages to version-bump per release. For an internal-only addon, pure overhead.

### Runtime

Three runtime concerns: dlopen cost, the lazy-loading argument, and symbol conflicts. None of them separates the two shapes at v0.1 scale.

- **dlopen cost.** A 4–5 MB `.node` dlopen is sub-ms warm, ~2–3 ms cold. Doubling the count is still sub-perceptible. dlopen happens at preload setup, not per file.
- **Lazy loading.** A multi-addon shape could defer `require('./nub-data-loaders.node')` until the first `.yaml`/`.toml`/`.json5`/`.jsonc` import. Savings: ~1–2 ms cold-start dlopen + ~1 MB RSS for projects with no data-format imports. Cost: a second `.node`, doubled CI, an extra branch in the load-hook hot path. Savings are below Node's own ~27 ms startup floor; cost is real engineering surface. **Non-argument at v0.1 scale.**
- **Symbol conflicts.** napi-rs registers via `napi_register_module_v1`; Rust mangles per-crate symbols into the cdylib. Combining oxc + parser crates into one cdylib has no known collision risk; no embedded jemalloc/mimalloc in these crates by default.

### Maintenance

Versioning, build complexity, and testing all favor one addon while Nub is the only consumer.

- **Versioning.** Independent versioning has external value only with independent consumers. Nub is the sole consumer; everything ships with the `nub` release. No benefit.
- **Build complexity.** Single = one crate, one napi-rs config, one `napi build` per platform. Multiple = N of each. Combining is purely additive at the Cargo.toml level.
- **Testing.** One integration surface vs N. Modest win for single; per-function tests are unchanged.

## 4. Recommendation

**(A) Single monolithic `nub-native.node` addon.**

One cargo crate (`nub-native` or similar) under the Rust workspace, depending on `oxc_transformer` + `oxc_parser` + `oxc_sourcemap` + `serde_yaml` / `yaml-rust2` + `toml` + `json5` + `jsonc-parser`, exposing one napi-rs `#[napi]` surface area with `transpileSync(...)`, `parseYamlSync(...)`, `parseTomlSync(...)`, `parseJson5Sync(...)`, `parseJsoncSync(...)`. Built per platform via napi-rs, shipped inside `@nubjs/nub-<platform>` next to the `nub` Rust binary.

### Rationale

The ecosystem-pattern justification for splitting (oxc's per-crate publish layout) **doesn't apply when there's no external consumer.**

One addon means one cargo crate, one CI matrix slot per platform, one version, one integration test surface. The strongest pro-split argument — lazy-loaded data parsers — saves wall-clock time invisible against Node's own startup floor, and the size/install/dlopen deltas round to zero at Nub's scale.

### Naming

The addon file inside `@nubjs/nub-<platform>` is loaded by the preload, never imported by user code, so its name is not user-facing.

Internal name `nub-native.node` is fine: nothing outside the `@nubjs/nub-*` install-plumbing package references it, so the brand boundary holds.

### Reversibility

Splitting later is cheap, so the single-addon choice is not a one-way door.

A later reason to split — a Phase-2 native addon for HTMLRewriter large enough to warrant lazy load, or a license issue forcing one parser into its own crate — is cheap to act on: extract the data-format functions into a separate cargo crate, add a second `napi build` step, add a second `.node` to `@nubjs/nub-*`, branch the load-hook dispatch on extension. Defer the split until evidence forces it.

## 5. Sources

All npm package data retrieved 2026-05-25.

- [`oxc-resolver` 11.19.1](https://www.npmjs.com/package/oxc-resolver) (2026-02-28) — 20 per-platform `@oxc-resolver/binding-*`.
- [`oxc-transform` 0.132.0](https://www.npmjs.com/package/oxc-transform) — independent publish cadence from oxc-resolver.
- [`oxc-parser` 0.132.0](https://www.npmjs.com/package/oxc-parser) — third independent oxc binding.
- [`@swc/core` 1.15.40](https://www.npmjs.com/package/@swc/core) (2026-05-23) — 12 per-platform optionalDependencies, single addon for parse/transform/codegen.
- [`@rolldown/binding-darwin-arm64` 1.0.2](https://www.npmjs.com/package/@rolldown/binding-darwin-arm64) (2026-05-20) — single 17.8 MB `.node` per platform.
- [`lightningcss` 1.32.0](https://www.npmjs.com/package/lightningcss) (2026-03-09) — 11 per-platform addons, single binding.
- [`@biomejs/biome` 2.4.15](https://www.npmjs.com/package/@biomejs/biome) (2026-05-09) — single CLI per platform via `@biomejs/cli-*`.
- [napi.rs](https://napi.rs/) — N-API binding generator; per-module `napi_register_module_v1` init pattern.
- [`wasm-vs-napi-for-transpile.md`](wasm-vs-napi-for-transpile.md) — establishes N-API as the v0.1 transpiler binding choice.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-05-25 — Initial write-up. Recommendation: single monolithic `nub-native.node` addon for v0.1.
- 2026-05-28 — Corrected the distribution-package scope throughout from `@nub/cli-<platform>-<arch>` to the shipped `@nubjs/nub-<platform>` (A23 doc-drift reconciliation; the bare `@nub` scope is used by nothing). No change to the single-monolithic-addon recommendation — only the package name the addon ships inside.
