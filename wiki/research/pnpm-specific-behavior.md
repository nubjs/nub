# PNPM-Specific Behavior

Reference inventory of behaviors, files, and `package.json` extensions that are **PNPM-specific** (not standard npm/Node). For each item: what it is, link to docs, whether a standard equivalent exists, and a Nub stance suggestion (**support** / **support-as-alias** / **ignore-with-warning** / **drop**). Suggestions are starting points, not final calls.

> Stance legend - **support**: standard or near-universal; Nub supports it. - **support-as-alias**: PNPM-only but load-bearing; Nub reads it (possibly under a Nub-native name too). - **ignore-with-warning**: parse but no-op; warn so users aren't surprised. - **drop**: refuse / error; the behavior is hostile to a clean design.

Sources are inline. Pnpm has moved fast — v10 (Jan 2025) and v11 (late 2025/2026) made big changes; flagged below.

---

## 1. Top-level files PNPM creates or reads

### 1.1 `pnpm-workspace.yaml`

The de facto PNPM config file as of v10/v11. Originally just listed workspace packages; now holds nearly all PNPM config because **v11 stopped reading the `pnpm` field in `package.json` and stopped reading PNPM keys from `.npmrc`**. Anything that used to live in `.npmrc` or `package.json#pnpm` now lives here.

Docs: https://pnpm.io/pnpm-workspace_yaml, https://pnpm.io/settings, https://pnpm.io/blog/releases/11.0

Known top-level keys (non-exhaustive — the settings page lists ~100):
- `packages` — workspace glob list (the original purpose)
- `catalog`, `catalogs` — see §3.2
- `overrides` — dependency graph overrides (npm has `overrides` in package.json; PNPM moved its version here)
- `packageExtensions` — patch missing deps/peers into other packages' manifests (originally a Yarn feature)
- `peerDependencyRules` — `ignoreMissing`, `allowedVersions`, `allowAny`
- `patchedDependencies` — map of `name@version` → patch file path
- `configDependencies` — see §1.6
- `allowBuilds` — v11 replacement for `onlyBuiltDependencies` / `neverBuiltDependencies` (see §4.4)
- `hoistPattern`, `publicHoistPattern`, `shamefullyHoist`, `hoist`, `hoistWorkspacePackages`
- `nodeLinker` — `isolated` (default) / `hoisted` / `pnp`
- `linkWorkspacePackages`, `preferWorkspacePackages`, `saveWorkspaceProtocol`, `sharedWorkspaceLockfile`
- `virtualStoreDir`, `modulesDir`, `storeDir`, `packageImportMethod`
- `autoInstallPeers`, `strictPeerDependencies`, `dedupePeerDependents`, `resolvePeersFromWorkspaceRoot`
- `resolutionMode` — `highest` / `time-based` / `lowest-direct`
- `minimumReleaseAge`, `minimumReleaseAgeExclude` — supply-chain guard (default 1440min = 1d in v11)
- `blockExoticSubdeps` — block transitive deps from non-trusted registries (default `true` in v11)
- `trustPolicy`, `trustPolicyExclude`, `trustPolicyIgnoreAfter`
- `auditConfig` (`ignoreGhsas` in v11, was `ignoreCves`)
- `updateConfig.ignoreDependencies`
- `supportedArchitectures` — `{ os, cpu, libc }` — controls which optional deps install
- `ignoredOptionalDependencies`
- `requiredScripts` — assert all workspace packages define these scripts
- `packageConfigs` — per-workspace-package settings (v11+)
- `namedRegistries`, `registries`
- `catalogMode` (`manual` / `strict` / `prefer`), `cleanupUnusedCatalogs`

Standard equivalent: **none for the file itself**. The preferred form is `package.json#workspaces`, which is what npm/yarn/Bun use. PNPM never reads it — confirmed at https://pnpm.io/workspaces ("A workspace must have a `pnpm-workspace.yaml` file in its root").

**Nub stance suggestion**: **support-as-alias** for reading (a typical PNPM monorepo will be unusable without it). Encourage Nub-native config in `package.json#workspaces` + a Nub config file (or `package.json#nub.*`). On `nub install`, if both `pnpm-workspace.yaml` `packages` and `package.json` `workspaces` are present, prefer the latter and warn. The non-workspace keys (catalogs, overrides, hoist, etc.) need per-key decisions — see below.

### 1.2 `pnpm-lock.yaml`

YAML lockfile. Schema is PNPM-versioned (v6, v7, v9, v10 — bumped on breaking changes). Not compatible with npm's `package-lock.json` or yarn's `yarn.lock`.

Docs: https://pnpm.io/symlinked-node-modules-structure (mentioned throughout)

Standard equivalent: none; every PM has its own lockfile.

**Nub stance suggestion**: **support-as-alias** for reading. Nub should be able to install deterministically from a `pnpm-lock.yaml` (this is table-stakes for "essentially compatible"). Whether Nub *writes* one or writes its own lockfile is a separate decision — writing PNPM's format perfectly is a maintenance burden (it changes), writing a Nub-native one breaks PNPM-using teammates. Pragmatic default: read PNPM lockfile; write Nub lockfile; offer a `--write-pnpm-lock` mode for migration.

### 1.3 `.pnpmfile.cjs` / `.pnpmfile.mjs` / `pnpmfile.cjs`

JS file with hooks PNPM calls during resolution and install. Hooks: `readPackage`, `afterAllResolved`, `preResolution`, `updateConfig` (v10.8+), `beforePacking` (v10.28+), `importPackage`. v11 added `.mjs` support and moved `hooks.fetchers` to top-level `fetchers`/`resolvers` exports.

Docs: https://pnpm.io/pnpmfile

Standard equivalent: none. Yarn has plugins, npm has nothing. This is a powerful escape hatch — and a footgun — because it lets userland code mutate every dependency's manifest.

**Nub stance suggestion**: **ignore-with-warning** initially. The hook API is unstable across PNPM versions, and a meaningful fraction of `.pnpmfile.cjs` files exist to patch peerDeps that Nub could handle differently (or that `packageExtensions` covers declaratively). Worth revisiting if there's user demand. If we ever support it, prefer the declarative `packageExtensions` / `peerDependencyRules` for the same effect.

### 1.4 `.npmrc` (PNPM-specific keys)

Until v11, PNPM read its config from `.npmrc` using dozens of custom keys. **v11 ripped this out**: `.npmrc` is now auth + registry only; everything else moved to `pnpm-workspace.yaml`. `npm_config_*` env vars are also no longer read in v11 — use `pnpm_config_*`.

Docs: https://pnpm.io/npmrc, https://pnpm.io/blog/releases/11.0

Notable PNPM-only keys (legacy; in v10 and earlier):
- `hoist-pattern`, `public-hoist-pattern`, `shamefully-hoist`
- `node-linker` (`isolated` / `hoisted` / `pnp`)
- `dedupe-peer-dependents`, `strict-peer-dependencies`, `auto-install-peers`
- `resolution-mode`, `prefer-workspace-packages`, `link-workspace-packages`, `save-workspace-protocol`
- `modules-dir`, `virtual-store-dir`, `store-dir`, `package-import-method`
- `side-effects-cache`, `verify-store-integrity`
- `prefer-frozen-lockfile`, `use-lockfile-v6`, `lockfile`
- `manage-package-manager-versions`, `only-built-dependencies`, `ignore-dep-scripts`

Standard equivalent: `.npmrc` itself is shared (npm/PNPM/yarn-classic). The **keys** above are PNPM-only.

**Nub stance suggestion**: **support-as-alias** for `.npmrc` *registry/auth keys* (cross-PM standard, e.g. `_authToken`, `registry`, scoped `@scope:registry`). **ignore-with-warning** for PNPM-only behavioral keys — they describe a node_modules layout Nub likely won't replicate verbatim. Map a small number that have natural Nub analogs (e.g. `node-linker=hoisted` → Nub's flat-install mode if we ever add one).

### 1.5 `node_modules/.pnpm/` and `node_modules/.modules.yaml`

The **virtual store**: every package is unpacked once into `node_modules/.pnpm/<name>@<version>/node_modules/<name>/`, then symlinked into consumers' `node_modules/`. Files inside are hard-linked from the global content-addressable store (default `~/.local/share/pnpm/store` on Linux).

`.modules.yaml` is a metadata sidecar: `storeDir`, `virtualStoreDir`, `layoutVersion`, `packageManager`, `pendingBuilds`, `hoistedAliases`, `skipped`, `shamefullyFlatten`, etc. PNPM uses it to detect a stale node_modules and rebuild.

Docs: https://pnpm.io/symlinked-node-modules-structure, https://www.npmjs.com/package/@pnpm/modules-yaml

Standard equivalent: none. npm/yarn use flat node_modules; yarn-berry uses PnP.

**Nub stance suggestion**: This is a layout choice, not a compatibility surface. Nub can pick **isolated symlink layout** (PNPM-style) for the same correctness benefits, or **hoisted** (npm-style) — it doesn't have to be the *same* virtual store. Concretely: **don't** read or write PNPM's `.modules.yaml`; write a Nub equivalent if needed. The `node_modules/.pnpm/` path should not be hard-coded by anyone but PNPM, so Nub is free to use `node_modules/wiki/` (or skip the layer entirely).

### 1.6 `configDependencies` (v10+)

Special dep type declared in `pnpm-workspace.yaml`. Installed *before* regular deps; cannot have transitive deps or lifecycle scripts; integrity-pinned in the lockfile. Used to share PNPM hooks/catalogs/overrides across repos via a published npm package. Packages matching `pnpm-plugin-*` / `@*/pnpm-plugin-*` / `@pnpm/plugin-*` auto-load their `pnpmfile.mjs`.

Docs: https://pnpm.io/config-dependencies

Standard equivalent: none. Effectively PNPM's plugin system.

**Nub stance suggestion**: **ignore-with-warning**. Niche, recent, PNPM-internal-machinery. If we later want a plugin system, design Nub-native instead.

---

## 2. `package.json` fields PNPM-specific or PNPM-extended

Big caveat: **v11 stopped reading the `pnpm` field from `package.json`.** All `pnpm.*` config moved to `pnpm-workspace.yaml`. The `package.json` fields still in active use are limited to publish-time things (`publishConfig`, `executableFiles`) and `dependenciesMeta` / `peerDependenciesMeta`. But every PNPM ≤ v10 project will still have `pnpm.*` in `package.json`, so Nub cannot ignore them in practice for some years.

Docs: https://pnpm.io/package_json, https://pnpm.io/blog/releases/11.0

### 2.1 `pnpm.overrides` (≤v10; moved to `pnpm-workspace.yaml` in v11)

Force a specific version of a transitive dep across the graph.

Standard equivalent: **npm has `overrides`** (different schema but same intent). Yarn has `resolutions`. This concept is universal.

**Nub stance suggestion**: **support**. Read npm-style `package.json#overrides` as the canonical form; also accept `pnpm.overrides` and `pnpm-workspace.yaml#overrides` for compat. Normalize internally.

### 2.2 `pnpm.packageExtensions` (≤v10; moved to workspace yaml in v11)

Inject missing `dependencies` / `peerDependencies` into other packages' manifests at resolve time. Originally a Yarn-berry feature; PNPM adopted the schema.

Standard equivalent: not in npm. Yarn berry supports it.

**Nub stance suggestion**: **support-as-alias**. This solves real, common manifest bugs in the ecosystem without modifying upstream. Cheap to implement; high value. Treat the schema as cross-PM.

### 2.3 `pnpm.peerDependencyRules`

- `ignoreMissing`: list of peer dep names whose absence won't warn
- `allowedVersions`: map of `pkg` → version range that overrides peer requirements
- `allowAny`: list of pkgs where any version is OK

Standard equivalent: none.

**Nub stance suggestion**: **support-as-alias**. Most modern PNPM monorepos use this to silence noisy peer-dep warnings; if Nub does its own peer resolution (likely, with `autoInstallPeers` semantics), it needs these as escape hatches.

### 2.4 `pnpm.neverBuiltDependencies` / `pnpm.onlyBuiltDependencies` / `pnpm.ignoredBuiltDependencies` / `ignoreDepScripts`

Allowlist/denylist for which deps may run lifecycle scripts (`preinstall`/`install`/`postinstall`). **PNPM v10 changed the default to "block all dep lifecycle scripts unless allowlisted"** (huge supply-chain win, breaking change). **PNPM v11 removed all four fields in favor of a single `allowBuilds` map** in `pnpm-workspace.yaml`.

Docs: https://pnpm.io/cli/approve-builds, https://socket.dev/blog/pnpm-10-0-0-blocks-lifecycle-scripts-by-default, https://pnpm.io/blog/releases/11.0

Standard equivalent: none, but **npm 10+ has `package.json#trustedDependencies` proposals** and yarn-berry has the concept. The *direction* is industry-wide.

**Nub stance suggestion**: **support** — Nub should adopt the v10 default (block by default) regardless of PNPM compat. Read `pnpm.onlyBuiltDependencies` and `pnpm-workspace.yaml#allowBuilds` as input to Nub's own allowlist. Could also alias under `nub.allowBuilds` or just standardize on the `allowBuilds` map.

### 2.5 `pnpm.allowedDeprecatedVersions`

Suppress deprecation warnings for listed packages.

Standard equivalent: none.

**Nub stance suggestion**: **ignore-with-warning**. Pure log-noise control. Cheap to support but low value; mostly people use it to hide a single annoying dep. A Nub verbosity flag is cleaner.

### 2.6 `pnpm.patchedDependencies`

Map `name@version` → relative path to a `.patch` file. Applied at install time after extraction from store. v11 changed failure semantics: all patch failures throw, errors aggregated.

Docs: https://pnpm.io/cli/patch

Standard equivalent: `patch-package` (userland), yarn-berry has built-in `yarn patch`.

**Nub stance suggestion**: **support-as-alias**. The `.patch` format is standard unified-diff; the field schema is reasonable. Adopt the schema; document `nub patch <pkg>` to produce one. Apply both `package.json#pnpm.patchedDependencies` and `pnpm-workspace.yaml#patchedDependencies`.

### 2.7 `pnpm.updateConfig`, `pnpm.auditConfig`, `pnpm.requiredScripts`, `pnpm.supportedArchitectures`

- `updateConfig.ignoreDependencies`: exclude from `pnpm outdated`/`pnpm update`
- `auditConfig.ignoreGhsas` (v11, was `ignoreCves`): silence specific advisories
- `requiredScripts`: list of script names that all workspace packages must define
- `supportedArchitectures`: `{os, cpu, libc}` — install optional deps for non-host platforms (useful for Docker builds)

Standard equivalent: none. `supportedArchitectures` is genuinely useful and Bun/Yarn don't have a clean equivalent.

**Nub stance suggestion**:
- `updateConfig`, `auditConfig`: **ignore-with-warning** unless Nub ships `update` / `audit` subcommands with similar semantics.
- `requiredScripts`: **support-as-alias** — small, useful, cheap.
- `supportedArchitectures`: **support** — genuinely needed for cross-platform builds; adopt the schema.

### 2.8 `pnpm.executionEnv.nodeVersion` (≤v10; **removed in v11**)

Pin a Node version per package; PNPM auto-downloaded it and ran scripts under it.

Docs: https://github.com/orgs/pnpm/discussions/10172

**Replaced in v11 by `engines.runtime` and `devEngines.runtime`** (which is a more general format covering Node/Deno/Bun). Stored in lockfile.

Standard equivalent: `engines.node` (advisory only in npm). The `devEngines` concept is partly standardized in npm.

**Nub stance suggestion**: **support** `engines.runtime` / `devEngines.runtime` if we want runtime auto-provisioning at all (Nub is its own runtime, so the answer is probably "no, Nub runs scripts under Nub"). **ignore-with-warning** for legacy `pnpm.executionEnv.nodeVersion`.

### 2.9 `dependenciesMeta`

- `dependenciesMeta.<dep>.injected`: hard-link a *copy* into the virtual store rather than symlinking. Forces fresh peer resolution per consumer. Used in monorepos where workspace packages need different peer instances.

Docs: https://pnpm.io/package_json#dependenciesmeta

Standard equivalent: yarn-berry has `installConfig.hoistingLimits` and similar; npm has nothing.

**Nub stance suggestion**: **support-as-alias** if Nub's node_modules layout has the same symlink-vs-copy distinction. If Nub goes flat/hoisted, this is meaningless and can be **ignore-with-warning**.

### 2.10 `peerDependenciesMeta`

- `peerDependenciesMeta.<dep>.optional`: peer dep won't error if missing.

This was originally PNPM-specific but is now in npm too (since npm 7).

Standard equivalent: **npm supports it**. This is effectively standard.

**Nub stance suggestion**: **support**.

### 2.11 `publishConfig` quirks

The base `publishConfig` field is npm-standard: it lets you override top-level fields *only for the published tarball*. Standard overrides: `registry`, `tag`, `access`, `bin`, `main`, `exports`, `types`, `module`, `browser`. PNPM extends with:

- **`publishConfig.directory`** — publish from a *subdirectory* (e.g., `dist/`). PNPM expects that subdir to contain a complete, ready-to-publish `package.json`. Used heavily with build steps that emit transformed manifests. Standard npm: **does not exist**. https://github.com/pnpm/pnpm/issues/6253
- **`publishConfig.linkDirectory`** — boolean (default **true** per PNPM docs). When true and `directory` is set, PNPM symlinks the *subdir* into `node_modules` for **local workspace consumers** during dev — so dependents resolve the built output instead of source. This is the "surprising" one. https://github.com/orgs/pnpm/discussions/5692
- **`publishConfig.executableFiles`** — array of relative paths to chmod +x on publish, beyond what `bin` already covers. Niche but real.

And the **publish-time rewrites** PNPM performs (see §5):
- `workspace:*` / `workspace:^` / `workspace:~` → resolved concrete versions
- `catalog:` / `catalog:foo` → resolved concrete versions
- `link:` and `file:` → behavior varies; generally rejected on publish
- `devDependencies` are stripped from the published tarball (this part is standard npm behavior)

Standard equivalent: `publishConfig` itself is standard; the PNPM additions (`directory`, `linkDirectory`, `executableFiles`) are not. **Bun, yarn, and npm don't recognize them.**

**Nub stance suggestion**:
- `publishConfig.directory`: **support-as-alias**. This is a legitimately useful pattern (publish `dist/` not source), but it's surprising because no other tool recognizes it. Nub should support it on `nub publish` and emit a clear log line ("publishing from `<dir>` per publishConfig.directory").
- `publishConfig.linkDirectory`: **ignore-with-warning** by default. It changes how *dependents resolve workspace packages during dev* in a way that's invisible from the depending package's perspective — that's the foot-gun. A Nub project should be able to express "dev resolves dist" via a clearer mechanism (e.g., `exports` conditions, or `nub.workspace.resolve` field). Warn if encountered; offer to migrate.
- `publishConfig.executableFiles`: **support**. Tiny, useful, low-controversy.

### 2.12 `workspaces` (package.json)

PNPM **does not read this field**. It is npm/yarn/Bun standard.

Docs: https://pnpm.io/workspaces

**Nub stance suggestion**: **support** as primary input. When migrating from PNPM, Nub can read `pnpm-workspace.yaml#packages` as a fallback (and offer to migrate it into `package.json#workspaces`).

### 2.13 `bundleDependencies` / `bundledDependencies`

npm-standard, not PNPM-specific. PNPM's support is partial/quirky historically. Mentioned here only to confirm: **not a PNPM-ism**.

**Nub stance suggestion**: **support** (standard).

---

## 3. Dependency protocols / specifiers PNPM supports beyond npm

### 3.1 `workspace:` protocol

PNPM's flagship workspace feature. Forms:

| Spec | Dev behavior | Published as |
|---|---|---|
| `workspace:*` | symlink to local workspace pkg | concrete `X.Y.Z` (the workspace pkg's current version) |
| `workspace:^` | symlink | `^X.Y.Z` |
| `workspace:~` | symlink | `~X.Y.Z` |
| `workspace:1.2.3` | symlink (must match) | `1.2.3` |
| `workspace:^1.2.3` | symlink | `^1.2.3` |
| `workspace:~1.2.3` | symlink | `~1.2.3` |
| `workspace:../foo` | path-based local link | concrete version |
| `"bar": "workspace:foo@*"` | aliased local link | concrete version of `foo` |

Docs: https://pnpm.io/workspaces

Standard equivalent: **yarn-berry and Bun both support `workspace:`** with substantially the same semantics. Effectively a de-facto standard now.

**Nub stance suggestion**: **support** (treat as standard). This is non-negotiable for "essentially compatible."

### 3.2 `catalog:` protocol

Centralize dep versions across a monorepo. Declared in `pnpm-workspace.yaml`:

```yaml
catalog:
  react: ^18.2.0
catalogs:
  react17:
    react: ^17.0.2
```

Used as `"react": "catalog:"` (default) or `"react": "catalog:react17"` (named). **Replaced with concrete versions on publish.** Bun added `catalog:` support recently; Yarn does not. `catalogMode` setting (`manual`/`strict`/`prefer`) controls auto-rewriting of literal versions into catalog refs at install time. `cleanupUnusedCatalogs` prunes unused entries.

Docs: https://pnpm.io/catalogs

Standard equivalent: **Bun has `catalog:`** as of 2025; not in npm or yarn. Trending toward standard.

**Nub stance suggestion**: **support**. Adopt the schema; could also expose a Nub-native location (e.g., `package.json#catalog`) since the data is just a flat object. A Nub project need not use `pnpm-workspace.yaml` but should be able to read it.

### 3.3 `link:` protocol

`"foo": "link:../foo"` — non-installed symlink. Yarn-classic, yarn-berry, and PNPM all support it. Differs from `file:` in that it doesn't copy.

Standard equivalent: shared across PNPM/yarn. Not in npm (npm's `link:` is different / not portable in package.json).

**Nub stance suggestion**: **support**. Useful for one-off local dev; trivial.

### 3.4 `file:` protocol

`"foo": "file:../foo"` — every PM supports this but **with subtly different semantics**:
- npm: copies on install, then symlinks
- PNPM: symlinks by default (treats it like `link:` mostly); has `prefer-symlinked-executables`
- yarn-berry: archives into a `.zip` for PnP

Standard equivalent: nominally standard; semantics drift.

**Nub stance suggestion**: **support**. Pick semantics closer to npm (copy) for predictability; document the choice.

### 3.5 `jsr:` protocol

Recent (PNPM ≥9). `"foo": "jsr:@scope/foo@^1"` — resolves from jsr.io. Deno-native. Bun supports it. PNPM supports it.

Standard equivalent: Bun + PNPM + Deno; not npm or yarn.

**Nub stance suggestion**: **support**. JSR isn't going away; cost is small (just an alternate resolver).

### 3.6 Patches via `patchedDependencies`

Not a specifier per se — see §2.6.

---

## 4. CLI / lifecycle behaviors diverging from npm

### 4.1 Symlinked `node_modules`

Implications:
- Tools that walk `node_modules` recursively will see different shapes.
- Node's default behavior is to **resolve symlinks** during module resolution (i.e., `require.resolve` returns the real path under `.pnpm/<name>@<v>/...`). `--preserve-symlinks` reverses this and breaks PNPM. Some bundlers/build tools have had bugs around this for years.
- `__dirname` / `import.meta.url` give the real path, not the symlinked path. Surprising for code that expects to find sibling packages by walking up.

**Nub stance suggestion**: Nub runtime decides its module resolution semantics independently of the PM. If Nub the PM produces a PNPM-like layout, Nub the runtime must handle symlinks correctly (it almost certainly will, as that's Node's default). Cross-reference with `wiki/runtime/` once decided.

### 4.2 `pnpm dlx` vs `npx`

`pnpm dlx <pkg>` = "fetch package to temp dir, run it, don't install." Caches by package+version in `~/.local/state/pnpm/dlx/` (config: `dlxCacheMaxAge`). Aliases: `pnx`, `pnpx`. **v11 has dlx honor `minimumReleaseAge` and trust policies.**

Docs: https://pnpm.io/cli/dlx

Standard equivalent: `npx` does the same job with different caching.

**Nub stance suggestion**: Nub should ship `nub x` / `nub dlx` for parity. Not a compatibility concern, just a UX concern.

### 4.3 `pnpm exec`

Run a command with `node_modules/.bin/` on PATH. Sets `INIT_CWD` (eventually — there was a compat bug, fixed). Sets `npm_config_*` env vars for legacy tooling. **PNPM-specific subtlety**: scripts inherit env vars prefixed with `pnpm_config_*` in addition to / instead of `npm_config_*`.

Docs: https://pnpm.io/cli/exec

Standard equivalent: `npm exec` exists; semantics close.

**Nub stance suggestion**: **support**. Nub's run/exec must export both `npm_config_*` and `INIT_CWD` for ecosystem compat; the `pnpm_config_*` vars are PNPM-only and **drop**.

### 4.4 Lifecycle script gating (the "approve-build" UX)

PNPM v10+: by default, no dependency's `preinstall`/`install`/`postinstall` runs. Allowlist via:
- `package.json#pnpm.onlyBuiltDependencies` (≤v10)
- `pnpm-workspace.yaml#onlyBuiltDependencies` (v10)
- `pnpm-workspace.yaml#allowBuilds` map (v11+, sole replacement)
- Or run `pnpm approve-builds` interactively

`pnpm approve-builds` lists deps that wanted to run scripts but were blocked; user picks; results are persisted.

Docs: https://pnpm.io/cli/approve-builds, https://socket.dev/blog/pnpm-10-0-0-blocks-lifecycle-scripts-by-default

Standard equivalent: industry direction. npm has discussions; yarn-berry has `enableScripts: false` per-package via plugins.

**Nub stance suggestion**: **support**. Adopt block-by-default for transitive deps. Read PNPM's allowlist locations. Ship `nub approve-builds` (alias of PNPM's command) for migration ease.

### 4.5 `pnpm.cjs` shim / `.pnpm-store`

PNPM's standalone executable (when distributed via `npx pnpm` or similar) drops a shim. The store dir defaults to `~/.local/share/pnpm/store/v{N}/` on Linux, `~/Library/pnpm/store/v{N}` on macOS, `%LOCALAPPDATA%\pnpm\store\v{N}` on Windows. Configurable via `storeDir`.

Standard equivalent: each PM has its own cache/store path.

**Nub stance suggestion**: not a compatibility concern. Nub should have its own store path. **drop** any PNPM-store interop (don't try to share files with PNPM; CAS schemes differ).

### 4.6 `pnpm install --frozen-lockfile` and `preferFrozenLockfile`

PNPM's frozen-lockfile semantics are stricter than npm's `npm ci`: PNPM fails if the manifest *or* the lockfile mismatch, even on missing peer deps. CI default in v10+: `preferFrozenLockfile: true`.

Standard equivalent: `npm ci`, yarn-classic `--frozen-lockfile`. Same concept; PNPM is the strictest.

**Nub stance suggestion**: **support**. Match PNPM's strictness for CI.

### 4.7 Recursive commands (`-r`, `--filter`)

PNPM's filter syntax (`--filter ./apps/web`, `--filter "...^@scope/pkg"`, `--filter "[origin/main]"`) is rich and partly idiosyncratic. Bun/turbo have similar concepts but different syntax.

**Nub stance suggestion**: **support-as-alias**. Adopt the most common forms (`--filter <name>`, `--filter ./path`, `--filter "...pkg"`, `--filter "[ref]"`); document that exotic forms may behave differently. Don't promise byte-for-byte parity.

---

## 5. Publish-time transforms

The "surprising" PNPM publish behaviors:

### 5.1 `workspace:` rewrites

On `pnpm publish`, every `workspace:*` / `workspace:^` / `workspace:~` / `workspace:1.2.3` etc. in `dependencies`/`devDependencies`/`peerDependencies`/`optionalDependencies` is rewritten to a concrete semver in the **published tarball's** `package.json`. The on-disk file is untouched.

Surprise factor: medium. Required for the published package to be installable by non-PNPM consumers (npm can't resolve `workspace:`). yarn-berry and Bun do the same.

**Nub stance**: **support**. Standard for any tool that supports `workspace:` at all.

### 5.2 `catalog:` rewrites

Same idea: `"react": "catalog:"` → `"react": "^18.2.0"` in the tarball. Required for publishability.

**Nub stance**: **support**.

### 5.3 `publishConfig.directory` (publish from subdir)

PNPM looks in `<package>/<publishConfig.directory>/package.json` for the *real* manifest to publish. The original `package.json` (with source paths, `workspace:`, etc.) is **not** what gets published. Pairs with build tools that emit a `dist/package.json` with rewritten paths.

Surprise factor: **high**. People expect `pnpm publish` to publish the package they `cd`'d into, not a subdir. And `npm publish` does not honor `publishConfig.directory` — so the same `package.json` behaves differently depending on which PM you invoke.

**Nub stance**: **support-as-alias** with loud logging. Worth supporting because real PNPM monorepos rely on it; emit a `publishing dist/ (publishConfig.directory)` line so it's never invisible.

### 5.4 `publishConfig.linkDirectory` (symlink dist for dev)

When `true` (default), workspace consumers of this package resolve to `<package>/<publishConfig.directory>` (i.e., the built output) during dev — not the source. Implemented by symlinking the dist dir into the consumer's `node_modules/<name>`.

Surprise factor: **highest**. A consumer importing `import { x } from 'my-pkg'` may get either source or built code depending on whether `linkDirectory` is true and whether the build has run. Debugging is awful, and this is worth calling out.

**Nub stance suggestion**: **ignore-with-warning** by default. Better alternatives exist (use `exports` conditions, or just point `main`/`module` at `dist/`). If we ever adopt it, make it opt-in per-project (`nub.publish.linkDirectory: true`) rather than honoring the PNPM-style default-true silently.

### 5.5 `publishConfig.executableFiles`

`chmod +x` the listed files in the tarball.

**Nub stance**: **support**. Useful, harmless, cheap.

### 5.6 `devDependencies` stripping

Standard npm behavior — not a PNPM-ism. Listed only to clarify.

### 5.7 `pnpm publish -r`

Publishes only workspace packages whose `version` isn't already in the registry. Pairs with `changesets`.

**Nub stance**: **support**. Worth replicating for monorepo UX.

### 5.8 LICENSE inheritance

PNPM auto-bundles the root workspace's `LICENSE` into each package's tarball if the package has no `LICENSE` of its own. Nice touch; not in npm.

**Nub stance**: **support**. Free correctness win.

---

## 6. Recommended baseline ("essentially compatible")

The smallest set of PNPM-isms Nub must support so a typical PNPM monorepo *Just Works*. Anything not listed here is opt-in.

**Must support (or 90% of real PNPM monorepos break):**

1. **`pnpm-lock.yaml` reading** — install reproducibility from existing lockfiles. (Writing it back is optional; reading is not.)
2. **`pnpm-workspace.yaml`** — at minimum the `packages` key, even if Nub's preferred input is `package.json#workspaces`. Also `catalog`/`catalogs`, `overrides`, `packageExtensions`, `patchedDependencies`, `allowBuilds`/`onlyBuiltDependencies`.
3. **`workspace:` protocol** — full variant support (`*`, `^`, `~`, pinned, ranged, `../path`, aliased). Rewrite to concrete versions on publish.
4. **`catalog:` protocol** — both default and named catalogs; rewrite on publish.
5. **`pnpm.overrides`** (in package.json) — read as alias of `npm`-style `overrides`.
6. **`pnpm.patchedDependencies`** — apply patches at install.
7. **`pnpm.packageExtensions`** / **`pnpm.peerDependencyRules`** — peer-dep escape hatches. Many real monorepos depend on `peerDependencyRules.ignoreMissing` or `allowedVersions` to install at all.
8. **`pnpm.supportedArchitectures`** — needed for Docker / cross-arch builds. Annoying to omit.
9. **Lifecycle script gating** — block-by-default; honor `onlyBuiltDependencies` and `allowBuilds`. Match PNPM v10 security default; surface a `nub approve-builds` UX.
10. **`publishConfig.directory`** — surprisingly common. With clear log line.
11. **`peerDependenciesMeta.optional`** — already standard, included for completeness.

**Should support (common but recoverable to drop):**

- `link:` and `file:` protocols (most projects don't, but the ones that do really need to).
- `dependenciesMeta.injected` — only matters if Nub adopts a symlink layout.
- `engines.runtime` / `devEngines.runtime` — only if Nub auto-provisions runtimes (probably no, Nub *is* the runtime).
- `publishConfig.executableFiles` — small, cheap.
- `requiredScripts` — cheap monorepo hygiene.

**Can ignore with warning (most users won't notice):**

- `.pnpmfile.cjs` / `.pnpmfile.mjs` — significant minority uses it, but most uses are stop-gap fixes that `packageExtensions` / `peerDependencyRules` cover. Warn loudly on encounter.
- `configDependencies` — niche, v10+, plugin-system-ish.
- `publishConfig.linkDirectory` — the sharpest of these. Warn and document the better alternative.
- `pnpm.allowedDeprecatedVersions` — log-noise only.
- `pnpm.updateConfig`, `pnpm.auditConfig` — only relevant if Nub ships update/audit.
- `hoistPattern` / `publicHoistPattern` / `shamefullyHoist` — Nub's layout choice supersedes these.
- `executionEnv.nodeVersion` — already removed in PNPM v11.

**Can drop:**

- `.modules.yaml` — Nub should write its own equivalent if needed; don't read PNPM's.
- `node_modules/.pnpm/` layout interop — Nub picks its own virtual store path.
- `pnpm_config_*` env vars — `npm_config_*` is enough.
- PNPM-store directory sharing — separate stores.
- All `.npmrc` keys that are pure PNPM behavior tuning (`node-linker`, `hoist-pattern`, etc.) — auth/registry keys stay supported.

---

## 7. Open questions

1. **Lockfile**: read PNPM, write Nub-native (recommended)? Or read+write PNPM? Or read+write both?
2. **Workspace config primary source**: `package.json#workspaces` only (preferred) with `pnpm-workspace.yaml#packages` as fallback? Or `nub.yaml` / `package.json#nub.workspaces` as Nub-native primary?
3. **Catalogs location**: PNPM puts them in `pnpm-workspace.yaml`. Nub could read them there *and* support `package.json#catalog` / `package.json#catalogs` for a single-file alternative.
4. **`publishConfig.linkDirectory`**: default-true (PNPM compat) or default-false-with-warning (cleaner)? My pick: default-false-with-warning.
5. **`.pnpmfile.cjs`**: ignore, error, or partially support? My pick: parse & warn, never execute, in v1.
6. **Block-by-default lifecycle scripts**: adopt unconditionally (correct for security)? Or gate behind a flag for migration? My pick: unconditional + auto-import existing PNPM allowlists.
7. **node_modules layout**: isolated/symlinked (PNPM-style) or hoisted (npm-style)? Out of scope here but downstream of every decision above. Tracked in `wiki/commands/pm/install.md` (deferred to v1.x; see `package-manager-strategy.md`).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
