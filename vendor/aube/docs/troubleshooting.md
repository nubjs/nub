# Troubleshooting

## Try disabling the global virtual store first

If an install or build is behaving oddly, turn the global virtual
store off for the project before digging further:

```sh
aube config set enableGlobalVirtualStore false --location project
```

Symptoms that usually point here:

- `Symlink ... is invalid, it points out of the filesystem root`
- `ENOENT: no such file or directory` for a module that clearly exists
  under `node_modules/.aube/`
- `Cannot find module '<pkg>'` from Next.js / Turbopack, Vite,
  VitePress, Nuxt, or Parcel during dev or build
- Plugin config discovery (PostCSS, Tailwind, Vite) silently misses a
  config file that lives at the project root
- `ERR_INVALID_PACKAGE_TARGET` or exports-resolution failures for a
  package that resolves fine under pnpm/npm

See [Global virtual store](/package-manager/global-virtual-store)
for what this changes.

## A package is missing from `node_modules`

Run:

```sh
aube install
aube list --depth Infinity <package>
```

In the isolated linker, only direct dependencies are symlinked at the top level.
Transitive dependencies live under the packages that declared them. If your app
imports a package directly, add it to your own `package.json`.

## A package config file cannot resolve its own dependency

Some tools discover dependency-owned config files by walking
`node_modules/<pkg>/...` and then evaluating the config file at that symlink
path. In an isolated linker, the package's own dependencies are linked next to
the package's real virtual-store path under `node_modules/.aube/`, not next to
the top-level symlink.

That means a package config file can fail to `require()` a dependency that the
same package declares, even though the package was installed correctly. Some
native or framework toolchains swallow the config-load error and fail later
with generated code or build output that looks unrelated.

This usually means the loader is not symlink-aware; it does not mean aube
linked the package incorrectly.

If you maintain the loader, resolve the config file's real path before
evaluating it:

```js
const configPath = fs.realpathSync(discoveredConfigPath);
```

When reporting this upstream, ask the tool maintainer to load
dependency-owned config files from their real path, or to create the `require`
function from the real config path, before evaluating the file. Include a small
repro that shows the config succeeds after `fs.realpathSync(...)`.

If the toolchain cannot be changed, use hoisted mode for that project:

```sh
aube install --node-linker=hoisted
```

Hoisted mode gives legacy tooling an npm-style tree where more packages are
visible from top-level `node_modules` paths. Treat it as a compatibility
workaround for loaders that are not symlink-aware.

## A dependency build script did not run

Dependency lifecycle scripts follow the pnpm v11 build approval model. Inspect the
pending list:

```sh
aube ignored-builds
aube approve-builds
aube rebuild
```

Use `--dangerously-allow-all-builds` only for a local diagnostic run. Do not use
it as a permanent CI default.

## A jailed dependency build needs more access

If `jailBuilds` is enabled and an approved dependency build fails only inside
the jail, keep the jail on and grant the narrow permission the package needs:

```yaml
jailBuildPermissions:
  "@vendor/*":
    env:
      - SOME_BUILD_FLAG
    write:
      - ~/.cache/vendor
```

If the package cannot run in the jail yet, disable the jail for that package
glob without bypassing build approval:

```yaml
jailBuildExclusions:
  - "@legacy-native/*"
```

See [Jailed builds](/package-manager/jailed-builds) for the default profile and
supported permission keys.

## A lockfile format is unsupported

aube reads and writes the current supported lockfile formats listed on the
[lockfiles page](/package-manager/lockfiles). Older pnpm v5/v6 lockfiles
should be upgraded with pnpm first. Yarn PnP projects need to switch to
`nodeLinker: node-modules` before using aube — aube writes a regular
`node_modules` tree, not `.pnp.cjs`.

If a project has multiple lockfiles, keep one canonical lockfile before
rolling aube into CI.

## The lockfile is out of sync

For a strict CI install:

```sh
aube ci
```

For a local repair:

```sh
aube install --fix-lockfile
```

For a full re-resolve:

```sh
aube install --no-frozen-lockfile
```

## `aube run` installed unexpectedly

Script commands check install freshness first. If `package.json` or the
lockfile changed, aube installs before running the script:

```sh
aube run build
aube test
```

If you want to skip that check for one command:

```sh
aube run --no-install build
aube test --no-install
```

## The registry is unavailable

If the cache already contains the required metadata and tarballs:

```sh
aube install --offline
```

If you want to prefer cached metadata but allow network misses:

```sh
aube install --prefer-offline
```

## A private registry or scope is not authenticated

aube reads npm-compatible registry configuration from `.npmrc` and environment
variables.

```ini
@myorg:registry=https://registry.example.test
//registry.example.test/:_authToken=${NPM_TOKEN}
```

Check what aube sees:

```sh
aube config get registry
aube config list --json
```

For publishing and login flows, see [Registry and auth](/package-manager/registry-auth).

## A workspace filter matches nothing

Check the workspace package names and paths:

```sh
aube list -r --depth 0
```

Then retry with an exact name, glob, path selector, or graph selector:

```sh
aube -F '@scope/*' run build
aube -F './packages/api' test
aube -F 'api...' run build
```

## You need to fall back temporarily

Keep the existing lockfile while evaluating aube. Since aube writes supported
lockfiles in place, the original package manager can keep using the same file
during rollout. If aube hits a bug in a project, fall back for that job,
keep the failing command and lockfile handy, and open a thread in
[GitHub Discussions](https://github.com/jdx/aube/discussions) with the
exact command and package manager versions.
