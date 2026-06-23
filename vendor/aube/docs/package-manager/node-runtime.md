# Node runtime switching

aube switches Node.js versions per project. When a project pins a Node
version, every command that spawns node — `aube run` / `aubr`,
`aube exec`, `aubx` / `aube dlx`, and lifecycle/build scripts — runs on
the pinned version. There are no shims and no shell activation to set
up: running through aube *is* the switch, and `node` outside aube is
untouched.

## Pinning a version

aube reads the desired version from three sources, highest precedence
first:

1. **`devEngines.runtime`** in `package.json` (the
   [OpenJS standard](https://github.com/openjs-foundation/package-metadata-interoperability-working-group),
   shared with pnpm 10.14+):

   ```json
   {
     "devEngines": {
       "runtime": { "name": "node", "version": "^24.4.0", "onFail": "download" }
     }
   }
   ```

2. **`.node-version`**
3. **`.nvmrc`**

Version files are searched upward from the project directory (through
monorepo roots, stopping at your home directory), so a repo-root
`.nvmrc` applies to every package below it. Accepted version requests:
exact versions (`24.4.1`), ranges (`^24`, `22`), `lts`, `latest`, and
LTS codenames (`lts/jod`).

The easiest way to pin is the CLI, which resolves the request, writes
`devEngines.runtime`, installs the runtime, and records the resolved
version in the lockfile:

```sh
aube runtime set node lts
aube runtime set node 24 --save-exact
aube runtime list
```

## Where the node comes from

aube looks for a satisfying version in order, stopping at the first
hit — the common cases never touch the network:

1. the `node` already on PATH;
2. installed versions, from **mise** (`~/.local/share/mise/installs/node/`)
   and from aube's own runtime dir (`~/.local/share/aube/nodejs/`);
3. download.

When a download is needed, the [`runtimeInstaller`](/settings/#runtimeinstaller)
setting decides who fetches it:

- `auto` (default): delegate to `mise install node@<version>` when
  [mise](https://mise.jdx.dev) is on PATH — mise users keep a single
  Node store on disk — and download from nodejs.org otherwise;
- `mise`: always delegate (errors if mise is missing);
- `aube`: always self-download.

Self-downloads are verified against Node's published `SHASUMS256.txt`
(or the lockfile's recorded checksum) before extraction. Corporate
mirrors are supported via [`nodeDownloadMirrors`](/settings/#nodedownloadmirrors).

## `onFail` policy

`devEngines.runtime.onFail` controls what happens when no satisfying
version is available locally:

| value | behavior |
| --- | --- |
| `download` | fetch the version (recommended) |
| `error` | fail the command (the OpenJS default when `onFail` is omitted) |
| `warn` | print a warning and keep the ambient node |
| `ignore` | silently keep the ambient node |

`.node-version` / `.nvmrc` pins have no `onFail` vocabulary and behave
as `download` — that's what writing one means. The
[`runtimeOnFail`](/settings/#runtimeonfail) setting overrides the policy
everywhere; set `runtimeOnFail=error` in air-gapped CI to forbid
runtime downloads outright.

## Lockfile pinning

When the pin comes from `devEngines.runtime`, the resolved exact
version — plus download URLs and checksums for every platform — is
recorded in `aube-lock.yaml` using pnpm's `node@runtime:` shape, so
teammates and CI resolve the identical release without consulting
nodejs.org. pnpm 10.14+ writes (and reads) the same entries, so the
two tools interoperate on a shared `pnpm-lock.yaml`.

Version-file pins (`.nvmrc` / `.node-version`) are deliberately *not*
recorded in the lockfile — pnpm wouldn't understand a pin with no
matching `devEngines`, and shared lockfiles must stay portable.

npm / yarn / bun lockfile formats have no runtime entry shape; on
those projects aube re-resolves the range on each run (a warning
points this out once per install).

## Engines checks

`engines.node` validation (and `engineStrict`) runs against the
*switched* node — the version your scripts will actually execute on.
The `nodeVersion` setting still overrides the version engines checks
compare against, and remains validation-only: it never switches the
runtime (pnpm semantics).

## Pinning aube itself

The same machinery manages aube's own version (corepack semantics,
pnpm's `managePackageManagerVersions` — on by default). Pin via either:

```json
{ "packageManager": "aube@1.18.2" }
```

```json
{
  "devEngines": {
    "packageManager": { "name": "aube", "version": "^1.18" }
  }
}
```

When the running aube doesn't satisfy the pin, it locates the pinned
version — mise installs (`~/.local/share/mise/installs/aube/`) are
reused, missing versions install per
[`runtimeInstaller`](/settings/#runtimeinstaller) (mise delegation, or
a GitHub release download into `~/.local/share/aube/self/` verified
against GitHub's server-computed asset digest) — and re-execs it with
the same
arguments. `aubr` and `aubx` switch the same way. The corepack
`packageManager` field takes exact versions; `devEngines.packageManager`
accepts ranges, `lts`-style aliases excluded, plus the usual `onFail`
vocabulary. Set
[`managePackageManagerVersions=false`](/settings/#managepackagemanagerversions)
to fall back to validation-only (`packageManagerStrict`).

## Inspecting

```sh
aube runtime list   # resolved version, source, provenance, installs
aube doctor         # node, node-source, node-requested, node-provenance, aube-pin
```
