# Linux in-place filesystem DENY — mechanisms, costs, and which tool for which case

**Status:** research, 2026-07-12. Companion to [`sandbox-policy-provenance-patterns.md`](sandbox-policy-provenance-patterns.md). Answers the mechanism-level question that thread raised: **Landlock is allow-only with no deny primitive — so how does nub deny access to a specific file *inside an otherwise-writable tree* on Linux (protecting a checked-in config from self-tamper, or a secret from read), and at what cost?** Backed by a working spike (built + measured on a real kernel) plus a prior-art + soundness survey. **Recommend-only.** Prong files: `/tmp/sbx-research2/{seccomp-spike,seccomp-priorart,dac-uidsep,secret-stores,cleanroom-view}.md` (provenance/citation trail).

> **Current correction (2026-07-13).** Nub selected Bubblewrap startup-existing mount masks for Linux instead of the Landlock/seccomp recommendation developed here. The Windows comparison in the historical analysis also overstated implementation status: a protected-DACL inheritance break is OS-feasible but is not implemented for arbitrary projects in the current Nub backend.

## TL;DR — the tiered answer

A per-file in-place deny on Linux **is** buildable, sound, and unprivileged (the spike proves it) — but it's the wrong tool for the two cases we actually care about, and each of those has a *free* answer:

| Need | Best mechanism | Cost | Why not seccomp |
|---|---|---|---|
| Protect **config** from self-tamper | Relocate out of the writable tree · tighten-only ceiling · CI fresh-checkout | free | seccomp would tax *every* write in the tree to protect one file |
| Protect **secrets** from **on-disk read** (`.env`) | Landlock read-omission (per-access-type sibling enumeration) — OR remove plaintext from disk (keychain / encrypted-`.env`, nub injects only-needed) | runtime-free, but compile-fiddly (nested `.env`, grant budget, the **hardlink/`REFER`** bypass) | seccomp read-deny taxes *every* read (~93×); **env-construction alone does NOT cover the file read** |
| Deny **escalation-file writes** inside a writable tree, write-light agent workload | seccomp-notify addfd, write-opens-only | reads free, ~tens-of-µs per write-open | — this is the one case it fits |
| Hard OS-enforced deny (paranoid tier) | DAC dedicated account | one-time `sudo` setup | needs privileged provisioning either way |
| Write-heavy untrusted code (install/build) | own-package-dir write scope (build-jail, already the design) | free | seccomp would trap every file creation |

## 1. Root cause: Landlock has no deny primitive

Landlock is an allow-only LSM: rights granted on a path inherit down the subtree, and there is **no deny rule** — you protect a path by *not granting* it. There is no deny primitive on the kernel roadmap (confirmed against LWN/landlock.io; the trajectory is audit/observability, not deny). Consequence: "allow directory `X` writable **except** file `Y` inside it" is **not expressible in Landlock** — grant `X` and `Y` inherits; grant `X`'s children individually and you lose new-file creation, and `REMOVE_FILE`+`MAKE_REG` on the dir reopens a delete-recreate hole. macOS Seatbelt expresses an in-place deny natively. Windows can express a controlled protected-DACL tree in principle, but current Nub does not build that boundary for arbitrary projects. The Linux mechanism question drove this historical investigation; Bubblewrap masks are the selected answer now.

## 2. seccomp user-notify — sound, but not a production-blessed deny boundary

**Soundness (grounded in `seccomp_unotify(2)` + Christian Brauner):** naive "inspect the path and deny/CONTINUE" is **TOCTOU-unsound** — the kernel man page states verbatim it *"can not be used to implement a security policy"* for pointer-arg syscalls, because a sibling thread can race-swap the path after the check. The **only sound pattern** is the supervisor performing the op itself and injecting the result fd via `SECCOMP_IOCTL_NOTIF_ADDFD` (guarded by `NOTIF_ID_VALID`), never `CONTINUE`-after-a-path-check. Scalar args (flags) are race-free; pointer args are not.

**Prior art (the key steer):** **no production system traps `open`/`openat` via seccomp-notify as a security deny.** Container runtimes (LXD, runc, crun, youki, Docker) deliberately restrict notify to *low-frequency* syscalls they emulate on-behalf (mknod/setxattr/mount/bind). The single system that traps `openat` via notify is **Sandlock** (arXiv:2605.26298, May 2026 — and it is nub's *exact* design: unprivileged Landlock+seccomp Rust agent-sandbox) — but it uses openat-notify for copy-on-write *write-capture, not the security deny*, and keeps **all path-based deny in Landlock** "which are TOCTOU-immune." For real hot-path FS mediation, gVisor went to a full user-kernel (ptrace/KVM) instead. So trapping open() *as the deny boundary* is sound-only-if-careful and **perf-novel-risky with zero battle-tested precedent.**

**No clean unprivileged alternative exists:** fanotify `FAN_OPEN_PERM` needs `CAP_SYS_ADMIN`, BPF-LSM needs `CAP_BPF`/root, overlay/bind needs mount-priv/userns (the same block that killed nub's bwrap backend). Landlock is the only unprivileged + path-granular + zero-per-syscall option, and it can't deny.

## 3. The spike — it works, and here is the exact cost

Built a standalone Rust seccomp-notify supervisor + child, run on a **real kernel** (Lima VM, Ubuntu 24.04, kernel 6.8 aarch64 — *not* Docker LinuxKit, which is suspect for kernel-security features). Spike: `/tmp/sbx-research2/seccomp-spike.md`, code `/tmp/sbx-research2/spike/main.rs`.

**Correctness — 9/9 PASS.** `work/` fully writable except `protected.json` + `.env`: write-open of those → EPERM, write-open of any other file → OK, read-open of protected → OK (only *writes* denied). **Delete-recreate closed** (supervisor mediates `unlinkat`/`renameat`/`renameat2`, not just opens). **TOCTOU-sound**: perform-and-inject via `openat2` with `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS` + `NOTIF_ADDFD` — a symlink `evil → protected.json` is refused with **ELOOP** (concrete proof the supervisor can't be tricked into handing out a writable fd to the protected inode).

**Performance (200k open+close, ns/op — `vz`-inflated absolutes; the *ratios* are the venue-robust signal):**

| Config | ns/op | vs baseline |
|---|---|---|
| baseline open, no filter | ~814 | — |
| write-only BPF, **read** loop | ~886 | **+9% — no trap** |
| trap-all, read loop | ~76,000 | ~93× |
| write-only BPF, **write** loop | ~76,500 | ~92× (sound perform-inject) |
| CONTINUE round-trip floor (unsafe) | ~42,000 | ~52× |

**The crux answer — not all-or-nothing across read/write, but effectively all-or-nothing across writes:**
- **Reads are ~free.** Classic BPF *can* match the `openat` flags scalar (`flags & O_ACCMODE`, arg a2) and `RET_ALLOW` read-only opens at the filter layer — they never trap. Module resolution / config reads (the bulk of a Node process's opens) pay only the +9% BPF overhead.
- **Every write-open pays, unavoidably.** BPF *cannot* dereference the path pointer, so it can't tell which file a write-open targets — every write-open traps to the supervisor, and a write to an *ordinary* file pays the same round-trip as a write to a protected one. The tax is **per-write-open, not per-protected-file.**
- Per-trap cost ≈ one notify round-trip (Sandlock's bare-metal figure ~35µs; the spike's ~42µs floor / ~76µs sound-path are `vz`-inflated and reducible via `ADDFD_FLAG_SEND` + skipping `/proc/mem`).

**Implication:** cheap for a **write-light agent tool-call** (reads free, a handful of writes each paying one round-trip → sub-ms total); meaningful added latency for a **write-heavy** workload (a build emitting thousands of files). So it's a per-profile toggle, engaged only where an in-tree write-deny is genuinely needed and write volume is low.

## 4. DAC dedicated account — the real OS deny, at the cost of one `sudo`

The other genuine in-place deny is Unix DAC: the protected file owned by a different uid (in a dir the sandbox uid can't write) → plain permissions deny write *and* delete, no Landlock, no perf cost. **srt does exactly this on Windows** (a real `srt-sandbox` account, one-time UAC install then unprivileged exec) — but Linux/macOS srt and all of Codex are same-uid (bwrap/Landlock+seccomp/Seatbelt). It **always needs a privileged one-time step**: a distinct account needs root to create + `CAP_SETUID` to switch into per-spawn. Unprivileged userns does *not* give it (maps the same real uid to a synthetic namespace id — no host DAC separation) and is blocked-by-default on Ubuntu 24.04+/RHEL/CI anyway. Realistic shape: a one-time **`nub sandbox-setup`** (one `sudo`) provisioning a dedicated system user + a narrow cached sudo/polkit rule — mirroring srt-Windows. The **elevation-broker** variant ("confined proc requests a bump, a privileged nub helper grants from a pre-declared policy") is sound and precedented (polkit, macOS `SMJobBless`, systemd `ExecStartPre`), with one hard rule: the grant comes from policy the child can't edit, never a rubber-stamp of the child's ask.

## 5. The free mechanisms that cover the cases we actually have

- **Config self-tamper** → the confined process rewriting `nub.jsonc` to escalate next run is defeated *without any deny* by: (a) **relocation** — config outside the granted writable subtree (Landlock just doesn't grant it — free; what Codex/`$CODEX_HOME`, srt/`$HOME`, and Sandlock all do); (b) **tighten-only ceiling** — an unwritable higher tier clamps the in-tree config so a rewrite can only tighten (the k8s-PSA pattern = nub's existing layering); (c) **CI fresh-checkout** — each run re-checks-out the reviewed git ref, so an in-run write is ephemeral (the git ref is the immutable source). Config is write-rare and single-location, so relocation is strictly better than paying seccomp's per-write tax.
- **Secret reads are TWO defenses, not one — and the second is the hard one.** (1) env-construction stops the child *inheriting* secrets through its environment (already built). (2) But the child can still `open()` the `.env` **file** off disk and read the plaintext itself — and if it can, (1) is theater. Preventing the `.env` *file read* is what makes env-isolation mean anything, and it is **not free**: it is the read-side dual of the config-write problem, and harder, because seccomp read-deny is the catastrophic ~93× case (the write-only-flags BPF trick does NOT apply to reads) and `.env` can't be relocated out of the project (the toolchain reads it in place). Two real Linux mechanisms:
  - **(a) Landlock read-omission via per-access-type rights.** Grant the project root `READ_DIR` (listable/traversable) but **not** `READ_FILE` at the root level; grant `READ_FILE` on each child *except* `.env*`. `.env` then has no read grant → `open()`-for-read is kernel-denied, zero runtime cost. Works for reads (unlike writes — no delete-recreate hole). **But fiddly to compile**: nested `.env` in monorepo packages, the grant-budget ceiling on large trees, and the load-bearing sharp edge — the **hardlink bypass** (if the child can `link()` `.env` into a read-granted dir it reads the secret through the new path; closing it means controlling `REFER`, which also governs legit cross-dir renames). **Verified 2026-07-12:** the fix is to *grant* `REFER`, not withhold it — Landlock's escalation guard (destination must be ≥ as restricted as source) refuses moving `.env` into a read-granted dir automatically, while legit renames between equal dirs still pass. An adversarial exhaustion (`RENAME_EXCHANGE` cross/same-dir, `RENAME_WHITEOUT`, `/proc/self/cwd|root` + `/proc/<pid>/cwd` laundering even with `/proc` granted, `O_PATH`→`/proc/self/fd/N` reopen, `copy_file_range`/`FICLONE`/`sendfile`, symlink/`..`) reached the canary on **zero** vectors, each block paired with an unsandboxed positive control — ABI v2 / kernel 6.8 aarch64. **One operational rule:** nub must never open the secret before `restrict_self`, nor pass an inherited secret fd to the child (Landlock governs path resolution, not already-open fds). Residual: it stays a prove-a-negative posture (the compiler must never over-grant), is v2-only, and wants confirmation on x86_64 + more kernels before shipping.
  - **(b) Remove the plaintext secret from disk entirely** — keychain, or encrypted-`.env` that nub decrypts *in the trusted parent*, injecting only the needed vars into the child env. The child's readable filesystem then contains **no plaintext secret** — it can read `.env` and get ciphertext/nothing. Solves by elimination, robust to all of (a)'s edge cases; CI form is encrypted-`.env` with the decrypt key injected into the parent, never the child. This is where the secret-store direction stops being ergonomics and becomes the clean answer to read-protection.
  - macOS Seatbelt expresses this read deny natively. Windows has a feasible protected-DACL design but no current Nub implementation for broad-project carve-outs. Linux now uses Bubblewrap startup-existing masks.

## 6. Secrets: killing the dotenv (from the secret-stores prong)

Full analysis: `/tmp/sbx-research2/secret-stores.md`. **Verdict:** removing `.env` as a *default* is off the table — it breaks the tools with their own dotenv parser + build-time inlining (Next.js `NEXT_PUBLIC_*`, Vite `VITE_`, Docker `--env-file`/compose, Prisma, foreman) and violates nub's byte-for-byte compat contract; CI also has no keychain (headless Linux has no D-Bus/keyring session). Every tool that works (aws-vault, `op run`, `doppler run`, Bun.secrets) is a **runtime injector into `process.env`, not a `.env` replacement** — the axis nub already owns. So: viable **opt-in** `nub env <profile>` (keychain-backed via `keyring-rs`, local-dev) sourced into the child env, with a `--materialize-env` escape hatch for file-readers and CI on ambient env. The compat-preserving alternative worth weighing: **encrypted `.env` in place (dotenvx-style)** — keep the file so nothing breaks, encrypt the values, decrypt key is an env var; kills on-disk plaintext without killing `.env`.

## 7. Clean-room filtered view: out for tool-calls (from the cleanroom prong)

Full analysis: `/tmp/sbx-research2/cleanroom-view.md`. Copy-project-minus-secrets is **not** instantaneous (reflink is per-byte-free but per-*file* costly — a real repo is seconds, not sub-100ms; Apple discourages whole-dir clonefile), mount-based fast views are **dead unprivileged on CI/hardened kernels** (userns), and the **writeback constraint** is decisive: a copy's writes don't reflect to the real tree, so it fits build-jail (already does isolate-then-link-back) and read-mostly server procs but **not a code-editing tool call**. Reserve it for build-jail + opt-in read-mostly confinement; keep in-place denial for editing calls.

## 8. Recommendation for nub (recommend-only)

1. **Config self-tamper dissolves for free** — relocate out-of-tree / tighten-only ceiling / CI-fresh-checkout; no deny needed.
2. **Secret `.env` read-protection is the load-bearing hard case, and env-construction does NOT cover it** (§5). On Linux it needs either (a) Landlock read-omission compiled carefully (verify the hardlink/`REFER` closure) or (b) removing plaintext secrets from disk (keychain / encrypted-`.env`, inject-only-needed). This — not config-write — is the problem that decides whether env-isolation is real.
2. **Do NOT make naive seccomp path-deny the boundary** — the kernel says it isn't one.
3. **Reserve seccomp-notify (addfd, write-opens-only) for the one case it fits:** denying escalation-file *writes* inside a writable tree in a write-light agent tool-call (the Linux analogue of srt's `DANGEROUS_FILES`). Ship it as a per-profile toggle; reads stay free, writes pay one round-trip each.
4. **Offer DAC dedicated-account as a paranoid/hard tier** via a one-time `nub sandbox-setup` `sudo`, mirroring srt-Windows.
5. **Secrets:** opt-in `nub env <profile>` + consider dotenvx-style encrypted-`.env`; never kill `.env` by default.

## Unverified / flagged

- Spike absolutes are `vz`-VM-inflated (aarch64 under Virtualization.framework); the structural read-free/write-pays finding is venue-independent, the exact per-write µs is not. Bare-metal/x86 second venue not run.
- Spike: `openat2` (nr 437) mediation path implemented but not exercised (glibc emits `openat`); `NOTIF_ID_VALID` re-check stubbed; single-threaded supervisor (a multi-threaded child issuing concurrent write-opens needs an epoll/threaded supervisor).
- Claude Code's srt invocation model (in-process library, per the DAC prong) is medium-confidence (reasoned from the bundled seccomp binary + inlined logic), not directly quoted.
- GitHub Actions `permissions:` hard-ceiling-vs-fallback question, and exact file modes for SELinux/Nix config, left unverified in the provenance prongs.

## Changelog

- 2026-07-13 — **REVERSAL:** marked Bubblewrap mount masks as the selected Linux mechanism and corrected Windows inheritance-break from a current capability to an unimplemented controlled-root design.
- 2026-07-12 — Initial write-up. Five-prong survey + a built-and-measured seccomp-notify spike on a real kernel (Lima Ubuntu 24.04, 6.8 aarch64). Companion to [`sandbox-policy-provenance-patterns.md`](sandbox-policy-provenance-patterns.md).
- 2026-07-12 (later) — Verified the read-omission + REFER-escalation-guard `.env` boundary: adversarial exhaustion held (0 breaches, positive controls) on ABI v2 / 6.8 aarch64; identified the inherited-fd operational rule (`/tmp/sbx-research2/landlock-envdeny-pentest.md`). Compat matrix (node:22, chmod-000 + ambient vars, decoy file values): **6/7 tolerate an unreadable `.env`** — the whole `dotenv` family returns a non-fatal `{error}` and falls back to `process.env`, Prisma + Next.js warn-and-proceed off ambient (Next inlined the ambient value, not the file's decoy). **Vite is the sole casualty** — an unguarded `fs.readFileSync` on a *present-but-unreadable* `.env` throws fatally; an *absent* file doesn't trip it. So the Vite break is specific to the in-place-deny ("present-but-EACCES") shape and is avoided by off-disk/hide-the-file. (`/tmp/sbx-research2/env-deny-compat.md`.)
