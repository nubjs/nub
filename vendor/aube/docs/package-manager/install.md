# Install dependencies

`aube install` installs the dependencies declared in `package.json` and the
workspace manifests.

```sh
aube install
```

Most local work does not need a separate install command. `aubr <script>`,
`aube test`, and `aube exec <bin>` check install freshness first. If
`package.json` or the lockfile changed, aube installs before running the
script or binary. For one-off tools, `aubx <pkg>` installs into a throwaway
environment and runs the binary.

Use `aube install` when the install itself is the task: first local setup
without running a script, lockfile updates, Docker layers, production-only
installs, offline installs, linker experiments, and CI flows.

## Lockfile modes

| Mode | Command | Use it when |
| --- | --- | --- |
| Prefer frozen | `aube install --prefer-frozen-lockfile` | Local default: reuse a fresh lockfile, re-resolve on drift. |
| Frozen | `aube install --frozen-lockfile` | CI should fail if `package.json` and lockfile disagree. |
| No frozen | `aube install --no-frozen-lockfile` | You want a full re-resolve. |
| Fix lockfile | `aube install --fix-lockfile` | You want to repair only entries that drifted. |
| Lockfile only | `aube install --lockfile-only` | You want to update the lockfile without linking `node_modules`. |

`aube ci` is the strict CI shortcut: it deletes `node_modules` and then runs a
frozen install.

## Dependency filters

```sh
aube install --prod
aube install --no-optional
```

`--prod` skips `devDependencies`. `--no-optional` skips optional dependencies.

## Network modes

```sh
aube install --prefer-offline
aube install --offline
```

`--prefer-offline` uses cached metadata when available and only hits the
network on a miss. `--offline` forbids network access entirely.

## Linker modes

```sh
aube install --node-linker=isolated
aube install --node-linker=hoisted
```

`isolated` is the pnpm-compatible default. It writes a strict symlink tree under
`node_modules/.aube/`. `hoisted` writes a flatter npm-style tree for projects
that need legacy `node_modules` assumptions. `pnp` is not supported.

## Store import methods

```sh
aube install --package-import-method=auto
aube install --package-import-method=hardlink
aube install --package-import-method=copy
aube install --package-import-method=clone-or-copy
```

`auto` probes the filesystem and chooses the fastest available strategy:
reflink, hardlink, then copy.

## References

- [pnpm install](https://pnpm.io/cli/install)
- [Bun install](https://bun.com/docs/pm/cli/install)
- [npm install](https://docs.npmjs.com/cli/v10/commands/npm-install)
- [Yarn classic install](https://classic.yarnpkg.com/lang/en/docs/cli/install/)
