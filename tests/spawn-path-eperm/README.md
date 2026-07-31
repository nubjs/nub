# Bare-name `spawn` fails EPERM under the build jail (macOS)

Under the macOS build jail, a lifecycle script that spawns a program by **bare name** with
no shell can fail `EPERM` at spawn, while the same program spawned by absolute path, or
through `sh -c`, runs. It is not a cwd problem and not a per-package problem: it depends on
the developer's ambient `PATH`, which the jail passes through to the child.

## Mechanism

libuv resolves a bare program name itself rather than calling `execvp`. It walks `PATH` and
calls `posix_spawn()` on each candidate, continuing **only** on `ENOENT`, `ENOTDIR` and
`EACCES` — every other errno is returned as the result of the entire resolution
(`deps/uv/src/unix/process.c`, `uv__spawn_resolve_and_spawn`).

Seatbelt answers a refused **symlink read** with `EPERM`, which is not in that continue
set. So a single `PATH` entry that the jail grants nothing on and that holds a symlink of
the right name aborts the whole search — later, granted directories are never tried.

`/bin/sh` is immune because its search is *test then exec*: a failed probe is
indistinguishable from "not here", so it just moves on. That asymmetry is the whole puzzle.

Two facts make this bite on a normal developer Mac:

- `(allow process-exec)` is unconditional in the base profile, so a **real** (non-symlink)
  ungranted binary spawns fine. Only the symlink read is refused. The trigger is
  specifically a symlink, not merely an ungranted directory.
- Homebrew links essentially everything: 1946 of 1947 entries in `/opt/homebrew/bin` are
  symlinks into `../Cellar/...`, and that directory usually precedes `/usr/bin`.

So the exposure is per-program: a tool is broken exactly when its first `PATH` hit is an
ungranted symlink, and fine when an earlier entry holds a real file or the hit lands in a
granted directory.

## Reproducing

```sh
tests/spawn-path-eperm/run.sh target/fast/nub git
```

Measured on `sandbox/integration`, macOS arm64, Node 26.5.0, jailed `PATH` of 58 entries:

```
PROBE tool=git entries=58 poison=[["/opt/homebrew/bin","EPERM"]]
PROBE jailed PATH verbatim : FAIL EPERM
PROBE poison dropped       : OK rc=0 git version 2.50.1 (Apple Git-155)
```

The two arms differ in one variable, so the poison entry is the cause rather than a
correlate. Pass a second argument to probe another tool: `cmake`, `pkg-config` and
`autoconf` reproduce, while `make`, `node`, `sh` and `python3` do not — each matching
whether that tool's first `PATH` hit is a Homebrew symlink.

## Scope

`shell: true`, `execSync`, and any absolute path are unaffected. Because npm-style
lifecycle scripts are themselves run through `sh -c`, only a program that *Node code*
spawns by bare name is exposed — `cmake-js`, for instance, calls
`execFile('cmake', ...)` directly.

Linux and Windows are not affected, by construction rather than by measurement: bubblewrap
leaves an ungranted path out of the mount namespace (`ENOENT`) and Landlock denies with
`EACCES`, both of which libuv continues past; libuv's Windows search tests each candidate
and returns `NULL` on failure instead of propagating an errno.
