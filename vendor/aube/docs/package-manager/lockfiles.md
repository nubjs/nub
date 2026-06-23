# Lockfiles

aube's default lockfile for new projects is `aube-lock.yaml`. For projects
that already have a different supported lockfile, aube keeps reading and
writing that file in place.

## Supported lockfile formats

aube reads *and writes* all of the following formats:

- `aube-lock.yaml` (default for new projects)
- `pnpm-lock.yaml` v9
- `package-lock.json`
- `npm-shrinkwrap.json`
- `yarn.lock` — both v1 classic and v2+ berry
- `bun.lock`

## Write behavior

On install (and on `add`, `remove`, `update`, `dedupe`), aube picks the
lockfile to write from whichever supported file already exists in the project
directory. Precedence is: `aube-lock.yaml` → `pnpm-lock.yaml` → `bun.lock` →
`yarn.lock` → `npm-shrinkwrap.json` → `package-lock.json`. When none of those
exist yet, aube writes `aube-lock.yaml`.

The practical upshot:

- A pnpm project keeps getting `pnpm-lock.yaml` updates.
- An npm project keeps getting `package-lock.json` updates.
- Only `aube import` (or manually removing the existing lockfile) switches a
  project onto `aube-lock.yaml`.

Keep the original lockfile while its package manager is still part of the
workflow — aube and the original package manager both read from and write to
the same file without conflicting.

## Frozen installs

```sh
aube install --frozen-lockfile
aube ci
```

Frozen mode fails when the lockfile no longer matches the manifest.

## Prefer frozen installs

```sh
aube install --prefer-frozen-lockfile
```

This is the local default. aube uses the lockfile if it is fresh and
re-resolves when the manifest changed.

## Lockfile-only updates

```sh
aube install --lockfile-only
```

Use this when CI or automation needs to update dependency metadata without
touching `node_modules`.

## Runtime pins

When `package.json` pins Node through `devEngines.runtime`, the
resolved exact version (plus per-platform download URLs and SHA-256
checksums) is recorded in the lockfile using pnpm 10.14+'s
`node@runtime:` entry shape — a synthetic dep on the root importer and
a `packages:` entry with a `variations` resolution. aube and pnpm read
each other's pins. Formats without a runtime shape (npm / yarn / bun)
skip the pin and re-resolve the range at run time. See
[Node runtime switching](/package-manager/node-runtime).

## Branch lockfiles

When `gitBranchLockfile` is enabled, aube writes branch-specific lockfile names
such as `aube-lock.<branch>.yaml`. Use this for long-running branches that
produce frequent lockfile conflicts.
