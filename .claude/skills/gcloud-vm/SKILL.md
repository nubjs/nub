---
name: gcloud-vm
description: Provision, start, reach, and use Google Cloud VMs for nub — for any real-OS work the local macOS host and Docker can't do (real Linux-kernel enforcement, real Windows/AppContainer/MSVC, a clean multi-GB build box). Invoke whenever you think "I need a Linux box" or "I need a Windows box" — you can START the existing `nub-linux`/`nub-win` instances OR CREATE a fresh one on demand with `gcloud compute instances create`. Encodes the load-bearing gotchas: IPs change on every restart (never trust a hardcoded one), SSH user is `nub` with a throwaway key per box under `~/.cache/nub/vm-keys/<name>/`, never through the SSH agent, a RUNNING box can be a wedged box (read the serial console), size any nub-building Linux box at ≥16 GB, and prefer cross-compiling on the Mac + scp'ing the artifact over building on the VM. AUTH IS NOT A BLOCKER — `gcloud` USER auth expires constantly (org session-control revokes the refresh token), but Keyward holds a durable SERVICE-ACCOUNT KEY that makes every VM op work: run each command as `keyward --reason "<what it is for>" -- gcloud …` (one Touch ID covers ten minutes of this repository's runs; a locked screen means wait for the unlock, not retry). Never conclude "the VMs are unavailable" from a `Reauthentication failed. cannot prompt during non-interactive execution` error — that is the user login; go through Keyward. Boxes you create do NOT get `--max-run-duration`: it cannot be changed while the instance runs, so it strands long jobs — use the guest-side idle auto-stop instead, and delete the box when done.
---

# Google Cloud VMs for nub

Any time a task needs a real OS the Mac host + Docker can't give you — real Linux-kernel Landlock/seccomp/netns enforcement, real Windows AppContainer/MSVC, a clean high-RAM build box, a genuinely-clean first-run environment — spin up or start a VM. Project `pullfrog`; the existing boxes live in **`us-central1-a`**. gcloud runs through Keyward with none of its saved configuration (no default zone, region or account), so **always pass `--project pullfrog` and `--zone us-central1-a` explicitly**. Every `gcloud` below runs as `keyward --reason "<what it is for>" -- gcloud …`; see [Auth](#auth--keyward-holds-the-service-account-key).

## ⛔⛔ THESE ARE GOOGLE CLOUD BOXES — THE AWS FREEZE DOES NOT TOUCH THEM

Project `pullfrog`. **An instruction that "the AWS account is frozen" says NOTHING about these**, and reading it as covering them cost a whole session of 20-minute CI round-trips while two admin-capable Windows VMs sat idle and billing. **The VMs are almost never the blocker — check before you route around them.**

## The standing instances (`gcloud compute instances list --project pullfrog` is the truth; this table rots)

| Name | OS | Purpose |
|---|---|---|
| `nub-linux` | Ubuntu 24.04 LTS, e2-standard-4 | Linux-kernel enforcement (Landlock/seccomp/bwrap/netns); carries a path-bound AppArmor `bwrap-userns` profile + `apparmor_restrict_unprivileged_userns=1` reproducing a locked-down 24.04 host. **The one box worth preserving** — that profile is hand-built, so it auto-STOPs rather than auto-deletes. It carries an 8h `maxRunDuration` from before that flag was dropped for new boxes; that cap CANNOT be raised while it runs, so a job here longer than 8h gets stopped mid-flight. `STOP` keeps the disk, so restart and resume rather than losing it |

**`nub-corpus-linux`, `nub-win2` and `nub-win3` were DELETED 2026-09-01** as part of the billing cleanup below, along with `nub-win`, `nub-devloop` and `nub-win4` before them. Do not go looking for them, and do not treat their absence as an outage: recreate what you need from the recipes below, at the spec the job actually needs. The Windows boxes in particular were confirmed admin (`IsInRole(Administrator)=True`) with `logman`/`wpr`/`tracerpt` present, so a fresh Server 2022 box gives you full ETW kernel tracing the same way.

They are usually TERMINATED to save billing. Start what you need, and DELETE it when done — deleting is what stops the disk charge.

## ⛔ DO NOT PUT `--max-run-duration` ON A BOX YOU CREATE (reversed 2026-09-04)

**A run-duration budget cannot be changed while the instance is running, and that is disqualifying.** Measured 2026-09-04, on a box holding a 1h25m acceptance sweep that needed more time than its budget allowed:

```
ERROR: Max run duration cannot be changed while the instance is running.
```

`gcloud compute instances update` does not accept the flag at all; `set-scheduling` accepts it and the API refuses. `--termination-time` is refused the same way, and *clearing* the duration counts as changing it. The only way out is to stop the instance — which kills the work the box exists for. So the flag turns a recoverable "this is taking longer than I thought" into an unrecoverable one, and it does it at the exact moment the box is most valuable.

Use idle auto-stop instead (below). It targets the actual waste — a box sitting doing nothing — rather than capping useful work, and it can be tuned from inside the guest at any time.

**The cost facts that motivated the budget are still true, and still worth knowing.** Measured 2026-09-01: five boxes alive, four idle between 10 hours and 26 days, carrying **$94/mo of disk that bills while TERMINATED**. `nub-win3` alone was ~$205/mo of runtime — 322 running hours in 30 days, of which **$119 was Windows licensing rather than compute** — on top of $34/mo for its 200 GB `pd-ssd`.

- **`DELETE` is what stops the standing charge.** The boot disk is `autoDelete` by default, so deleting the VM takes it too; merely stopping one leaves a 200 GB `pd-ssd` billing ~$34/mo forever. Delete anything named `-tmp`, `-probe` or a builder the moment it is done.
- **Windows is where the money is.** The licence is [$0.046 per vCPU-hour](https://docs.cloud.google.com/compute/docs/instances/windows/ms-licensing-faq), so an `e2-standard-8` Windows box costs **more in licence than in compute** ($0.368/h vs $0.268/h). Size Windows boxes by what the job needs, and prefer `--boot-disk-type pd-balanced` ($0.10/GB/mo) over `pd-ssd` ($0.17) unless the disk is genuinely the bottleneck.
- **Verify a cleanup by listing EVERY instance and reading the list** — never by grepping for the name you expected. A cleanup check that matched `nub-win` passed while its own box, named `wingrants-…`, ran on for hours.

Automated short-lived builders under `scripts/remote-build.ts` are a separate case and still carry the flag: they run ~45m unattended, nobody is there to extend anything, and the launcher dying is a real leak path.

### Idle auto-stop — the mechanism that replaces a run-duration cap

This is the cost mechanism, now that the run-duration cap is gone: it targets idleness directly rather than capping useful work. Every box ships it in the same `create` call so none exists without it, and the box powers itself off once nothing has happened for 30 minutes. GCE reports a guest shutdown as `TERMINATED`, so the disk survives and a `start` brings it back. It needs no credential and tells nobody: the maintainer does not want to hear about idle boxes, only for them to stop.

| OS | Script | Counts as busy |
|---|---|---|
| Linux | [`scripts/vm-idle/linux-startup.sh`](../../../scripts/vm-idle/linux-startup.sh), passed as the box's `startup-script` | an established connection on port 22; a `cargo`, `rustc` or `nub` process; 1-minute load above 0.2 |
| Windows | [`scripts/vm-idle/windows-startup.ps1`](../../../scripts/vm-idle/windows-startup.ps1), appended to the box's `windows-startup-script-ps1` | an established connection on port 22 or 3389; a running scheduled task in the root folder, which is how a long build runs. CPU is deliberately not a signal, because a Defender spike would keep restarting the idle window |

- **An open connection is the load-bearing check**, because an agent driving a long job over SSH holds one the whole time, and the timer must not shoot the box out from under it. It is read from the socket table: `who` lists only sessions with a terminal, so it misses `ssh host command`.
- **Tune the idle time up, never down, for a box you interact with by hand.** Thirty minutes is chosen so a poweroff never lands mid-thought. There is no run-duration backstop behind it any more, so a wedged timer means a box that bills until someone deletes it — check `instances list` when you finish with a box.
- **A stopped box still bills its disk.** The timer ends the compute charge, not the storage one, so DELETE a box when the job is done and confirm with a full `instances list`.

## ⛔ LIFECYCLE HYGIENE — DIAGNOSE AT FIRST DETECTION, THEN DELETE

**These boxes are THROWAWAY.** The maintainer's standing instruction: kill anything unreachable rather than nursing it, and create a fresh one at whatever spec the job needs. A borked box that keeps running is pure burn.

**The rule that actually matters: the moment you find a box unreachable, DIAGNOSE IT THEN — not later.** Once it is deleted, or once weeks pass, every trace of what went wrong is gone and you are left guessing. Read the serial console (`keyward --reason "nub: read <name>'s serial console" -- gcloud compute instances get-serial-port-output <name> --zone us-central1-a --project pullfrog | tail -40`) BEFORE deleting, and write down what you find.

```sh
keyward --reason "nub: delete the VM <name>" -- \
  gcloud compute instances delete <name> --zone us-central1-a --project pullfrog --quiet   # a stopped box still bills its disk
rm -rf ~/.cache/nub/vm-keys/<name>                                      # its key goes with it
```

**⛔ A FULL DISK IS INDISTINGUISHABLE FROM A BROKEN BOX, AND IT IS THE MOST COMMON CAUSE HERE.** Measured 2026-08-05 on `nub-linux`: `/dev/root 193G 193G 0 100%`, of which **166 GB was abandoned `~/.cache/nub-search-*` harness fixture roots** (8 of them) — no runaway process, just temp dirs nothing ever swept. The symptoms all look like a dead machine: `scp: write remote "x": Failure`, zero-byte outputs from commands that "succeeded", a transferred file that reads as `cannot execute binary file`. **Check `df -h /` FIRST on any box behaving strangely**; `rm -rf ~/.cache/nub-search-*` took it from 100% to 14% and fully restored the box, no recreation needed.

**Re-test reachability rather than trusting a remembered "unreachable".** The external IP changes on every start, so a stale IP reads exactly like a dead box.

```sh
keyward --reason "nub: list the VMs" -- gcloud compute instances list --project pullfrog # names + STATUS + current external IP
keyward --reason "nub: start nub-linux" -- gcloud compute instances start nub-linux --zone us-central1-a --project pullfrog  # ~30-60s; Windows boot is slower
keyward --reason "nub: stop nub-linux" -- gcloud compute instances stop  nub-linux --zone us-central1-a --project pullfrog   # when you finish — they bill while RUNNING
```

## SSH — user `nub`, a throwaway key per box, and the IP is DYNAMIC

- **User is `nub`** — not `ubuntu`/`nubuser`/`colinmcd94` (all fail `Permission denied (publickey)`).
- **Each box has its own throwaway key, at `~/.cache/nub/vm-keys/<name>/id_ed25519`.** There is no shared key: the old `~/.ssh/nub-vm` is gone, so a box that still trusts only it needs a key of its own first (next section). Not under `~/.cache/nub/remote-build/keys/`, whose keys `remote-build --reap` sweeps once their VM is not a listed builder.
- **Never let a VM connection reach the SSH agent.** The maintainer's ssh config routes every connection through the Keyward agent, where each signature can raise a Touch ID sheet. Pass `-o IdentitiesOnly=yes -o IdentityAgent=none -i "$KEY"` to every `ssh` and `scp`; `IdentitiesOnly` alone still asks the agent.
- **The external IP changes on every start** (no static IP reserved). Never hardcode it or trust a memory/skill note:
  ```sh
  NAME=nub-linux
  KEY=~/.cache/nub/vm-keys/$NAME/id_ed25519
  SSH_OPTS=(-i "$KEY" -o IdentitiesOnly=yes -o IdentityAgent=none
            -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=15)
  IP=$(keyward --reason "nub: look up the address of $NAME" -- \
        gcloud compute instances describe "$NAME" --zone us-central1-a --project pullfrog \
        --format='value(networkInterfaces[0].accessConfigs[0].natIP)')
  ssh "${SSH_OPTS[@]}" nub@"$IP" "echo ok"
  ```
- **Re-resolve the IP on every reconnect, not once per session.** A VM can be stopped out from under you mid-session and return on a different IP, so an address resolved at the top of a long run can be dead an hour later. Treat a sudden `Connection refused`/timeout on a previously-working box as "it moved" and re-read the IP before reading the serial console.
- **Always reachability-guard a VM dispatch** (`ConnectTimeout`, `timeout`), and after a fresh start retry with backoff for a few minutes — sshd (Windows especially) isn't up the instant STATUS flips RUNNING. Never let a sub-agent hang on a VM: check reachability, act, report, exit.

### Giving a Linux box a throwaway key

One key per box, shared by every session on this Mac that reaches it: make it if it is missing, then put its public half in the instance's `ssh-keys` metadata, which the guest agent installs for `nub` within seconds on a running box.

```sh
NAME=nub-linux
KEY=~/.cache/nub/vm-keys/$NAME/id_ed25519
mkdir -p ~/.cache/nub/vm-keys && chmod 700 ~/.cache/nub/vm-keys
mkdir -p -m 700 "$(dirname "$KEY")"
[ -f "$KEY" ] || ssh-keygen -q -t ed25519 -N "" -C "$NAME" -f "$KEY"
chmod 600 "$KEY" "$KEY.pub"
keyward --reason "nub: give $NAME a throwaway SSH key" -- \
  gcloud compute instances add-metadata "$NAME" --zone us-central1-a --project pullfrog \
  --metadata ssh-keys="nub:$(cat "$KEY.pub")"
```

- **`add-metadata ssh-keys=…` replaces the box's whole `ssh-keys` value.** That is what retires the old `~/.ssh/nub-vm` entry, and it is also why the key is per box rather than per session: a second session making its own key would lock the first one out. Reuse the key that is there.
- **The key goes with the box**: `rm -rf ~/.cache/nub/vm-keys/<name>` when you delete it.
- **A box that runs as a service account refuses this.** Per Google's docs, setting metadata on such a VM needs `iam.serviceAccounts.actAs`, which the key does not have (see Auth). A box made before the switch to Keyward may carry the default compute account: check with `describe "$NAME" --zone us-central1-a --project pullfrog --format='value(serviceAccounts[].email)'`. If it prints one, the box has to lose it before it can take a key. Stop it, run `set-service-account "$NAME" --zone us-central1-a --project pullfrog --no-service-account --no-scopes`, then start it again. Unverified: whether that call itself goes through without `actAs`. If it does not, recreate the box rather than widening the key.

## Creating a NEW VM on demand

For an isolated/ephemeral box (a clean first-run env, a second Linux box so you don't contend with `nub-linux`, a specific image), create one — don't wait for permission:

```sh
# Linux — size ≥16 GB if it will COMPILE nub (see the OOM gotcha); e2-standard-4 is the proven size.
# Run from the repo root. The last line is NOT optional: it is what stops a forgotten box billing for
# weeks. Nor is --no-service-account --no-scopes: without it the create needs a permission the key lacks (see Auth).
keyward --reason "nub: create the Linux VM nub-linux-tmp" -- \
gcloud compute instances create nub-linux-tmp \
  --zone us-central1-a --project pullfrog \
  --machine-type e2-standard-4 \
  --image-family ubuntu-2404-lts-amd64 --image-project ubuntu-os-cloud \
  --boot-disk-size 30GB \
  --no-service-account --no-scopes \
  --metadata-from-file startup-script=scripts/vm-idle/linux-startup.sh

# Then give it a throwaway key exactly as above, with NAME=nub-linux-tmp.
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
# The box's throwaway key, made first because the script below carries its public half.
NAME=nub-win-tmp
KEY=~/.cache/nub/vm-keys/$NAME/id_ed25519
mkdir -p ~/.cache/nub/vm-keys && chmod 700 ~/.cache/nub/vm-keys
mkdir -p -m 700 "$(dirname "$KEY")"
[ -f "$KEY" ] || ssh-keygen -q -t ed25519 -N "" -C "$NAME" -f "$KEY"
chmod 600 "$KEY" "$KEY.pub"

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
Set-Content -Path \$ak -Value '$(cat "$KEY.pub")' -Encoding ascii
icacls \$ak /inheritance:r
icacls \$ak /grant 'Administrators:F' /grant 'SYSTEM:F'   # sshd REFUSES a loosely-ACL'd key file
Restart-Service sshd
PS

# Run from the repo root. NOT optional: this is what powers the box off when it goes idle, and the
# licence makes this the most expensive box shape in the project. Still delete it when the job is done.
cat scripts/vm-idle/windows-startup.ps1 >> /tmp/win-ssh.ps1

keyward --reason "nub: create the Windows VM nub-win-tmp" -- \
gcloud compute instances create nub-win-tmp \
  --zone us-central1-a --project pullfrog \
  --machine-type e2-standard-8 \
  --image-family windows-2022 --image-project windows-cloud \
  --boot-disk-size 200GB --boot-disk-type pd-ssd \
  --no-service-account --no-scopes \
  --metadata-from-file windows-startup-script-ps1=/tmp/win-ssh.ps1
```

- **Budget ~10-15 minutes** from `create` to first successful SSH; first boot plus sysprep is slow, and the startup script runs partway through it. Poll rather than waiting on one attempt.
- **The default SSH shell is `cmd.exe`, not PowerShell.** A `;`-separated PowerShell one-liner dies with `Invalid argument/option - ';'`. Wrap it: `ssh … 'powershell -NoProfile -Command "…"'`.
- **Disk: 200 GB.** The image is 50 GB and gcloud warns the root partition may need manual resizing; Server 2022 resized it automatically here (179 GB free on first login). A debug box that builds nub and installs npm trees fills a 50 GB disk.
- **Read the serial console to confirm the script ran** — `get-serial-port-output … | grep windows-startup-script-ps1` shows each line's output, including `processed file: C:\ProgramData\ssh\administrators_authorized_keys`.
- A firewall rule is NOT the problem: `default-allow-ssh` is `0.0.0.0/0 tcp:22` with **no target tags**, so it already covers every instance. Don't go hunting network tags.
- **Swapping the key on an EXISTING Windows box is unverified.** Its key lives in `administrators_authorized_keys`, written by the startup script above, not by `ssh-keys` metadata, so the Linux recipe does nothing there. The likely route is to replace the script with one carrying the new public half (`add-metadata … --metadata-from-file windows-startup-script-ps1=/tmp/win-ssh.ps1`) and reboot (`instances reset`), since GCE runs a Windows startup script on every boot and `Set-Content` overwrites the old key — but that has not been run. The measured path is a fresh box from this recipe.

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
scp "${SSH_OPTS[@]}" /tmp/dobuild.bat nub@"$IP":C:/nub/dobuild.bat   # SSH_OPTS from the SSH section, with NAME set to this box
# /RU SYSTEM is fine HERE because a build only needs a toolchain — never reuse this line for a measurement.
ssh "${SSH_OPTS[@]}" nub@"$IP" 'cmd /c "schtasks /Create /TN nubbuild /TR C:\nub\dobuild.bat /SC ONCE /ST 00:00 /RL HIGHEST /RU SYSTEM /F & schtasks /Run /TN nubbuild"'
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
keyward --reason "nub: delete the VM <name>" -- gcloud compute instances delete <name> --zone us-central1-a --project pullfrog --quiet
rm -rf ~/.cache/nub/vm-keys/<name>
```

## Auth — Keyward holds the service-account key

The USER credential (`colin@pullfrog.com`) has its refresh token revoked periodically by org session-control policy, so an interactive gcloud sign-in is not durable. A service-account key is exempt, and Keyward keeps it sealed (stored as `GCLOUD_KEY`) and writes it out for one run at a time — there is no key file on disk, and `~/.config/pullfrog/vertex-service-account.json` is gone:

```sh
keyward --reason "nub: list the VMs" -- gcloud compute instances list --project pullfrog
```

- **One Touch ID covers ten minutes of this repository's gcloud runs**, from any worktree; locking the screen ends the window. Tell the maintainer before you raise the sheet, and batch the calls you need inside one window.
- **A locked screen fails at once**: `keyward: not released: screen locked`, exit 1. Wait for the unlock (the `keyward` skill has the check and how to wait), then run it again — never in a retry loop. `not released: declined or authentication failed` means no: stop and ask.
- **The command's first word must be `gcloud`.** `keyward -- sh -c '…'` or a runner is refused, so give each gcloud call its own `keyward`; the shell around it is fine (`IP=$(keyward … -- gcloud …)`, `keyward … -- gcloud … | tail -40`).
- **gcloud runs with an empty configuration**: no saved account, zone or region. The project defaults to the key's own (`pullfrog`); pass `--zone` on everything that takes one.
- **Pass `--quiet` when no terminal is attached.** gcloud's yes/no questions (enable an API? delete this?) otherwise wait forever for an answer.
- **A missing `GCLOUD_KEY` makes keyward print the command that stores it.** That is the maintainer's step; never recreate a key file to route around it.

⛔ **`GCLOUD_KEY` is a service account of its own with exactly one grant, `roles/compute.instanceAdmin.v1` on `pullfrog`, and that is all this skill needs.** Per Google's role reference it covers creating, starting, stopping, deleting and describing instances, their metadata, labels, scheduling and serial console, the default network and external IPs, and creating images and booting from an image family. It has no `iam.serviceAccounts.actAs`, so **every box is created with `--no-service-account --no-scopes`**. Without that, GCE attaches the default compute account, and running a VM as an account needs `actAs`. Nothing these boxes run calls a Google API. Do not widen the key to make a recipe work. Drop the service account from the recipe instead.

## Gotchas

- **A RUNNING instance can be a DEAD instance — read the serial console FIRST:**
  ```sh
  keyward --reason "nub: read nub-linux's serial console" -- \
    gcloud compute instances get-serial-port-output nub-linux --zone us-central1-a --project pullfrog | tail -40
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
