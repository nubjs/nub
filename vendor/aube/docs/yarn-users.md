# For yarn users

aube can install directly from both Yarn classic (v1) and Yarn berry (v2+)
lockfiles. You do not need to delete `yarn.lock` or remove `node_modules`
before trying aube.

## Yarn classic (v1)

```sh
aube install
```

Run this once when you specifically want to verify that aube can read and
write the existing Yarn lockfile. For normal local work, run the command you
wanted instead: `aubr build`, `aube test`, and `aube exec <bin>` auto-install
first when dependencies are stale; `aubx <pkg>` handles one-off tools.

aube reads and updates Yarn v1 `yarn.lock` in place and installs packages
into `node_modules/.aube/`.

Commit the updated `yarn.lock` so Yarn classic users and aube users see the
same resolved versions. You do not need `aube import` for a normal rollout;
`aube install` keeps `yarn.lock` as the shared source of truth.

Use `aube import` only if the team intentionally wants to convert the project
to `aube-lock.yaml`. After import succeeds, remove `yarn.lock` so future
installs keep writing `aube-lock.yaml`.

## Yarn berry (v2+)

```sh
aube install
```

aube reads berry's YAML-format `yarn.lock` (the one with the
`__metadata:` header, `resolution:` / `checksum:` fields, and per-block
`linkType`) and writes the same format back. Berry's `checksum:`
values are preserved verbatim so `yarn install` against the rewritten
file still validates cached tarballs.

Supported protocols: `npm:` (the common case), `patch:` for local
patch files against npm-backed packages, `workspace:`, `file:`, `link:`,
`portal:`, `exec:`, plus `git:` / `git+ssh:` /
`git+https:` / `https:` URLs for remote sources. Entries that use
unsupported protocols are skipped with a warning — aube's dependency
graph doesn't model those yet, and they round-trip better through Yarn
itself.

## Yarn PnP

aube does not support Yarn's Plug'n'Play linker. Berry projects using
`nodeLinker: pnp` (the default) need to switch to `nodeLinker:
node-modules` before using aube as the install command:

```yaml
# .yarnrc.yml
nodeLinker: node-modules
```

Once the project writes a regular `node_modules` tree, `aube install`
can drop in against the same `yarn.lock`.

## Differences from Yarn

- aube keeps package files in a global content-addressable store.
- aube uses isolated symlinks instead of a hoisted flat tree by default.
- Workspace package discovery follows `aube-workspace.yaml` (or
  `pnpm-workspace.yaml` when the project already has one).
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`) do
  not run by default. Yarn runs them for every dependency; aube runs them
  only for packages approved in `allowBuilds`; legacy
  `pnpm.onlyBuiltDependencies` entries are still honored. This follows
  the pnpm v11 model. Approved dependency builds can also run in a
  [jail](/package-manager/jailed-builds) with package-specific env, path,
  and network permissions.

References:
[Yarn classic install](https://classic.yarnpkg.com/lang/en/docs/cli/install/)
·
[Yarn berry install](https://yarnpkg.com/cli/install)
