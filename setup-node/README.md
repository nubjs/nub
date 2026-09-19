# setup-node

[`actions/setup-node`](https://github.com/actions/setup-node), with [nub](https://github.com/nubjs/nub) installed. Swap the `uses:` line; the inputs and outputs are setup-node's.

```yaml
- uses: actions/checkout@v4     # first: the action reads the project's pin files
- uses: nubjs/nub/setup-node@v0
- run: npm ci
- run: npm test
```

Two defaults differ from `actions/setup-node`:

- **With no `node-version` input, the project's own pin wins.** Nub reads it: `package.json#devEngines.runtime`, `.node-version`, `.nvmrc`, `.tool-versions`, then `package.json#engines.node`. The runner's Node stays when it satisfies the pin, and when there is no pin. A pin the runner's Node does not satisfy is downloaded and put first on PATH for the rest of the job, with a notice in the job log naming the pin file; a pin that cannot be provisioned fails the action. `actions/setup-node` leaves the runner's Node in place unless given a version; `provision-node: false` restores that.
- **The package manager the project pins is the one that runs.** `npm`, `npx`, `pnpm`, `pnpx`, `yarn` and `yarnpkg` in later steps resolve to Nub's shims. In a project with `packageManager` or `devEngines.packageManager`, they run that version, provisioned on demand, which is the job of `corepack enable` and `pnpm/action-setup`. In an unpinned project they fall through to the runner's own tool. The shims never route a command into Nub's own installer: `npm ci` runs npm. `shim: false` leaves the runner's tools alone.

Everything else matches. An explicit `node-version` or `node-version-file` is provisioned and fronted on PATH for the rest of the job, so bare `node`, `npm` and `npx` are that version. `cache: npm` (any value, or none) caches Nub's store, keyed on the lockfile. `registry-url` and `scope` write the `.npmrc`, keeping the other lines of an existing one. The `tsc` and `eslint` problem matchers are registered. `check-latest`, `architecture`, `mirror` and `mirror-token` are accepted and ignored.

`nub` is on PATH afterwards. `nub install` installs the lockfile the project already has (`package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`) from the cached store; `nub run <script>` and `nub <file.ts>` run on the resolved Node.

## Migration

```yaml
# before
- uses: actions/setup-node@v4
  with:
    node-version: 20
    cache: npm

# after
- uses: nubjs/nub/setup-node@v0
  with:
    node-version: 20      # provisioned and fronted, as before
    cache: npm            # accepted; Nub's store is cached instead of npm's
```

A workflow that omits `node-version` keeps running on the runner's Node when the project has no pin or the pin allows it. When the pin excludes the runner's Node, the job now runs on the pinned version and the log says so; add `provision-node: false` to keep the runner's.

## What the action does

1. Installs `nub` from the release archive for the runner's platform, verified by Nub's own installer against the release's sha256 sidecar: about a second on Linux and macOS, a few seconds on Windows. If the archive path fails, `npm install -g @nubjs/nub`.
2. Restores Nub's store and provisioned Node toolchains from the cache. The key is `nub-<os>-<arch>-<prefix>-<hash(pin files)>-<hash(lockfile)>`, with a restore-keys ladder so a changed lockfile still gets a warm store. `cache: false` or `package-manager-cache: false` disables it.
3. Writes a temporary user-level `.npmrc` for `registry-url`, wired to `NODE_AUTH_TOKEN` through `NPM_CONFIG_USERCONFIG`.
4. Resolves Node: the input, else the project's pin. Nothing is downloaded when a Node on disk satisfies it.
5. Puts the package-manager shims first on PATH.

## Inputs

| Input | Default | Behavior |
|---|---|---|
| `node-version` | the project's pin | Provision this Node and front it on PATH for the rest of the job. Any range npm understands (`20`, `22.14.0`, `^20.10`) or an nvm alias (`lts/*`, `lts/iron`, `node`). |
| `node-version-file` | — | Read the version from this file (`.node-version`, `.nvmrc`, `package.json`) and treat it as `node-version`. The file must exist. |
| `provision-node` | `true` | `false` leaves Node alone: nothing installed, nothing fronted, `node-version` output empty. For a job where `actions/setup-node` already ran. |
| `shim` | `true` | `false` leaves `npm`/`pnpm`/`yarn` as the runner ships them. |
| `cache` | auto | Caching of Nub's store. A boolean; a package-manager name is accepted and treated as `true`. Unset: on when a lockfile or a `packageManager`/`devEngines` field exists. |
| `package-manager-cache` | `true` | `false` disables the automatic caching. An explicit `cache` still wins. |
| `cache-dependency-path` | the project's lockfile | Lockfile path(s) whose hash keys the cache. Globs and newline-delimited lists. |
| `cache-key-prefix` | — | A segment in the cache key, to scope or bust caches independently of the lockfile. |
| `working-directory` | checkout root | Where the pin files and lockfile live, for a project in a subdirectory. |
| `registry-url` | — | Registry to configure auth for. |
| `scope` | repo owner | Scope for a scoped registry (GitHub Packages). |
| `always-auth` | `false` | Write `always-auth=true` into the `.npmrc`; an existing `always-auth` line is replaced either way. |
| `token` | `github.token` | GitHub API rate-limit relief when resolving the `nub-version` range. |
| `nub-version` | `latest` | Version of nub to install: any range npm understands. |

Accepted and ignored: `check-latest`, `architecture`, `mirror`, `mirror-token`.

## Outputs

| Output | Description |
|---|---|
| `node-version` | The Node version on PATH after the action, as `node --version` prints it (`v20.19.0`) and as setup-node reports it. Empty with `provision-node: false`. |
| `nub-version` | The installed nub version (`v<semver>`). |
| `cache-hit` | `true` on an exact cache key hit, `false` on a restore-keys partial hit, empty on a miss, as `actions/cache` reports it. |
| `caching-enabled` | Whether caching is active for this run (`true`/`false`). |

## Registry auth

```yaml
- uses: nubjs/nub/setup-node@v0
  with:
    registry-url: https://npm.pkg.github.com
    scope: "@my-org"
- run: npm ci
  env:
    NODE_AUTH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

The `.npmrc` is written to `$RUNNER_TEMP/.npmrc` and pointed at through `NPM_CONFIG_USERCONFIG`; npm, pnpm, yarn and nub all read it.

## Versioning

- `nubjs/nub/setup-node@v0`: floating, moves to each stable Nub `v0.x` release. A `v0.x` release can carry a breaking change to the action.
- `nubjs/nub/setup-node@v0.x.y`: the action as of that Nub release, never moves.
- `nubjs/nub/setup-node@<commit>`: a commit on `main`, never moves.
