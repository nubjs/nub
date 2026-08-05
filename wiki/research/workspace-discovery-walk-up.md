# Research: workspace-root discovery for recursive commands across JS/TS package managers (and cargo)

**Status:** v1, 2026-05-21. Researched same day, on macOS 25.5 (darwin
arm64) with locally installed `node 24.14.0`, `npm 11.9.0`, `pnpm
10.15.1`, `yarn 1.13.0` (classic) + `yarn 4.15.0` (berry, via Corepack
0.34.6), `bun 1.3.9`, `deno 2.7.14`, `cargo 1.83.0`. **Informs:**
`commands/run.md` (specifically the `nub run -r <script>` recursive
script invocation design). **Caveat:** This research doc is **not
edited retroactively**. If a finding here later turns out wrong, the
*command* doc updates; this one stays.

## TL;DR

- **Every tool tested except yarn classic walks up the directory tree
  past leaf `package.json` files to find the workspace root.** pnpm,
  npm, yarn 4, bun, cargo, and deno all do the walk-up. Yarn 1 does
  it too, but errors gracefully outside a workspace instead of
  falling back to filesystem-recursion.
- **The walk is unbounded** in pnpm, npm, yarn 4, cargo, and deno —
  they search from cwd up to the filesystem root. None of them stop
  at the nearest leaf `package.json`. The bounding signal is a
  *root-shaped* marker (`pnpm-workspace.yaml`, `"workspaces"` field,
  `[workspace]` table, `deno.json` with `workspace`), not "first
  manifest seen."
- **pnpm and bun have a footgun fallback:** if no workspace-root
  marker is found in the walk-up, they fall back to *descending* from
  cwd, treating every `package.json` under cwd as a "package" to run
  in. Demonstrated empirically below: `pnpm -r exec pwd` from `/tmp`
  enumerated every test monorepo present under `/tmp` and ran in each
  package; from `/` it timed out at 10s walking the entire
  filesystem. Bun behaves the same way for `--filter='*'`.
- **npm is the outlier in semantics:** from a sub-package it
  successfully finds the workspace root, but `npm run --workspaces`
  *silently scopes to only the current sub-package* — it doesn't run
  in siblings unless you pass explicit `--workspace=name` flags or
  invoke from the root. This is different from every other tool and
  is the trap to watch out for if Nub mimics npm's flag.
- **Cargo and yarn classic are the only ones that fail loudly when
  there is no workspace,** with a clean "could not find Cargo.toml in
  X or any parent directory" / "Cannot find the root of your
  workspace" error. Yarn 4 also errors cleanly ("No project found in
  /"). Deno errors when there's no `deno.json` / `package.json` to
  anchor to.
- **No tool surveyed uses a "bounded walk" (stop at first
  package.json) for recursive commands.** All of them either walk to
  the filesystem root looking for a workspace marker, or — in
  pnpm/bun's case — fall back to filesystem-descent. The design
  alternative Nub is considering (bound the walk at the nearest
  `package.json`) is not represented in the surveyed ecosystem.
- **Practical recommendation for Nub's `nub run -r`:** mirror cargo's
  contract — walk up looking for a workspace-root marker (a
  `package.json` with `"workspaces"`, a `pnpm-workspace.yaml`, or
  whatever Nub standardises on), error clearly if none is found
  before reaching the filesystem root. **Do not** ship pnpm's
  fallback-descent behaviour; it's the source of every recursive
  command's worst-case (`pnpm -r` from `$HOME` walks the user's home
  directory). Whether to also support npm's "scoped to current
  sub-package" semantic is a separate decision documented in
  `commands/run.md` — this research doc has no opinion on it.

## (1) Test scaffold

All tools were tested against an identical-shape monorepo, one per
tool to avoid cross-contamination, under
`/tmp/nub-ws-test.H8RRFa/<tool>-mono/`:

```
<tool>-mono/
├── package.json              # root with workspaces field
├── pnpm-workspace.yaml       # pnpm only
├── packages/
│   ├── alpha/
│   │   ├── package.json      # "scripts": {"where": "pwd"}
│   │   └── src/              # empty subdir, no package.json
│   └── beta/
│       └── package.json
```

Cargo got a parallel `Cargo.toml`/`crates/{alpha,beta}/src/lib.rs`
shape; deno got `deno.json` + `packages/{alpha,beta}/deno.json` with
a `"tasks": {"where": "pwd"}` field.

Three invocation locations per tool:

1. `…/<tool>-mono` — workspace root
2. `…/<tool>-mono/packages/alpha` — sub-package (has `package.json`,
   no `workspaces` field)
3. `…/<tool>-mono/packages/alpha/src` — sub-sub-dir (no manifest)

## (2) Per-tool results

### 2.1 pnpm 10.15.1 — `pnpm -r exec pwd`

All three invocation locations produced **identical** output (both
packages, ~180 ms wall-clock):

```
$ cd …/pnpm-mono/packages/alpha/src && pnpm -r exec pwd
/private/tmp/nub-ws-test.H8RRFa/pnpm-mono/packages/alpha
/private/tmp/nub-ws-test.H8RRFa/pnpm-mono/packages/beta
```

Wall-clock: 0.18 s (root), 0.18 s (sub-package), 0.22 s
(sub-sub-dir). The walk-up cost is negligible.

**Discovery mechanism (verified from source):** `pnpm` walks up from
cwd looking for a `pnpm-workspace.yaml` (or `NPM_CONFIG_WORKSPACE_DIR`
env var). From
[`workspace/root-finder/src/index.ts`](https://github.com/pnpm/pnpm/blob/main/workspace/root-finder/src/index.ts):

```ts
const workspaceManifestLocation = workspaceManifestDirEnvVar
  ? path.join(workspaceManifestDirEnvVar, WORKSPACE_MANIFEST_FILENAME)
  : await findUp([WORKSPACE_MANIFEST_FILENAME, ...INVALID_WORKSPACE_MANIFEST_FILENAME],
      { cwd: await getRealPath(cwd) })
```

`findUp` walks to the filesystem root unconditionally.

**Footgun: fallback-descent when no workspace marker found.** If the
walk-up finds nothing, `pnpm -r` doesn't error — it descends from cwd
treating every `package.json` it finds as a package.

```text
$ cd /tmp && pnpm -r exec pwd     # 5 test monorepos enumerated, all packages ran
/private/tmp/nub-ws-test.H8RRFa/bun-mono
/private/tmp/nub-ws-test.H8RRFa/bun-mono/packages/alpha
/private/tmp/nub-ws-test.H8RRFa/bun-mono/packages/beta
/private/tmp/nub-ws-test.H8RRFa/npm-mono
/private/tmp/nub-ws-test.H8RRFa/npm-mono/packages/alpha
/private/tmp/nub-ws-test.H8RRFa/npm-mono/packages/beta
…(15 packages total)
exit=0

$ cd / && timeout 10 pnpm -r exec pwd
exit=124    # hung walking entire FS for 10 s, killed

$ cd $HOME && timeout 10 pnpm -r exec pwd
exit=124    # also hung
```

This is documented in pnpm's source but not surfaced in the user-facing
[`pnpm -r` docs](https://pnpm.io/cli/recursive), which only describe
*what `-r` does* given a workspace, not how the workspace is found.

### 2.2 npm 11.9.0 — `npm run --workspaces where`

**From root:** runs in both packages. **From sub-package or
sub-sub-dir: runs only in `alpha`.** npm finds the workspace root —
verbose logs confirm `npm info config found workspace root at /…/npm-mono` —
but `--workspaces` is implicitly scoped to the current workspace from
inside a sub-package.

```text
$ cd …/npm-mono/packages/alpha && npm run --workspaces where

> alpha@0.0.0 where
> pwd
/tmp/nub-ws-test.H8RRFa/npm-mono/packages/alpha

# only one package — beta NOT executed
```

```text
$ cd …/npm-mono/packages/alpha && npm run --workspaces where --loglevel=verbose
…
npm info config found workspace root at /private/tmp/nub-ws-test.H8RRFa/npm-mono
…
```

To run across siblings from a sub-package you must use explicit names
(`npm run --workspace=alpha --workspace=beta where`) or invoke from
the root.

Wall-clock: 0.36 s (root), 0.12 s (sub-package), 0.12 s (sub-sub-dir).
The faster sub-package time is consistent with fewer packages
executed, not faster discovery.

**No-workspace case:** `npm run --workspaces` from a standalone
package errors cleanly: `npm error No workspaces found!`. From `/` it
fails with `ENOENT: …/package.json`.

**Docs:** [`docs.npmjs.com/cli/v11/using-npm/workspaces`](https://docs.npmjs.com/cli/v11/using-npm/workspaces)
says "if your current directory is in a workspace, the `workspace`
configuration is implicitly set, and `prefix` is set to the root
workspace." The "implicit scoping to current sub-package" semantic is
not described in plain English on that page; you have to read the
verbose log output to see it.

### 2.3 yarn berry / yarn 4.15.0 — `yarn workspaces foreach -A exec pwd`

All three invocation locations produced identical output. `-A`
(`--all`) is required; `yarn workspaces foreach` errors without one
of `-A`, `-R` (recursive on dependencies), `--since`, or `-W`
(worktree). The default with `-A` includes the root workspace too,
hence three lines instead of two:

```text
$ cd …/yarn4-mono/packages/alpha/src && yarn workspaces foreach -A exec pwd
/private/tmp/nub-ws-test.H8RRFa/yarn4-mono/packages/alpha
/private/tmp/nub-ws-test.H8RRFa/yarn4-mono/packages/beta
/private/tmp/nub-ws-test.H8RRFa/yarn4-mono
Done in 0s 29ms
```

Wall-clock: ~170 ms from each location.

**Caveat: yarn 4 requires `yarn install` before `foreach` works.**
Before install, it errors with "This package doesn't seem to be
present in your lockfile" — but importantly that error fires from
all three invocation locations, indicating the project root was
found regardless of cwd.

**No-workspace case:** clean error from `/`:

```text
Usage Error: No project found in /
```

**Docs:** [`yarnpkg.com/cli/workspaces/foreach`](https://yarnpkg.com/cli/workspaces/foreach)
describes the flags but not the discovery mechanism. The behavior is
documented only indirectly via the "Usage Error: No project found in
X" message.

### 2.4 yarn classic / yarn 1.13.0 — `yarn workspaces run where`

Yarn classic *does* support recursive workspaces, and *does* walk up
to find the workspace root. All three invocation locations ran both
packages:

```text
$ cd …/yarn1-mono/packages/alpha/src && yarn workspaces run where
yarn workspaces v1.13.0
yarn run v1.13.0
$ pwd
/private/tmp/nub-ws-test.H8RRFa/yarn1-mono/packages/alpha
Done in 0.03s.
yarn run v1.13.0
$ pwd
/private/tmp/nub-ws-test.H8RRFa/yarn1-mono/packages/beta
Done in 0.03s.
Done in 0.27s.
```

Wall-clock: ~360 ms (consistently across all three locations).

**No-workspace case:** clean error message:

```text
error Cannot find the root of your workspace - are you sure you're currently in a workspace?
info Visit https://yarnpkg.com/en/docs/cli/workspaces for documentation about this command.
```

Yarn 1 is the cleanest of the JS package managers tested for this
particular edge case — walks up, errors gracefully, no
fallback-descent footgun.

### 2.5 bun 1.3.9 — `bun run --filter='*' where`

Identical output from all three invocation locations. Wall-clock
~12 ms (the fastest of any tool tested, by a factor of ~15 over the
JS package managers):

```text
$ cd …/bun-mono/packages/alpha/src && bun run --filter='*' where
alpha where $ pwd
│ /private/tmp/nub-ws-test.H8RRFa/bun-mono/packages/alpha
beta  where $ pwd
│ /private/tmp/nub-ws-test.H8RRFa/bun-mono/packages/beta
Done in 12 ms
```

Bun's docs ([`bun.com/docs/cli/filter`](https://bun.com/docs/cli/filter))
state: "Filters respect your workspace configuration: If you have a
`package.json` file that specifies which packages are part of the
workspace, `--filter` will be restricted to only these packages."

**Footgun: same fallback-descent as pnpm.** When invoked from a dir
with no enclosing workspace, `bun run --filter='*' where` *still*
runs in any `package.json` found by descent from cwd:

```text
$ cd /tmp/standalone-pkg-dir && bun run --filter='*' where
a where $ pwd
│ /private/tmp/standalone-pkg-dir/a
```

From `/` bun errors with `error: ENOTDIR` rather than hanging — so it's
slightly better-behaved than pnpm at FS root, but the descent
behavior is the same.

### 2.6 cargo 1.83.0 — `cargo test --workspace`

Identical behavior from all three locations: walks up, finds
`Cargo.toml` with `[workspace]`, compiles + tests both members.

```text
$ cd …/cargo-mono/crates/alpha/src && cargo test --workspace --no-run
   …
   Executable unittests src/lib.rs (target/debug/deps/alpha-…)
   Executable unittests src/lib.rs (target/debug/deps/beta-…)
```

The first invocation took 5 s (compile from scratch); subsequent ones
~70 ms (cached). Walk-up cost is well below that.

**Docs are explicit about the mechanism** ([`doc.rust-lang.org/cargo/reference/workspaces.html`](https://doc.rust-lang.org/cargo/reference/workspaces.html)):

> When inside a subdirectory within the workspace, Cargo will
> automatically search the parent directories for a `Cargo.toml`
> file with a `[workspace]` definition to determine which workspace
> to use.

And the explicit override:

> The `package.workspace` manifest key can be used in member crates
> to point at a workspace's root to override this automatic search.
> The manual setting can be useful if the member is not inside a
> subdirectory of the workspace root.

**No-workspace case:** clean error, no fallback descent:

```text
$ cd / && cargo test --workspace
error: could not find `Cargo.toml` in `/` or any parent directory
```

Cargo is the gold-standard model for this behavior: documented,
bounded by a marker file (`[workspace]` table), errors loudly outside.

### 2.7 deno 2.7.14 — `deno task -r where`

Identical behavior from all three locations:

```text
$ cd …/deno-mono/packages/alpha/src && deno task -r where
Task where (@scope/alpha) pwd
/private/tmp/nub-ws-test.H8RRFa/deno-mono/packages/alpha
Task where (@scope/beta) pwd
/private/tmp/nub-ws-test.H8RRFa/deno-mono/packages/beta
```

Wall-clock: ~12-30 ms.

Deno discovers the workspace by walking up looking for a `deno.json`
(or `deno.jsonc`) with a `workspace` field. The `-r` / `--recursive`
flag exists and is documented in `deno task --help`:

```
-r, --recursive    Run the task in all projects in the workspace
-f, --filter       Filter members of the workspace by name, implies --recursive flag
```

**Public docs are sparse:** the
[workspaces page](https://docs.deno.com/runtime/fundamentals/workspaces/)
does not mention `-r` / `--recursive` at all — the feature is only
discoverable via `--help`. WebFetch on the docs page returned "no
mention of `deno task --recursive`"; the empirical behavior is the
authoritative source.

**No-workspace case:** errors cleanly with `deno task couldn't find
deno.json(c) or package.json`.

**Subtle:** if the only manifest found in walk-up is a `package.json`
(no `deno.json`), `deno task -r` still works — it falls back to
treating that `package.json` as the project, which means in our
"isolated standalone" test deno ran in the lonely package. Not a
filesystem-descent footgun like pnpm/bun, but worth noting that
deno's walk-up criterion is "any manifest" not "workspace marker."

## (3) Cross-cutting answers

### (3.1) Which tools walk past leaf `package.json` files?

**All of them, in the sense that "leaf package.json" doesn't stop the
walk for any tool.** None of pnpm, npm, yarn 4, yarn 1, bun, or deno
treats the first `package.json` it finds going up as a bounding
signal. The bounding signal is *workspace-marker-shaped*:

| Tool        | Walk-up bounding marker                                              |
|-------------|----------------------------------------------------------------------|
| pnpm 10     | `pnpm-workspace.yaml` (any case-variant errors)                      |
| npm 11      | `package.json` with `"workspaces"` field                             |
| yarn 4      | `package.json` with `"workspaces"` field                             |
| yarn 1      | `package.json` with `"workspaces"` field                             |
| bun 1.3     | `package.json` with `"workspaces"` field                             |
| deno 2      | `deno.json[c]` with `"workspace"` field (falls back to any manifest) |
| cargo 1.83  | `Cargo.toml` with `[workspace]` table                                |

### (3.2) How far up do they walk?

**Unbounded — to the filesystem root** for pnpm, npm, yarn 4, cargo,
deno. (Bun and yarn 1 also walk unbounded; the filesystem-root
behavior is where they differ — see 3.3.)

None of the tools use a heuristic like "stop at $HOME" or "stop at a
.git directory" or "stop at the nearest package.json that doesn't
have `workspaces`." The walk is determined purely by the
workspace-marker check.

### (3.3) What happens at the filesystem root?

| Tool   | From `/` (with `-r` / `--workspaces`)                                              |
|--------|------------------------------------------------------------------------------------|
| pnpm   | **Hangs** walking the FS tree as fallback-descent; killed at 10 s timeout         |
| npm    | `npm error code ENOENT … Could not read package.json: … '/package.json'`           |
| yarn 4 | `Usage Error: No project found in /`                                               |
| yarn 1 | `error Cannot find the root of your workspace — are you sure you're in a workspace?` |
| bun    | `error: ENOTDIR`                                                                   |
| cargo  | `error: could not find Cargo.toml in / or any parent directory`                    |
| deno   | `error: deno task couldn't find deno.json(c) or package.json`                     |

**Cargo, yarn (both versions), npm, and deno error cleanly.** Bun
errors with a slightly-cryptic `ENOTDIR` but at least returns. **pnpm
is the outlier**: with no `pnpm-workspace.yaml` found, it falls
through to "descend from cwd and run in every `package.json` you find,"
which from `/` means walking the entire filesystem.

### (3.4) Standalone package, no enclosing workspace

| Tool   | `cd standalone-pkg && <recursive>`                                                |
|--------|-----------------------------------------------------------------------------------|
| pnpm   | Runs in the standalone package (treats it as a degenerate one-package workspace) |
| npm    | `npm error No workspaces found!` — exit 1                                         |
| yarn 4 | `Usage Error: No project found in <dir>` (after `--all`) — exit 1                 |
| yarn 1 | `error Cannot find the root of your workspace …` — exit 1                         |
| bun    | Runs in the standalone package, same as pnpm                                      |
| deno   | Runs in the standalone package (since `deno.json` was found)                      |
| cargo  | Compiles + tests the standalone crate (Cargo treats a single-crate dir as a valid workspace) |

**Split decision:** pnpm/bun/deno/cargo treat a single-package
invocation as legal and equivalent to "workspace of one." npm,
yarn 1, and yarn 4 require an explicit workspace setup or error.

### (3.5) Is there any "bounded walk" (stop at first package.json)?

**No.** Every tool surveyed either walks unbounded to the FS root
looking for a workspace-shaped marker, or — in pnpm/bun's case —
walks unbounded *and* descends from cwd on miss.

The "stop at first `package.json` you encounter going up" design
Nub is considering is not represented in the surveyed ecosystem. This
isn't necessarily wrong — Nub's situation is different from these
tools in that Nub is the runtime and the package manager is something
else (pnpm/npm/yarn/bun). The semantics of `nub run -r <script>` are
Nub's to define. The closest analog is **cargo's contract**: walk up
to a clearly-marked workspace root, error if none found.

### (3.6) Documented justification for the walk-past-leaf behavior

**Almost none of the tools document the discovery mechanism directly
in user-facing docs.** WebFetches against the official docs pages
returned "documentation focuses on usage, not discovery" for pnpm,
yarn 4, and bun. The clear documented behavior is:

**Cargo (best documented):**

> When inside a subdirectory within the workspace, Cargo will
> automatically search the parent directories for a `Cargo.toml`
> file with a `[workspace]` definition to determine which workspace
> to use. The `package.workspace` manifest key can be used in member
> crates to point at a workspace's root to override this automatic
> search.
>
> — <https://doc.rust-lang.org/cargo/reference/workspaces.html>

**npm (workspaces page, terse):**

> If your current directory is in a workspace, the `workspace`
> configuration is implicitly set, and `prefix` is set to the root
> workspace.
>
> — <https://docs.npmjs.com/cli/v11/using-npm/workspaces>

**Bun (filter page, indirect):**

> Filters respect your workspace configuration: If you have a
> `package.json` file that specifies which packages are part of the
> workspace, `--filter` will be restricted to only these packages.
>
> — <https://bun.com/docs/cli/filter>

**pnpm, yarn (both), deno:** no direct documentation of the
walk-up. Behavior is observable only by running the tool or reading
source. For pnpm, the relevant source file is
`workspace/root-finder/src/index.ts` (uses `find-up` library).

The lack of explicit documentation across the ecosystem suggests this
is treated as a "just walk up like everything else does" tribal
norm rather than an explicit design decision. Cargo's docs are the
exception.

## (4) Summary table

| Tool         | Recursive flag                  | Walks up?        | From `/`        | From sub-pkg with no workspace marker | Runs root too?  | Wall-clock (ms) |
|--------------|---------------------------------|------------------|-----------------|---------------------------------------|------------------|------------------|
| pnpm 10      | `-r exec`                       | Yes (unbounded) | **Hangs**       | Fallback-descend from cwd            | Yes (if listed)  | ~180             |
| npm 11       | `run --workspaces`              | Yes (unbounded) | ENOENT           | Error: no workspaces                  | No (use `--include-workspace-root`) | ~360 / ~120 (scoped) |
| yarn 4       | `workspaces foreach -A exec`    | Yes (unbounded) | Usage Error      | Error: no project                     | Yes (`-A` includes root) | ~170             |
| yarn 1       | `workspaces run`                | Yes (unbounded) | Cannot find root | Error: cannot find root               | No               | ~360             |
| bun 1.3      | `run --filter='*'`              | Yes (unbounded) | ENOTDIR          | Fallback-descend from cwd            | No (use `*` matches members) | ~12              |
| deno 2       | `task -r`                       | Yes (unbounded) | Error: no config | Treats lone manifest as project       | If member        | ~12-30           |
| cargo 1.83   | `test --workspace`              | Yes (unbounded) | Error: no Cargo.toml | Treats single crate as one-pkg workspace | N/A          | ~70 (cached)     |

## (5) Implications for `nub run -r`

(Notes; the actual design decision lives in `commands/run.md`.)

1. **Walk-up is table stakes.** Every tool surveyed does it; users
   expect `nub run -r where` to "just work" from anywhere inside the
   monorepo.
2. **Avoid pnpm/bun's fallback-descent.** This is unambiguously a
   footgun: it's the only behavior that produces "hangs at `/`" and
   "runs in random places under `/tmp`." Cargo's contract — walk up
   for a marker, error if absent — is cleaner and more
   debuggable. Nub's compatibility posture (compat with Node and the
   user's existing package manager) gives no reason to ship the
   footgun.
3. **The workspace marker is whatever Nub decides.** Likely a
   `package.json` with `"workspaces"` field for ecosystem
   consistency, but if Nub introduces its own root marker (it
   shouldn't, per the no-`nub:*`-namespace rule in CLAUDE.md), it
   would attach there. Reading the user's existing workspace markers
   (pnpm-workspace.yaml, package.json workspaces, …) is the
   compatibility-friendly path.
4. **Npm's "scoped to current sub-package" semantic** is unique and
   worth thinking about explicitly: from `packages/alpha`, do you
   want `nub run -r build` to (a) build everything in the workspace,
   or (b) build only the current package? Every tool tested *except
   npm* picks (a). Nub should probably pick (a) for
   least-surprise; (b) can be a separate flag if needed.
5. **At the filesystem root, error like cargo does.** Print "could
   not find a workspace root (package.json with `workspaces`, …) in
   `<cwd>` or any parent directory" and exit non-zero. Don't hang,
   don't ENOTDIR, don't ENOENT-on-/package.json.

## (6) Sources

- pnpm `findWorkspaceDir` source: <https://github.com/pnpm/pnpm/blob/main/workspace/root-finder/src/index.ts>
- pnpm `-r` CLI docs: <https://pnpm.io/cli/recursive>
- npm workspaces docs: <https://docs.npmjs.com/cli/v11/using-npm/workspaces>
- yarn 4 `workspaces foreach`: <https://yarnpkg.com/cli/workspaces/foreach>
- yarn 1 workspaces: <https://classic.yarnpkg.com/en/docs/cli/workspaces>
- bun filter: <https://bun.com/docs/cli/filter>
- bun workspaces: <https://bun.com/docs/install/workspaces>
- cargo workspaces: <https://doc.rust-lang.org/cargo/reference/workspaces.html>
- deno workspaces: <https://docs.deno.com/runtime/fundamentals/workspaces/>
- deno task `--help` (in-tool) for `-r` / `-f` flags (not on docs page as of 2026-05-21)

All empirical commands above were run on macOS 25.5 (Darwin 25.5.0,
arm64) on 2026-05-21. Raw test scaffolds were under
`/tmp/nub-ws-test.H8RRFa/` and are not preserved beyond the research
session.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus.
