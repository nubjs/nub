# Hardcoded store-dir markers for project-root detection — ecosystem scan

**Question.** The `simple-git-hooks` package breaks under Nub because it finds the project root by walking up from `cwd` and pattern-matching a hardcoded virtual-store dir name (`.pnpm` / `.deno` / `.store` / `.bun`) — none of which is nub's `.nub`. Which other popular npm packages hardcode a store-dir name, or a fixed `node_modules` depth, to locate the project root or resolve paths? This scopes whether renaming nub's virtual store `.nub` → `.store` is a broadly-effective fix, and which packages would still break.

**Bottom line:** the marker-hardcoding class is small and dominated by the git-hooks cluster. Renaming `.nub` → `.store` fixes the breakage that motivated the question (`simple-git-hooks` + `bun-git-hooks`, ~570k downloads/wk combined) and adopts the vendor-neutral isolated-store convention — `.store` is npm's own isolated-mode store name (npm RFC-0042 / arborist), Yarn Berry's pnpm-linker default, and cnpm/npminstall's layout. It does not fix the packages that hardcode only `.pnpm` (`@percy/cli`, `app-root-path`'s global-install edge, `blitz`), which are fewer and mostly low-download or largely robust anyway, and need `.pnpm` specifically or an upstream fix. Naming the store `.pnpm` would fix the most but masquerades as pnpm — a brand-boundary violation with a tooling-confusion cost — and is not recommended.

## The flaw, precisely (from `simple-git-hooks` 2.13.1)

A dependency's `postinstall` runs with `cwd` set to the package's own location inside the virtual store (pnpm: `node_modules/.pnpm/simple-git-hooks@2.13.1/node_modules/simple-git-hooks`; nub: `node_modules/.nub/simple-git-hooks@…/node_modules/simple-git-hooks`). Rather than read `INIT_CWD`, which every PM sets to the real project root, `simple-git-hooks` splits `process.cwd()` on `[\\/]` and index-matches hardcoded store names:

```js
function getProjectRootDirectoryFromNodeModules(projectPath) {
    const projDir = projectPath.split(/[\\/]/)
    const indexOfPnpmDir = projDir.indexOf('.pnpm')
    if (indexOfPnpmDir > -1) return projDir.slice(0, indexOfPnpmDir - 1).join('/');
    const indexOfDenoDir = projDir.indexOf('.deno')
    if (indexOfDenoDir > -1) return projDir.slice(0, indexOfDenoDir - 1).join('/');
    const indexOfStoreDir = projDir.indexOf('.store')
    if (indexOfStoreDir > -1) return projDir.slice(0, indexOfStoreDir - 1).join('/');
    // …yarn-PnP stub, then a node_modules/simple-git-hooks tail check…
}
```

The `slice(0, indexOf<Marker> - 1)` math assumes the store dir sits directly under `<root>/node_modules/`, which matches nub's layout exactly (nub's store is `node_modules/.nub/`). The only reason it fails under nub is the name: `.nub` is not in the `{.pnpm, .deno, .store}` list. It falls through to the `node_modules/simple-git-hooks` tail check, which returns the store subdir, reads the wrong `package.json`, and never sets the hooks. The flaw is entirely name-driven; the depth math already works for nub. (`simple-git-hooks` HEAD also adds `.bun`, making the recognized set `{.pnpm, .deno, .store, .bun}`.)

## What `.store` is

Not a `simple-git-hooks` invention — it is the vendor-neutral name for an isolated virtual store, converged on independently by three tools:

- **npm's own isolated-install mode** (RFC-0042) — arborist installs to `node_modules/.store/<name>@<version>/node_modules/<name>`, verified in `workspaces/arborist/lib/arborist/isolated-reifier.js` (`join('node_modules', '.store', key, 'node_modules', …)`).
- **Yarn Berry's pnpm node-linker** (`nodeLinker: "pnpm"`) — the `pnpmStoreFolder` config defaults to `./node_modules/.store`, verified in the bundled `yarn-4.10.3.cjs`: *"By default, the store is stored in the `node_modules/.store` of the project."*
- **cnpm/npminstall** — the same `.store/<name>@<version>/node_modules/<name>` layout.

By contrast `.pnpm` (pnpm), `.deno` (Deno), and `.bun` (Bun) are vendor-branded, and the brand boundary rules them out for nub. Of nub's remaining options, `.store` is the generic, already-recognized one.

## Affected packages

Ranked by whether nub's `.nub` → `.store` rename fixes them.

### Recognize `.store` → the rename FIXES them

| Package | Weekly DL | The check | Recognizes | INIT_CWD-fixable upstream? |
|---|---|---|---|---|
| `simple-git-hooks` | 519k | `projDir.indexOf('.pnpm'\|'.deno'\|'.store'\|'.bun')`, slice back to root | `.pnpm .deno .store .bun` | Yes — it deliberately uses `process.cwd()` + marker slicing instead of `INIT_CWD` |
| `bun-git-hooks` | 49k | `const i = projDir.indexOf('.store'); if (i>-1) return projDir.slice(0, i-1)…` | **`.store` ONLY** (the bun port dropped `.pnpm`/`.deno`) | Yes — same design |

Both work under pnpm because they special-case its store, and both break under nub only because `.nub` is not a recognized name. Renaming to `.store` puts nub back in their recognized set.

### Recognize ONLY `.pnpm` → the `.store` rename does NOT fix them

| Package | Weekly DL | The check | Impact under nub | Fix path |
|---|---|---|---|---|
| `app-root-path` | 5.6M | `isInstalledWithPNPM`: `const pnpmDir = sep+'.pnpm'; …globalPath.indexOf(pnpmDir)…resolved.indexOf(pnpmDir)` | **Narrow.** The `.pnpm` check only guards a **global-install** edge; the primary local path (`getFirstPartFromNodeModules` splits on the first `/node_modules`) is **store-name-agnostic** and returns the right root under nub. Only a globally-installed CLI loaded from a nub store home could misfire. | Generic (already mostly works); a store-agnostic global-edge fix upstream |
| `@percy/cli` | 495k | `const i = root.indexOf('.pnpm'); if (i!==-1) siblings.push(join(root.substring(0,i),'@percy'))` | Runtime resolver — used to discover sibling `@percy/*` plugin packages in the store. Under `.nub` it wouldn't find them → Percy plugins/commands silently don't load. `.store` not recognized. | Needs `.nub`/generic handling upstream, or nub uses `.pnpm` |
| `blitz` (`blitz-next` postinstall) | ~5k | `if (blitzPkg.includes('.pnpm')) return join(blitzPkg,'../'.repeat(n))` — pnpm-depth-hardcoded walk-up | postinstall path resolution assuming pnpm's exact nesting depth | Uses `INIT_CWD` for `chdir` but the pnpm-depth branch is independent; upstream generic fix |
| `@talend/icons` | ~1.5k | `main.indexOf('.pnpm')` in a runtime icon-path resolver | low-impact | upstream |

### Not affected (robust — store-name-agnostic or unrelated)

| Package | Weekly DL | Why it's fine |
|---|---|---|
| `husky` | 28.8M | v9 runs from the project root (`prepare` script), uses `cwd`/`.git` + `git config core.hooksPath`. No store assumption. |
| `lint-staged` | 23.6M | Walks up for its config / `.git`. Store-agnostic. |
| `patch-package` | 5.7M | `getAppRootPath` walks up from `cwd` for the first `package.json`. Run as a CLI from the project root. Store-agnostic. |
| `pre-commit` | 359k | Starts from a fixed `__dirname/../..` but then **walks up for `.git`**, so it recovers regardless of store name. |
| `pkg-dir` / `find-up` / `find-up-simple` / `escalade` / `walk-up-path` / `pkg-up` / `find-root` / `root-check` | high (utility libs) | Generic walk-up for a marker file (`package.json`/lockfile/`.git`). **No store-name hardcoding.** A flawed one of these would break many dependents — none are flawed. |
| `preferred-pm` / `which-pm` / `@pnpm/find-workspace-dir` / `package-manager-detector` | med–high | Detect the PM by **lockfile name**, not store layout. Unrelated. |
| `global-dirs` / `is-installed-globally` | med | Detect **global** npm/yarn install prefixes. Unrelated to the local virtual store. |
| `node-gyp` | very high | No store assumption. |

### Store-name-agnostic but still store-DEPTH-fragile

| Package | Weekly DL | Note |
|---|---|---|
| `yorkie` | 341k | **Deprecated** (Vue's old git-hooks tool). Uses a `>1 node_modules` skip-heuristic (`(depDir.match(/node_modules/g)\|\|[]).length > 1`) to avoid double-install; under any nested/isolated store it skips installing hooks. Store-*name*-agnostic — breaks identically under pnpm — so `.store` doesn't help, but neither is it a marker-hardcoder. Low priority. |

## Positive counter-example

The `@ax-llm/ax` postinstall does it correctly: prefer `INIT_CWD`, else `segments.indexOf('node_modules')` — generic, no store-marker literal. Any of the packages above could be fixed upstream this way. `electron-builder` (2.6M/wk) does `split(sep).includes('.pnpm')`, but as a deliberate, documented pnpm-only hoist-vs-isolated probe with a fallback, so it is excluded.

## Synthesis / recommendation (recommend-only)

1. **The class is small.** Only ~4 meaningfully-downloaded published packages hardcode a store *marker* for root/path detection: the git-hooks pair (`simple-git-hooks`, `bun-git-hooks`) plus `@percy/cli` and the largely-robust `app-root-path`. The rest of the find-project-root ecosystem — husky, lint-staged, patch-package, and every `find-up`/`pkg-dir`-family lib — is store-agnostic. The shared utility libs are the highest-leverage failure point and are all clean, so there is no one-flawed-lib-breaks-thousands amplifier.

2. **`.store` fixes the reported pain and is the brand-right name.** Renaming `.nub` → `.store` resolves the git-hooks breakage by adopting the vendor-neutral isolated-store convention (npm RFC-0042 / Yarn pnpm-linker / npminstall) rather than an arbitrary or vendor-branded name, and `.store` squats no PM's brand.

3. **`.store` is not universal.** `@percy/cli`, `blitz`, `@talend/icons`, and `app-root-path`'s global edge hardcode only `.pnpm` and would still misresolve under `.store`. These are lower download and/or narrow impact — Percy plugin discovery; `app-root-path`'s local path is fine. Naming the store `.pnpm` would fix them, but masquerades as pnpm and risks confusing real pnpm and `which-pm`-style detection tooling that keys on `node_modules/.pnpm`. Not recommended.

4. **The universal fix is upstream and nub already enables it.** All of these break because they use `process.cwd()` plus a marker instead of `INIT_CWD` or a generic `node_modules`-segment split. Nub already sets `INIT_CWD` correctly for lifecycle scripts (`vendor/aube/crates/aube-scripts/src/lib.rs`, `…/aube/src/commands/run.rs`), matching pnpm, so any of these packages could fix themselves robustly with no nub change. That is the durable path — upstream issues/PRs recognizing `.nub` or preferring `INIT_CWD` — but it is out of nub's control.

**Net:** `.store` is a low-cost, brand-clean rename that fixes the git-hooks class and rides the neutral convention. A residual `.pnpm`-only tail (Percy, blitz, the `app-root-path` global edge) remains and is best closed upstream.

## Reproduction / evidence

- Flaw source: `simple-git-hooks@2.13.1` `simple-git-hooks.js` `getProjectRootDirectoryFromNodeModules` + `postinstall.js` (`npm pack simple-git-hooks`).
- `bun-git-hooks@0.3.2` `dist/index.js` — `.store`-only slice.
- `app-root-path@3.1.0` `lib/resolve.js` — `isInstalledWithPNPM` + `getFirstPartFromNodeModules`.
- `@percy/cli@1.32.3` `dist/commands.js` `getSiblings` — `root.indexOf('.pnpm')`.
- The `.store` provenance: npm's isolated-mode `isolated-reifier.js`; the `pnpmStoreFolder` default in `yarn-4.10.3.cjs`; Deno's `cli/tools/clean.rs` (`.deno`).
- Nub store name: `crates/nub-cli/src/pm_engine/present.rs` (`virtualStoreDir=node_modules/.nub`); the engine default `.aube` in `vendor/aube/crates/aube-linker/src/lib.rs`.
- GitHub code search (`gh search code "indexOf('.pnpm')"`, `"includes('.pnpm')"`) corroborated the narrow real-world set.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus; the store-dir naming question it scopes is a product decision recorded elsewhere.
- 2026-07-07 — Initial write-up. Scanned the git-hooks/lifecycle tools + find-project-root utility libs + GitHub code search. Established that `.store` is the vendor-neutral isolated-store name (npm RFC-0042 / Yarn-pnpm-linker / npminstall), that the marker-hardcoding class is small and git-hooks-dominated, that `.store` fixes `simple-git-hooks`+`bun-git-hooks` but not the `.pnpm`-only tools (`@percy/cli`, `blitz`, `app-root-path` global edge), and that the durable fix is upstream `INIT_CWD` adoption (which nub already enables).
