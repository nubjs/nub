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

`scripts/remote-build.ts` dispatches a build/gate to a throwaway GCE spot VM and reports the result. **For a macOS binary use the `mac-build` skill instead** — it builds natively on a real macOS runner, with no stub TBDs, no pinned zig, and a correct deployment target. Measurements and decision record: [`wiki/research/remote-build-offload.md`](../../../wiki/research/remote-build-offload.md).

Remote VM operations require `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE=~/.config/pullfrog/vertex-service-account.json`. The script sets that override for every `gcloud` call, so invoke the script normally rather than running its VM commands by hand.

Before the first remote sync in a checkout, generate the ignored resolver data that the allowlisted sync needs:

```sh
nub vendor/aube/scripts/generate-primer.mjs --popular-names-only --popular-names-out vendor/aube/crates/aube-resolver/data/popular-top100000-v1.json
```

`popular-top100000-v1.json` is generated and gitignored; without it, `remote-build.ts` refuses to sync a clean checkout rather than sending a builder an incomplete source tree.

```sh
nub scripts/remote-build.ts --job clippy                 # the CI gate, off-box
nub scripts/remote-build.ts --job test                   # cargo test (whole workspace)
nub scripts/remote-build.ts --fanout 10 --job clippy     # 10 builders at once
nub scripts/remote-build.ts --reap                       # delete stray builder VMs
nub scripts/remote-build.ts --build-image                # re-bake the golden image (rare)
```

## What goes remote, and what must NOT

Measured, `n2-standard-16` vs the Mac:

| Job | Remote | Mac | Verdict |
|---|---|---|---|
| warm incremental | 8.1s | ~5s | **stays local — remote loses** |
| `clippy --all-targets --all-features` | 35.3s | — | remote |
| `cargo test` (whole workspace) | 39.4s warm | — | remote |

**The inner loop is deliberately not a job type.** Do not route `cargo build --profile fast` through this while iterating; you will make your loop slower.

**Why remote helps is disk, not cores.** Under load the Mac sits at ~30% idle CPU with a load average of 155, sys ~25%, disk at 3000–4000 tps at 5–6 KB/transfer — cargo fingerprint/stat churn across a dozen multi-GB target dirs on one APFS volume. Each remote builder brings its own disk; more local cores would not have helped.

## Gotchas

- **macOS ships openrsync ("2.6.9 compatible"), not rsync 3.x.** Any 3.x-only flag fails the whole sync. Sync uses a `--files-from` **allowlist** built from `git ls-files`; an `--exclude` blocklist makes rsync walk ~99 GB of gitignored tree and time out at 120s.
- **A builder can silently degrade the binary three ways** — `aube-resolver/build.rs` ships an *empty primer* (falling back to network packument fetches, exit 0) if `node` is missing, if `generate-primer.mjs` fails to spawn, or if it exits non-zero. A `command -v node` check catches only the first, so the job script exports **`AUBE_REQUIRE_PRIMER=1`** — the same var `release.yml` sets to make `build.rs` fail loud.
- **Under `--all-features`, `crates/nub-core/build.rs` panics** unless `runtime/addons/nub-native.node` is staged. The job script stages a placeholder, as CI does.
- **`cmake` is mandatory** on the builder — `libz-ng-sys` fails ~35s in without it.

## Orphaned builders cannot outlive their TTL (three layers)

A local `finally` is defeated by SIGKILL, so it is not trusted alone:

1. `finally` + SIGINT/SIGTERM handlers delete the VM on normal and interrupted paths.
2. Every builder carries `--max-run-duration=45m --instance-termination-action=DELETE`, so **GCE deletes it server-side even if the launching process dies outright.** This is the layer that holds.
3. Every VM is labelled `nub-builder=1`, so `nub scripts/remote-build.ts --reap` sweeps strays with no local state.

Layer 2 covers every instance, including the image bake. `--instance-termination-action` applies to `--max-run-duration`, not only spot preemption, and accepts `STOP` as well as `DELETE` — so the bake gets `--max-run-duration=90m` with `STOP` (what it does to itself anyway before imaging the disk) and a builder gets 45m with `DELETE` (a merely-stopped VM still bills its disk). The create window is not covered by layer 1 — GCE can have the VM up before `gcloud` returns — but layer 2 is set at create time.

**`--reap` will not touch a VM younger than 90 minutes.** With many agents sharing one GCP project, an unfiltered sweep would destroy a sibling's in-flight build. Layer 2 guarantees a healthy instance is gone by its TTL, so age *is* the definition of stray. `--reap-all` forces the unfiltered sweep. `--reap` exits non-zero if any delete fails, so "no output, exit 0" genuinely means clean.

Builds run in the ssh **foreground** and are never detached — a detached build reparents to PID 1, outlives its launcher, holds locks, and is not reaped (see `rust-build-hygiene`).

## The golden image

`--build-image` bakes a `nub-builder` image family with apt deps, rustup + the darwin target + clippy, pinned zig, cargo-zigbuild, Node, a warmed crate registry, and pre-compiled dependency artifacts in `$HOME/.cargo-shared-target`. **That path is load-bearing and must match the one every job exports** — the bake deletes `~/src` when it finishes, so a target dir inside it would be destroyed while the image advertised warm artifacts. Re-bake when the toolchain or dependency graph moves substantially.

**Known gap: the image is registry-warm, not artifact-warm.** The bake's warm step currently stops after `cargo fetch` (exit 0, image published, no compile); the `bash -s` stdin-truncation fix resolved this for the JOB path but not the bake, and `set -euxo` tracing is on to name the last command executed. Consequence: builders cold-compile, so clippy takes ~250s rather than ~35s and a darwin build ~560s. Correctness is unaffected — every timing here is pessimistic, not wrong.

## What this does NOT give you

- **No provenance binding.** The artifact is checked for being a runnable arm64 Mach-O with a valid ad-hoc signature — which attests *runnability*, not origin. Nothing cryptographically ties the binary to the source that was sent.
- **The golden image is a trust concentration.** Baked once and reused for every build, so anyone with write access to the `pullfrog` project (or its service-account key) could bake something into every dev binary you later run. An accepted property of the design, not an oversight.
- **`StrictHostKeyChecking=no`.** Unavoidable with ephemeral VMs on recycled IPs. Closing it properly means `--no-address` + IAP tunnelling.

## Cost

Spot `c3-standard-8` is a few cents per build; a 7-minute release build is about $0.03, and a stray cannot outlive 45 minutes. Cost is not a reason to hesitate — contention is what you are spending money to avoid.
