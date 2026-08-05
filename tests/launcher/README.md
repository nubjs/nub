# Self-heal launcher test harness — how the shim-tax avoidance is regression-tested

This directory is the working system for testing nub's POSIX self-heal launcher — the code in [`npm/nub/bin/launch.js`](../../npm/nub/bin/launch.js) that makes `npm i -g @nubjs/nub` fast and robust without a postinstall. It exists because the heal cannot be faithfully unit-tested: it's a runtime mutation of an on-PATH bin entry, racing concurrent processes, with a non-owner filesystem fallback — behaviors that only reproduce against a real install tree, a real Node, and (for the non-owner case) a real privilege drop in a container. The mechanism and its decision record live in the sidecar `nub-self-heal-shim-tax` finding; this README documents the *loop* so a future agent reproduces it in minutes.

## What the launcher does (and why it needs testing)

The cross-platform `@nubjs/nub` package ships `bin/nub` / `bin/nubx` as `#!/usr/bin/env node` shims, because at publish time it can't know the target platform's binary. On its **first POSIX call** the launcher self-heals two things:

- **`healPathEntry`** — rewrites the on-PATH `nub` entry (an npm/bun/yarn symlink, or a pnpm cmd-shim) into a tiny `#!/bin/sh` sh/node **polyglot** trampoline that `exec`s the native binary. Every later call then resolves PATH → trampoline → native, skipping Node (~50ms → ~1-4ms). The polyglot shape is load-bearing: a concurrent Node that already passed the `#!node` shebang before the heal renamed the file in re-reads the swapped file as its script, and would choke parsing sh-as-JS — unless the file is *also* valid JS (the polyglot's third line spawns native). Measured: pure-sh heal ~6%/200 concurrent first-call failures on npm/bun; polyglot 0/600.
- **`ensureExecutable`** — npm strips +x from non-`bin`-field files on extract, so the native binary lands 0o644. When postinstall is skipped (npm v12 default, or `--ignore-scripts`), the runtime is the only net. We chmod in place when we own the file; when we don't (root installs, image drops to non-root `USER`), we stage a validated user-owned bundle under `~/.cache/nub/bin/<identity>/<verb>` and exec that.
- **`leadsToUs`** — a realpath guard so the heal never clobbers an unrelated `nub` on PATH (there is a real `nub@1.0.0` on npm).

Neither heal depends on postinstall having run — that's the whole point.

## The loop

1. `make-fixture.sh [dest] [style]` builds a reproducible "npm-global-style install" tree under `dest` (default `/tmp/nub-launcher-fixture`): the real launcher package wired to a **fake native** binary (the heal is binary-agnostic — it only rewrites the on-PATH entry and exec's `bin/<verb>`, so a fake native that echoes its argv0-derived verb stands in for a platform build). The style picks the on-PATH entry shape — see [the shim styles](#the-shim-styles) below. The fake native lands **0o644 on purpose** so `ensureExecutable` is exercised, not bypassed.
2. `run-launcher-matrix.sh [node-bin-dir ...]` runs every host scenario against fresh fixtures, once per Node version. With no args it sweeps `~/.nvm/versions/node/*`; pass explicit bin dirs to target versions (or a container's `/usr/local/bin`).
3. `docker-non-owner.sh` builds a Linux image (root install → drop to non-root `USER app` → postinstall NOT run) and asserts the staged-copy fallback. This leg is **only** reproducible in a container.

The fast inner loop while editing `launch.js`: `run-launcher-matrix.sh "$(dirname "$(command -v node)")"` (one Node, ~5s). Before trusting a result, sweep multiple Nodes and run the Docker leg.

## The shim styles

The style argument picks the shape of the on-PATH entry that dispatched us — the thing `leadsToUs` has to recognize and `healPathEntry` then rewrites. The first three are what a real package manager writes. The last three are **derived**: they exist so that each mechanism inside `leadsToUs` has a leg that can go red on its own.

| Style | Shape | Fails when |
| --- | --- | --- |
| `symlink` | npm / bun / yarn — a symlink to `../node_modules/@nubjs/nub/bin/<verb>` | the symlink realpath branch breaks |
| `pnpm` | pnpm 10 cmd-shim — one `exec node …` line, no empty quote pair, no trailer | the quote scan breaks for the pnpm 10 shape |
| `pnpm11` | pnpm >=11 cmd-shim, byte-identical to `@zkochan/cmd-shim` 9.0.6 output: 5-branch exec chain, the `*WSL2*` arm, the `cygpath` call, the empty `exe=""`/`msys=""` pairs, and a `# cmd-shim-target=` trailer | `leadsToUs` stops recognizing real pnpm 11 shims by any route |
| `scan` | the `pnpm11` template with the trailer withheld | the quoted-path scan regresses — notably the `[^"]*` class narrowing back to `[^"]+` |
| `decl` | target assembled into an **unquoted** variable, plus an absolute trailer | the `# cmd-shim-target=` branch is dropped |
| `declrel` | as `decl`, with a **relative** trailer | that branch is dropped, or its value stops being resolved against the shim's directory |

**Why the derived styles are not redundant.** `leadsToUs` tries the trailer before the quote scan and returns on a match, so a fixture carrying both hazards — which is exactly what a real pnpm 11 shim is — still heals when either mechanism alone regresses. That is how the pnpm 11 bug shipped green the first time: the matrix had no leg that could fail for it. Each derived style withholds every route but one.

The rule this encodes: **a style that cannot go red is not coverage.** When you change `leadsToUs`, verify by reverting your change and watching the matrix go red — not by watching it stay green. Every launcher change is currently pinned this way:

| Revert | Goes red |
| --- | --- |
| `[^"]*` back to `[^"]+` | `scan` |
| the `# cmd-shim-target=` branch | `decl`, `declrel` |
| `path.resolve(basedir, …)` back to the bare value | `declrel` |
| the `exec` corroboration on the trailer | the foreign "declares our target, never execs" case |
| `[ \t]*` back to `\s*` in the trailer key | the foreign "bare `#` then newline then key" case |
| the `#!` precondition | the foreign "no shebang, declares our target" case |

The `#!` check is on that list because it is a clobber guard and not only a perf guard — it is what stops a file with no shebang from being renamed over on the strength of a `# cmd-shim-target=` line. The size cap above it is genuinely perf-only and has no probe, which is the honest state: a 64 KB fixture would cost more than the guard is worth.

`run_block` runs every style; the concurrency scenario runs only on `symlink`, `pnpm`, and `pnpm11`. The polyglot race it guards lives in the heal **write**, which is identical whichever route matched, so sweeping it per parse style bought nothing and cost 200 forks each.

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
| `foreign` | a `nub` on PATH that does **not** realpath to us is left byte-for-byte untouched | three foreign `#!/bin/sh` files are each placed first on PATH and asserted unchanged: a plain one, one that names our launcher in a `# cmd-shim-target=` line but never execs it, and one where a bare `#` line precedes the key on the next line. The heal renames over its match with no backup, so a false positive is unrecoverable loss in a file we do not own — and an unrelated `nub@1.0.0` really is on npm |
| `concurrency` | N concurrent **first** calls → 0 failures | forks N (default 200) processes at the unhealed entry at once; asserts every one printed the native's version, and the entry ended up healed |
| non-owner (Docker) | root-owned 0o644 native + non-root first call → works via a user-owned staged copy under `~/.cache/nub/bin/` | the container drops to `USER app`; asserts the staged copy exists, is +x, is owned by `app`, keeps the bare verb name, and both calls succeed. This is also the `--ignore-scripts` case (postinstall never runs) |

### The concurrency test is not ceremony

To confirm `concurrency` + `polyglot` genuinely catch the race the polyglot closes (not pass trivially), the heal was temporarily reverted to a pure-sh trampoline (drop the JS fallback line). Result on macOS/Node 26: the symlink concurrency test reported **199/200 first-call failures** and `polyglot` reported the `SyntaxError` — both red. The pnpm leg stayed green because pnpm's entry is already an sh cmd-shim, so sh→sh is race-free by construction (exactly what `launch.js`'s comment claims). Restoring the polyglot turns all of it green. So these tests fail when the property they guard is broken.

## What's tested where, and the honest gaps

- **Host (macOS + Linux CI):** heal, zero-node, polyglot, nubx-verb, ensure-chmod (owner path), foreign, concurrency — every shim style above, swept across Node versions.
- **Docker (Linux only):** the non-owner staged-copy path — the one case the host can't make (it needs a root-owned file + a non-root runner). On a macOS/arm64 host the image runs `linux/arm64`; the heal is arch-independent.
- **Windows — NOT tested here, by design.** The heal is a deliberate **no-op on Windows**: there's no shebang/symlink fast path, so npm's generated `nub.cmd` invokes `node bin/nub` on every call. Windows therefore keeps the JS launcher and pays the ~50ms Node tax on every invocation — *working but taxed*, which is the intended degradation, not a bug. This harness does not (and cannot, without a Windows host) assert the Windows path; the `release.yml` / `verify-install.yml` `npm install -g` smoke on `windows-latest` covers that the JS launcher works there. Do not claim a Docker or host run verified Windows.
- **The ~1-4ms sh-hop timing** (vs ~50ms Node) is a perf claim, not asserted here — `zero-node` proves the *mechanism* (no Node spawned), which is the testable part; the absolute millisecond delta is environment-dependent and left to the bench harness.

## CI

`.github/workflows/launcher.yml` runs the host matrix on `ubuntu-latest` (dash) and `macos-latest` (bash), and the Docker non-owner leg on `ubuntu-latest`, on any change under `npm/nub/**` or `tests/launcher/**`. (The older `.github/scripts/heal-test.sh` was the first-pass inline version of the host scenarios; this directory supersedes it with the fixture/matrix split, the concurrency + non-owner coverage, and the version sweep.)
