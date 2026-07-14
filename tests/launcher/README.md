# Self-heal launcher test harness — how the shim-tax avoidance is regression-tested

This directory is the working system for testing nub's POSIX self-heal launcher — the code in [`npm/nub/bin/launch.js`](../../npm/nub/bin/launch.js) that makes `npm i -g @nubjs/nub` fast and robust without a postinstall. It exists because the heal cannot be faithfully unit-tested: it's a runtime mutation of an on-PATH bin entry, racing concurrent processes, with a non-owner filesystem fallback — behaviors that only reproduce against a real install tree, a real Node, and (for the non-owner case) a real privilege drop in a container. The mechanism and its decision record live in the sidecar `nub-self-heal-shim-tax` finding; this README documents the *loop* so a future agent reproduces it in minutes.

## What the launcher does (and why it needs testing)

The cross-platform `@nubjs/nub` package ships `bin/nub` / `bin/nubx` as `#!/usr/bin/env node` shims, because at publish time it can't know the target platform's binary. On its **first POSIX call** the launcher self-heals two things:

- **`healPathEntry`** — rewrites the on-PATH `nub` entry (an npm/bun/yarn symlink, or a pnpm cmd-shim) into a tiny `#!/bin/sh` sh/node **polyglot** trampoline that `exec`s the native binary. Every later call then resolves PATH → trampoline → native, skipping Node (~50ms → ~1-4ms). The polyglot shape is load-bearing: a concurrent Node that already passed the `#!node` shebang before the heal renamed the file in re-reads the swapped file as its script, and would choke parsing sh-as-JS — unless the file is *also* valid JS (the polyglot's third line spawns native). Measured: pure-sh heal ~6%/200 concurrent first-call failures on npm/bun; polyglot 0/600.
- **`ensureExecutable`** — npm strips +x from non-`bin`-field files on extract, so the native binary lands 0o644. When postinstall is skipped (npm v12 default, or `--ignore-scripts`), the runtime is the only net. We chmod in place when we own the file; when we don't (root installs, image drops to non-root `USER`), we stage a validated user-owned bundle under `~/.cache/nub/bin/<identity>/<verb>` and exec that.
- **`leadsToUs`** — a realpath guard so the heal never clobbers an unrelated `nub` on PATH (there is a real `nub@1.0.0` on npm).

Neither heal depends on postinstall having run — that's the whole point.

## The loop

1. `make-fixture.sh [dest] [symlink|pnpm]` builds a reproducible "npm-global-style install" tree under `dest` (default `/tmp/nub-launcher-fixture`): the real launcher package wired to a **fake native** binary (the heal is binary-agnostic — it only rewrites the on-PATH entry and exec's `bin/<verb>`, so a fake native that echoes its argv0-derived verb stands in for a platform build). `symlink` reproduces npm/bun/yarn's on-PATH shape; `pnpm` reproduces the cmd-shim shape. The fake native lands **0o644 on purpose** so `ensureExecutable` is exercised, not bypassed.
2. `run-launcher-matrix.sh [node-bin-dir ...]` runs every host scenario against fresh fixtures, once per Node version. With no args it sweeps `~/.nvm/versions/node/*`; pass explicit bin dirs to target versions (or a container's `/usr/local/bin`).
3. `docker-non-owner.sh` builds a Linux image (root install → drop to non-root `USER app` → postinstall NOT run) and asserts the staged-copy fallback. This leg is **only** reproducible in a container.

The fast inner loop while editing `launch.js`: `run-launcher-matrix.sh "$(dirname "$(command -v node)")"` (one Node, ~5s). Before trusting a result, sweep multiple Nodes and run the Docker leg.

## Why the Node-version sweep

The launcher is the same JS on every Node, but nub's runtime splits by tier — the **fast tier** (Node 22.15+) and the **compat tier** (18.19–22.14) take different code paths elsewhere, and the dev box runs one modern Node (often 26) that masks floor-only behavior. Driving the launcher onto a specific Node is cheap (`PATH=<nvm>/bin:$PATH`), so the sweep is cheap insurance even though the heal itself is tier-independent. Verified passing on 18.19.0, 20.10.0, and 26.2.0.

## The scenarios, and what each guards

| Scenario | What it asserts | How it's a real exercise |
| --- | --- | --- |
| `heal` | first call runs; the on-PATH entry is rewritten to a `#!/bin/sh` trampoline that names the native | asserts the healed file's shebang AND that it references `nub-host/bin/nub` — not just "the call worked" |
| `zero-node` | the second call spawns **zero** node | PATH includes a `node` wrapper that logs every spawn to `node.log`; the test asserts the log is empty after the 2nd call |
| `polyglot` | the healed entry, executed **as a node script**, still exec's native | runs `node <healed-entry>` directly; a pure-sh heal throws `SyntaxError` here (verified — see below) |
| `nubx-verb` | `nubx` keeps its verb through the heal | the fake native reports `nubx-mode` only when argv0 basename is `nubx`; asserts the healed `nubx` names `bin/nubx` |
| `ensure-chmod` | a 0o644 native we **own** is chmod'd +x in place | the fixture lands the native 0o644; the test asserts it's +x after the first call (no postinstall ran) |
| `foreign` | a `nub` on PATH that does **not** realpath to us is left byte-for-byte untouched | a foreign `#!/bin/sh` `nub` is placed first on PATH; asserts its content is unchanged after our launcher runs |
| `concurrency` | N concurrent **first** calls → 0 failures | forks N (default 200) processes at the unhealed entry at once; asserts every one printed the native's version, and the entry ended up healed |
| non-owner (Docker) | root-owned 0o644 native + non-root first call → works via a user-owned staged copy under `~/.cache/nub/bin/` | the container drops to `USER app`; asserts the staged copy exists, is +x, is owned by `app`, keeps the bare verb name, and both calls succeed. This is also the `--ignore-scripts` case (postinstall never runs) |

### The concurrency test is not ceremony

To confirm `concurrency` + `polyglot` genuinely catch the race the polyglot closes (not pass trivially), the heal was temporarily reverted to a pure-sh trampoline (drop the JS fallback line). Result on macOS/Node 26: the symlink concurrency test reported **199/200 first-call failures** and `polyglot` reported the `SyntaxError` — both red. The pnpm leg stayed green because pnpm's entry is already an sh cmd-shim, so sh→sh is race-free by construction (exactly what `launch.js`'s comment claims). Restoring the polyglot turns all of it green. So these tests fail when the property they guard is broken.

## What's tested where, and the honest gaps

- **Host (macOS + Linux CI):** heal, zero-node, polyglot, nubx-verb, ensure-chmod (owner path), foreign, concurrency — both shim shapes, swept across Node versions.
- **Docker (Linux only):** the non-owner staged-copy path — the one case the host can't make (it needs a root-owned file + a non-root runner). On a macOS/arm64 host the image runs `linux/arm64`; the heal is arch-independent.
- **Windows — NOT tested here, by design.** The heal is a deliberate **no-op on Windows**: there's no shebang/symlink fast path, so npm's generated `nub.cmd` invokes `node bin/nub` on every call. Windows therefore keeps the JS launcher and pays the ~50ms Node tax on every invocation — *working but taxed*, which is the intended degradation, not a bug. This harness does not (and cannot, without a Windows host) assert the Windows path; the `release.yml` / `verify-install.yml` `npm install -g` smoke on `windows-latest` covers that the JS launcher works there. Do not claim a Docker or host run verified Windows.
- **The ~1-4ms sh-hop timing** (vs ~50ms Node) is a perf claim, not asserted here — `zero-node` proves the *mechanism* (no Node spawned), which is the testable part; the absolute millisecond delta is environment-dependent and left to the bench harness.

## CI

`.github/workflows/launcher.yml` runs the host matrix on `ubuntu-latest` (dash) and `macos-latest` (bash), and the Docker non-owner leg on `ubuntu-latest`, on any change under `npm/nub/**` or `tests/launcher/**`. (The older `.github/scripts/heal-test.sh` was the first-pass inline version of the host scenarios; this directory supersedes it with the fixture/matrix split, the concurrency + non-owner coverage, and the version sweep.)
