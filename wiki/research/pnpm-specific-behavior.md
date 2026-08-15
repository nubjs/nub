# Reference: pnpm-specific behavior

Inventory of behaviors, files, and `package.json` extensions that are **pnpm-specific** (not standard npm/Node). For each item: what it is, a link to the docs, whether a standard equivalent exists, and a Nub stance suggestion.

Suggestions are starting points, not final calls.

Stance legend:

- **support** — standard or near-universal; Nub supports it.
- **support-as-alias** — pnpm-only but load-bearing; Nub reads it (possibly under a Nub-native name too).
- **ignore-with-warning** — parse but no-op; warn so users aren't surprised.
- **drop** — refuse / error; the behavior is hostile to a clean design.

Sources are inline. Big changes landed in v10 (Jan 2025) and v11 (late 2025/2026); those are flagged below.

---

## 1. Top-level files pnpm creates or reads

Files pnpm reads or writes at the project root, plus the virtual store it builds inside `node_modules`. Only `.npmrc` is shared with other package managers; the rest are pnpm's own.

### 1.1 `pnpm-workspace.yaml`

The de facto pnpm config file as of v10/v11. It originally just listed workspace packages; it now holds nearly all pnpm config, because **v11 stopped reading the `pnpm` field in `package.json` and stopped reading pnpm keys from `.npmrc`**.

Docs: https://pnpm.io/pnpm-workspace_yaml, https://pnpm.io/settings, https://pnpm.io/blog/releases/11.0

Known top-level keys (non-exhaustive — the settings page lists ~100):
- `packages` — workspace glob list (the original purpose)
- `catalog`, `catalogs` — see §3.2
- `overrides` — dependency graph overrides (npm has `overrides` in package.json; pnpm moved its version here)
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

Standard equivalent: **none for the file itself**. The npm/yarn/Bun form is `package.json#workspaces`, which pnpm never reads — confirmed at https://pnpm.io/workspaces ("A workspace must have a `pnpm-workspace.yaml` file in its root").

**Nub stance suggestion**: **support-as-alias** for reading — a typical pnpm monorepo is unusable without it. Prefer Nub-native config in `package.json#workspaces` plus a Nub config file, and on `nub install` warn when both that and `pnpm-workspace.yaml` `packages` are present. The non-workspace keys (catalogs, overrides, hoist) need per-key decisions, below.

### 1.2 `pnpm-lock.yaml`

YAML lockfile. Schema is pnpm-versioned (v6, v7, v9, v10 — bumped on breaking changes). Not compatible with npm's `package-lock.json` or yarn's `yarn.lock`.

Docs: https://pnpm.io/symlinked-node-modules-structure (mentioned throughout)

Standard equivalent: none; every PM has its own lockfile.

**Nub stance suggestion**: **support-as-alias** for reading — installing deterministically from a `pnpm-lock.yaml` is table stakes for "essentially compatible." Whether Nub *writes* one is a separate decision: pnpm's format is a moving maintenance burden, and a Nub-native one breaks pnpm-using teammates. Pragmatic default: read pnpm lockfile; write Nub lockfile; offer a `--write-pnpm-lock` mode for migration.

### 1.3 `.pnpmfile.cjs` / `.pnpmfile.mjs` / `pnpmfile.cjs`

JS file with hooks pnpm calls during resolution and install.

Hooks: `readPackage`, `afterAllResolved`, `preResolution`, `updateConfig` (v10.8+), `beforePacking` (v10.28+), `importPackage`. v11 added `.mjs` support and moved `hooks.fetchers` to top-level `fetchers`/`resolvers` exports.

Docs: https://pnpm.io/pnpmfile

Standard equivalent: none. Yarn has plugins, npm has nothing. It is a powerful escape hatch and a footgun: userland code can mutate every dependency's manifest.

**Nub stance suggestion**: **ignore-with-warning** initially. The hook API is unstable across pnpm versions, and a meaningful fraction of `.pnpmfile.cjs` files exist to patch peerDeps that Nub could handle differently, or that the declarative `packageExtensions` / `peerDependencyRules` cover. Worth revisiting on user demand.

### 1.4 `.npmrc` (pnpm-specific keys)

Until v11, pnpm read its config from `.npmrc` using dozens of custom keys. **v11 ripped this out**: `.npmrc` is now auth + registry only, everything else moved to `pnpm-workspace.yaml`, and `npm_config_*` env vars are no longer read.

The replacement env prefix is `pnpm_config_*`.

Docs: https://pnpm.io/npmrc, https://pnpm.io/blog/releases/11.0

Notable pnpm-only keys (legacy; in v10 and earlier):
- `hoist-pattern`, `public-hoist-pattern`, `shamefully-hoist`
- `node-linker` (`isolated` / `hoisted` / `pnp`)
- `dedupe-peer-dependents`, `strict-peer-dependencies`, `auto-install-peers`
- `resolution-mode`, `prefer-workspace-packages`, `link-workspace-packages`, `save-workspace-protocol`
- `modules-dir`, `virtual-store-dir`, `store-dir`, `package-import-method`
- `side-effects-cache`, `verify-store-integrity`
- `prefer-frozen-lockfile`, `use-lockfile-v6`, `lockfile`
- `manage-package-manager-versions`, `only-built-dependencies`, `ignore-dep-scripts`

Standard equivalent: `.npmrc` itself is shared (npm/pnpm/yarn-classic). The **keys** above are pnpm-only.

**Nub stance suggestion**: **support-as-alias** for `.npmrc` *registry/auth keys* (cross-PM standard: `_authToken`, `registry`, scoped `@scope:registry`). **ignore-with-warning** for the pnpm-only behavioral keys, which describe a node_modules layout Nub likely won't replicate verbatim — mapping only those with natural Nub analogs (`node-linker=hoisted` → a Nub flat-install mode, if one is ever added).

### 1.5 `node_modules/.pnpm/` and `node_modules/.modules.yaml`

The **virtual store**: every package is unpacked once into `node_modules/.pnpm/<name>@<version>/node_modules/<name>/`, then symlinked into consumers' `node_modules/`.

Files inside are hard-linked from the global content-addressable store (default `~/.local/share/pnpm/store` on Linux).

A metadata sidecar, `.modules.yaml`, carries `storeDir`, `virtualStoreDir`, `layoutVersion`, `packageManager`, `pendingBuilds`, `hoistedAliases`, `skipped`, `shamefullyFlatten` and more; pnpm uses it to detect a stale node_modules and rebuild.

Docs: https://pnpm.io/symlinked-node-modules-structure, https://www.npmjs.com/package/@pnpm/modules-yaml

Standard equivalent: none. npm/yarn use flat node_modules; yarn-berry uses PnP.

**Nub stance suggestion**: a layout choice, not a compatibility surface. Nub can pick **isolated symlink layout** (pnpm-style) for the same correctness benefits, or **hoisted** (npm-style), without using pnpm's virtual store: never read or write `.modules.yaml`, and write a Nub equivalent if needed. Nothing but pnpm should hard-code `node_modules/.pnpm/`, so Nub can use its own directory or skip the layer.

### 1.6 `configDependencies` (v10+)

Special dep type declared in `pnpm-workspace.yaml`, installed *before* regular deps, unable to carry transitive deps or lifecycle scripts, and integrity-pinned in the lockfile.

It shares pnpm hooks/catalogs/overrides across repos via a published npm package; packages matching `pnpm-plugin-*` / `@*/pnpm-plugin-*` / `@pnpm/plugin-*` auto-load their `pnpmfile.mjs`.

Docs: https://pnpm.io/config-dependencies

Standard equivalent: none. Effectively pnpm's plugin system.

**Nub stance suggestion**: **ignore-with-warning**. Niche, recent, pnpm-internal machinery. A future plugin system should be designed Nub-native instead.

---

## 2. `package.json` fields pnpm-specific or pnpm-extended

Big caveat: **v11 stopped reading the `pnpm` field from `package.json`**, moving all `pnpm.*` config to `pnpm-workspace.yaml`.

The `package.json` fields still in active use are the publish-time ones (`publishConfig`, `executableFiles`) plus `dependenciesMeta` / `peerDependenciesMeta`. Every pnpm ≤ v10 project still carries `pnpm.*` in `package.json`, so Nub cannot ignore them in practice for some years.

Docs: https://pnpm.io/package_json, https://pnpm.io/blog/releases/11.0

### 2.1 `pnpm.overrides` (≤v10; moved to `pnpm-workspace.yaml` in v11)

Force a specific version of a transitive dep across the graph.

Standard equivalent: the concept is universal — **npm has `overrides`** (different schema, same intent) and Yarn has `resolutions`.

**Nub stance suggestion**: **support**. Read npm-style `package.json#overrides` as the canonical form; also accept `pnpm.overrides` and `pnpm-workspace.yaml#overrides` for compat. Normalize internally.

### 2.2 `pnpm.packageExtensions` (≤v10; moved to workspace yaml in v11)

Inject missing `dependencies` / `peerDependencies` into other packages' manifests at resolve time. Originally a Yarn-berry feature; pnpm adopted the schema.

Standard equivalent: not in npm. Yarn berry supports it.

**Nub stance suggestion**: **support-as-alias**. It solves real, common manifest bugs without modifying upstream, and is cheap to implement. Treat the schema as cross-PM.

### 2.3 `pnpm.peerDependencyRules`

- `ignoreMissing`: list of peer dep names whose absence won't warn
- `allowedVersions`: map of `pkg` → version range that overrides peer requirements
- `allowAny`: list of pkgs where any version is OK

Standard equivalent: none.

**Nub stance suggestion**: **support-as-alias**. Most modern pnpm monorepos use it to silence noisy peer-dep warnings, and Nub needs the same escape hatches if it does its own peer resolution (likely, with `autoInstallPeers` semantics).

### 2.4 `pnpm.neverBuiltDependencies` / `pnpm.onlyBuiltDependencies` / `pnpm.ignoredBuiltDependencies` / `ignoreDepScripts`

Allowlist/denylist for which deps may run lifecycle scripts (`preinstall`/`install`/`postinstall`). **pnpm v10 changed the default to "block all dep lifecycle scripts unless allowlisted"** (a supply-chain win, breaking change).

**pnpm v11 removed all four fields in favor of a single `allowBuilds` map** in `pnpm-workspace.yaml`.

Docs: https://pnpm.io/cli/approve-builds, https://socket.dev/blog/pnpm-10-0-0-blocks-lifecycle-scripts-by-default, https://pnpm.io/blog/releases/11.0

Standard equivalent: none, but **npm 10+ has `package.json#trustedDependencies` proposals** and yarn-berry has the concept. The *direction* is industry-wide.

**Nub stance suggestion**: **support** — Nub should adopt the v10 default (block by default) regardless of pnpm compat. Read `pnpm.onlyBuiltDependencies` and `pnpm-workspace.yaml#allowBuilds` as input to Nub's own allowlist, or standardize on the `allowBuilds` map.

### 2.5 `pnpm.allowedDeprecatedVersions`

Suppress deprecation warnings for listed packages.

Standard equivalent: none.

**Nub stance suggestion**: **ignore-with-warning**. Pure log-noise control, mostly used to hide a single annoying dep; a Nub verbosity flag is cleaner.

### 2.6 `pnpm.patchedDependencies`

Map `name@version` → relative path to a `.patch` file. Applied at install time after extraction from store. v11 changed failure semantics: all patch failures throw, errors aggregated.

Docs: https://pnpm.io/cli/patch

Standard equivalent: `patch-package` (userland), yarn-berry has built-in `yarn patch`.

**Nub stance suggestion**: **support-as-alias**. The `.patch` format is standard unified-diff and the field schema is reasonable, so adopt it and document `nub patch <pkg>` to produce one. Apply both `package.json#pnpm.patchedDependencies` and `pnpm-workspace.yaml#patchedDependencies`.

### 2.7 `pnpm.updateConfig`, `pnpm.auditConfig`, `pnpm.requiredScripts`, `pnpm.supportedArchitectures`

- `updateConfig.ignoreDependencies`: exclude from `pnpm outdated`/`pnpm update`
- `auditConfig.ignoreGhsas` (v11, was `ignoreCves`): silence specific advisories
- `requiredScripts`: list of script names that all workspace packages must define
- `supportedArchitectures`: `{os, cpu, libc}` — install optional deps for non-host platforms (useful for Docker builds)

Standard equivalent: none; `supportedArchitectures` is genuinely useful and Bun/Yarn have no clean equivalent.

**Nub stance suggestion**:
- `updateConfig`, `auditConfig`: **ignore-with-warning** unless Nub ships `update` / `audit` subcommands with similar semantics.
- `requiredScripts`: **support-as-alias** — small, useful, cheap.
- `supportedArchitectures`: **support** — genuinely needed for cross-platform builds; adopt the schema.

### 2.8 `pnpm.executionEnv.nodeVersion` (≤v10; **removed in v11**)

Pin a Node version per package; pnpm auto-downloaded it and ran scripts under it.

Docs: https://github.com/orgs/pnpm/discussions/10172

**Replaced in v11 by `engines.runtime` and `devEngines.runtime`** (a more general format covering Node/Deno/Bun). Stored in lockfile.

Standard equivalent: `engines.node` (advisory only in npm). The `devEngines` concept is partly standardized in npm.

**Nub stance suggestion**: **support** `engines.runtime` / `devEngines.runtime` if runtime auto-provisioning is in scope at all (Nub is its own runtime, so the answer is probably "no, Nub runs scripts under Nub"). **ignore-with-warning** for legacy `pnpm.executionEnv.nodeVersion`.

### 2.9 `dependenciesMeta`

- `dependenciesMeta.<dep>.injected`: hard-link a *copy* into the virtual store rather than symlinking. Forces fresh peer resolution per consumer. Used in monorepos where workspace packages need different peer instances.

Docs: https://pnpm.io/package_json#dependenciesmeta

Standard equivalent: yarn-berry has `installConfig.hoistingLimits` and similar; npm has nothing.

**Nub stance suggestion**: **support-as-alias** if Nub's node_modules layout has the same symlink-vs-copy distinction. If Nub goes flat/hoisted, this is meaningless and can be **ignore-with-warning**.

### 2.10 `peerDependenciesMeta`

- `peerDependenciesMeta.<dep>.optional`: peer dep won't error if missing.

Standard equivalent: originally pnpm-specific, but **npm has supported it since npm 7**. Effectively standard.

**Nub stance suggestion**: **support**.

### 2.11 `publishConfig` quirks

The base `publishConfig` field is npm-standard: it overrides top-level fields *only for the published tarball* (`registry`, `tag`, `access`, `bin`, `main`, `exports`, `types`, `module`, `browser`). pnpm extends it with:

- **`publishConfig.directory`** — publish from a *subdirectory* (e.g., `dist/`). pnpm expects that subdir to contain a complete, ready-to-publish `package.json`. Used heavily with build steps that emit transformed manifests. Standard npm: **does not exist**. https://github.com/pnpm/pnpm/issues/6253
- **`publishConfig.linkDirectory`** — boolean (default **true** per pnpm docs). When true and `directory` is set, pnpm symlinks the *subdir* into `node_modules` for **local workspace consumers** during dev — so dependents resolve the built output instead of source. This is the surprising one. https://github.com/orgs/pnpm/discussions/5692
- **`publishConfig.executableFiles`** — array of relative paths to chmod +x on publish, beyond what `bin` already covers. Niche but real.

And the **publish-time rewrites** pnpm performs (see §5):
- `workspace:*` / `workspace:^` / `workspace:~` → resolved concrete versions
- `catalog:` / `catalog:foo` → resolved concrete versions
- `link:` and `file:` → behavior varies; generally rejected on publish
- `devDependencies` are stripped from the published tarball (this part is standard npm behavior)

Standard equivalent: `publishConfig` itself is standard; the pnpm additions (`directory`, `linkDirectory`, `executableFiles`) are not. **Bun, yarn, and npm don't recognize them.**

**Nub stance suggestion**:
- `publishConfig.directory`: **support-as-alias**. A legitimately useful pattern (publish `dist/` not source), but surprising because no other tool recognizes it. Nub should support it on `nub publish` and emit a clear log line ("publishing from `<dir>` per publishConfig.directory").
- `publishConfig.linkDirectory`: **ignore-with-warning** by default. It changes how *dependents resolve workspace packages during dev* in a way that's invisible from the depending package's perspective — that's the footgun. A Nub project should be able to express "dev resolves dist" via a clearer mechanism (e.g., `exports` conditions). Warn if encountered; offer to migrate.
- `publishConfig.executableFiles`: **support**. Tiny, useful, low-controversy.

### 2.12 `workspaces` (package.json)

pnpm **does not read this field**. It is npm/yarn/Bun standard.

Docs: https://pnpm.io/workspaces

**Nub stance suggestion**: **support** as primary input. When migrating from pnpm, Nub can read `pnpm-workspace.yaml#packages` as a fallback (and offer to migrate it into `package.json#workspaces`).

### 2.13 `bundleDependencies` / `bundledDependencies`

npm-standard, with historically partial and quirky pnpm support. Listed only to confirm: **not a pnpm-ism**.

**Nub stance suggestion**: **support** (standard).

---

## 3. Dependency protocols / specifiers pnpm supports beyond npm

Five specifier prefixes: `workspace:`, `catalog:`, `link:`, `file:` and `jsr:`. Only `file:` is universal, and its semantics differ per tool; Yarn or Bun support each of the others, so none is pnpm-only today.

### 3.1 `workspace:` protocol

pnpm's flagship workspace feature. Forms:

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

**Nub stance suggestion**: **support** (treat as standard). Non-negotiable for "essentially compatible."

### 3.2 `catalog:` protocol

Centralize dep versions across a monorepo. Declared in `pnpm-workspace.yaml`:

```yaml
catalog:
  react: ^18.2.0
catalogs:
  react17:
    react: ^17.0.2
```

Used as `"react": "catalog:"` (default) or `"react": "catalog:react17"` (named). **Replaced with concrete versions on publish.** The `catalogMode` setting (`manual`/`strict`/`prefer`) controls auto-rewriting of literal versions into catalog refs at install time, and `cleanupUnusedCatalogs` prunes unused entries.

Docs: https://pnpm.io/catalogs

Standard equivalent: **Bun has `catalog:`** as of 2025; not in npm or yarn. Trending toward standard.

**Nub stance suggestion**: **support**. Adopt the schema, and optionally expose a Nub-native location (`package.json#catalog`) since the data is a flat object. A Nub project need not use `pnpm-workspace.yaml` but should be able to read it.

### 3.3 `link:` protocol

A `link:` spec (`"foo": "link:../foo"`) is a non-installed symlink, supported by yarn-classic, yarn-berry and pnpm. Unlike `file:`, it does not copy.

Standard equivalent: shared across pnpm/yarn. Not in npm (npm's `link:` is different / not portable in package.json).

**Nub stance suggestion**: **support**. Useful for one-off local dev; trivial.

### 3.4 `file:` protocol

Every PM supports `"foo": "file:../foo"`, but **with subtly different semantics**:
- npm: copies on install, then symlinks
- pnpm: symlinks by default (treats it like `link:` mostly); has `prefer-symlinked-executables`
- yarn-berry: archives into a `.zip` for PnP

Standard equivalent: nominally standard; semantics drift.

**Nub stance suggestion**: **support**. Pick semantics closer to npm (copy) for predictability; document the choice.

### 3.5 `jsr:` protocol

Recent (pnpm ≥9). A `"foo": "jsr:@scope/foo@^1"` spec resolves from jsr.io.

Standard equivalent: Deno-native, and supported by Bun and pnpm; not npm or yarn.

**Nub stance suggestion**: **support**. JSR isn't going away, and the cost is one alternate resolver.

### 3.6 Patches via `patchedDependencies`

Not a specifier per se — see §2.6.

---

## 4. CLI / lifecycle behaviors diverging from npm

Seven divergences at the command surface or during install. Two change observable behavior rather than ergonomics: the symlinked `node_modules` layout, which alters what module resolution sees, and pnpm v10's block-scripts-by-default gating.

### 4.1 Symlinked `node_modules`

Implications:
- Tools that walk `node_modules` recursively will see different shapes.
- Node's default behavior is to **resolve symlinks** during module resolution (i.e., `require.resolve` returns the real path under `.pnpm/<name>@<v>/...`). `--preserve-symlinks` reverses this and breaks pnpm. Some bundlers/build tools have had bugs around this for years.
- `__dirname` / `import.meta.url` give the real path, not the symlinked path. Surprising for code that expects to find sibling packages by walking up.

**Nub stance suggestion**: the Nub runtime decides its module resolution semantics independently of the PM. If Nub the PM produces a pnpm-like layout, the runtime must handle symlinks correctly — almost certain, since that is Node's default.

### 4.2 `pnpm dlx` vs `npx`

The `pnpm dlx <pkg>` verb fetches a package to a temp dir, runs it, and does not install it. Caches by package+version in `~/.local/state/pnpm/dlx/` (config: `dlxCacheMaxAge`).

Aliases: `pnx`, `pnpx`. **v11 has dlx honor `minimumReleaseAge` and trust policies.**

Docs: https://pnpm.io/cli/dlx

Standard equivalent: `npx` does the same job with different caching.

**Nub stance suggestion**: Nub should ship `nub x` / `nub dlx` for parity. Not a compatibility concern, just a UX concern.

### 4.3 `pnpm exec`

Runs a command with `node_modules/.bin/` on PATH, setting `INIT_CWD` (eventually — there was a compat bug, since fixed) and `npm_config_*` for legacy tooling.

**pnpm-specific subtlety**: scripts also inherit env vars prefixed `pnpm_config_*`, in addition to or instead of `npm_config_*`.

Docs: https://pnpm.io/cli/exec

Standard equivalent: `npm exec` exists; semantics close.

**Nub stance suggestion**: **support**. Nub's run/exec must export both `npm_config_*` and `INIT_CWD` for ecosystem compat; the `pnpm_config_*` vars are pnpm-only and **drop**.

### 4.4 Lifecycle script gating (the "approve-build" UX)

Under pnpm v10+, no dependency's `preinstall`/`install`/`postinstall` runs by default. Allowlist via:
- `package.json#pnpm.onlyBuiltDependencies` (≤v10)
- `pnpm-workspace.yaml#onlyBuiltDependencies` (v10)
- `pnpm-workspace.yaml#allowBuilds` map (v11+, sole replacement)
- Or run `pnpm approve-builds` interactively

The `pnpm approve-builds` command lists deps that wanted to run scripts but were blocked, the user picks, and the results are persisted.

Docs: https://pnpm.io/cli/approve-builds, https://socket.dev/blog/pnpm-10-0-0-blocks-lifecycle-scripts-by-default

Standard equivalent: industry direction. npm has discussions; yarn-berry has `enableScripts: false` per-package via plugins.

**Nub stance suggestion**: **support**, per §2.4. Ship `nub approve-builds` under pnpm's own name for migration ease.

### 4.5 `pnpm.cjs` shim / `.pnpm-store`

The standalone executable (distributed via `npx pnpm` or similar) drops a shim.

The store dir defaults to `~/.local/share/pnpm/store/v{N}/` on Linux, `~/Library/pnpm/store/v{N}` on macOS and `%LOCALAPPDATA%\pnpm\store\v{N}` on Windows, configurable via `storeDir`.

Standard equivalent: each PM has its own cache/store path.

**Nub stance suggestion**: not a compatibility concern — Nub should have its own store path. **drop** any pnpm-store interop; the CAS schemes differ, so sharing files is not an option.

### 4.6 `pnpm install --frozen-lockfile` and `preferFrozenLockfile`

pnpm's frozen-lockfile semantics are stricter than npm's `npm ci`: pnpm fails if the manifest *or* the lockfile mismatch, even on missing peer deps. CI default in v10+: `preferFrozenLockfile: true`.

Standard equivalent: `npm ci`, yarn-classic `--frozen-lockfile`. Same concept; pnpm is the strictest.

**Nub stance suggestion**: **support**. Match pnpm's strictness for CI.

### 4.7 Recursive commands (`-r`, `--filter`)

pnpm's filter syntax (`--filter ./apps/web`, `--filter "...^@scope/pkg"`, `--filter "[origin/main]"`) is rich and partly idiosyncratic. Bun/turbo have similar concepts but different syntax.

**Nub stance suggestion**: **support-as-alias**. Adopt the most common forms (`--filter <name>`, `--filter ./path`, `--filter "...pkg"`, `--filter "[ref]"`); document that exotic forms may behave differently. Don't promise byte-for-byte parity.

---

## 5. Publish-time transforms

These transforms apply to the published tarball, never to the files on disk. The `workspace:` and `catalog:` rewrites are what make a published package installable by npm; the `publishConfig` extensions and LICENSE inheritance are pnpm's own.

### 5.1 `workspace:` rewrites

On `pnpm publish`, every `workspace:` specifier is rewritten to a concrete semver in the **published tarball's** `package.json`. The on-disk file is untouched.

This covers `workspace:*` / `workspace:^` / `workspace:~` / `workspace:1.2.3` etc. in `dependencies`/`devDependencies`/`peerDependencies`/`optionalDependencies`.

Surprise factor: medium. The rewrite is required for the published package to be installable by non-pnpm consumers, since npm cannot resolve `workspace:`. yarn-berry and Bun do the same.

**Nub stance**: **support**. Standard for any tool that supports `workspace:` at all.

### 5.2 `catalog:` rewrites

Same idea: `"react": "catalog:"` → `"react": "^18.2.0"` in the tarball. Required for publishability.

**Nub stance**: **support**.

### 5.3 `publishConfig.directory` (publish from subdir)

pnpm looks in `<package>/<publishConfig.directory>/package.json` for the *real* manifest to publish; the original `package.json` (with source paths, `workspace:`, etc.) is **not** what gets published.

Pairs with build tools that emit a `dist/package.json` with rewritten paths.

Surprise factor: **high**. People expect `pnpm publish` to publish the package they `cd`'d into, not a subdir, and `npm publish` does not honor `publishConfig.directory` — so the same `package.json` behaves differently depending on which PM you invoke.

**Nub stance**: **support-as-alias**, emitting a `publishing dist/ (publishConfig.directory)` line so it is never invisible.

### 5.4 `publishConfig.linkDirectory` (symlink dist for dev)

When `true` (default), workspace consumers of this package resolve to `<package>/<publishConfig.directory>` — the built output, not the source — implemented by symlinking the dist dir into the consumer's `node_modules/<name>`.

Surprise factor: **highest**. A consumer importing `import { x } from 'my-pkg'` may get either source or built code depending on whether `linkDirectory` is true and whether the build has run, which makes debugging awful.

**Nub stance suggestion**: **ignore-with-warning** by default; better alternatives exist (`exports` conditions, or `main`/`module` pointed at `dist/`). If ever adopted, make it opt-in per project rather than honoring pnpm's default-true silently.

### 5.5 `publishConfig.executableFiles`

Marks the listed files `chmod +x` in the tarball.

**Nub stance**: **support**. Useful, harmless, cheap.

### 5.6 `devDependencies` stripping

Standard npm behavior — not a pnpm-ism. Listed only to clarify.

### 5.7 `pnpm publish -r`

Publishes only workspace packages whose `version` isn't already in the registry. Pairs with `changesets`.

**Nub stance**: **support**. Worth replicating for monorepo UX.

### 5.8 LICENSE inheritance

pnpm auto-bundles the root workspace's `LICENSE` into each package's tarball if the package has no `LICENSE` of its own. Not in npm.

**Nub stance**: **support**. Free correctness win.

---

## 6. Recommended baseline ("essentially compatible")

The smallest set of pnpm-isms Nub must support so a typical pnpm monorepo just works. Anything not listed here is opt-in.

**Must support (or 90% of real pnpm monorepos break):**

1. **`pnpm-lock.yaml` reading** — install reproducibility from existing lockfiles. (Writing it back is optional; reading is not.)
2. **`pnpm-workspace.yaml`** — at minimum the `packages` key, even if Nub's preferred input is `package.json#workspaces`. Also `catalog`/`catalogs`, `overrides`, `packageExtensions`, `patchedDependencies`, `allowBuilds`/`onlyBuiltDependencies`.
3. **`workspace:` protocol** — full variant support (`*`, `^`, `~`, pinned, ranged, `../path`, aliased). Rewrite to concrete versions on publish.
4. **`catalog:` protocol** — both default and named catalogs; rewrite on publish.
5. **`pnpm.overrides`** (in package.json) — read as alias of npm-style `overrides`.
6. **`pnpm.patchedDependencies`** — apply patches at install.
7. **`pnpm.packageExtensions`** / **`pnpm.peerDependencyRules`** — peer-dep escape hatches. Many real monorepos depend on `peerDependencyRules.ignoreMissing` or `allowedVersions` to install at all.
8. **`pnpm.supportedArchitectures`** — needed for Docker / cross-arch builds.
9. **Lifecycle script gating** — block-by-default; honor `onlyBuiltDependencies` and `allowBuilds`. Match the pnpm v10 security default; surface a `nub approve-builds` UX.
10. **`publishConfig.directory`** — surprisingly common. With a clear log line.
11. **`peerDependenciesMeta.optional`** — already standard, included for completeness.

**Should support (common but recoverable to drop):**

- `link:` and `file:` protocols (most projects don't, but the ones that do really need to).
- `dependenciesMeta.injected` — only matters if Nub adopts a symlink layout.
- `engines.runtime` / `devEngines.runtime` — only if Nub auto-provisions runtimes (probably no, Nub *is* the runtime).
- `publishConfig.executableFiles` — small, cheap.
- `requiredScripts` — cheap monorepo hygiene.

**Can ignore with warning (most users won't notice):**

- `.pnpmfile.cjs` / `.pnpmfile.mjs` — a significant minority uses it, but most uses are stop-gap fixes that `packageExtensions` / `peerDependencyRules` cover. Warn loudly on encounter.
- `configDependencies` — niche, v10+, plugin-system-ish.
- `publishConfig.linkDirectory` — the sharpest of these. Warn and document the better alternative.
- `pnpm.allowedDeprecatedVersions` — log-noise only.
- `pnpm.updateConfig`, `pnpm.auditConfig` — only relevant if Nub ships update/audit.
- `hoistPattern` / `publicHoistPattern` / `shamefullyHoist` — Nub's layout choice supersedes these.
- `executionEnv.nodeVersion` — already removed in pnpm v11.

**Can drop:**

- `.modules.yaml` — Nub should write its own equivalent if needed; don't read pnpm's.
- `node_modules/.pnpm/` layout interop — Nub picks its own virtual store path.
- `pnpm_config_*` env vars — `npm_config_*` is enough.
- pnpm-store directory sharing — separate stores.
- All `.npmrc` keys that are pure pnpm behavior tuning (`node-linker`, `hoist-pattern`, etc.) — auth/registry keys stay supported.

---

## 7. Open questions

Seven decisions this inventory does not settle, each with a recommended answer where one is clear. The `node_modules` layout question is deferred to v1.x and sits downstream of the rest.

1. **Lockfile**: read pnpm, write Nub-native (recommended)? Or read+write pnpm? Or read+write both?
2. **Workspace config primary source**: `package.json#workspaces` only (preferred) with `pnpm-workspace.yaml#packages` as fallback? Or a Nub-native primary?
3. **Catalogs location**: pnpm puts them in `pnpm-workspace.yaml`. Nub could read them there *and* support `package.json#catalog` / `package.json#catalogs` for a single-file alternative.
4. **`publishConfig.linkDirectory`**: default-true (pnpm compat) or default-false-with-warning (cleaner)? Recommended: default-false-with-warning.
5. **`.pnpmfile.cjs`**: ignore, error, or partially support? Recommended: parse and warn, never execute, in v1.
6. **Block-by-default lifecycle scripts**: adopt unconditionally (correct for security)? Or gate behind a flag for migration? Recommended: unconditional + auto-import existing pnpm allowlists.
7. **node_modules layout**: isolated/symlinked (pnpm-style) or hoisted (npm-style)? Out of scope here but downstream of every decision above; deferred to v1.x.

## Changelog

Revision history for this document. The only entry so far records its migration out of the internal research corpus.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
