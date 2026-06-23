# Getting Started

aube is a fast Node.js package manager that can run in existing projects
without changing the lockfile format first. If your project already has
`pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, or
`bun.lock`, aube reads it and writes updates back to the same file.

## Install

See the [installation guide](/installation).

## Use it

```sh
# run a script from package.json
aubr build

# add a dependency
aube add lodash

# install + run the test script (equivalent to `pnpm install-test`)
aube test
```

::: tip Just run the command you wanted
`aubr build`, `aube test`, and `aube exec vitest` all check install freshness
before running. If `package.json` or the lockfile changed, aube installs first;
otherwise it skips straight to the script or binary. For one-off tools, use
`aubx cowsay hi` instead of running an install step yourself.

You rarely need a separate `aube install` step in day-to-day work. Use it when
the install itself is the task: first local setup without running a script,
lockfile updates, Docker layers, production-only installs, or CI flows.
:::

::: tip Shortcut binaries: `aubr` and `aubx`
`aubr` is shorthand for `aube run`, and `aubx` is shorthand for
`aube dlx`. They ship alongside `aube` in every release, so you can
write `aubr build` instead of `aube run build`, or `aubx cowsay hi`
instead of `aube dlx cowsay hi`.
:::

## Learn the package-manager flow

- [For pnpm users](/pnpm-users) maps the common pnpm commands and files
  to aube.
- [Install dependencies](/package-manager/install) covers lockfile modes,
  production installs, offline installs, and linker modes.
- [Manage dependencies](/package-manager/dependencies) covers adding,
  removing, updating, deduping, and pruning dependencies.
- [Workspaces](/package-manager/workspaces) covers filters, recursive runs,
  catalogs, workspace dependencies, and deploys.
- [Lifecycle scripts](/package-manager/lifecycle-scripts) explains the
  dependency script allowlist model.
- [Jailed builds](/package-manager/jailed-builds) explains how to run
  approved dependency scripts with restricted environment, filesystem, and
  network access.
