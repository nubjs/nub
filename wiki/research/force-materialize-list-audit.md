# Force-materialize list — pre-ship confidence audit

Status: complete (2026-07-03). Verdict: **the shipped 18-package `forceMaterializePackages` list is SHIP-READY with one clean correction (remove `lib0`) and one item to resolve (`@storybook/builder-webpack5`).**

The audit surfaced a larger, adjacent finding that is NOT about this list: a class of *transitive* phantom breaks under default GVS (Firebase RTDB, Nx, es-abstract) that force-materialize is structurally incapable of fixing — those belong to `disableGlobalVirtualStoreForPackages` or a structural GVS fix, and are a separate product call.

The corpus and detector artifacts behind this audit are retained privately; this document is the catalog of what they showed.

## The question and the answer

The list disk-materializes 18 packages (instead of symlinking into the machine-global virtual store) so an undeclared "phantom" import resolves under Nub's default GVS.

Only 3 of the 18 had differential fixtures before this audit. The audit asked: does each of the 18 actually break (or is it dead weight), and are there popular packages that break but aren't listed?

**Answer:** 16/18 are justified verified-breaks; 1 is a false-inclusion (`lib0`, remove); 1 is inconclusive (`@storybook/builder-webpack5`, needs a webpack repro). No meaningful force-materialize *additions* exist in the top-5000 phantom set. The important holes that remain (Firebase RTDB, Nx) are a different mechanism the list cannot address.

## The core finding: two phantom classes, two remedies

A store-resident package's realpath is the machine-global virtual store, so Node's upward `node_modules` walk from it escapes the project. Whether force-materialize can fix the resulting break depends entirely on **where the phantom target lives**:

- **Class 1 — consumer-direct-backend adapter.** The phantom target is a dependency the *consumer installs directly* (at the project root): `@hookform/resolvers/zod → zod`, `swiper/react → react`. The user picks a backend and installs it. **Force-materialize FIXES this** — a project-local materialized copy of the package walks up to the root and finds the direct dep. **This is the class the 18-list targets, and it works** (P2a 9/9 + P2b 7 verified-break-then-fixed).
- **Class 2 — transitive phantom.** The phantom target is a *transitive* (a co-package / umbrella dep), never at the project root: `firebase (@firebase/database) → @firebase/app`, `es-abstract → available-typed-arrays`, the `@nx/*` plugin chain. **Force-materialize CANNOT fix this** — verified: materializing `firebase` *and the entire `@firebase/*` family* still leaves `@firebase/database → @firebase/app` broken, because a materialized transitive lands at `node_modules/.nub/<pkg>@v/node_modules/<pkg>`, not at the root the walk reaches. The remedy is `disableGlobalVirtualStoreForPackages` (whole-install GVS-off → the pnpm-parity hidden hoist tree `node_modules/.nub/node_modules/` is built → transitives resolve; verified) or the structural fix below.

The two Nub settings map exactly onto the two classes: `forceMaterializePackages` ↔ Class 1, `disableGlobalVirtualStoreForPackages` ↔ Class 2. The audit's methodology correction ("use the full HardPhantom set, not just subpath-adapters") is right that main-graph phantoms are candidate holes — but empirically they are almost all Class 2, so they yield `disableGVS` additions, not list additions.

### The structural root cause

Nub-GVS has no shared hoist dir equivalent to pnpm's `node_modules/.pnpm/node_modules/`.

pnpm populates that dir with the whole closure (default `hoist=true`), so a store package's upward walk resolves undeclared transitives; Nub builds its equivalent (`node_modules/.nub/node_modules/`) *only where GVS is OFF* (`crates/nub-cli/src/pm_engine/mod.rs`). Under GVS-on, an undeclared transitive that is actually `require`d at runtime → `MODULE_NOT_FOUND`.

A shared hoist dir under GVS would close the entire Class-2 gap at once while keeping GVS's shared store; adding packages to `disableGVS` is the per-package workaround.

## Bucket A — the shipped 18

Every one of the 18 was run twice, once with force-materialize suppressed and once at the default, and diffed against pnpm.

Harness (all fixtures): dev `nub` v0.2.10 (rebuilt fresh at HEAD `fcba5837`), GVS engaged via `env -u CI` (+ `npm_config_enable_global_virtual_store=true`), external store outside the fixture, `npm_config_force_materialize_packages=<dummy>` for the A(break) run vs default for the B(fix) run, differential vs pnpm 10.15.1. Two harness bugs were caught, corrected, and independently confirmed: `--enable-global-virtual-store` is not a `nub` CLI flag (a rejected install falsely reads as a break), and the external store must live *outside* the fixture dir (a nested store lets the walk climb back into the project root and falsely resolve — a false negative for breaks).

| package | class | verdict | note |
|---|---|---|---|
| @hookform/resolvers | 1 | VERIFIED-BREAK | flagship; backend = the chosen validator (zod, …) |
| cypster→ cypress | 1 | VERIFIED-BREAK | backend = the chosen framework (react/vue/…) |
| langsmith | 1 | VERIFIED-BREAK | backend = the chosen test runner (vitest/jest) |
| @storybook/addon-interactions | 1 | VERIFIED-BREAK* | *default-fix advances past `react` but the `manager` subpath needs transitive `@storybook/instrumenter` (peer `storybook`); `manager` is a bundler target, low Node blast radius |
| @storybook/core | 1 | VERIFIED-BREAK | react/react-dom |
| @testing-library/jest-dom | 1 | VERIFIED-BREAK | vitest / @jest/globals |
| drizzle-orm | 1 | VERIFIED-BREAK* | *react phantom only via the React-Native driver subpaths; a plain-Node backend app never hits it |
| storybook | 1 | VERIFIED-BREAK | react/react-dom |
| swiper | 1 | VERIFIED-BREAK | swiper/react → react |
| @angular/common | 1 | VERIFIED-BREAK | @angular/common/upgrade → @angular/upgrade |
| @angular/router | 1 | VERIFIED-BREAK | @angular/router/upgrade → @angular/upgrade |
| @apollo/client | 1 | VERIFIED-BREAK | relay bridge subpath → relay-runtime |
| @vercel/analytics | 1 | VERIFIED-BREAK | @vercel/analytics/nuxt → @nuxt/kit |
| next-themes | @types | VERIFIED-BREAK | tsc: `.d.ts` needs undeclared `@types/react` (#286) |
| @react-pdf/renderer | @types | VERIFIED-BREAK | tsc: same @types/react class; residual errors are upstream, present under pnpm too |
| preact | 1 | VERIFIED-BREAK | preact-render-to-string |
| **lib0** | — | **FALSE-INCLUSION → REMOVE** | its only phantom (`isomorphic-webcrypto`) is imported solely from `webcrypto.react-native.js`; the `./webcrypto` export map routes the `node` condition to `webcrypto.node.js` (no such import), so Node/Nub never load it. Force-materializing lib0 is pure waste. |
| **@storybook/builder-webpack5** | ? | **INCONCLUSIVE** | `@storybook/global` resolves as a webpack virtual-module from the project cwd, not the builder's store realpath, so force-materialize-of-the-builder is likely the wrong lever. Needs a real `storybook build` webpack repro before keeping or removing. |

No detector red flags: 16/18 independently re-detect as runtime subpath-adapters; next-themes + @react-pdf correctly don't (they are the ambient-@types class, added manually via the #286 tsc differential, not the runtime import-graph detector).

## Bucket B — holes (packages that break under default GVS but aren't listed)

The holes split by class: three Class-2 entries the list is structurally unable to fix, and one marginal Class-1 addition.

### Class 2 (transitive) — real, popular, but NOT force-materialize-fixable → `disableGlobalVirtualStoreForPackages`

Three verified breaks whose phantom target is a transitive, so only turning GVS off resolves them: Firebase Realtime Database, the Nx plugin chain, and the ljharb micro-utils.

- **`firebase`** — `import 'firebase/database'` (Realtime Database) breaks under Nub's true default (`Cannot find package '@firebase/app'`); pnpm resolves. **Blast radius is narrow:** only `firebase/database` breaks — `firebase/app`, `firestore`, `auth`, `storage`, `functions`, `analytics` all resolve. So it hits RTDB users, not all Firebase apps (Firestore, the most common, is fine). Verified fixed by `disableGVS+=firebase` (and by GVS-off). Force-materialize provably cannot fix it.
- **`nx` / `@nx/*`** — the plugin chain (`@nx/js → nx/src/*`, `@nx/eslint → nx` which transitively loads a store-resident `@nx/js`) breaks; force-materialize is whack-a-mole across the graph. Verified fixed by GVS-off. → `disableGVS+=nx`.
- **`es-abstract` (#395), `typed-array-byte-length`, `object.hasown`** — ljharb micro-util transitives (→ `available-typed-arrays` / `call-bind`). Real breaks but low real-world blast radius (es-abstract's phantom path is only the wholesale-main load; modern per-op sibling packages declare completely). Not force-materialize-fixable. Candidates for the structural fix rather than per-package `disableGVS`.

### Class 1 (FM-fixable) — the only genuine force-materialize addition, and it is marginal

One package in the top-5000 phantom set is both a real break and fixable by materializing it.

- **`@base44/vite-plugin`** — genuinely undeclared `vite` (its only deps are `@babel/*`), and `vite` is a real direct devDep at the root, so force-materialize fixes it. Niche (rank ~2638). Optional, low priority — adding one niche package to a global default is marginal.

## Bucket C — investigated non-holes (do NOT act on these)

Four groups of candidates that read as breaks and are not: lazy or guarded imports, detector artifacts, build-tool targets the list already excludes by decision record, and packages that break on pnpm too.

- **Lazy / guarded / optional / bin-only imports** (target unresolvable from the store yet the import never throws at load): `drizzle-kit → drizzle-orm` (bin runs clean), `@oclif/core → ts-node`, `nx → ts-node`, `@nx/devkit → rxjs`, `unzipper → @aws-sdk/client-s3` (optional S3 source), `swagger2openapi → should` (test-only), `@azure/core-rest-pipeline → react-native`, `@typespec/ts-http-runtime → react-native`, `@electric-sql/pglite → ws`, `typeorm → expo-sqlite` (optional driver). The detector's `from_main` static reachability includes these lazy/guarded requires that never run at module load.
- **Detector loader/bundled artifacts:** `requirejs → commonJs/env/lang/…` (AMD module IDs, not npm packages), `@vue/compiler-sfc → coffee-script/ect/just` (consolidate optional preprocessors, guarded), `@tanstack/query-devtools → solid-js/…` (bundled devtools), `nise → inherits/type-detect` (co-present), `playwright-core → chromium-bidi` (bundled).
- **Build-tool targets the list already excludes by decision-record:** `@nrwl/devkit → tslib`, and the subpath build-helpers `event-target-shim → @babel/runtime`, `goober`/`react-i18next → babel-plugin-macros`, `ox → tslib`, `@nestjs/swagger → typescript` — all verified to resolve (not runtime-reached).
- **Broken on pnpm too (out of scope — not a Nub-specific hole):** `bare-fs → bare-os`, `@ardatan/relay-compiler → glob`, `react-native → @react-native-community/cli` (Flow syntax breaks on both PMs), and the whole **Expo family** (`expo-font`/`expo-constants`/`expo-file-system`/… → `expo-modules-core`) — untestable in plain Node because they ship extensionless relative imports only Metro resolves; break on both PMs at the Node layer.

## Detector validation

Three checks on the detector itself: a fresh-rebuild reproducibility rescan, an exhaustive-versus-reachable scoping diff, and a cross-check against Yarn's compatibility patches.

- **Reproducibility (P1):** the detector rebuilt fresh from `main`; the top-1000 rescan is byte-identical to the committed `scan-top1000.json` (zero drift).
- **Reachability scoping is sufficient (P6):** a 16-package exhaustive-vs-reachable diff (every tarball `.js` vs the exports/main/bin-reachable graph) found zero real breakers among the dropped specifiers — all were JSDoc/comment false-positives or dev/build files. Exhaustive scanning would make the detector *worse* (false-positive flood). No pre-ship change.
- **Yarn cross-check (P3):** `@yarnpkg/extensions` v2.0.6 (125 stale ~2019-22 peer-declaration patches) surfaces zero detector gaps that survive verification (21/25 candidates are version-bounded below current latest = fixed upstream; the 4 unbounded are already-declared or statically-invisible dynamic-resolution, the same class as Nub's documented requirejs caveat). Yarn's list is a different lens (missing-peer declarations) and does not drive force-materialize decisions.

## Residual risk

**The Expo / React-Native / Metro layer is untestable with a plain-Node import harness.**

Whether Nub-GVS breaks Metro's resolution of `expo-modules-core` from a store-resident package is unresolved, and a break there would affect every Expo/RN app under Nub. A Metro/bundler-level harness is the one gap this audit could not close.

## Recommendations

Five actions: one clean removal, one repro to run before ship, the Class-2 remedy that needs product sign-off, one optional addition, and a harness to commission.

1. **Remove `lib0` from `forceMaterializePackages`** — proven false-inclusion (Node never loads its react-native-only phantom). Clean, safe, isolated edit to `NUB_FORCE_MATERIALIZE_PACKAGES` in `crates/nub-cli/src/pm_engine/mod.rs`. Ready to cut as a small PR with the evidence above.
2. **Resolve `@storybook/builder-webpack5`** with a real `storybook build` webpack repro before ship — keep or remove based on whether GVS actually breaks the builder's `@storybook/global` resolution (webpack virtual-module, likely unaffected).
3. **Decide the Class-2 remedy (the higher-value call):** add `firebase` and `nx` to `disableGlobalVirtualStoreForPackages` (currently `next,nuxt,parcel`) as the immediate per-package fix, and/or scope the structural shared-hoist-dir GVS fix that closes the whole Class-2 gap without disabling GVS. This is a default/product decision requiring sign-off.
4. **`@base44/vite-plugin`** — optional marginal force-materialize add; fine to drop.
5. **Commission a Metro-level harness** to close the Expo/RN residual-risk gap.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-03 — Initial write-up. Seven-prong audit (detector rebuild + reproducibility, 18-package breakage verification split two ways, yarn cross-check, holes hunt across the full main-graph hard-phantom set, reachable-vs-exhaustive detector check). Established the two-class model, verified 16/18, flagged `lib0` (remove) and `@storybook/builder-webpack5` (inconclusive), and surfaced the Class-2 transitive-phantom gap (Firebase RTDB / Nx / es-abstract) as an adjacent `disableGVS`/structural finding rather than a force-materialize-list gap.
