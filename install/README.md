# install

Install the project's dependencies from its lockfile, whichever package manager wrote it. One line replaces the step that runs `npm ci`, `pnpm install --frozen-lockfile`, `yarn install --immutable` or `bun install --frozen-lockfile`: [Nub](https://github.com/nubjs/nub) reads the lockfile unchanged, installs it into `node_modules`, and caches its store across runs.

```yaml
- uses: actions/checkout@v4
- uses: actions/setup-node@v4
  with:
    node-version: 22
    cache: npm
- uses: nubjs/nub/install@v0     # was: - run: npm ci
- run: npm test
```

Nothing else changes. Node still comes from setup-node, and `node`, `npm`, and `npx` in later steps are the binaries they were. The `cache: npm` line can stay: it restores npm's own cache directory, which Nub does not read, and setup-node only warns when it finds nothing to save.

Measured on a GitHub-hosted `ubuntu-latest` runner with a 1,168-package `package-lock.json`: `npm ci` takes about 30 s per job when setup-node's cache hits (9 s to restore, 21 s to install) and about 80 s when it misses. This action installs the same lockfile in about 9 s, with no cache step. The action's own overhead is about 3.5 s on `ubuntu-latest`, 2.5 s on `macos-latest` and 10 s on `windows-latest`: installing `nub` from its release archive, and restoring the store cache.

## pnpm, yarn, bun

The same line installs a `pnpm-lock.yaml`, `yarn.lock` or `bun.lock`. With `shim: true`, Nub also puts its package-manager shims first on PATH: `pnpm`, `yarn` and `npm` in later steps run the version the project pins in `packageManager` or `devEngines`, provisioned on demand, the job corepack does. A project with a pin can then drop `pnpm/action-setup` or `corepack enable`:

```yaml
- uses: actions/checkout@v4
- uses: actions/setup-node@v4
  with:
    node-version: 22
- uses: nubjs/nub/install@v0     # was: pnpm/action-setup, then - run: pnpm install --frozen-lockfile
  with:
    shim: true
- run: pnpm test              # the pinned pnpm, provisioned by the shim
```

Setup-node's `cache: pnpm` needs `pnpm` on PATH before setup-node runs, so it goes when `pnpm/action-setup` goes. A project without a `packageManager` pin keeps its own way of putting `pnpm` on PATH.

## What runs

1. [`nubjs/setup-nub`](https://github.com/nubjs/setup-nub), pinned by commit, with `provision-node: false`. It installs `nub` from the release archive for the runner's platform and caches Nub's store, keyed on the lockfile. The Node on PATH is left alone. With `shim: true` it also links the shims.
2. `nub install --frozen-lockfile` in `working-directory`, plus any `args`. As with `npm ci`, the install fails when the lockfile is out of date with `package.json`; `frozen-lockfile: false` lets it update the lockfile instead.

Dependency build scripts run for packages on Nub's [default trust list](https://nubjs.com/docs/install#default-trust-floor) and for packages the project names in `allowScripts`, where `npm ci` runs every script. A package outside both gets an `allowScripts` entry, which npm 12 reads too.

## Inputs

| Input | Default | Behavior |
|---|---|---|
| `working-directory` | `.` | Directory holding the `package.json` and lockfile to install. |
| `frozen-lockfile` | `true` | Install exactly what the lockfile says and fail if it is out of date. `false` lets the install update the lockfile, as `npm install` does. |
| `args` | — | Extra flags for `nub install`: `--prod`, `--ignore-scripts`, `--offline`. |
| `shim` | `false` | Put Nub's package-manager shims first on PATH, so `npm`, `pnpm` and `yarn` in later steps run the pinned version, provisioned on demand. |
| `cache` | `true` | Cache Nub's store across runs, keyed on the lockfile. |
| `cache-dependency-path` | the lockfile | Path(s) whose hash keys the cache. Globs and newline-delimited lists. |
| `cache-key-prefix` | — | Prefix for the cache key, to scope or bust caches independently of the lockfile. |
| `nub-version` | `latest` | Version of nub to install: any npm semver range. |
| `token` | `github.token` | Rate-limit relief when resolving nub's version. |

## Outputs

| Output | Description |
|---|---|
| `nub-version` | The installed nub version (bare `v<semver>`). |
| `cache-hit` | `true` on an exact store-cache hit, `false` on a partial hit, empty on a miss. |

## From bahmutov/npm-install

Swap the `uses:` line. `working-directory` and `cache-key-prefix` keep their meaning; `useLockFile: false` becomes `frozen-lockfile: false`. There is no `install-command`: the command is `nub install`.

## Versioning

- `nubjs/nub/install@v0`: floating, moves to each stable Nub `v0.x` release. A `v0.x` release can carry a breaking change to the action.
- `nubjs/nub/install@v0.x.y`: the action as of that Nub release, never moves.
- `nubjs/nub/install@<commit>`: a commit on `main`, never moves.

