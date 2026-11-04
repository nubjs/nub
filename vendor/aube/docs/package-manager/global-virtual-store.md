# Global virtual store

aube's global virtual store reuses fully materialized package directories across
projects. It is enabled by default for local installs and disabled under CI.

This is separate from the global content store:

- The **global content store** (`$XDG_DATA_HOME/aube/store/v1/`) stores
  package files by BLAKE3 hash. Every install uses it.
- The **global virtual store**
  (`<cacheDir>/virtual-store/v1/`, defaulting to
  `$XDG_CACHE_HOME/aube/virtual-store/v1/`, then
  `~/.cache/aube/virtual-store/v1/`) stores package directory trees keyed by
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
    react -> <cacheDir>/virtual-store/v1/react@18.2.0/<graph-hash>/node_modules/react

project-b/
  node_modules/
    react -> <cacheDir>/virtual-store/v1/react@18.2.0/<graph-hash>/node_modules/react
```

The global virtual store still imports package files from the global content
store. The win is that aube avoids rebuilding the same package directory tree in
every checkout.

## Cleanup

Run `aube store prune` to remove graph entries that are not referenced by any registered project.
Each global-virtual-store install registers its project, and pruning removes
records for projects that no longer exist before sweeping unreachable entries.
On upgrade, entries that predate the project registry are preserved because
they may still belong to checkouts that have not run a newer aube version yet.
Use `aube store prune --dry-run` to report what would be removed.

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

It is usually less useful in CI. CI jobs often start without a warm global
virtual store, so aube disables it under CI and materializes packages per
project instead.

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

### Moving it off the default volume

Two settings relocate the global virtual store, and neither requires moving
`XDG_CACHE_HOME` (which would drag every other tool's cache along with it).

[`globalVirtualStoreDir`](/settings/#setting-globalvirtualstoredir) moves the
virtual store on its own and leaves packument metadata where it is:

```sh
export AUBE_GLOBAL_VIRTUAL_STORE_DIR=/Volumes/Mini/dev/aube-virtual-store
export AUBE_STORE_DIR=/Volumes/Mini/dev/stores/aube
```

[`cacheDir`](/settings/#setting-cachedir) moves the whole cache — virtual store
and metadata together:

```sh
export AUBE_CACHE_DIR=/Volumes/Mini/dev/cache/aube
export AUBE_STORE_DIR=/Volumes/Mini/dev/stores/aube
```

Either way, keep the virtual store on the *same* volume as `storeDir`. Its
entries are hardlinked out of the content store, so a split across filesystems
makes every global-virtual-store install fall back to a per-file copy. aube
warns (`WARN_AUBE_GVS_CROSS_VOLUME`) when it detects that split:

```text
global virtual store dir is on a different volume than `storeDir`; install will
fall back to per-file copy.
```

`aube doctor` prints both resolved paths under `dirs` so you can confirm the
settings took effect.

## Limitations

Some tools canonicalize `node_modules/<pkg>` symlinks to their real path and
then walk upward looking for project files, app roots, or hoisted dependencies.
When the real path is in the global virtual store, that walk has
escaped the project and the tool can fail.

aube automatically falls back to per-project materialization when an importer
depends on a package with a known global-virtual-store incompatibility. The
default trigger list is:

- `next` — track the upstream Turbopack compatibility bug in
  [vercel/next.js#93556](https://github.com/vercel/next.js/issues/93556)
- `expo`, `react-native`, and `metro` — track the merged Metro file-map support
  in [expo/expo#45460](https://github.com/expo/expo/pull/45460). aube keeps the
  fallback for now because the compatible behavior is not yet available across
  the framework and Metro versions these package names can select.

When that happens, install still succeeds and aube prints a warning. Repeat
installs of that project just won't share materialized package directories
across projects.

Nuxt and Parcel work with the shared layout because aube records each
package's complete sibling dependency links inside the global virtual store.
This keeps Node resolution inside the installed dependency graph even after a
tool follows a package symlink to its physical location.

Vite 8.1 and newer support shared virtual stores. aube writes the effective
store location to `node_modules/.modules.yaml`, including in linked workspace
importers, so Vite can add the directory to its development-server filesystem
allow-list. The file is pnpm-compatible integration metadata; aube continues to
use `node_modules/.aube-state` as its own install state.

For older Vite versions, aube materializes the legacy Vite dependency and its
framework ancestry in the project-local virtual store, then backports Vite
8.1's metadata lookup into that local copy. Unrelated dependency branches stay
in the global virtual store, and aube never modifies the shared Vite package.

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
