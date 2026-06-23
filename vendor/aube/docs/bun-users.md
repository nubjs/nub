# For bun users

aube can install directly from Bun lockfiles. You do not need to delete
`bun.lock` or remove `node_modules` before trying aube.

## Try the Bun lockfile

```sh
aube install
```

Run this once when you specifically want to verify that aube can read and
write the existing Bun lockfile. For normal local work, run the command you
wanted instead: `aubr build`, `aube test`, and `aube exec <bin>` auto-install
first when dependencies are stale; `aubx <pkg>` handles one-off tools.

aube reads and updates the text-format `bun.lock` at `lockfileVersion: 1`
in place and installs packages into `node_modules/.aube/`.

aube does not read Bun's older binary `bun.lockb` format. Projects still
on `bun.lockb` can generate the text lockfile with a modern Bun once:

```sh
bun install --save-text-lockfile
```

Commit the resulting `bun.lock` and drop `bun.lockb` before switching to
aube.

## Keep Bun working during rollout

Commit the updated `bun.lock` so both Bun and aube users see the same
resolved versions. You do not need `aube import` for a normal rollout;
`aube install` keeps `bun.lock` as the shared source of truth.

Use `aube import` only if the team intentionally wants to convert the
project to `aube-lock.yaml`. After import succeeds, remove `bun.lock` so
future installs keep writing `aube-lock.yaml`.

## Differences from Bun

- aube keeps package files in a global content-addressable store.
- aube produces an isolated symlink layout under `node_modules/.aube/`
  rather than Bun's hoisted tree.
- aube does not manage a JavaScript runtime. Use
  [mise](https://mise.jdx.dev) (`mise use node@22`) if you need a Node
  version alongside or in place of Bun.
- Dependency lifecycle scripts (`preinstall`, `install`, `postinstall`)
  are gated by an allowlist. aube reads Bun's top-level
  `trustedDependencies` array in addition to pnpm's
  `pnpm.allowBuilds` / `pnpm.onlyBuiltDependencies`, so an existing
  Bun manifest keeps running its install scripts without edits.
  Install writes unreviewed packages into `aube-workspace.yaml`'s
  `allowBuilds` with `false` (or `pnpm-workspace.yaml` if one already
  exists); `aube approve-builds` flips reviewed entries to `true`. Approved dependency builds can also run in a
  [jail](/package-manager/jailed-builds) with package-specific env, path,
  and network permissions.

Reference: [bun install](https://bun.sh/docs/cli/install)
