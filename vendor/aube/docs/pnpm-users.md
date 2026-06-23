# For pnpm users

aube should be a drop-in replacement for pnpm projects. There are only
minor differences in behavior.

## Behavior differences

A handful of commands behave differently in a way that's worth knowing
before you ship an aube-based workflow:

| Command | Difference |
| --- | --- |
| `aube run <script>` | Checks install staleness and **auto-installs** before running. `pnpm run` does not. |
| `aube test` | **Auto-installs** first, then runs the `test` script — equivalent to `pnpm install-test` in one command. |
| `aube exec <bin>` | **Auto-installs** on stale state before running. `pnpm exec` does not install. |
| `aube install` (new project) | Creates `aube-lock.yaml` if there's no existing lockfile. pnpm creates `pnpm-lock.yaml`. In an existing pnpm project, aube reads and writes `pnpm-lock.yaml` in place. |

Everything else — `add`, `remove`, `update`, `dlx`, `list`, `why`, `pack`,
`publish`, `approve-builds` — matches pnpm's behavior.

## Command map

Do not translate every `pnpm install && pnpm run ...` habit literally.
`aubr <script>`, `aube test`, and `aube exec <bin>` check install freshness
and install first only when needed. Use `aubx <pkg>` for one-off tools.

| pnpm | aube | Notes |
| --- | --- | --- |
| `pnpm install` | `aube install` | Reads and updates an existing `pnpm-lock.yaml` in place. Only new projects (no supported lockfile on disk yet) default to `aube-lock.yaml`. |
| `pnpm add react` | `aube add react` | Supports dependency sections, exact pins, peer deps, workspace root adds, and globals. |
| `pnpm remove react` | `aube remove react` | Removes from the manifest and relinks. |
| `pnpm update` | `aube update` | Updates all or named direct dependencies. |
| `pnpm run build` | `aube run build` | Runs scripts with an auto-install staleness check first. |
| `pnpm test` | `aube test` | Shortcut for the `test` script; aube auto-installs first (equivalent to `pnpm install-test`). |
| `pnpm exec vitest` | `aube exec vitest` | Runs local binaries with project `node_modules/.bin` on `PATH`. |
| `pnpm dlx cowsay hi` | `aubx cowsay hi` | Installs into a throwaway environment and runs the binary. |
| `pnpm list` | `aube list` | Supports depth, JSON, parseable, long, prod/dev, and global modes. |
| `pnpm why debug` | `aube why debug` | Shows reverse dependency paths. |
| `pnpm pack` | `aube pack` | Creates a publishable tarball with npm-style file selection. |
| `pnpm publish` | `aube publish` | Publishes to the configured registry; workspace fanout is available via filters. |
| `pnpm approve-builds` | `aube approve-builds` | Records packages allowed to run lifecycle build scripts. |

## Files and directories

| Concept | pnpm | aube |
| --- | --- | --- |
| Default lockfile (new projects) | `pnpm-lock.yaml` | `aube-lock.yaml` |
| Virtual store | `node_modules/.pnpm/` | `node_modules/.aube/` |
| Global content-addressable store | `~/.pnpm-store/` | `$XDG_DATA_HOME/aube/store/v1/` (defaulting to `~/.local/share/aube/store/v1/`). Run `aube store path` to see the resolved location. |
| Install state | `node_modules/.modules.yaml` | `node_modules/.aube-state` |
| Workspace manifest | `pnpm-workspace.yaml` | `aube-workspace.yaml` |

aube reads pnpm v11 YAML files for compatibility. `aube-lock.yaml` and
`aube-workspace.yaml` use pnpm-compatible shapes today but are the long-term
contract and may diverge over time.

aube never touches pnpm's `node_modules/.pnpm/` or `~/.pnpm-store/`. The two
virtual stores can coexist under `node_modules`. For the lockfile and
workspace YAML, aube reads and writes whichever file already exists on disk
— `pnpm-lock.yaml` keeps getting updates in place, and an existing
`pnpm-workspace.yaml` is mutated in place (aube does not spawn a parallel
`aube-workspace.yaml` alongside it). When neither workspace yaml exists,
aube creates `aube-workspace.yaml`.

## What's different

- **Separate install locations.** Installs go into `node_modules/.aube/` and
  `$XDG_DATA_HOME/aube/store/` (defaulting to `~/.local/share/aube/store/`)
  instead of pnpm's `.pnpm/` and `~/.pnpm-store/`. If a project already has
  a pnpm-built `node_modules`, aube installs alongside — the two virtual
  stores live side by side.
- **Default YAML filenames for new projects.** A project with no lockfile
  yet gets `aube-lock.yaml`. If it already has `pnpm-lock.yaml` (or any
  other supported lockfile — `package-lock.json`, `npm-shrinkwrap.json`,
  `yarn.lock`, `bun.lock`), aube reads and writes that file in place.
  Install auto-adds unreviewed dependency builds to the workspace yaml's
  `allowBuilds` map with `false`; `aube approve-builds` flips reviewed
  entries to `true` (matching pnpm v11). When no workspace yaml exists,
  aube creates `aube-workspace.yaml`; an existing `pnpm-workspace.yaml`
  is mutated in place.
- **Build approvals.** Dependency lifecycle script approval follows pnpm
  v11's allowlist model. Use explicit policy fields in `package.json` or
  `aube-workspace.yaml` to opt in. aube can also run approved dependency
  builds in a [jail](/package-manager/jailed-builds) with package glob
  permissions for env, path, and network exceptions.
- **Speed.** See the [benchmarks](/benchmarks).

## Supported pnpm lockfile versions

aube reads and writes `pnpm-lock.yaml` at **lockfile version 9** — the
format shipped by pnpm v9 and later. Older pnpm lockfiles (versions 5, 6,
7, and 8, used by pnpm 7.x and 8.x) are not supported and will cause aube
to refuse the install.

To upgrade an older pnpm lockfile, run a modern pnpm once to convert it:

```sh
npx pnpm@latest install
```

That rewrites `pnpm-lock.yaml` at v9. Commit the result, then switch to
`aube install`.

## Node runtime management

aube matches pnpm 11's runtime surface: `devEngines.runtime` in
package.json drives [Node version switching](/package-manager/node-runtime),
and `aube runtime set node <version>` writes the pin the same way
`pnpm runtime set` does. The resolved version is recorded in the
lockfile using pnpm's exact `node@runtime:` shape, so aube and pnpm
10.14+ can read each other's pins. The deprecated `pnpm env use` has no
aube equivalent — use `aube runtime set`.

Unlike pnpm, aube also reads `.nvmrc` / `.node-version`, and it
delegates runtime installs to [mise](https://mise.jdx.dev) when mise is
installed so you don't keep two copies of Node (see the
[`runtimeInstaller`](/settings/#runtimeinstaller) setting).

aube also manages its own version the way pnpm does
(`managePackageManagerVersions`, on by default): a
`packageManager: "aube@<version>"` or `devEngines.packageManager` pin
re-execs the pinned aube automatically. See
[Node runtime switching](/package-manager/node-runtime#pinning-aube-itself).

## Out of scope

`pnpm setup` and `pnpm self-update` are intentionally not implemented —
aube is best installed and updated through [mise](https://mise.jdx.dev):

```sh
mise use -g aube
```
