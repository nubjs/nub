# registerHooks coverage & sync/async-composition matrix (empirical)

Empirical verification (2026-07-14) of every `module.registerHooks` gap documented in nub's preload
comments, probed across installed Node versions with hook-only-resolvable virtual specifiers. Probe
fixtures were `rh-probe/` (resolve-hook coverage) and `pnp-fix/` (a real yarn 4.12.0 PnP fixture
with `.pnp.loader.mjs`); each is about five files and is reconstructable from the method below.

## Method

- `register.cjs` preloads a sync resolve hook that is the ONLY resolver for `virtual-dep`
  (maps to a real `.cjs`). Any require path that bypasses the hook throws `MODULE_NOT_FOUND`.
- Paths probed: plain-chain `require()`, `require()` from a CJS parent loaded via the ESM
  CJS-translator (`import './x.cjs'`), `createRequire()(…)`, `require.resolve()` (both parents),
  dynamic `import()` (control).
- Composition probe: sync passthrough `registerHooks` + async `module.register` loader whose
  customization is load-bearing (`virtual2`); plus the real Yarn PnP `.pnp.loader.mjs` registered
  on top of sync hooks in a real berry fixture (`--require .pnp.cjs --import <stack>` + ESM entry
  importing `ms`).

## Findings

| Behavior | Broken | Fixed | Fix |
|---|---|---|---|
| `require.resolve()` bypasses sync resolve hooks | 22.15.0 – 24.14.0, **and ALL 22.x incl. 22.23.1 (latest, verified)** | 24.15.0+, 25.x, 26.x | [#62028](https://github.com/nodejs/node/pull/62028) (Joyee Cheung) — **not backported to 22.x** |
| sync hooks + async `module.register` loader: async loader's `import()` customization errors (`ERR_INVALID_RETURN_PROPERTY_VALUE`) | 22.15.0 – 22.22.x, 24.x ≤ 24.11.0 | 22.23.0, 24.11.1+, 25.x, 26.x | [#60380](https://github.com/nodejs/node/pull/60380) chain on 24; 22.23.0 backport cluster ([#59929](https://github.com/nodejs/node/pull/59929), [#61088](https://github.com/nodejs/node/pull/61088), [#61529](https://github.com/nodejs/node/pull/61529)) |
| Yarn `.pnp.loader.mjs` registered ON TOP of sync hooks (the "deadlock" in preload.cjs) | crashes 22.15.0, 24.11.0 (`ERR_INVALID_RETURN_PROPERTY_VALUE`; nub observed a silent-exit variant with its real hooks) | **works 22.23.1, 24.17.0, 26.5.0** — same fix family as above | same as above |
| `require()` / translator-require / `createRequire` through sync resolve hooks | never broken in this generic shape | covered since 22.15.0 | — (nub's 22.15 `.cts`-parent tsconfig/extensionless gap was a narrower nub-specific shape; the generic path was always hooked) |
| `require()` consulting async `module.register` loaders | ALL versions incl. 26.5 | never | **by design** — `module.register` is ESM-loader-only; this is the raison d'être of registerHooks. Not a bug. |

## Consequences

- **The one live upstream ask: backport #62028 to v22.x** (maintenance until 2027-04).
  `require.resolve()` silently skipping registered resolve hooks on latest 22.23.1 is a real,
  reproducible hole on a supported line, fixed everywhere else, authored by Joyee — a clean ask.
- **Everything else in nub's documented registerHooks gap list is fixed on all current release
  lines.** The sync×async composition family (#59666) — including the Yarn-loader stacking crash —
  is green on 22.23.0+/24.11.1+/25/26. Nothing to report upstream; useful only as evidence that a
  composition test matrix should gate stabilization.
- **nub follow-ups (optional):**
  - `node_hook_compose_broken` in `crates/nub-core/src/node/spawn.rs` gates the force-async-tier
    workaround at `22.15.0 ..= 24.11.0`, which over-covers fixed 22.23.0+ (comment already
    anticipates this as harmless). Narrowable now that the 22.x boundary is known: exclude
    `>= 22.23.0`.
  - `runtime/preload.cjs` composition comment ("fixed in Node 24.11.1") can note the 22.23.0
    backport; the PnP-deadlock comment can note the stacking crash is fixed on current lines
    (nub's own-hook PnP routing stays — it's better regardless).
  - The `_resolveFilename` shim (`installCjsRequireHooks`) remains load-bearing regardless: it
    covers the compat tier (18.19–22.14, no registerHooks at all) and the 22.x
    `require.resolve` hole.
- Joyee has personally been grinding this exact bug class (five fixes in the recent changelogs are
  hers: #62028, #61529, #61088, #59929, #59011) — feedback to her should lead with verified-green
  production evidence + the 22.x backport ask, not a stale gap list.

## Changelog

- 2026-07-14 — Initial write-up.
