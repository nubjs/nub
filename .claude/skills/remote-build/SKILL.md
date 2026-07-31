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

`scripts/remote-build.ts` dispatches a build/gate to a throwaway GCE spot VM and reports the result.
**For a macOS binary use the `mac-build` skill instead** — it builds natively on a real
macOS runner, with no stub TBDs, no pinned zig, and a correct deployment target. Full measurements and the
decision record: [`wiki/research/remote-build-offload.md`](../../../wiki/research/remote-build-offload.md).

```sh
nub scripts/remote-build.ts --job clippy --detach        # start it, print the VM name, exit
nub scripts/remote-build.ts --attach <vm-name>           # stream + collect; deletes the VM
nub scripts/remote-build.ts --job clippy                 # foreground; only if you can wait
nub scripts/remote-build.ts --job test                   # the whole-workspace test suite
nub scripts/remote-build.ts --fanout 10 --job clippy     # 10 builders at once
nub scripts/remote-build.ts --reap                       # delete stray builder VMs
nub scripts/remote-build.ts --build-image                # re-bake the golden image (rare)
```

**Driving this from an agent harness? Use `--detach`, then `--attach`.** A foreground run is
SIGKILLed at the harness timeout — two minutes by default, ten at most — and SIGKILL cannot be
caught, so layer 1 never runs and the VM leaks until its server-side TTL. Measured twice: a cold
clippy killed at 2m13s, then again at 10m, each orphaning a builder. `--attach` polls in bounded
windows and exits **75** for "still running, call again"; re-run it until it returns the job's own
exit code.

## What goes remote, and what must NOT

Measured, `n2-standard-16` vs the Mac:

| Job | Remote | Mac | Verdict |
|---|---|---|---|
| warm incremental | 8.1s | ~5s | **stays local — remote loses** |
| `clippy --all-targets --all-features` | 35.3s | — | remote |
| `cargo test` (whole workspace) | 39.4s warm | — | remote (718 passed / 0 failed on Linux) |

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

- **macOS ships openrsync ("2.6.9 compatible"), not rsync 3.x.** Any 3.x-only flag fails the
  whole sync. Sync uses a `--files-from` **allowlist** built from `git ls-files`; an `--exclude`
  blocklist makes rsync walk ~99 GB of gitignored tree and time out at 120s.
- **A builder can silently degrade the binary three ways** — `aube-resolver/build.rs` ships an
  *empty primer* (falling back to network packument fetches, exit code 0) if `node` is missing, if
  `generate-primer.mjs` fails to spawn, or if it exits non-zero. The job script's `command -v node`
  check catches only the first, and that is deliberate: it does **not** set `AUBE_REQUIRE_PRIMER=1`.
  That guard protects a *shipped binary*, which is why `release.yml` sets it and `ci.yml` does not —
  a lint or test gate ships nothing. Setting it here made every remote job die in `build.rs`, because
  the primer JSON is gitignored (so the `git ls-files`-driven sync cannot carry it) and regenerating
  it needs the networked registry crawl only the release pipeline runs.
- **Under `--all-features`, `crates/nub-core/build.rs` panics** unless `runtime/addons/nub-native.node`
  is staged. The job script stages a placeholder, the same trick CI uses for its addon-less job.
- **`cmake` is mandatory** on the builder — `libz-ng-sys` fails ~35s in without it.

## Orphaned builders cannot outlive their TTL (three layers)

Stray builders are the exact failure this repo keeps paying for, so a local `finally` — which a
SIGKILL defeats — is not trusted on its own:

1. `finally` + SIGINT/SIGTERM handlers delete the VM on normal and interrupted paths.
2. Every builder carries `--max-run-duration=45m --instance-termination-action=DELETE`, so **GCE
   deletes it server-side even if the launching process dies outright.** This is the layer that
   actually holds.
3. Every VM is labelled `nub-builder=1`, so `nub scripts/remote-build.ts --reap` sweeps strays
   with no local state at all.

**Layer 2 covers every instance, including the image bake — there is no exception to remember.**
`--instance-termination-action` applies to `--max-run-duration`, not only to spot preemption, and it
accepts `STOP` as well as `DELETE`. So the bake gets `--max-run-duration=90m` with `STOP`, which is
exactly what it does to itself next anyway before imaging the disk; a builder gets 45m with `DELETE`,
because a merely-stopped VM still bills its disk.

The *create* window is genuinely not covered by layer 1 — GCE can have the VM up before `gcloud`
returns, and the placement walk spans several zones — but layer 2 is set at create time, so nothing
this tool creates can outlive its TTL.

**`--reap` will not touch a VM younger than 90 minutes.** With many agents sharing one GCP project,
an unfiltered sweep would destroy a sibling's in-flight build — the safety net becoming the hazard.
Age *is* the definition of stray here: layer 2 guarantees a healthy instance is gone by its TTL, so
anything older is abandoned. `--reap-all` forces the unfiltered sweep; it is not the default for a
reason. `--reap` exits non-zero if any delete fails, so "no output, exit 0" genuinely means clean.

A foreground build runs in the ssh foreground and is never detached LOCALLY: a detached local
build reparents to PID 1, outlives its launcher, holds locks, and is not reaped — see
`rust-build-hygiene`. `--detach` does not violate that rule. It detaches on the disposable REMOTE
VM, which is single-purpose and carries a hard `--max-run-duration`, so a forgotten job cannot
outlive its TTL or contend with anything on the dev host.

## The golden image

`--build-image` bakes a `nub-builder` image family with apt deps, rustup + the darwin target +
clippy, pinned zig, cargo-zigbuild, Node, a warmed crate registry, and pre-compiled dependency
artifacts in `$HOME/.cargo-shared-target`. That path is load-bearing and must match the one every
job exports: the bake deletes `~/src` when it finishes, so a target dir inside it would be destroyed
and every builder would cold-compile while the image advertised warm artifacts. Without it every builder would spend minutes installing a toolchain before doing any
work — the image is what makes this fast enough to reach for by default. Re-bake it when the
toolchain or the dependency graph moves substantially; day to day it just sits there.

## What this does NOT give you

- **No provenance binding.** The artifact is checked for being a runnable arm64 Mach-O with a valid
  ad-hoc signature — an ad-hoc signature attests *runnability*, not origin; anyone can produce one.
  Nothing cryptographically ties the binary to the source that was sent.
- **The golden image is a trust concentration.** It is baked once and reused for every subsequent
  build, so anyone with write access to the `pullfrog` project (or its service-account key) could
  bake something into every dev binary you later run. That is an accepted property of the design for
  a single-developer tool, not an oversight — but know it rather than discover it.
- **`StrictHostKeyChecking=no`.** Unavoidable with ephemeral VMs on recycled IPs, and the same
  pattern `gcloud-vm` already uses. Closing it properly means `--no-address` + IAP tunnelling.

## Known gap: the image is registry-warm, not artifact-warm

The bake's warm step currently stops after `cargo fetch` — exit 0, image published, no compile.
The `bash -s` stdin-truncation fix resolved this for the JOB path (a real `--job clippy` runs all
three legs to completion) but **not** for the bake, so the root cause there is still unknown and
`set -euxo` tracing is on to name the last command executed. Consequence: builders cold-compile,
so clippy takes ~250s rather than the ~35s the warm path would give, and a darwin build ~560s.
Correctness is unaffected — every measured timing here is pessimistic, not wrong.

## Cost

Spot `c3-standard-8` is a few cents per build; a 7-minute release build is about **$0.03**. A
stray cannot outlive 45 minutes. Cost is not a reason to hesitate — contention is the thing you
are spending money to avoid.
