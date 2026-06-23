<p align="center">
  <a href="https://aube.jdx.dev">
    <img src="assets/logo.svg" alt="aube logo" width="140" height="140">
  </a>
</p>

<h1 align="center">aube</h1>

<p align="center">
  <strong>Never forget to install.</strong>
</p>

<p align="center">
  Aube installs automatically when you run a script. The tightest security defaults of any Node.js package manager - and the only one with a lifecycle-script jail. Drops into existing projects using existing lockfiles.
</p>

<p align="center">
  <strong>aube</strong> means dawn in French. Pronounced <code>/ob/</code>, like "ohb".
</p>

<p align="center">
  <strong><a href="https://aube.jdx.dev">Read the docs</a></strong>
</p>

<p align="center">
  Sponsored by <a href="https://37signals.com">37signals</a>.<br>
  <a href="https://en.dev/sponsors.html">View all sponsors</a>.
</p>

## Why Try It

<!-- BENCH_RATIOS:START -->
**[Fast installs](https://aube.jdx.dev/benchmarks).** Warm installs are about 5x faster than pnpm and about 2x faster than Bun in the current benchmarks. Repeat test commands run up to 34x faster than pnpm and up to 4x faster than Bun.
<!-- BENCH_RATIOS:END -->

**[Existing lockfiles](https://aube.jdx.dev/package-manager/lockfiles).** Reads and writes `pnpm-lock.yaml`, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock` in place.

**[Cheap repeat commands](https://aube.jdx.dev/package-manager/scripts).** `aubr test`, `aube test`, and `aube exec vitest` auto-install when dependencies are stale, then skip that work when nothing changed. `aubx` uses a local binary when one is installed, or a throwaway environment for one-off tools.

**[Less disk use](https://aube.jdx.dev/package-manager/node-modules).** A global content-addressable store lets projects share package files instead of keeping a full copy of the same dependencies in every checkout.

**[Secure defaults](https://aube.jdx.dev/security).** Out of the box, exotic transitive deps are blocked, lifecycle scripts wait for approval, trust downgrades fail at resolve, and brand-new releases sit in a 24h cooling window. One `paranoid: true` line adds the build jail and turns the soft gates into hard fails.

## Install

The recommended path is mise:

```sh
mise use -g aube
```

aube switches [Node.js versions](https://aube.jdx.dev/package-manager/node-runtime)
itself: if a project pins Node through `package.json`
(`devEngines.runtime`), `.nvmrc`, or `.node-version`, every script and
binary run through aube gets that version — no shims or shell hooks.
When a pinned version is missing, aube delegates the install to mise if
you have it (one shared Node store on disk) and downloads from
nodejs.org otherwise.

Check that it is on your `PATH`:

```sh
aube --version
```

Inside a project, you can also pin aube with mise:

```sh
mise use aube
```

aube is also published on npm:

```sh
npm install -g --ignore-scripts=false @endevco/aube
npx --ignore-scripts=false @endevco/aube --version
```

The npm package uses an install script to fetch native binaries for
performance. The npm commands above include the flag that keeps that
working even if your npm config disables scripts. We still recommend mise
for the smoothest install and runtime management path.

Homebrew installs come from the jdx tap:

```sh
brew install jdx/tap/aube
```

See [other install methods](https://aube.jdx.dev/installation).

## First Run

Run aube in an existing Node.js project:

```sh
aubr test
```

`aubr` is shorthand for `aube run`. Before it starts the script, aube checks
whether `node_modules` is fresh for the current `package.json` and lockfile.
If dependencies are missing or stale, it installs them first; otherwise it
goes straight to the script.

You usually do not need a separate `aube install` in day-to-day work. Run the
script or binary you actually wanted:

```sh
aubr build
aube test
aube exec vitest
aubx cowsay hi
```

Use `aube install` when the install itself is the task: first local setup
without running a script, updating a lockfile, Docker layers, production-only
installs, or CI flows.

If the project already has a supported lockfile, aube reads it and writes updates back to the same file. That makes it easy to try aube locally without forcing the rest of the team to switch package managers first.

For a new project with no lockfile, aube creates `aube-lock.yaml`.

## Daily Commands

```sh
aube add react            # add a dependency
aube add -D vitest        # add a dev dependency
aube remove react         # remove a dependency
aube update               # update dependencies within package.json ranges
aubr build                # run a package.json script, auto-installing first if needed
aube test                 # run the test script, auto-installing first if needed
aube exec vitest          # run a local binary, auto-installing first if needed
aubx cowsay hi            # run a local bin, or fetch one in a throwaway environment
aube install              # install only, for setup or lockfile/install modes
aube ci                   # clean, frozen install for CI
```

You can also run scripts directly:

```sh
aube dev
aube build
aube lint
```

If the script exists in `package.json`, aube treats that as `aube run <script>`.

### Shortcuts: `aubr` and `aubx`

`aubr` and `aubx` are multicall shims for `aube run` and `aube dlx`. They
share a binary with `aube` and dispatch purely on `argv[0]`, so every flag
that works on the full command also works on the shim:

```sh
aubr build            # aube run build
aubx cowsay hi        # aube dlx cowsay hi
```

`aubr <name>` runs a package script first, then falls back to a matching
local binary. `aubx <name>` prefers an installed local binary before fetching
into a throwaway environment; pass `-p` / `--package` to force the package
that should be installed for a one-off command.

The release archives ship all three binaries side by side; no extra
setup is needed when you install aube via mise or the tarball.

## CI

Use `aube ci` when the lockfile must be treated as the source of truth:

```sh
aube ci
```

It removes `node_modules`, verifies the lockfile is fresh for the current `package.json`, then installs.

For Docker layers or workflows where you only want to update the lockfile:

```sh
aube install --lockfile-only
```

For production-only installs:

```sh
aube install --prod
```

## Workspaces

aube supports workspace projects and the `workspace:` protocol.

```sh
aube install -r
aube run test -r
aube add zod --filter @acme/api
```

If a project already uses `pnpm-workspace.yaml`, aube can read and write it. New aube-first workspaces can use `aube-workspace.yaml`.

## Lockfile Compatibility

| File | Reads | Writes in place |
| --- | --- | --- |
| `aube-lock.yaml` | yes | yes |
| `pnpm-lock.yaml` v9 (written by pnpm 9–11) | yes | yes |
| `package-lock.json` v2/v3 | yes | yes |
| `npm-shrinkwrap.json` | yes | yes |
| `yarn.lock` (v1 classic + v2+ berry) | yes | yes |
| `bun.lock` | yes | yes |

aube is not compatible with every historical lockfile shape. Older pnpm v5/v6 lockfiles should be upgraded with pnpm before switching. Yarn PnP projects need to move to a `node_modules` linker first — aube writes `node_modules`, not `.pnp.cjs`.

When more than one lockfile exists, prefer keeping one canonical lockfile for the project so teammates and CI do not fight over dependency state.

## Dependency Scripts

aube skips dependency lifecycle scripts by default. That protects installs from unexpected build steps in transitive packages.

To allow packages that need build scripts:

```sh
aube approve-builds
```

You can inspect packages whose scripts were skipped:

```sh
aube ignored-builds
```

For approved packages, `jailBuilds: true` runs lifecycle scripts with a scrubbed env and temporary `HOME`. It defaults to `false` today and is planned to default to `true` in the next major version. Use package globs in `jailBuildPermissions` or `jailBuildExclusions` for packages that need specific env vars, paths, network, or a full opt-out.

## Package Layout

aube uses an isolated `node_modules` layout. Packages are linked through `node_modules/.aube/`, and package files are stored once in `$XDG_DATA_HOME/aube/store/` (defaulting to `~/.local/share/aube/store/`).

That means:

- several projects with similar dependencies share package files and use less disk space;
- dependencies stay isolated, so phantom dependencies are harder to rely on accidentally;
- repeated installs can reuse package files already on disk.

## Commands You May Recognize

aube supports the common package-manager surface:

```sh
aube list
aube why react
aube outdated
aube audit
aube pack
aube publish
aube link
aube unlink
aube config get registry
aube store path
aube store prune
```

aube also matches pnpm 11's runtime surface: `aube runtime set node <version>` pins a Node version in `devEngines.runtime` and the lockfile, and `aube runtime list` shows what a project resolves to. Some pnpm commands are intentionally out of scope: `setup` and `self-update` belong in tools like mise, and registry account helpers such as `whoami`, `token`, `owner`, `search`, `pkg`, and `set-script` are compatibility stubs that point you to the npm command instead.

## Learn More

- [Documentation](https://aube.jdx.dev)
- [Benchmarks](https://aube.jdx.dev/benchmarks)
- [Lockfile compatibility](https://aube.jdx.dev/package-manager/lockfiles)
- [Run scripts and binaries](https://aube.jdx.dev/package-manager/scripts)

## CI

<p>
  <a href="https://namespace.so">
    <img src="assets/namespace-logo.svg" alt="Namespace" width="64">
  </a>
</p>

Thanks to [Namespace](https://namespace.so) for providing CI for aube.

## Star History

<a href="https://www.star-history.com/?repos=jdx%2Faube&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=jdx/aube&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=jdx/aube&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=jdx/aube&type=date&legend=top-left" />
 </picture>
</a>

## Contributors

[![Contributors](https://contrib.rocks/image?repo=jdx/aube)](https://github.com/jdx/aube/graphs/contributors)

<p>
  <a href="https://jdx.dev">
    <img src="https://github.com/jdx.png?size=96" alt="jdx" width="42" height="42" align="left">
  </a>
  Built by <a href="https://jdx.dev">jdx</a>.
</p>

<br clear="left">

## License

MIT
