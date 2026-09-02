---
name: remote-build
description: >-
  Run a nub Rust build, clippy gate, test suite, or ad-hoc fixture script on an ephemeral
  Google Cloud spot VM instead of the dev Mac. Invoke (via the Skill tool) whenever you
  are about to start a COLD build, `cargo clippy --all-targets --all-features`, a full
  `cargo test`, a `release` build, or a fixture sweep that needs no macOS-specific
  behavior. THE RULE THIS SKILL EXISTS TO CARRY: the VM is the DEFAULT for all of those —
  not a contention fallback — and only the ~5s warm incremental loop and macOS-native
  checks stay local, because remote loses exactly those two. Also the go-to when someone
  asks to "build this without hammering my machine" or to reclaim CPU from builds. For a
  macOS binary use scripts/mac-build.ts (a real macOS runner), never a cross-compile.
  Pairs with `dev-loop` (the local loop), `rust-build` (target-dir sharing),
  `rust-build-hygiene` (not orphaning builds), `ad-hoc-test` (what to put in an adhoc
  script), `cpu-reduction` (clearing residue that already accumulated), and `gcloud-vm`
  (the underlying VM mechanics).
metadata:
  internal: true
---

# Remote builds — get the heavy Rust jobs off the Mac

`scripts/remote-build.ts` dispatches a build/gate/probe to a throwaway GCE spot VM and reports the result. **The VM is the default home for every job listed below; the Mac keeps only the warm incremental loop and macOS-native behavior.** For a macOS binary use [`scripts/mac-build.ts`](../../../scripts/mac-build.ts) instead — it builds natively on a real macOS runner, with no stub TBDs, no pinned zig, and a correct deployment target. Read its file header before you run it: it carries the full rationale and the one trade-off that bites, which is that the transport is git, so your work must be PUSHED before it can be built. Measurements and decision record: `internal/research/remote-build-offload.md` (gitignored; absent in a clean checkout).

```sh
nub scripts/remote-build.ts --job clippy --detach        # start it, print the VM name, exit
nub scripts/remote-build.ts --attach <vm-name>           # stream + collect; deletes the VM
nub scripts/remote-build.ts --job clippy                 # foreground; only if you can wait
nub scripts/remote-build.ts --job test                   # the whole-workspace test suite
nub scripts/remote-build.ts --job adhoc --script f.sh    # build nub, then run YOUR script
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
| `cargo test` (whole workspace) | 39.4s warm | — | remote |
| ad-hoc fixture runs (`--job adhoc`) | — | — | remote, unless macOS-specific or a tight iterate loop |

**The inner loop is deliberately not a job type.** Do not route `cargo build --profile fast` through this while iterating; you will make your loop slower.

**`--job adhoc` runs YOUR script against a freshly built binary.** The payload runs at the synced repo root with `NUB_BIN` naming a `--profile fast` build of the synced tree (real addon staged, not the placeholder), and its exit code is the job's. The image carries Node 26 + npm; install any other reference tool (pnpm, bun) inside the script. Each invocation is its own throwaway VM — batch a sweep into one script rather than one VM per fixture. What belongs in the script: the `ad-hoc-test` skill.

**Why remote helps is disk, not cores.** Under load the Mac sits at ~30% idle CPU with a load average of 155, sys ~25%, disk at 3000–4000 tps at 5–6 KB/transfer — cargo fingerprint/stat churn across a dozen multi-GB target dirs on one APFS volume. Each remote builder brings its own disk; more local cores would not have helped.

## Gotchas

- **macOS ships openrsync ("2.6.9 compatible"), not rsync 3.x.** Any 3.x-only flag fails the whole sync. Sync uses a `--files-from` **allowlist** built from `git ls-files`; an `--exclude` blocklist makes rsync walk ~99 GB of gitignored tree and time out at 120s.
- **A builder can silently degrade the binary three ways** — `aube-resolver/build.rs` ships an *empty primer* (falling back to network packument fetches, exit 0) if `node` is missing, if `generate-primer.mjs` fails to spawn, or if it exits non-zero. The job script’s `command -v node` check catches only the first, and that is deliberate: it does **not** set `AUBE_REQUIRE_PRIMER=1`. That guard protects a *shipped binary*, which is why `release.yml` sets it and `ci.yml` does not — a lint or test gate ships nothing. Setting it here made every remote job die in `build.rs`, because the primer JSON is gitignored (so the `git ls-files`-driven sync cannot carry it) and regenerating it needs the networked registry crawl only the release pipeline runs.
- **Under `--all-features`, `crates/nub-core/build.rs` panics** unless `runtime/addons/nub-native.node` is staged AND the vendored `runtime/node_modules` are present. The job script stages a placeholder addon and grants `NUB_ALLOW_INCOMPLETE_RUNTIME=1`, as CI's clippy job does — a gate ships nothing, so it opts out rather than vendoring npm packages onto the builder. The image bake carries the same grant; without it the warm-up legs fail best-effort and publish a cold image.
- **`cmake` is mandatory** on the builder — `libz-ng-sys` fails ~35s in without it.

## Orphaned builders cannot outlive their TTL (three layers)

A local `finally` is defeated by SIGKILL, so it is not trusted alone:

1. `finally` + SIGINT/SIGTERM handlers delete the VM on normal and interrupted paths.
2. Every builder carries `--max-run-duration=45m --instance-termination-action=DELETE`, so **GCE deletes it server-side even if the launching process dies outright.** This is the layer that holds.
3. Every VM is labelled `nub-builder=1`, so `nub scripts/remote-build.ts --reap` sweeps strays with no local state.

Layer 2 covers every instance, including the image bake. `--instance-termination-action` applies to `--max-run-duration`, not only spot preemption, and accepts `STOP` as well as `DELETE` — so the bake gets `--max-run-duration=90m` with `STOP` (what it does to itself anyway before imaging the disk) and a builder gets 45m with `DELETE` (a merely-stopped VM still bills its disk). The create window is not covered by layer 1 — GCE can have the VM up before `gcloud` returns — but layer 2 is set at create time.

**`--reap` will not touch a VM younger than 90 minutes.** With many agents sharing one GCP project, an unfiltered sweep would destroy a sibling's in-flight build. Layer 2 guarantees a healthy instance is gone by its TTL, so age *is* the definition of stray. `--reap-all` forces the unfiltered sweep. `--reap` exits non-zero if any delete fails, so "no output, exit 0" genuinely means clean.

A build is never detached **locally** — a detached local build reparents to PID 1, outlives its launcher, holds locks, and is not reaped (see `rust-build-hygiene`). `--detach` is not that: it detaches on the disposable **remote** VM, which is single-purpose and carries a hard `--max-run-duration`, so a forgotten job cannot outlive its TTL or contend with anything on the dev host.

## The golden image

`--build-image` bakes a `nub-builder` image family with apt deps, rustup + the darwin target + clippy, pinned zig, cargo-zigbuild, Node, a warmed crate registry, and pre-compiled dependency artifacts in `$HOME/.cargo-shared-target`. **That path is load-bearing and must match the one every job exports** — the bake deletes `~/src` when it finishes, so a target dir inside it would be destroyed while the image advertised warm artifacts. Re-bake when the toolchain or dependency graph moves substantially, **or when the warm block below changes** — a warm-up that no longer matches `jobScript` is exactly as cold as no warm-up.

**The bake covers every job — but a given image is only as warm as its bake.** The warm block runs every cargo invocation `jobScript` emits, verbatim: the root clippy, the `nub-native` clippy, `cargo test --workspace --no-run`, `(cd crates/nub-native && cargo build)`, and `cargo build -p nub-cli --profile fast` (the adhoc job's binary). `--build-image` is manual-only, though — no workflow or cron invokes it — so the live `nub-builder` family stays exactly as it was last baked. **Check the image date before sizing a run** (`gcloud compute images list --project pullfrog --no-standard-images --format='table(name,family,creationTimestamp)' --sort-by=~creationTimestamp` — the default columns carry no timestamp): an image baked before the warm block covered clippy leaves builders cold-compiling at ~250s rather than ~35s, and that is the state of the family until someone re-bakes. Cargo fingerprints on the command shape, so a warm-up differing by driver, profile, package scope, or feature set produces artifacts the job cannot use and the image goes silently cold.

## What this does NOT give you

- **No provenance binding.** The artifact is checked for being a runnable arm64 Mach-O with a valid ad-hoc signature — which attests *runnability*, not origin. Nothing cryptographically ties the binary to the source that was sent.
- **The golden image is a trust concentration.** Baked once and reused for every build, so anyone with write access to the `pullfrog` project (or its service-account key) could bake something into every dev binary you later run. An accepted property of the design, not an oversight.
- **`StrictHostKeyChecking=no`.** Unavoidable with ephemeral VMs on recycled IPs. Closing it properly means `--no-address` + IAP tunnelling.

## Cost

Spot `c3-standard-8` is a few cents per build; a 7-minute release build is about $0.03, and a stray cannot outlive 45 minutes. Cost is not a reason to hesitate — contention is what you are spending money to avoid.
