# Manage dependencies

Use `add`, `remove`, `update`, `dedupe`, and `prune` to change a project's
dependency graph.

## Add

```sh
aube add react
aube add -D vitest
aube add -O fsevents
aube add -E typescript
aube add --save-peer react
aube add -g cowsay
```

`add` writes to the correct dependency section, updates the lockfile, fetches
packages into the store, and relinks `node_modules`.

Dependency specifiers can use npm aliases, ranges, dist-tags, workspace
protocols, JSR packages, local directories, tarballs, git URLs, and direct
tarball URLs:

```sh
aube add react@latest
aube add alias-name@npm:actual-name@^1
aube add jsr:@std/collections@^1.0.0
aube add workspace:*
aube add file:../local-package
aube add link:../linked-package
aube add https://registry.example.test/pkg/-/pkg-1.0.0.tgz
```

`jsr:@scope/name` specifiers resolve against JSR's npm-compat endpoint at
<https://npm.jsr.io>. aube registers the `@jsr` scope for you, so no
`.npmrc` setup is needed — the install fetches the package under its
compat name (`@jsr/<scope>__<name>`) and writes `jsr:<range>` back to
`package.json`.

## Remove

```sh
aube remove react
aube remove -g cowsay
```

`remove` updates the manifest and relinks the install.

## Update

```sh
aube update
aube update react
aube update --latest react
```

`--latest` updates past the current manifest range and rewrites the manifest
specifier to the resolved version.

## Dedupe

```sh
aube dedupe
aube dedupe --check
```

`dedupe` re-resolves the lockfile to collapse duplicate versions where ranges
allow it. `--check` exits non-zero when the lockfile would change.

## Prune

```sh
aube prune
aube prune --prod
aube prune --no-optional
```

`prune` removes extraneous packages from `node_modules`, including stale
virtual-store entries and dangling `.bin` links.

It reads the lockfile to decide what should remain installed, but it does not
modify `package.json` or the lockfile. Use `aube store prune` instead when you
want to clean unreferenced files from the global store printed by
`aube store path`.
