# Force-materialization scope — which packages break under symlink-materialization, and the right shape for the fix

**Question.** nub symlink-materializes packages into a shared machine-global store by default (fast). A minority of packages break under symlink-materialization. The fix is to disk-materialize just those consumer packages while everything else stays symlink-materialized. This doc scopes that set — how big, what patterns cluster it, and whether a curated denylist or a structural heuristic is the right shape — from a cross-section of nub's own issues plus pnpm's and bun's hard-won history.

Recommend-only. No code, no denylist landed here.

## Terminology (locked)

- **Materialization** — *symlink-materialized* (absolute symlink into the shared store) vs *disk-materialized* (real project-local dirs). This is the axis this doc scopes. Never "GVS" or "hoisting" for it.
- **The store / GVS-the-concept** — the shared machine-global content+version-keyed CAS. Holds every version side by side; multi-version is not a limitation.
- **Public hoisting** — surfacing to project-root `node_modules/<name>` (`publicHoistPattern`).
- **Hidden-tree hoisting** — the unversioned bare-name `node_modules/.nub/node_modules/<name>` phantom-dep fallback tree (`hoist`/`hoistPattern`). The only thing that can't be shared across projects.

## The two breakage axes, and the orthogonal third distinction

Every catalogued breakage is classified on **two orthogonal axes**. Getting this split right is load-bearing: conflating them inflates the force-materialize set.

**Axis 1 — WHAT resolution walks up and fails:**

- **TYPE-resolution** — a package's shipped `.d.ts` imports an undeclared dependency (typically `@types/*`); tsc's ambient-type walk from the consumer package's realpath escapes the project. The #286 class.
- **RUNTIME/BUILD walk-up** — a package's runtime code makes an undeclared (phantom) `require`/`import`, or a build tool canonicalizes a symlink and walks up / refuses to read outside the project root (`points out of the filesystem root`).

**Axis 2 — WHICH lever fixes it (the decisive one for scoping):**

- **(b) Hidden-tree failure — MATERIALIZATION-INDEPENDENT.** The breakage is "there is no reachable phantom-dep fallback tree." The fix is the **hidden hoist tree** (pnpm's `hoistPattern=['*']` default; bun's `.bun/node_modules/`; nub's PR [#293](https://github.com/nubjs/nub/pull/293) builds it whenever the store is per-project). It is **not** fixed by force-materializing the consumer per se — it is fixed by building a reachable hidden tree. Over half the catalogued "symlink breakages" are actually this class, and **they do not belong in the force-materialize set.**
- **(a) Realpath-escape failure — MATERIALIZATION-DEPENDENT (the true force-materialize class).** The breakage is that the consumer's realpath lives in the **shared machine-global store**, so a realpath-anchored walk (tsc `preserveSymlinks=false`, a bundler canonicalizing symlinks, Node's runtime realpath resolution) escapes the project and can't reach even a project-local hidden tree. The **only** project-side fix is to disk-materialize that consumer so its realpath is project-local. This is the class `disableGlobalVirtualStoreForPackages` targets and per-package force-materialization is designed for.

**The subtlety that ties the axes together.** Under **active GVS** (nub's local-dev default), a phantom-dep consumer needs *both* conditions satisfied: a hidden tree must exist **(b)** *and* the consumer must be able to reach it, which under GVS requires its realpath be project-local **(a)**. So a phantom-dep breakage that looks like pure (b) in a GVS-*off* world (where disk-materialization already puts the realpath project-local) becomes an (a) problem the moment GVS is on. This is exactly why the `@vue/compiler-sfc` finding fails under *both* nub-symlink and nub-disk today — under disk it fails because nub's `hoist=false` default builds no hidden tree at all (the Layer-1 gap PR [#293](https://github.com/nubjs/nub/pull/293) closes); under symlink it fails because the realpath escapes. The practical consequence: **build the hidden tree universally first** (kills class (b) everywhere except GVS-on), then the residual force-materialize set is just class (a).

## Catalog

### Prong A — nub's own issues and internal investigations

| Source | Package(s) | Axis 1 | Axis 2 | Root cause | Resolution |
|---|---|---|---|---|---|
| [#286](https://github.com/nubjs/nub/issues/286) | `next-themes`, `@react-pdf/renderer` (any pkg whose `.d.ts` imports an undeclared `@types/*`) | TYPE | **(a)** realpath-escape | Consumer realpath in shared store → tsc ambient-type walk (`preserveSymlinks=false`) escapes into the store; no project-side placement is ever consulted (tsc-types-matrix cell 6). | PR [#293](https://github.com/nubjs/nub/pull/293) (Layer 1) fixes the whole no-GVS population; GVS-on local-dev residual is the force-materialize target. |
| [#280](https://github.com/nubjs/nub/issues/280) | `@crawlee/basic` → `@apify/datastructures` | RUNTIME | **(b)** hidden-tree (→ (a) under GVS) | Undeclared phantom `require`; nub's isolated linker lacks a reachable phantom-dep fallback tree. | Fixed by an always-on hidden tree; under GVS-on also needs consumer realpath project-local. |
| [#6](https://github.com/nubjs/nub/issues/6) | `@hookform/resolvers` → `zod` (bare import) | RUNTIME | **(b)** hidden-tree | Phantom bare import. Now fixed — but it is *why* the residual (a)/(b) classes exist (the #6 fix keeps the hidden tree project-local, off a GVS consumer's walk-up path). | Fixed. |
| gvs-nuxt-revalidate | `vue-router` → `@vue/compiler-sfc` (phantom) | RUNTIME | **(b)** hidden-tree | Fails identically GVS-on **and** GVS-off — proves this is the hidden-tree gap, not a materialization failure. `hoist=true` (builds the tree) resolves it. | Open; nub isolated-linker phantom-dep gap. The canonical evidence for the (a)/(b) split. |
| turbopack-out-of-project-deps-gvs | `next` (Turbopack only, not webpack-mode) | RUNTIME/BUILD | **(a)** realpath-escape | Turbopack refuses to read/watch outside its inferred project root; canonicalizes symlinks → `points out of the filesystem root`. nub's store path isn't on Vercel's proposed multi-root whitelist (which only covers pnpm's `storeDir`). | On the `disableGlobalVirtualStoreForPackages` list. Recommend-only eval; GVS-off-for-Next already CAS-reflinks, so the loss is near-zero on reflink FS. |
| settings.toml rationale | `parcel` | RUNTIME/BUILD | **(a)** realpath-escape | Resolver walks up for `.parcelrc` / worker serialization crash. | On the list. |
| gvs-nuxt-revalidate | `nuxt` | RUNTIME/BUILD | **(a)** realpath-escape | Historically walked up; **latest Nuxt (4.4.8) is CLEAN under forced GVS** (install + prepare + build + dev all rc=0). The trigger is version-blind and over-broad. | On the list but a **version-gate candidate** — modern Nuxt should not be force-materialized. |
| gvs-nuxt-revalidate | `vite`, `vitepress`, `@sveltejs/kit` | — | — | Tested; confirmed **NOT** to need force-materialization on nub. | Should NOT be on nub's list (aube upstream still lists vite/vitepress — see inconsistency below). |
| gvs-correctness-audit | Metro / React Native / Expo | RUNTIME/BUILD | (a) if real | Haste file-map rejected a watch-root realpath escape. | **Refuted** — does not reproduce on current Expo/RN/Metro (TreeFS); original was a stale pre-TreeFS artifact + a bin-shim trap. Not on the list. |
| gvs-correctness-audit | `lmdb`, `msgpackr-extract` (Gatsby native cache) | neither | **Class 3 (not this axis)** | `link_bins` never populates a per-dep `.bin/` in the virtual store → a dep's install script can't find its dependency's CLI. Reproduces identically GVS-on/off. | Open, unrelated to materialization — a nub-vs-pnpm lifecycle-bin parity gap. |
| gvs-multistage-docker-relocatability | any project | portability | **Class 3 (whole-tree)** | Absolute symlinks into `~/.cache/nub/pm/virtual-store` don't survive `COPY --from` in multi-stage Docker. | Fixed for the `nub ci` path (PR [#261](https://github.com/nubjs/nub/pull/261)) — forces per-project, COPY-safe. |

### Prong B — pnpm's decade of symlinked-node_modules experience

**`publicHoistPattern` default, over time** — the trend line is the finding:

| Era | Default | |
|---|---|---|
| ≤ v6 | `['*types*','*eslint*','@prettier/plugin-*','*prettier-plugin-*']` | |
| v7 (2022) | `['*eslint*','@prettier/plugin-*','*prettier-plugin-*']` — `*types*` **removed** ([pnpm#4459](https://github.com/pnpm/pnpm/issues/4459)) | |
| v11 (current) | **`[]` — empty. No default public hoisting at all.** | |

- **Why `*types*` was removed ([pnpm#4459](https://github.com/pnpm/pnpm/issues/4459) / [#4457](https://github.com/pnpm/pnpm/issues/4457)):** hoisting all `*types*` to root **injects** an undeclared transitive `@types/*` into tsc's ambient walk, silently changing the whole project's effective types (`sort-package-json` → `@types/node@17` suppressed real `es2017` errors). A structural hoist glob doesn't just *fix* reachability — it *injects* unwanted ambient types. Collateral damage is why the glob shrank to nothing.
- **pnpm's TYPE-axis remedy today ([pnpm#11542](https://github.com/pnpm/pnpm/issues/11542)):** "declare the type dep yourself." pnpm auto-hoists an optional-peer type dep **only** if some real dep already pulled it into the tree.
- **`@yarnpkg/extensions` compatibility DB — the curated per-package mechanism.** pnpm imports Yarn's community-curated compat DB directly (`createReadPackageHook`) and applies it on every install. Each entry patches a specific package-version's missing `peerDependencies`/`dependencies`. **162 entries, ~1000 lines — low hundreds, not thousands.** Escape hatch `ignoreCompatibilityDb`. This is the phantom-dep-declaration-gap class (mostly Axis-2 **(b)**), *not* the force-materialize set.
- **`shamefully-hoist`** (`publicHoistPattern=['*']`) — the last-resort flatten-everything valve for RUNTIME/BUILD tools whose own walk-up assumes flatness. Reintroduces phantom + multi-instance risk ([pnpm#7743](https://github.com/pnpm/pnpm/issues/7743) shows it can be *worse* than npm/yarn).
- **`node-linker=hoisted`** — real flat `node_modules`, no store. Recurring for Gradle/React-Native ([pnpm#4263](https://github.com/pnpm/pnpm/issues/4263)), `@playwright/test` ([pnpm#9904](https://github.com/pnpm/pnpm/issues/9904)), serverless packaging.

**pnpm's verdict on curated-vs-structural: both, cleanly split by problem shape.** A structural glob heuristic for whole tool-categories that self-resolve from root — which **shrank 4→0** as the collateral damage outweighed the benefit. A curated, per-package-version list (162, borrowed from Yarn) for precise missing-declaration bugs — which **grew**. Four-year trend: away from any structural default, toward isolation-by-default + precise per-package patches.

### Prong C — bun's isolated install

- **The headline precedent: bun shipped a machine-global symlink store, then reverted it to off-by-default within one release.** [oven-sh/bun#29489](https://github.com/oven-sh/bun/pull/29489) ("7x faster warm installs") → reverted by [#30473](https://github.com/oven-sh/bun/pull/30473) (2026-05-11), explicitly because true cross-project symlinking "changes module-resolution realpaths in a way that can surprise tooling that doesn't follow symlinks, and breaks the phantom-dependency fallback." Root cause [#29614](https://github.com/oven-sh/bun/issues/29614): rspack/webpack's default resolver canonicalizes symlinks, walks up from the real store path, never reaches the project — a **tooling-class** failure (Axis-2 (a)), not per-package.
- bun's default isolated linker is **project-local** (clonefile/hardlink into `node_modules/.bun/`, non-hoisted) with a `.bun/node_modules/` hidden fallback tree — only `globalStore=true` symlinks cross-project.
- `publicHoistPattern`/`hoistPattern` are a near-verbatim copy of pnpm's; default `[]`.
- Global-store eligibility is **categorical, not name-based**: patched deps, trusted (postinstall-script) deps, and non-npm deps are ineligible.
- **No hardcoded per-package linker carve-out.** The closest attempt — an arethetypeswrong-style structural heuristic keeping type-shipping packages project-local ([#29728](https://github.com/oven-sh/bun/pull/29728), walk `exports`, pair `.d.ts`) — was **closed unmerged** (moot once the global store went off-by-default).
- The one hardcoded name list (`postinstall_optimizer.rs`: `esbuild`, `sharp`, `@anthropic-ai/claude-code`) is a postinstall-script optimization, unrelated to the linker.

## Synthesis (Prong D)

### 1. The force-materialize set is class (a) only — and it is SMALL

Split the catalogued breakages by Axis 2:

- **Class (b) — hidden-tree failures (NOT force-materialize):** `@crawlee→@apify` (#280), `@hookform→zod` (#6, fixed), `vue-router→@vue/compiler-sfc`, bun's `@types/node` existence-check, and the entire `@yarnpkg/extensions` 162-package long tail. **Fixed by building the hidden tree** (pnpm/bun both always build one; nub's isolated linker currently has a GAP here — the `@vue/compiler-sfc` finding is the proof — separate from the GVS question). Optionally vendor `@yarnpkg/extensions` for the missing-peer-declaration tail exactly as pnpm/bun do.
- **Class (a) — realpath-escape failures (the force-materialize set):**
  - **(a-i) realpath-canonicalizing build tools:** `next` (Turbopack), `parcel`, and conditionally `rspack`/`webpack`/`metro`. `nuxt` version-gated (modern Nuxt is clean). `vite`/`vitepress`/`@sveltejs/kit` ruled out. **Order of magnitude: single digits.**
  - **(a-ii) #286-class type/phantom consumers under active GVS:** packages whose realpath-in-store can't reach even a project-local hidden tree (`next-themes`, `@react-pdf/renderer`, …). The practically-hit set is **low tens** — the "162" figure is the full phantom-declaration DB (a (b) mechanism), not this cut.

**Total practically-hit force-materialize set: ~5–20 names, not hundreds.** The hundreds live in the hidden-tree/`packageExtensions` mechanism, which is a different lever.

### 2. Cost

Per-package force-materialization disk-materializes a handful of packages (real dirs, but reflinked/cloned from the CAS on APFS/btrfs → near-zero on-disk cost — turbopack-out-of-project-deps-gvs Option C). Versus the current **whole-install nuclear switch** (any trigger → the entire ~571-pkg graph goes disk: 21,002 files, +1.44s, 2.97× warm — hoist-gvs-default-architecture bench). Per-package pays for ~5–20 packages instead of 571 — the decoupling is the whole point. Machinery does not exist yet (grep-confirmed): today only the whole-install switch exists.

### 3. Shape — curated denylist vs structural heuristic

**The pnpm/bun "structural heuristics cause collateral damage" lesson does NOT fully transfer — and that is nub's structural advantage.** pnpm's `publicHoistPattern` glob failed because **hoisting** by glob *injects* unwanted packages and changes correctness (the #4459 ambient-type leak). **Force-materialization is correctness-preserving** — disk-materializing an extra package only makes it project-local real dirs; over-inclusion costs *only* perf, never correctness. So nub can afford a more aggressive/structural rule than pnpm ever could for hoisting.

Recommended **layered** shape (not either/or):

1. **First, close the hidden-tree gap universally** — build the private phantom-dep hoist tree always (pnpm/bun parity; PR [#293](https://github.com/nubjs/nub/pull/293) does this for the GVS-off path). This removes the entire class (b) from the force-materialize conversation. Consider vendoring `@yarnpkg/extensions` for the missing-declaration tail.
2. **Then force-materialize class (a) only, per-package (not whole-install):**
   - **(a-i) build tools → curated denylist** (the `disableGlobalVirtualStoreForPackages` mechanism, refined). It is unavoidably a *behavioral-property* list (there is no manifest signal for "this tool canonicalizes symlinks") — pnpm and bun both punt these to "use the hoisted linker," and a curated denylist is nub doing that automatically. Refine two ways: **(a) per-package** force-materialize instead of whole-install nuclear; **(b) version-gate** (modern Nuxt is clean). Keep it small (2–5 names).
   - **(a-ii) #286-class consumers → curated seed + escape hatch now; structural heuristic is viable later.** Seed with `next-themes`, `@react-pdf/renderer`; expose a config knob to grow it. A structural signal exists and is *safe to over-apply* (undeclared-import detection in shipped `.d.ts`/JS, à la bun #29728 / arethetypeswrong) — feasible for a v2 because force-materialize can't hurt correctness. Defer unless the curated list proves unwieldy.

### 4. Cleanups this research surfaced

- **List inconsistency (live):** nub ships `next,nuxt,parcel` (`pm_engine/mod.rs:2048`); standalone aube defaults to `["next","nuxt","vite","vitepress","parcel"]` (`settings.toml:1082`); the test `gvs_disable_list_embedder_default.rs:33` references an aspirational `["@sveltejs/kit","next","nuxt","parcel"]` **nub does not ship**. Reconcile against this doc's class-(a-i) set: drop `vite`/`vitepress` (ruled out), version-gate `nuxt`, keep `next`/`parcel`.
- **The nub isolated-linker phantom-dep gap (class (b))** is a distinct, higher-priority finding than the force-materialize scoping itself — nub lacks pnpm's always-on hidden tree, so class-(b) breakages hit even under disk-materialization today. Fix that first.
- **Precedent worth recording:** bun shipped a machine-global symlink store and reverted it to off-by-default one release later, over this same class of breakage. A global virtual store that is on by default therefore needs the always-on hidden tree plus per-package force-materialization to hold up.

## Changelog

- 2026-07-03 — Initial write-up. Cross-section of nub issues (#286, #280, #6, next/parcel/nuxt, refuted Metro), pnpm history (publicHoistPattern 4→0, `@yarnpkg/extensions` 162-entry DB, shamefully-hoist, node-linker=hoisted), and bun history (global-store ship-then-revert #29489/#30473, abandoned type-heuristic #29728). Established the orthogonal Axis-2 (materialization-dependent (a) vs hidden-tree (b)) split; scoped the force-materialize set to class (a) only, ~5–20 names; recommended a layered shape (universal hidden tree first, then per-package + version-gated curated denylist for build tools, curated seed + optional structural heuristic for the #286 type class).
- 2026-07-30 — Migrated from the internal research corpus. The implementation entry below was condensed to the mechanism; the deliberation around it is not reproduced.
- 2026-07-03 — **Implemented (per-package force-materialize).** The phantom-under-GVS class is fixed by force-materializing the offending *subpath adapters* on disk (per-package), NOT packageExtensions — one materialize fixes all ~15 of an adapter's backends and unifies with the #286 type class. New `forceMaterializePackages` list<string> aube setting (default `[]`, so standalone aube is byte-identical); the linker's GVS pass materializes a listed package as a real project-local dir (realpath project-local → the upward walk reaches the consumer-installed backend) while everything else stays a shared-store symlink. nub seeds a curated **16-adapter** embedder default from the phantom-dep detector's top-5000 subpath-adapter offenders (`@hookform/resolvers`, `cypress`, `langsmith`, `@storybook/*`, `swiper`, `drizzle-orm`, `@apollo/client`, `preact`, `@angular/common`+`router`, `@vercel/analytics`, `lib0`, `@testing-library/jest-dom`), EXCLUDING the 5 build-time/helper subpath imports the detector bucketed as not-the-runtime-consumer-backend class (`event-target-shim`→@babel/runtime, `goober`/`react-i18next`→babel-plugin-macros, `ox`→tslib, `@nestjs/swagger`→typescript). Supporting changes: install-state hash folds the list (a change forces a relink); `detect_aube_dir_gvs_mode` is now symlink-priority so the legitimately MIXED tree (forced real dirs + shared-store symlinks) is not misread as per-project and wiped. List-1 cleanup: nub already ships the validated-clean `next,nuxt,parcel` whole-install trigger; only the stale aube test `NUB_LIST` was corrected (dropped `@sveltejs/kit`); the engine's own `settings.toml` default is left untouched. Empirically verified under genuine default GVS: `@hookform/resolvers` disk-materialized project-local, `react-hook-form`/`zod` stay machine-global symlinks, ESM+CJS `import '@hookform/resolvers/zod'` resolve (was broken), pnpm matches, re-install idempotent. The recommended packageExtensions/`@yarnpkg/extensions`-DB route and the universal-hidden-tree-first step were NOT taken — the single per-package mechanism was chosen instead.
