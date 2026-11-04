# Framework GVS acceptance matrix

Regression harness that proves real front-end frameworks work under nub's
**global virtual store (GVS)** — the default install layout where a project's
deps are symlinked into the machine-global store (`~/.cache/nub/pm/virtual-store`,
OUTSIDE the project root) rather than copied project-local. Frameworks are the
hard case for GVS because their bundlers/dev-servers resolve modules by
**realpath** and enforce **fs allow-lists**, so a symlinked-out-of-tree dep is
exactly what breaks them if the linker gets it wrong.

Every fixture here is scaffolded by the framework's **official `create-*`
generator** — never hand-rolled — so the dependency tree, the pinned
framework/bundler versions, and the injected-dep shape are what a real user gets.
Each framework is driven through its **full lifecycle**:

```
install → dev-serve + page-load 200 → production build → build-serve + page-load 200
```

The production build and the served-build load are not redundant with dev: they
exercise different code paths (bundler realpath resolution, prerender/SSG, the
SSR/preview server), which is where a GVS break that dev masks tends to surface.

## Layout

| File | Role |
|---|---|
| `frameworks.sh` | The manifest: framework name → official `create-*` command (pinned) + the `DEV`/`BUILD`/`PREVIEW`/`PROBE`/`GVS` keys. ONE place to add a framework. |
| `matrix.sh` | Orchestrator: scaffold each framework → drive `run.sh` under GVS → collect the per-stage pass/fail, the linking layout, and the injected deps → print a summary table. |
| `run.sh` | Per-project runner: the four-stage lifecycle + a linking-layout probe, for one already-scaffolded project. Framework-agnostic; `matrix.sh` calls it once per framework. |

## Running

```sh
# build the dev binary first (see AGENTS.md dev-loop): cargo build -p nub-cli --profile fast
tests/framework-matrix/matrix.sh -b target/fast/nub                    # all frameworks
tests/framework-matrix/matrix.sh -b target/fast/nub vite-react astro   # a subset
tests/framework-matrix/matrix.sh -b target/fast/nub --force-gvs        # force GVS on (non-excluded)
tests/framework-matrix/matrix.sh -b target/fast/nub --isolate-store    # fresh per-framework store (genuine cold)
```

`frameworks.sh list` prints the known names. Each scaffolder is pinned via a
`*_CREATE` env var (`VITE_CREATE`, `ASTRO_CREATE`, `SVELTE_CREATE`, `NUXT_CREATE`,
`NEXT_CREATE`; `NEXT_GVS_VERSION` pins the Next release the `next-gvs` arm installs) — pin a major (e.g. `VITE_CREATE=vite@7`) to freeze the emitted
framework/bundler version. `run.sh` records the exact resolved versions and
layout in each row, so any generator drift is visible, not silent.

## The scaffolders (exact commands)

| Framework | Official generator | Command (as run) |
|---|---|---|
| `vite-react` / `vite-vue` / `vite-svelte` | create-vite | `npm create vite@latest <dir> -- --template {react,vue,svelte}-ts` |
| `astro` | create-astro | `npm create astro@latest <dir> -- --template minimal --install false --git false --skip-houston --yes` |
| `sveltekit` | `sv create` (SvelteKit CLI) | `npx sv@latest create <dir> --template minimal --types ts --no-add-ons` |
| `nuxt` | `nuxi init` | `npx nuxi@latest init <dir> --template minimal --no-install --packageManager npm --gitInit false` |
| `next` | create-next-app | `npx create-next-app@latest <dir> --ts --no-eslint --no-tailwind --no-src-dir --app --no-turbopack --import-alias '@/*' --use-npm --skip-install` |
| `next-gvs` | create-next-app | the same scaffold with `--turbopack`, `next` pinned to `NEXT_GVS_VERSION` (default `16.4.0-canary.37`), plus a `next.config.ts` that sets `experimental.turbopackAdditionalRoots` to nub's store and an `.npmrc` lifting the release-age floor |

Generator CLIs change flags often; treat these as the verified starting point and
adjust per release. Adding a framework is one `case` arm in `frameworks.sh` (its
`create-*` command + the four lifecycle keys) plus its name in `FRAMEWORKS`.

## What "injected deps" means (the GVS finding)

Under GVS most deps are **symlinked** into the machine-global store (their
realpath ESCAPES the project root). A dep whose realpath stays **inside** the
project root was **injected** — disk-materialized project-local while the rest of
the tree stayed symlinked. nub does this deliberately in three cases:

1. **vite dist backport** (`vite_compat`) — a *direct* `vite < 8.1` dep is ejected
   project-local and its dist patched so its `/@fs` fs-allow-list admits the store
   (the #315 `403 … outside of Vite serving allow list` fix).
2. **phantom-eject ancestor closure** (#352 collective hidden tree) — a framework
   that embeds `vite < 8.1` *transitively* (Astro, Nuxt) has its `[framework … vite]`
   ancestor closure disk-materialized so the framework loads a project-local vite
   the backport can patch. Bounded to the framework subtree; the rest stays symlinked.
3. **`diskMaterializePackages`** — an explicit force-eject list.

`matrix.sh`'s `INJECTED DEPS` column is exactly the set of project-local deps
`run.sh`'s probe found under an otherwise-GVS install — the empirical evidence of
which packages nub had to materialize for that framework. `none` means the whole
tree stayed symlinked (e.g. a `vite ≥ 8.1` app needs no eject).

## GVS-excluded frameworks (expected project-local)

Two frameworks are on nub's `disableGlobalVirtualStoreForPackages` realpath-locality
trigger and install **project-local by design** — GVS off, not a bug:

- **`next`** — Turbopack canonicalizes through symlinks and chroots to a single
  project root, so the machine-global store is unreachable.
- **`react-native`** (bare RN) — Metro crawls by realpath and only sees the
  project root; even declared deps report unresolved under GVS.

The matrix runs these with the DEFAULT install (the trigger flips them
project-local), asserts they build/serve, and asserts the layout is
`project-local` — forcing GVS on for them would test an unsupported config, so
`--force-gvs` never applies to a `GVS=exclude` framework. For these the
`INJECTED` column is `n/a` (the whole tree is project-local — GVS isn't engaged).

## Next on the shared store (`next-gvs`, expected `gvs-store`)

Next.js 16.4 adds `experimental.turbopackAdditionalRoots`
([vercel/next.js#98003](https://github.com/vercel/next.js/pull/98003)): a
configured root that Turbopack may follow symlinks into. Pointed at nub's store
(`~/.cache/nub/pm/store`, or `$XDG_CACHE_HOME`/`NUB_CACHE_DIR` equivalents),
it lifts the realpath-locality break that puts `next` on the trigger above. The
`next-gvs` arm is the same create-next-app scaffold with that config written in,
`next` pinned to a release that has the option, and `GVS=require`: the store is
forced on regardless of `--force-gvs`, and the row PASSes only with the
`gvs-store` layout. Under that install `next` itself lands project-local (it is
phantom-ejected), while React and the rest link into the store — which is exactly
what the root must make reachable. Build, dev, start, and a relocated
`output: "standalone"` all pass on 16.4.0-canary.37. Bump `NEXT_GVS_VERSION`
to the first stable that carries the option once 16.4.0 ships, and drop the
`.npmrc` release-age override with it.

## Known issue flagged (not a matrix regression)

- **Nuxt under GVS** — a scule-phantom bug (a transitive undeclared import the
  closure alone can't place) can fail Nuxt's dev/prepare under GVS; a fix is in
  flight. A `nuxt` failure here is that known issue, not a new one — the row
  records the actual observed behavior.

## Acceptance

A framework PASSes when install, dev-serve (HTTP 200 + a clean server log),
production build, and build-serve (HTTP 200) all succeed. A `GVS=exclude`
framework additionally must land `project-local`, and a `GVS=require` framework
must land `gvs-store`. `matrix.sh` exits non-zero on any non-excluded FAIL.
