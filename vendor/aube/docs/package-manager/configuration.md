# Configuration

aube reads pnpm-compatible configuration from project `.npmrc`, user `.npmrc`,
user aube config, `aube-workspace.yaml`, environment variables, and supported
CLI flags. Existing `pnpm-workspace.yaml` files are migration inputs.

## Defaults worth knowing

| Area | Default | Why it matters |
| --- | --- | --- |
| Linker | `nodeLinker=isolated` | Keeps transitive dependencies scoped to the packages that declared them. |
| Package imports | `packageImportMethod=auto` | Hardlinks files from the store, falling back to copy on cross-filesystem boundaries. Opt into reflink with `clone` or `clone-or-copy`. |
| New releases | `minimumReleaseAge=1440` | Avoids installing versions published in the last 24 hours by default. |
| Exotic transitive deps | `blockExoticSubdeps=true` | Blocks transitive git and tarball dependencies unless you opt out. |
| Dependency scripts | approval required | Build scripts in dependencies stay skipped until approved. |
| Jailed builds | `jailBuilds=false` | Opt in to running approved dependency scripts with a restricted env, temporary `HOME`, and native macOS jail. Planned to default to `true` in the next major version. |
| Auto-install before scripts | enabled | `aube run`, `aube test`, and `aube exec` repair stale installs first. |

## User aube config

`aube config set` writes user-scope settings to `~/.config/aube/config.toml` by
default. If `XDG_CONFIG_HOME` is set, the path is `$XDG_CONFIG_HOME/aube/config.toml`.

```toml
minimumReleaseAge = 2880
autoInstallPeers = true
nodeLinker = "isolated"
packageImportMethod = "auto"
```

aube reads configuration from `.npmrc` regardless of which tool wrote it.
Writes follow a routing rule: settings marked `npmShared = true` in
[`crates/aube-settings/settings.toml`][settings-toml] (plus per-host auth/cert
templates and scoped registries) land in `.npmrc` so npm, yarn, and pnpm see
the same value. Aube-only and pnpm-only settings land in
`~/.config/aube/config.toml` instead, so unknown-to-npm keys don't trigger
warnings from sibling tools.

[settings-toml]: https://github.com/jdx/aube/blob/main/crates/aube-settings/settings.toml

## .npmrc

```ini
registry=https://registry.npmjs.org/
@mycorp:registry=https://npm.mycorp.internal/
//registry.npmjs.org/:_authToken=${NPM_TOKEN}
https-proxy=http://corp-proxy:3128/
```

`.npmrc` holds the keys that npm, yarn, and pnpm all read: registries, scoped
registries, per-host auth, proxy/TLS, and the npm-standard scalars tagged
`npmShared` in the settings registry. aube preserves symlinked `.npmrc` files
when it writes to one. See the [settings reference](/settings/) — each entry
lists its `.npmrc` key alongside the other sources.

Aube map settings (`allowBuilds`, `overrides`, `packageExtensions`, …) accept
**dotted writes** at project scope to edit one entry at a time:

```sh
aube config set --local allowBuilds.@mongodb-js/zstd true
aube config set --local overrides.lodash 4.17.21
```

The write lands in `pnpm-workspace.yaml#<map>.<entry>` when a workspace yaml
exists, otherwise `package.json#aube.<map>.<entry>` — the same place install
reads from. User-scope dotted writes for these maps error: aube only reads
them per project. For `allowBuilds`, `aube approve-builds <pkg>` is the
interactive equivalent.

## Workspace YAML

```yaml
nodeLinker: isolated
minimumReleaseAge: 1440
publicHoistPattern:
  - "*eslint*"
jailBuilds: true
jailBuildPermissions:
  "@vendor/*":
    env:
      - SHARP_DIST_BASE_URL
    write:
      - ~/.cache/sharp
jailBuildExclusions:
  - "@legacy-native/*"
```

See the [settings reference](/settings/) — workspace YAML keys are listed per setting.
The jail-related keys are described in [Jailed builds](/package-manager/jailed-builds).

## Environment variables

pnpm-compatible `NPM_CONFIG_*` aliases are supported:

```sh
NPM_CONFIG_REGISTRY=https://registry.example.test aube install
NPM_CONFIG_NODE_LINKER=hoisted aube install
```

See the [settings reference](/settings/) — environment variables are listed per setting.

## CLI flags

CLI flags take precedence for the settings they expose:

```sh
aube install --node-linker=hoisted
aube install --network-concurrency=32
aube install --resolution-mode=time-based
```

See the [settings reference](/settings/) — CLI flags are listed per setting.

## Inspecting config

```sh
aube config get registry
aube config set auto-install-peers false
aube config list --json
```

Writes land in `.npmrc` only for the npm-shared surface (auth, registries,
npm-standard scalars). Everything else — aube settings, pnpm-only knobs, and
unknown keys — is stored in aube's own config. `--local` and
`--location project` write the project-scope equivalents (`<cwd>/.npmrc` and
`<cwd>/.config/aube/config.toml`).

## `package.json` — `pnpm.*` and `aube.*` namespaces

aube reads pnpm's `package.json` config keys so existing projects keep
working unchanged. Every key under `pnpm.*` is also accepted under
`aube.*` for projects that want to declare aube-native config without
piggy-backing on the pnpm namespace:

```json
{
  "aube": {
    "overrides": { "lodash": "4.17.21" },
    "catalog": { "react": "^18.0.0" },
    "supportedArchitectures": { "os": ["current", "linux"] },
    "allowBuilds": { "sharp": true },
    "patchedDependencies": { "foo@1.0.0": "patches/foo.patch" },
    "peerDependencyRules": { "ignoreMissing": ["react-native"] }
  }
}
```

Merge semantics when both namespaces are present:

- **Map-valued keys** (`overrides`, `catalog`, `catalogs`,
  `patchedDependencies`, `allowBuilds`, `allowedDeprecatedVersions`,
  `packageExtensions`, `peerDependencyRules.allowedVersions`):
  `aube.*` wins on key conflict; disjoint keys from either namespace
  merge.
- **List-valued keys** (`onlyBuiltDependencies`,
  `neverBuiltDependencies`, `ignoredOptionalDependencies`,
  `peerDependencyRules.ignoreMissing`, `peerDependencyRules.allowAny`,
  `updateConfig.ignoreDependencies`, `supportedArchitectures.{os,cpu,libc}`):
  entries from both namespaces union. `onlyBuiltDependencies` and
  `neverBuiltDependencies` are legacy build-policy inputs; new review state is
  written to `allowBuilds`.
- Top-level npm-standard keys (`overrides`, `packageExtensions`,
  `allowedDeprecatedVersions`, `updateConfig`) still take highest
  precedence, so the `aube.*` alias doesn't change existing npm /
  pnpm precedence rules — it only adds a second namespace that beats
  `pnpm.*` but loses to the top-level form.
