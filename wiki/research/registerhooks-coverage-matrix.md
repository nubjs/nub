# registerHooks coverage & sync/async-composition matrix (empirical)

Empirical verification (2026-07-14) of every `module.registerHooks` gap documented in nub's preload comments, probed across installed Node versions with hook-only-resolvable virtual specifiers.

Probe fixtures were `rh-probe/` (resolve-hook coverage) and `pnp-fix/` (a real yarn 4.12.0 PnP fixture with `.pnp.loader.mjs`); each is about five files and is reconstructable from the method below.

## Method

Three probes: resolve-hook coverage across every require path, sync hooks composed with an async `module.register` loader, and a real Yarn PnP loader stacked on top of sync hooks.

- `register.cjs` preloads a sync resolve hook that is the ONLY resolver for `virtual-dep`, mapping it to a real `.cjs`. Any require path bypassing the hook throws `MODULE_NOT_FOUND`.
- Paths probed: plain-chain `require()`, `require()` from a CJS parent loaded via the ESM CJS-translator (`import './x.cjs'`), `createRequire()(…)`, `require.resolve()` (both parents), and dynamic `import()` as a control.
- Composition probe: sync passthrough `registerHooks` plus an async `module.register` loader whose customization is load-bearing (`virtual2`); plus the real Yarn PnP `.pnp.loader.mjs` registered on top of sync hooks in a real berry fixture (`--require .pnp.cjs --import <stack>` with an ESM entry importing `ms`).

## Availability by release line

Before any coverage question, the API has to exist. It does not exist on one band that a single version comparison reads as modern.

`module.registerHooks` is a semver-minor ([#55698](https://github.com/nodejs/node/pull/55698), Joyee Cheung) that shipped on two release lines independently: 23.5.0 on the 23.x Current line (2024-12-19) and 22.15.0 on the 22.x LTS line (2025-04-23, four months later). Node 23.0.0 through 23.4.x therefore sorts *above* 22.15.0 while having no synchronous hooks API at all.

Probed on the installed toolchains, `typeof require('module').registerHooks`:

| Node | 22.14.0 | 22.15.0 | 23.0.0 | 23.3.0 | 23.4.0 | 23.5.0 | 23.6.0 | 24.0.0 |
|---|---|---|---|---|---|---|---|---|
| `registerHooks` | undefined | function | undefined | undefined | undefined | function | function | function |

Any tier gate written as a plain `>= 22.15.0` claims the API on 23.0–23.4. Nub's did, which sent those releases down the `--require preload.cjs` fast path and crashed every run at startup with `TypeError: module_.registerHooks is not a function`. The predicate has to exclude the band explicitly, or test the capability rather than the version.

## Findings

Five behaviors, each with the version range where it is broken, the range where it is fixed, and the upstream PR that fixed it.

| Behavior | Broken | Fixed | Fix |
|---|---|---|---|
| `require.resolve()` bypasses sync resolve hooks | 22.15.0 – 24.14.0, **and ALL 22.x incl. 22.23.1 (latest, verified)** | 24.15.0+, 25.x, 26.x | [#62028](https://github.com/nodejs/node/pull/62028) (Joyee Cheung) — **not backported to 22.x** |
| sync hooks + async `module.register` loader: async loader's `import()` customization errors (`ERR_INVALID_RETURN_PROPERTY_VALUE`) | 22.15.0 – 22.22.x, 24.x ≤ 24.11.0 | 22.23.0, 24.11.1+, 25.x, 26.x | [#60380](https://github.com/nodejs/node/pull/60380) chain on 24; 22.23.0 backport cluster ([#59929](https://github.com/nodejs/node/pull/59929), [#61088](https://github.com/nodejs/node/pull/61088), [#61529](https://github.com/nodejs/node/pull/61529)) |
| Yarn `.pnp.loader.mjs` registered ON TOP of sync hooks (the "deadlock" in preload.cjs) | crashes 22.15.0, 24.11.0 (`ERR_INVALID_RETURN_PROPERTY_VALUE`; nub observed a silent-exit variant with its real hooks) | **works 22.23.1, 24.17.0, 26.5.0** — same fix family as above | same as above |
| `require()` / translator-require / `createRequire` through sync resolve hooks | never broken in this generic shape | covered since 22.15.0 | — (nub's 22.15 `.cts`-parent tsconfig/extensionless gap was a narrower nub-specific shape; the generic path was always hooked) |
| `require()` consulting async `module.register` loaders | ALL versions incl. 26.5 | never | **by design** — `module.register` is ESM-loader-only; this is the raison d'être of registerHooks. Not a bug. |

## Consequences

One upstream ask survives the matrix, a v22.x backport; everything else in nub's documented gap list is already green on current release lines.

- **The one live upstream ask: backport #62028 to v22.x** (in maintenance until 2027-04). `require.resolve()` silently skipping registered resolve hooks on the latest 22.23.1 is a reproducible hole on a supported line, fixed everywhere else.
- **Everything else in nub's documented registerHooks gap list is fixed on all current release lines.** The sync×async composition family (#59666), Yarn-loader stacking crash included, is green on 22.23.0+/24.11.1+/25/26. Nothing to report upstream; useful only as evidence that a composition test matrix should gate stabilization.
- **Nub follow-ups (optional):**
  - `node_hook_compose_broken` in [`crates/nub-core/src/node/spawn.rs`](../../crates/nub-core/src/node/spawn.rs) gates the force-async-tier workaround at `22.15.0 ..= 24.11.0`, over-covering the fixed 22.23.0+ range — harmless, as its comment already anticipates. Narrowable now that the 22.x boundary is known: exclude `>= 22.23.0`.
  - The composition comment in [`runtime/preload.cjs`](../../runtime/preload.cjs) ("fixed in Node 24.11.1") can note the 22.23.0 backport, and the PnP-deadlock comment can note that the stacking crash is fixed on current lines. Nub's own-hook PnP routing stays either way, being better regardless.
  - The `_resolveFilename` shim (`installCjsRequireHooks`) remains load-bearing: it covers the compat tier (18.19–22.14, no registerHooks at all) and the 22.x `require.resolve` hole.
- Joyee Cheung authored five of the recent fixes in this bug class (#62028, #61529, #61088, #59929, #59011), so feedback should lead with verified-green production evidence plus the 22.x backport ask rather than a stale gap list.

## Changelog

Each entry dates the probe run behind a change to the matrix.

- 2026-07-14 — Initial write-up.
- 2026-08-27 — Added the availability-by-release-line section. The matrix had only ever asked what `registerHooks` does where it exists; it never recorded that 23.0–23.4 does not have it, which is what let a `>= 22.15.0` tier gate crash the preload on that band.
