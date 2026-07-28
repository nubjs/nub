---
name: remote-build
description: >-
  Run a nub Rust build, clippy gate, or test suite on an ephemeral Google Cloud spot VM
  instead of the dev Mac — and, for a macOS artifact, cross-compile aarch64-apple-darwin
  on Linux and pull the signed binary back. Invoke (via the Skill tool) whenever you are
  about to start a COLD build, `cargo clippy --all-targets --all-features`, a full
  `cargo test`, or a `release` build, and whenever the host is contended (load high, many
  agent worktrees building, a benchmark needs a quiet box). THE RULE THIS SKILL EXISTS TO
  CARRY: the heavy, cold-anyway jobs belong on a remote builder; the ~5s warm incremental
  loop stays local, because remote loses that one. Also the go-to when someone asks to
  "build this without hammering my machine" or to reclaim CPU from builds. Pairs with
  `dev-loop` (the local loop), `rust-build` (target-dir sharing), `rust-build-hygiene`
  (not orphaning builds), `cpu-reduction` (clearing residue that already accumulated),
  and `gcloud-vm` (the underlying VM mechanics).
metadata:
  internal: true
---

# Remote builds — get the heavy Rust jobs off the Mac

`scripts/remote-build.ts` dispatches a build/gate to a throwaway GCE spot VM and, for a
darwin build, brings back a runnable arm64 macOS binary. Full measurements and the
decision record: [`wiki/research/remote-build-offload.md`](../../../wiki/research/remote-build-offload.md).

```sh
nub scripts/remote-build.ts --job clippy                 # the CI gate, off-box
nub scripts/remote-build.ts --job test                   # cargo test -p nub-cli
nub scripts/remote-build.ts --job build --profile release # darwin binary -> target/remote/nub
nub scripts/remote-build.ts --fanout 10 --job clippy     # 10 builders at once
nub scripts/remote-build.ts --reap                       # delete stray builder VMs
nub scripts/remote-build.ts --build-image                # re-bake the golden image (rare)
```

## What goes remote, and what must NOT

Measured, `n2-standard-16` vs the Mac:

| Job | Remote | Mac | Verdict |
|---|---|---|---|
| warm incremental | 8.1s | ~5s | **stays local — remote loses** |
| cold `release` | 7m 00s | ~15m | remote, 2× |
| `clippy --all-targets --all-features` | 35.3s | — | remote |
| `cargo test -p nub-cli` | 39.4s warm | — | remote (718 passed / 0 failed on Linux) |
| cold `fast` | 3m 55s | ~3m | a wash |

**The inner loop is deliberately not a job type.** Do not try to route `cargo build --profile
fast` through this while iterating; you will make your loop slower. This tool exists for the
cold, expensive gates — which are also exactly what ~20 concurrent agent worktrees hammer the
Mac with.

## Why remote helps at all (it is not about cores)

Under load the Mac sits at **~30% idle CPU with a load average of 155**, sys time ~25%, disk at
**3000–4000 tps at 5–6 KB/transfer**. The bottleneck is cargo fingerprint/stat churn across a
dozen multi-GB target dirs on one APFS volume — not compute. Each remote builder brings **its
own disk**, which is where the relief comes from. Adding local cores would not have fixed this.

## The gotchas, each of which cost real time

- **zig is PINNED at 0.16.0 and that pin is load-bearing.** 0.14.1 and 0.15.2 SIGSEGV in the
  Mach-O linker, presenting as `error: linking … exit status: 1` with an **empty** `= note:` and
  a zero-byte output — no diagnostic at all. cargo-zigbuild's README claims "0.15+"; not enough.
- **No Apple SDK is involved.** zig ships its own `libSystem.tbd`, so nothing Apple-licensed is
  installed on the builder. This is why the SDK licence clause (which restricts running the SDK
  on non-Apple hardware) never applies to this route.
- **arm64 macOS SIGKILLs any unsigned binary.** zig emits a valid ad-hoc signature itself; the
  tool verifies `file` + `codesign` on every pulled artifact and says `UNSIGNED` loudly rather
  than handing you something that dies on exec and looks like a build failure.
- **macOS ships openrsync ("2.6.9 compatible"), not rsync 3.x.** Any 3.x-only flag fails the
  whole sync. Sync uses a `--files-from` **allowlist** built from `git ls-files`; an `--exclude`
  blocklist makes rsync walk ~99 GB of gitignored tree and time out at 120s.
- **A builder without `node` silently degrades the binary** — `aube-resolver/build.rs` emits
  "shipping empty primer". The job script hard-fails if `node` is missing rather than shipping it.
- **Under `--all-features`, `crates/nub-core/build.rs` panics** unless `runtime/addons/nub-native.node`
  is staged. The job script stages a placeholder, the same trick CI uses for its addon-less job.
- **`cmake` is mandatory** on the builder — `libz-ng-sys` fails ~35s in without it.
- **Deployment target comes out macOS 13.0, not 11.0.** cargo-zigbuild forces
  `-platform_version macos 13.0.0` and `MACOSX_DEPLOYMENT_TARGET` does not override it. Fine for
  a dev binary; **this must be solved before cross-compiled artifacts go anywhere near a release.**

## Orphaned builders cannot happen (three layers)

Stray builders are the exact failure this repo keeps paying for, so a local `finally` — which a
SIGKILL defeats — is not trusted on its own:

1. `finally` + SIGINT/SIGTERM handlers delete the VM on normal and interrupted paths.
2. Every builder carries `--max-run-duration=45m --instance-termination-action=DELETE`, so **GCE
   deletes it server-side even if the launching process dies outright.** This is the layer that
   actually holds.
3. Every VM is labelled `nub-builder=1`, so `nub scripts/remote-build.ts --reap` sweeps strays
   with no local state at all.

The image bake is the one exception to layer 2 — it stops the instance and images its disk, and a
mid-bake server-side DELETE would destroy that disk. It stays labelled and reapable.

Builds run in the ssh **foreground** and are never detached. A detached build reparents to PID 1,
outlives its launcher, holds locks, and is not reaped — see `rust-build-hygiene`.

## The golden image

`--build-image` bakes a `nub-builder` image family with apt deps, rustup + the darwin target +
clippy, pinned zig, cargo-zigbuild, Node, a warmed crate registry, and pre-compiled dependency
artifacts. Without it every builder would spend minutes installing a toolchain before doing any
work — the image is what makes this fast enough to reach for by default. Re-bake it when the
toolchain or the dependency graph moves substantially; day to day it just sits there.

## Cost

Spot `c3-standard-8` is a few cents per build; a 7-minute release build is about **$0.03**. A
stray cannot outlive 45 minutes. Cost is not a reason to hesitate — contention is the thing you
are spending money to avoid.
