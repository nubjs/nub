---
name: gcloud-vm
description: Provision, start, reach, and use Google Cloud VMs for nub — for any real-OS work the local macOS host and Docker can't do (real Linux-kernel enforcement, real Windows/AppContainer/MSVC, a clean multi-GB build box). Invoke whenever you think "I need a Linux box" or "I need a Windows box" — you can START the existing `nub-linux`/`nub-win` instances OR CREATE a fresh one on demand with `gcloud compute instances create`. Encodes the load-bearing gotchas: IPs change on every restart (never trust a hardcoded one), SSH user is `nub` with key `~/.ssh/nub-vm`, a RUNNING box can be a wedged box (read the serial console), size any nub-building Linux box at ≥16 GB, and prefer cross-compiling on the Mac + scp'ing the artifact over building on the VM. AUTH IS NOT A BLOCKER — `gcloud` USER auth expires constantly (org session-control revokes the refresh token), but a durable SERVICE-ACCOUNT KEY makes every VM op work non-interactively: prefix any command with `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE=~/.config/pullfrog/vertex-service-account.json`. Never conclude "the VMs are unavailable" from a `Reauthentication failed. cannot prompt during non-interactive execution` error until you have tried that override.
---

# Google Cloud VMs for nub

You have `gcloud` and can create/start/stop VMs yourself. **Stop forgetting this** — any time a
task needs a real OS the Mac host + Docker can't give you (real Linux-kernel Landlock/seccomp/netns
enforcement, real Windows AppContainer/MSVC, a clean high-RAM build box, a genuinely-clean first-run
environment), spin up or start a VM. Project `pullfrog`, the existing boxes live in **`us-central1-a`**
(NOTE: the gcloud default compute zone is `us-west1-a`, so **always pass `--zone us-central1-a`
explicitly** for the existing instances).

## The two standing instances

| Name | OS | Purpose |
|---|---|---|
| `nub-linux` | Ubuntu 24.04 LTS, e2-standard-4 (4 vCPU / 16 GB), 30 GB disk | Linux-kernel enforcement (Landlock/seccomp/bwrap/netns); it has a path-bound AppArmor `bwrap-userns` profile + `apparmor_restrict_unprivileged_userns=1` reproducing a locked-down 24.04 host |
| `nub-win` | Windows Server 2022 | Windows AppContainer / DACL / MSVC — the only place real-MSVC FFI/runtime behavior surfaces (windows-gnu on the Mac is the cross-compile proxy, not a runtime) |

They are usually **TERMINATED** (stopped) to save billing. Start what you need; stop it when done.

```sh
gcloud compute instances list                                   # names + STATUS + current external IP
gcloud compute instances start nub-linux --zone us-central1-a   # ~30-60s; Windows boot is slower
gcloud compute instances stop  nub-linux --zone us-central1-a   # when you finish — they bill while RUNNING
```

## SSH — user `nub`, key `~/.ssh/nub-vm`, and the IP is DYNAMIC

- **User is `nub`** — NOT `ubuntu`/`nubuser`/`colinmcd94` (all fail `Permission denied (publickey)`).
- **Key is `~/.ssh/nub-vm`.**
- **The external IP CHANGES on every start** (no static IP reserved). NEVER hardcode it or trust a
  memory/skill note — **re-read it after every start**:
  ```sh
  IP=$(gcloud compute instances describe nub-win --zone us-central1-a \
        --format='value(networkInterfaces[0].accessConfigs[0].natIP)')
  ssh -i ~/.ssh/nub-vm -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=15 nub@"$IP" "echo ok"
  ```
- **A VM can be stopped out from under you MID-SESSION, and it comes back on a DIFFERENT IP
  (measured 2026-07-30).** This is stronger than "the IP changes on every start", and it breaks the
  natural reading of that rule — an agent that correctly resolved the IP once, at the top of its run,
  can still be holding a dead address an hour later, because something else stopped and restarted the
  box in between. A long-running lane hit exactly this: `nub-linux` disappeared under it and returned
  as `136.65.151.34`. So **re-resolve the IP on every reconnect, not once per session**, and treat a
  sudden `Connection refused`/timeout on a previously-working box as "it moved", not "it wedged" —
  re-read the IP before you go reading the serial console.
- **Always reachability-guard a VM dispatch** (`ConnectTimeout`, `timeout`), and after a fresh start
  retry with backoff for a few minutes — sshd (Windows especially) isn't up the instant STATUS flips
  RUNNING. Never let a sub-agent hang on a VM: check reachability, act, report, exit.

## Creating a NEW VM on demand

When you want an isolated/ephemeral box (a clean first-run env, a second Linux box so you don't
contend with `nub-linux`, a specific image), create one — don't wait for permission:

```sh
# Linux — size ≥16 GB if it will COMPILE nub (see the OOM gotcha); e2-standard-4 is the proven size.
gcloud compute instances create nub-linux-tmp \
  --zone us-central1-a --project pullfrog \
  --machine-type e2-standard-4 \
  --image-family ubuntu-2404-lts-amd64 --image-project ubuntu-os-cloud \
  --boot-disk-size 30GB

# Windows Server 2022
gcloud compute instances create nub-win-tmp \
  --zone us-central1-a --project pullfrog \
  --machine-type e2-standard-4 \
  --image-family windows-2022 --image-project windows-cloud \
  --boot-disk-size 50GB

# Wire the `nub` SSH key so you can reach it the same way (Linux):
gcloud compute instances add-metadata nub-linux-tmp --zone us-central1-a \
  --metadata ssh-keys="nub:$(cat ~/.ssh/nub-vm.pub)"
```

**Delete an ephemeral box when done** (`gcloud compute instances delete <name> --zone us-central1-a
--quiet`) — a created VM keeps billing its disk even when stopped.

## The gotchas that have actually bitten (each cost a cycle)

- **A RUNNING instance can be a DEAD instance — read the serial console FIRST.** `nub-linux` sat
  `RUNNING` with the firewall open yet SSH-dead for ~11 days; one command diagnosed it instantly:
  ```sh
  gcloud compute instances get-serial-port-output nub-linux --zone us-central1-a | tail -40
  ```
  Cause was `Out of memory: Killed process (rustc)` on an undersized box that wedged sshd. Serial
  output beats guessing "network problem" every time.
- **Size ≥16 GB for anything that compiles the nub Rust workspace.** An e2-small (2 GB) cannot build
  it and will OOM-wedge. e2-standard-4 (16 GB) is the proven size.
- **Write every script you send to `nub-win` as ASCII + CRLF (cost two cycles, 2026-07-30).**
  PowerShell 5.1 reads a BOM-less script in the ANSI codepage, so a UTF-8 character anywhere in the
  file — an em-dash in a *comment* is the usual culprit — fails with `The string is missing the
  terminator`, and the error points at the **last line of the file**, not the offending one. The
  diagnostic is useless and sends you hunting a quoting bug that does not exist. Keep remote
  PowerShell ASCII-only, or emit a UTF-8 BOM.
- **`IsOutputRedirected` is always True over SSH, so the first-run TTY path is UNREACHABLE there.**
  A `PowerShell` session reached by SSH reports both stdout and stderr redirected, so any
  `is_terminal()` branch silently takes the non-TTY path and cannot be exercised. Testing real
  console behavior on Windows needs a ConPTY harness or an interactive RDP session. (On Linux the
  equivalent is easy — wrap the run in `script(1)` and the TTY branch runs.)
- **For a nub RUST BUILD, use the `remote-build` skill, not this one.** `scripts/remote-build.ts`
  provisions an ephemeral spot builder from a pre-baked image, cross-compiles
  `aarch64-apple-darwin` on Linux (cargo-zigbuild, no Apple SDK) and pulls the signed binary
  back — which inverts the next bullet for the Linux/Rust case. This skill remains the right
  entry point for Windows/MSVC and for an interactive box.
- **Prefer cross-compile-on-Mac + scp the artifact over building on the VM.** *Running* a binary
  needs almost no RAM, so it sidesteps VM build capacity entirely and is much faster. For Windows,
  the VM's MSVC BuildTools is often a broken shell with no `cl.exe` — cross-compile for
  `x86_64-pc-windows-gnu` on the Mac (`rustup target add …`; `brew install mingw-w64` if the linker
  is missing), strip, `scp` the `.exe`, run it. `harness = false` test binaries are self-contained
  and ideal for this. Note the Windows home dir may be `C:/Users/nub.<HOST>/`, not `C:/Users/nub`.
- **`gcloud` auth expires** — the USER credential (`colin@pullfrog.com`) has its refresh token revoked
  periodically (org session-control policy), so interactive `gcloud auth login` is NOT durable and
  "keeps signing out." **The durable, NON-INTERACTIVE fix is a SERVICE-ACCOUNT KEY** (SA keys are exempt
  from user-session revocation). A key already exists at `~/.config/pullfrog/vertex-service-account.json`
  (SA `pullfrog-vertex-e2e@pullfrog.iam.gserviceaccount.com`, project `pullfrog` — same project as the
  VMs). Use it WITHOUT changing gcloud's global state:
  ```sh
  CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE=~/.config/pullfrog/vertex-service-account.json \
    gcloud compute instances list --project=pullfrog
  ```
  **CONFIRMED WORKING (2026-07-26): the SA key now has full compute access** — the maintainer granted the SA
  Owner on project `pullfrog`, so `list`/`describe`/`start`/`stop`/`create` all work non-interactively via the
  `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE` override above. **This is the durable, PREFERRED path — no
  `! gcloud auth login` needed for VM ops.** (Earlier the SA was Vertex-scoped and errored
  `Required 'compute.instances.list' permission`; that is resolved.) Only fall back to `! gcloud auth login`
  if the override ever errors `Reauthentication failed. cannot prompt during non-interactive execution` (SA
  key removed/rotated).
- **Judge results by behavioral/differential evidence** (EPERM vs success, byte counts, a before/after
  delta), not wall-clock — a shared VM may be contended. See [[machine-load-floor-benchmarking]].

## Related

- Memory: `nub-vm-ssh-access` (the SSH facts; its hardcoded IPs are stale by design — always re-read).
- `ci-adhoc-test` skill — the branch-scoped CI route for macOS/Windows probes when you want CI rather
  than a VM (no PR needed). Use a VM when you need an interactive box or a create-from-scratch env; use
  `ci-adhoc-test` when a committed probe on a real runner is enough.
- The Docker section of AGENTS.md — for Linux-only checks that DON'T need a real cloud kernel (a VM is
  for real-kernel enforcement and Windows; Docker is the lighter local option where it suffices).
