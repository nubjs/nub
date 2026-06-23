# Global virtual store

aube's global virtual store reuses fully materialized package directories across
projects. It is enabled by default for local installs and disabled under CI.

This is separate from the global content store:

- The **global content store** (`$XDG_DATA_HOME/aube/store/v1/`) stores
  package files by BLAKE3 hash. Every install uses it.
- The **global virtual store**
  (`$XDG_CACHE_HOME/aube/virtual-store/`, defaulting to
  `~/.cache/aube/virtual-store/`) stores package directory trees keyed by
  dependency graph. Project `node_modules` entries symlink into it.

## Default behavior

Without the global virtual store, each project gets its own virtual store under
`node_modules/.aube/`. Package files are still deduplicated through the global
content store, but the directory tree is rebuilt for each checkout.

```text
project-a/
  node_modules/
    react -> .aube/react@18.2.0/node_modules/react
    .aube/
      react@18.2.0/
        node_modules/
          react/       # files imported from the content store

project-b/
  node_modules/
    react -> .aube/react@18.2.0/node_modules/react
    .aube/
      react@18.2.0/
        node_modules/
          react/       # same file content, separate directory tree
```

## With the global virtual store

With the global virtual store enabled, aube builds the package tree once in the
shared cache. Each project points directly at that shared tree:

```text
project-a/
  node_modules/
    react -> $XDG_CACHE_HOME/aube/virtual-store/react@18.2.0/<graph-hash>/node_modules/react

project-b/
  node_modules/
    react -> $XDG_CACHE_HOME/aube/virtual-store/react@18.2.0/<graph-hash>/node_modules/react
```

The global virtual store still imports package files from the global content
store. The win is that aube avoids rebuilding the same package directory tree in
every checkout.

## Package identity

Entries are keyed by the resolved dependency graph, not just by package name and
version. Two projects can share `react@18.2.0` when the surrounding dependency
graph matches. If peer dependencies or transitive dependencies differ, aube
creates a separate entry with a different graph hash.

That keeps Node's resolution semantics intact: sharing only happens when the
materialized package tree is safe to reuse.

## Compared with pnpm

pnpm has a similar
[global virtual store](https://pnpm.io/global-virtual-store), but project
installs leave it disabled by default. aube enables the global virtual store by
default for local installs, then turns it off automatically under CI and for
known symlink-sensitive toolchains.

## When it helps

The global virtual store is most useful on developer machines:

- multiple worktrees or checkouts of the same repo
- repeated fresh installs after deleting `node_modules`
- several projects using the same package versions
- one-off `aubx` and script workflows that benefit from warm local state

It is usually less useful in CI. CI jobs often start without a warm
`$XDG_CACHE_HOME/aube/virtual-store/`, so aube disables the global virtual store
under CI and materializes packages per project instead.

## Configuration

Set the project default in `.npmrc`:

```ini
enableGlobalVirtualStore=true
```

or:

```ini
enableGlobalVirtualStore=false
```

Override a single command with:

```sh
aube install --enable-global-virtual-store
aube install --disable-global-virtual-store
```

## Limitations

Some tools canonicalize `node_modules/<pkg>` symlinks to their real path and
then walk upward looking for project files, app roots, or hoisted dependencies.
When the real path is in `$XDG_CACHE_HOME/aube/virtual-store/`, that walk has
escaped the project and the tool can fail.

aube automatically falls back to per-project materialization when an importer
depends on a package with a known global-virtual-store incompatibility. The
default trigger list is:

- `next`
- `nuxt`
- `vite`
- `vitepress`
- `parcel`

When that happens, install still succeeds and aube prints a warning. Repeat
installs of that project just won't share materialized package directories
across projects.

To add a package to the trigger list, append entries to
`disableGlobalVirtualStoreForPackages` in `.npmrc`:

```ini
disableGlobalVirtualStoreForPackages[]=my-tool
```

To silence the warning while keeping the fallback, set:

```ini
enableGlobalVirtualStore=false
```

To opt out of the compatibility heuristic entirely, set:

```ini
disableGlobalVirtualStoreForPackages=[]
```

Only use that when you know the project's tools tolerate symlinks that point
outside the project.
