---
name: gcloud-vm
description: Provision, start, reach, and use Google Cloud VMs for nub — for any real-OS work the local macOS host and Docker can't do (real Linux-kernel enforcement, real Windows/AppContainer/MSVC, a clean multi-GB build box). Invoke whenever you think "I need a Linux box" or "I need a Windows box" — you can START the existing `nub-linux`/`nub-win` instances OR CREATE a fresh one on demand with `gcloud compute instances create`. Encodes the load-bearing gotchas: IPs change on every restart (never trust a hardcoded one), SSH user is `nub` with key `~/.ssh/nub-vm`, a RUNNING box can be a wedged box (read the serial console), size any nub-building Linux box at ≥16 GB, and prefer cross-compiling on the Mac + scp'ing the artifact over building on the VM. AUTH IS NOT A BLOCKER — `gcloud` USER auth expires constantly (org session-control revokes the refresh token), but a durable SERVICE-ACCOUNT KEY makes every VM op work non-interactively: prefix any command with `CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE=~/.config/pullfrog/vertex-service-account.json`. Never conclude "the VMs are unavailable" from a `Reauthentication failed. cannot prompt during non-interactive execution` error until you have tried that override.
---

# Google Cloud VMs for nub

Any time a task needs a real OS the Mac host + Docker can't give you — real Linux-kernel Landlock/seccomp/netns enforcement, real Windows AppContainer/MSVC, a clean high-RAM build box, a genuinely-clean first-run environment — spin up or start a VM. Project `pullfrog`; the existing boxes live in **`us-central1-a`**. The gcloud default zone is `us-west1-a`, so **always pass `--zone us-central1-a` explicitly**.

## ⛔⛔ THESE ARE GOOGLE CLOUD BOXES — THE AWS FREEZE DOES NOT TOUCH THEM

Project `pullfrog`. **An instruction that "the AWS account is frozen" says NOTHING about these**, and reading it as covering them cost a whole session of 20-minute CI round-trips while two admin-capable Windows VMs sat idle and billing. **The VMs are almost never the blocker — check before you route around them.**

## The standing instances (`gcloud compute instances list` is the truth; this table rots)

| Name | OS | Purpose |
|---|---|---|
| `nub-linux` | Ubuntu 24.04 LTS, e2-standard-4 | Linux-kernel enforcement (Landlock/seccomp/bwrap/netns); carries a path-bound AppArmor `bwrap-userns` profile + `apparmor_restrict_unprivileged_userns=1` reproducing a locked-down 24.04 host |
| `nub-corpus-linux` | Ubuntu, e2-standard-8 | corpus / harness work |
| `nub-win2`, `nub-win3` | Windows Server 2022, e2-standard-8 | Windows AppContainer / DACL / MSVC — the only place real-MSVC FFI/runtime behavior surfaces. **Both confirmed admin (`IsInRole(Administrator)=True`) with `logman`/`wpr`/`tracerpt` present, so full ETW kernel tracing is available.** |

They are usually TERMINATED to save billing. Start what you need; stop it when done.

## ⛔ LIFECYCLE HYGIENE — DIAGNOSE AT FIRST DETECTION, THEN DELETE

**These boxes are THROWAWAY.** The maintainer's standing instruction: kill anything unreachable rather than nursing it, and create a fresh one at whatever spec the job needs. A borked box that keeps running is pure burn.

**The rule that actually matters: the moment you find a box unreachable, DIAGNOSE IT THEN — not later.** Once it is deleted, or once weeks pass, every trace of what went wrong is gone and you are left guessing. Read the serial console (`gcloud compute instances get-serial-port-output <name> --zone us-central1-a | tail -40`) BEFORE deleting, and write down what you find.

```sh
gcloud compute instances delete <name> --zone us-central1-a --quiet   # a stopped box still bills its disk
```

**⛔ A FULL DISK IS INDISTINGUISHABLE FROM A BROKEN BOX, AND IT IS THE MOST COMMON CAUSE HERE.** Measured 2026-08-05 on `nub-linux`: `/dev/root 193G 193G 0 100%`, of which **166 GB was abandoned `~/.cache/nub-search-*` harness fixture roots** (8 of them) — no runaway process, just temp dirs nothing ever swept. The symptoms all look like a dead machine: `scp: write remote "x": Failure`, zero-byte outputs from commands that "succeeded", a transferred file that reads as `cannot execute binary file`. **Check `df -h /` FIRST on any box behaving strangely**; `rm -rf ~/.cache/nub-search-*` took it from 100% to 14% and fully restored the box, no recreation needed.

**Re-test reachability rather than trusting a remembered "unreachable".** The external IP changes on every start, so a stale IP reads exactly like a dead box.

```sh
gcloud compute instances list                                   # names + STATUS + current external IP
gcloud compute instances start nub-linux --zone us-central1-a   # ~30-60s; Windows boot is slower
gcloud compute instances stop  nub-linux --zone us-central1-a   # when you finish — they bill while RUNNING
```

## SSH — user `nub`, key `~/.ssh/nub-vm`, and the IP is DYNAMIC

- **User is `nub`** — not `ubuntu`/`nubuser`/`colinmcd94` (all fail `Permission denied (publickey)`).
- **Key is `~/.ssh/nub-vm`.**
- **The external IP changes on every start** (no static IP reserved). Never hardcode it or trust a memory/skill note:
  ```sh
  IP=$(gcloud compute instances describe nub-win --zone us-central1-a \
        --format='value(networkInterfaces[0].accessConfigs[0].natIP)')
  ssh -i ~/.ssh/nub-vm -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o ConnectTimeout=15 nub@"$IP" "echo ok"
  ```
- **Re-resolve the IP on every reconnect, not once per session.** A VM can be stopped out from under you mid-session and return on a different IP, so an address resolved at the top of a long run can be dead an hour later. Treat a sudden `Connection refused`/timeout on a previously-working box as "it moved" and re-read the IP before reading the serial console.
- **Always reachability-guard a VM dispatch** (`ConnectTimeout`, `timeout`), and after a fresh start retry with backoff for a few minutes — sshd (Windows especially) isn't up the instant STATUS flips RUNNING. Never let a sub-agent hang on a VM: check reachability, act, report, exit.

## Creating a NEW VM on demand

For an isolated/ephemeral box (a clean first-run env, a second Linux box so you don't contend with `nub-linux`, a specific image), create one — don't wait for permission:

```sh
# Linux — size ≥16 GB if it will COMPILE nub (see the OOM gotcha); e2-standard-4 is the proven size.
gcloud compute instances create nub-linux-tmp \
  --zone us-central1-a --project pullfrog \
  --machine-type e2-standard-4 \
  --image-family ubuntu-2404-lts-amd64 --image-project ubuntu-os-cloud \
  --boot-disk-size 30GB

# Wire the `nub` SSH key so you can reach it the same way (Linux):
gcloud compute instances add-metadata nub-linux-tmp --zone us-central1-a \
  --metadata ssh-keys="nub:$(cat ~/.ssh/nub-vm.pub)"
```

### Windows: `ssh-keys` metadata alone does NOT get you in — provision it yourself

Measured end-to-end 2026-08-04 while building `nub-win3`, after `nub-win` and `nub-win2` both proved unreachable. **A Windows box created the "obvious" way is not SSH-able**, and each of the three failures below looks like a different problem, so use the failure MODE to tell them apart rather than guessing:

| symptom | meaning |
| --- | --- |
| `Operation timed out` | OpenSSH Server is **not installed** — Windows Server 2022 does not ship it enabled, and `enable-windows-ssh=TRUE` does **not** install it |
| `Connection refused` | sshd installed but not started yet (still booting) |
| `Permission denied (publickey…)` | sshd is **up and listening**; only the key is missing |

Do it in ONE creation, with a startup script that installs sshd *and* provisions the key itself. The guest agent did not provision keys here across two resets, so **do not depend on it**:

```sh
cat > /tmp/win-ssh.ps1 <<PS
Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd
\$pw = ConvertTo-SecureString (([guid]::NewGuid()).ToString() + '!Aa1') -AsPlainText -Force
if (-not (Get-LocalUser -Name nub -ErrorAction SilentlyContinue)) {
  New-LocalUser -Name nub -Password \$pw -PasswordNeverExpires -AccountNeverExpires
}
Add-LocalGroupMember -Group Administrators -Member nub -ErrorAction SilentlyContinue
# ⛔ An ADMIN user authenticates via administrators_authorized_keys — a key in the user's own
# ~/.ssh/authorized_keys is SILENTLY IGNORED, which is what "Permission denied" was really saying.
\$ak = 'C:\ProgramData\ssh\administrators_authorized_keys'
Set-Content -Path \$ak -Value '$(cat ~/.ssh/nub-vm.pub)' -Encoding ascii
icacls \$ak /inheritance:r
icacls \$ak /grant 'Administrators:F' /grant 'SYSTEM:F'   # sshd REFUSES a loosely-ACL'd key file
Restart-Service sshd
PS

gcloud compute instances create nub-win-tmp \
  --zone us-central1-a --project pullfrog \
  --machine-type e2-standard-8 \
  --image-family windows-2022 --image-project windows-cloud \
  --boot-disk-size 200GB --boot-disk-type pd-ssd \
  --metadata-from-file windows-startup-script-ps1=/tmp/win-ssh.ps1
```

- **Budget ~10-15 minutes** from `create` to first successful SSH; first boot plus sysprep is slow, and the startup script runs partway through it. Poll rather than waiting on one attempt.
- **The default SSH shell is `cmd.exe`, not PowerShell.** A `;`-separated PowerShell one-liner dies with `Invalid argument/option - ';'`. Wrap it: `ssh … 'powershell -NoProfile -Command "…"'`.
- **Disk: 200 GB.** The image is 50 GB and gcloud warns the root partition may need manual resizing; Server 2022 resized it automatically here (179 GB free on first login). A debug box that builds nub and installs npm trees fills a 50 GB disk.
- **Read the serial console to confirm the script ran** — `get-serial-port-output … | grep windows-startup-script-ps1` shows each line's output, including `processed file: C:\ProgramData\ssh\administrators_authorized_keys`.
- A firewall rule is NOT the problem: `default-allow-ssh` is `0.0.0.0/0 tcp:22` with **no target tags**, so it already covers every instance. Don't go hunting network tags.

### Running a LONG job (a cargo build) on a Windows box — use a scheduled task, and scp the script

A cold `cargo build --release -p nub-cli` is ~30-45 min, far past any single SSH call. Four approaches were tried on `nub-win3`; three failed, and the failure MODES are the useful part because two of them are SILENT:

| approach | what happened |
| --- | --- |
| `Start-Process … -WindowStyle Hidden` | ran, then **died after the dependency downloads with NO error line**. The SSH session's job object closes and takes the child with it. A vanished process with an empty log is this, not a build error. |
| multi-line PowerShell piped to `powershell -Command -` over SSH stdin | the script silently **never materialised** (`if exist … NO_BAT`). Quoting dies somewhere between zsh, ssh and PowerShell. |
| scheduled task running as **SYSTEM**, cargo via `~/.cargo/bin/cargo.exe` | `error: rustup could not choose a version of cargo to run` → `EXIT=1`. **rustup's default toolchain is per-USER**, and SYSTEM has none. |
| **scp the `.bat`, then run it as a scheduled task, calling the toolchain binary directly** | works |

```sh
# Write the .bat LOCALLY with CRLF and scp it — do NOT try to author it over SSH stdin.
printf '@echo off\r\ncd /d C:\\nub\r\nset RUSTUP_HOME=C:\\Users\\nub\\.rustup\r\nset CARGO_HOME=C:\\Users\\nub\\.cargo\r\n"C:\\Users\\nub\\.rustup\\toolchains\\stable-x86_64-pc-windows-msvc\\bin\\cargo.exe" build --release -p nub-cli > C:\\nub\\build.log 2>&1\r\necho EXIT=%%ERRORLEVEL%% >> C:\\nub\\build.log\r\n' > /tmp/dobuild.bat
scp -i ~/.ssh/nub-vm /tmp/dobuild.bat nub@"$IP":C:/nub/dobuild.bat
# /RU SYSTEM is fine HERE because a build only needs a toolchain — never reuse this line for a measurement.
ssh -i ~/.ssh/nub-vm nub@"$IP" 'cmd /c "schtasks /Create /TN nubbuild /TR C:\nub\dobuild.bat /SC ONCE /ST 00:00 /RL HIGHEST /RU SYSTEM /F & schtasks /Run /TN nubbuild"'
# then POLL: (Get-Process cargo,rustc).Count, plus the tail of build.log
```

⛔⛔ **USE THIS FOR BUILDS ONLY — NEVER FOR A MEASUREMENT. A scheduled task runs as `SYSTEM`, and `SYSTEM` IS NOT A NORMAL USER.** For a build that costs a `PATH` fix (the rustup row above). For anything that MEASURES OS-enforced behaviour it silently changes the answer, because `SYSTEM` holds privileges an ordinary account does not — `SeCreateSymbolicLinkPrivilege` above all — and `os.homedir()` becomes `C:\Windows\system32\config\systemprofile`.

Measured 2026-08-04, and the verdict did not give it away: a build-jail package whose real failure is a refused symlink was re-measured under a SYSTEM task, produced **exactly the expected grant** with a clean control, and was reported as a validated reproduction. The artifact refuted it — the per-cell log was 1,476 bytes containing only a catalog warning naming the `systemprofile` path, and the symlink error appeared in **3 of 54** logs where the real-user CI run shows **51 of 54**. Running as `SYSTEM` had bypassed the very mechanism under test while landing on the same answer by another route.

- **`whoami` is the cheap guard.** Print it as the first line of any script whose result you will believe, and assert on it: `nub-win3\nub` good, `nt authority\system` void.
- **Get the right context by running through SSH itself, not a task** — an SSH session already runs as `nub`. Wrap the call in a harness-tracked background command (`run_in_background`) rather than a scheduled task: the session stays alive for hours, so the ~10-minute foreground cap that pushed you toward `schtasks` never applies. Detaching *within* the session (`Start-Process`, `nohup`-alikes) still dies to the job object — the point is that the SSH call itself is the long-lived process.
- **`schtasks /RU <user> /RP <password>` also works but needs a password you probably do not have** — the `nub` account is created with a throwaway GUID password, and `net user nub <new>` fails `The user name or password is incorrect` from a non-elevated SSH token. Prefer the SSH route.
- **Call the TOOLCHAIN binary, not the rustup shim** (`.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe`) so it does not matter which user rustup was configured for.
- **The scheduled task is what makes failure VISIBLE** — it redirects to a log and records `EXIT=<n>`, where the detached-process approach just disappears.
- **Toolchain prerequisites, ~15 min before any build:** VS Build Tools (`--add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.Windows11SDK.22621 --includeRecommended`) then `rustup-init.exe -y --default-toolchain stable --profile minimal`. Verify by running `cargo --version` from its absolute path, not by trusting the installer's exit.
- **Node/git come from vendor installers**, silently: node `.msi` via `msiexec /qn`, Git-for-Windows `.exe` via `/VERYSILENT /NORESTART`. Neither is on `PATH` for an existing SSH session — read `[Environment]::GetEnvironmentVariable("Path","Machine")` or use absolute paths (`C:\Program Files\Git\cmd\git.exe`).

**Delete an ephemeral box when done** — a created VM keeps billing its disk even when stopped:

```sh
gcloud compute instances delete <name> --zone us-central1-a --quiet
```

## Auth — the service-account key is the durable path

The USER credential (`colin@pullfrog.com`) has its refresh token revoked periodically by org session-control policy, so `gcloud auth login` is not durable. A service-account key is exempt and works non-interactively without changing gcloud's global state:

```sh
CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE=~/.config/pullfrog/vertex-service-account.json \
  gcloud compute instances list --project=pullfrog
```

The SA (`pullfrog-vertex-e2e@pullfrog.iam.gserviceaccount.com`, project `pullfrog`) has Owner, so `list`/`describe`/`start`/`stop`/`create` all work through the override. **This is the preferred path.** Fall back to `! gcloud auth login` only if the override itself errors `Reauthentication failed. cannot prompt during non-interactive execution` (key removed/rotated).

## Gotchas

- **A RUNNING instance can be a DEAD instance — read the serial console FIRST:**
  ```sh
  gcloud compute instances get-serial-port-output nub-linux --zone us-central1-a | tail -40
  ```
  A wedged box typically shows `Out of memory: Killed process (rustc)`. Serial output beats guessing "network problem."
- **Size ≥16 GB for anything that compiles the nub Rust workspace.** An e2-small (2 GB) cannot build it and will OOM-wedge. e2-standard-4 (16 GB) is the proven size.
- **Write every script you send to `nub-win` as ASCII + CRLF.** PowerShell 5.1 reads a BOM-less script in the ANSI codepage, so a UTF-8 character anywhere in the file — an em-dash in a *comment* is the usual culprit — fails with `The string is missing the terminator`, and the error points at the **last line of the file**, not the offending one. Keep remote PowerShell ASCII-only, or emit a UTF-8 BOM.
- **`IsOutputRedirected` is always True over SSH, so the first-run TTY path is unreachable there.** Any `is_terminal()` branch silently takes the non-TTY path. Testing real console behavior on Windows needs a ConPTY harness or an interactive RDP session. (On Linux, wrap the run in `script(1)` and the TTY branch runs.)
- **For a nub RUST BUILD, use the `remote-build` skill, not this one.** `scripts/remote-build.ts` provisions an ephemeral spot builder from a pre-baked image and cross-compiles `aarch64-apple-darwin` on Linux. This skill remains the entry point for Windows/MSVC and for an interactive box.
- **Prefer cross-compile-on-Mac + scp the artifact over building on the VM.** Running a binary needs almost no RAM. For Windows, the VM's MSVC BuildTools is often a broken shell with no `cl.exe` — cross-compile for `x86_64-pc-windows-gnu` on the Mac (`rustup target add …`; `brew install mingw-w64` if the linker is missing), strip, `scp` the `.exe`, run it. `harness = false` test binaries are self-contained and ideal for this. The Windows home dir may be `C:/Users/nub.<HOST>/`, not `C:/Users/nub`.
- **Judge results by behavioral/differential evidence** (EPERM vs success, byte counts, a before/after delta), not wall-clock — a shared VM may be contended.

## Related

- `ci-adhoc-test` — the branch-scoped CI route for macOS/Windows probes (no PR needed). Use a VM when you need an interactive box or a create-from-scratch env; use `ci-adhoc-test` when a committed probe on a real runner is enough.
- AGENTS.md's Docker section — for Linux-only checks that don't need a real cloud kernel.
