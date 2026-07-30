# Sandbox executable allowlist — feasibility of `exec: [...]` as a confinement axis

**Status:** research / recommend-only, 2026-07-10. Lands NO product code.
**Question (maintainer):** is an executable allowlist feasible, and can our existing
enforcement mechanisms limit the set of binaries a sandboxed process **and its
subprocesses** may spawn? Should an `exec: ["git","node",…]` config axis be in scope?
**Grounded in:** the real backends on branch `850ebe622f` —
`crates/nub-sandbox/src/backend/{macos,linux,windows}.rs`,
`macos_seatbelt_base.sbpl`, `policy.rs` — plus host-macOS and Linux-VM fixtures
(marked **VM-verified** / **host-verified** / **source-predicted** below).

---

## TL;DR

- **An exec allowlist is feasible on macOS and Linux as a genuine kernel primitive**, and both are **inherited by all descendants and unsheddable** — a child can only exec what the policy allowed. **host-verified** (Seatbelt) / **VM-verified** (Landlock).
- **Today nub gates nothing on exec.** macOS emits an unconditional `(allow process-exec)`; Linux deliberately strips `AccessFs::Execute` from the handled set (so exec is ungated); Windows uses a plain AppContainer whose token still holds `ALL APPLICATION PACKAGES`, so every System32 binary stays executable. So an allowlist is *new* enforcement on all three, not a tightening of an existing one.
- **The maintainer's "run but don't read" (execute-without-read) does NOT work on Linux and is not clean on macOS/Windows.** Landlock **VM-verified**: `FS_EXECUTE` without `FS_READ_FILE` → `execve` denied with `EACCES`, even for a *statically*-linked binary. Exec requires read. On macOS, dyld reads dylibs in-process, so the lib closure must be readable. Verdict: **execute-without-read is not achievable** as a portable primitive.
- **The W^X hazard is real but structurally avoidable:** both backends grant per-path bits, so the sugar simply must never emit `execute` on a writable path. macOS `(literal <path>)` is inherently per-binary (not a subtree); Linux grants explicit bits per `PathBeneath`.
- **The dynamic-library + config-path closure is the honest limit.** Static closure (`otool -L`/`ldd`) is discoverable; runtime `dlopen`/plugins and per-tool config paths (`~/.gitconfig`, `/etc/…`) are not. `exec: [name]` sugar can grant the static closure best-effort and WILL miss runtime-loaded plugins and config unless the fs axis also grants them.
- **Attack surface is well-bounded:** the exec'd subprocess still runs *under the full fs/net/env sandbox*, so the allowlist only governs *which* binaries run, not what they can do. It reduces surface; it does not add a new trust sink.
- **Recommendation: in scope for macOS + Linux as a thin sugar over the fs axis** (`exec:[names]` → resolve names→paths at nub-load → grant execute + the discoverable read/lib closure), NOT a separate enforcement axis. **Windows: degrade honestly** (needs LPAC + full DLL closure; ship coarse or defer). Pinned-hash integrity is a cheap, worthwhile add. Be explicit in docs about the `dlopen`/config/shebang limits.

---

## 0. Where exec sits today (ground truth, per backend)

| Backend | Exec posture TODAY | Consequence |
|---|---|---|
| **macOS** (`macos_seatbelt_base.sbpl:25`) | `(allow process-exec)` + `(allow process-fork)`, unconditional, under `(deny default)` | Any binary the child can read+`file-map-executable` may be exec'd. `/bin`,`/usr/bin`,`/usr/sbin`,`/usr/libexec` are read-granted by the base → all system tools exec-able. |
| **Linux** (`backend/linux.rs:17-23,334-336`) | `read_access_bits()` **strips** `AccessFs::Execute`; `handle_access` never lists Execute | Landlock only restricts *handled* access types; an unhandled type is always allowed → **exec is entirely ungated**. The header states this is deliberate ("fragile way to break dynamic linking for zero security gain"). |
| **Windows** (`backend/windows.rs:568,8-14`) | Read grants are `GENERIC_READ \| GENERIC_EXECUTE`; **plain** AppContainer (no LPAC) | The LowBox token carries `ALL APPLICATION PACKAGES`; System32 objects carry AAP ACEs → **cmd/powershell/system binaries stay executable** regardless of nub's grants. Only non-AAP user paths are confined. |

So "add an exec allowlist" means, per OS: **flip macOS to `(deny process-exec*)` + per-path allows**; **add `Execute` to the Linux handled set + grant it on the closure**; **switch Windows to LPAC + grant the AC SID execute on the closure**. The IR has no exec representation yet — `policy.rs` models only `fs`/`net`/`env`/`pid`, and `FsAccess` is `Read | ReadWrite` (no execute variant).

---

## 1. CAN we limit the exec set, and is it inherited? (per-OS)

### macOS — Seatbelt `process-exec*` path allowlist — **YES, host-verified**

`(deny process-exec*)` with `(allow process-exec* (literal <path>))` per allowed binary is a real per-binary exec allowlist, and SBPL is last-match-wins + one-way-ratchet inherited (`macos_seatbelt_base.sbpl:24` comment: "child processes inherit the parent policy").

Host fixture (macOS, `sandbox-exec`):
- Allowlist `{/bin/echo,/bin/sh,/bin/bash,/bin/ls}`; `/bin/echo` runs; **`/bin/date` (not listed) → `execvp() … Operation not permitted` (rc 71).**
- **Inheritance:** `sh -c '/bin/ls'` (child bash execs allowlisted `ls`) → runs; `sh -c '/bin/date'` (child execs non-allowlisted `date`) → `Operation not permitted` (child rc 126). The grandchild is bound by the same allowlist.
- **Shebang gating:** a `#!/bin/sh` script at a non-allowlisted path → `execvp() … Operation not permitted`. Seatbelt gates the **script's own path**, not just the interpreter — so a script trampoline needs the script path allowlisted too.

Escapes closed by this: PATH tricks (the filter is on the resolved path, not the name); `sh -c` (the shell can only exec allowlisted binaries); LD_PRELOAD/DYLD_INSERT (loading a dylib still needs `file-map-executable` on it — governed by the read/exec grants). **Residual gotcha found:** `/bin/sh` on macOS **re-execs `/bin/bash`** as a variant — so allowlisting `sh` without `bash` breaks it. Any binary that re-execs a sibling (busybox-style multi-call, `sh`→`bash`, wrapper shims) needs the re-exec target listed too.

### Linux — Landlock `LANDLOCK_ACCESS_FS_EXECUTE` — **YES, VM-verified**

Add `AccessFs::Execute` to the handled set and add an `Execute` right on each allowlisted binary's `PathBeneath`. Landlock's `restrict_self` ruleset is inherited across `fork`/`execve` and unsheddable under `no_new_privs` (already relied on for the fs/net/ptrace boundaries — `backend/linux.rs:1-15`).

VM fixture (kernel 6.17, Landlock ABI 7; handled = `EXECUTE|READ_FILE|READ_DIR|WRITE_FILE`):
- Grant exec+read on `/usr/bin/date` + lib dirs, then try to exec **non-granted** `/usr/bin/head` → **`execv … Permission denied`**; exec the granted `/usr/bin/date` → runs. So it is a real allowlist.

Cost the Linux header warned about is real: because Landlock's `from_read` bundles Execute and the **loader** maps every shared library `PROT_EXEC`, governing exec means every needed **library directory** must also carry the Execute right, or dynamic linking breaks. That is exactly the closure work in §3, and it is why exec was left ungated originally. It is tractable (grant the lib dirs), just not free.

seccomp adds nothing here: seccomp can deny `execve` wholesale or filter by *register* args, but it cannot match on the **resolved path** of the target (the filename is a userspace pointer seccomp won't deref safely) — so it cannot express "these paths only." Landlock is the right primitive; seccomp stays for the net/ptrace axes.

### Windows — AppContainer read+execute ACL — **partial, source-predicted (no Windows host / not Docker-reachable)**

Execution requires the AC SID (or a capability SID, or AAP) to hold read-execute on the binary's ACL. In principle that yields an allowlist: grant the per-run AC SID read-execute only on allowlisted binaries. **But the current backend uses a plain AppContainer**, whose token includes `ALL APPLICATION PACKAGES`; System32/Program Files objects carry AAP ACEs, so those binaries execute no matter what nub grants. A true allowlist requires a **Less-Privileged AppContainer (LPAC)** — opt out of AAP via `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY` — after which *every* binary AND its full DLL closure (including the System32 DLLs the CRT/loader pull in) must be explicitly granted the AC SID. That closure is large and fragile on Windows. Inheritance: the LowBox token is inherited by the child process tree (the Job Object already reaps it), so grants apply tree-wide — but the closure-management burden is the blocker, not inheritance.

---

## 2. The W^X concern — writable-and-executable defeats the allowlist

If any allowed path is **both writable and executable**, the process writes its own binary there and execs it, defeating the allowlist. This is a real hazard and the allowlist design MUST guarantee no path carries both bits.

- **macOS:** naturally safe — `(allow process-exec* (literal <path>))` is **per-file, not a subtree**, so an exec grant never covers a directory the child can drop a new file into. As long as the sugar emits `literal` (never `subpath`) exec grants, W^X cannot arise from the exec axis. (The fs write axis is separate and already refuses dangerous write roots — `is_dangerous_write_root`.)
- **Linux:** Landlock grants are **explicit per-path bit sets** (`PathBeneath` with a chosen `BitFlags<AccessFs>`). The sugar must emit `Execute` only on read-only binary/lib paths and never OR it into a `WriteSubtree` grant. The existing derivation already separates read grants from write grants (`derive_read_grants`/`derive_write_grants`), so exec bits ride the read set exclusively — structurally W^X-safe if implemented there.
- **Windows:** write grants are `GENERIC_READ|GENERIC_WRITE|GENERIC_EXECUTE|DELETE` — they **already include EXECUTE** (`windows.rs:584`). Under LPAC that is a live W^X hole: a write-granted dir is executable. An exec allowlist on Windows must **drop `GENERIC_EXECUTE` from write grants** and grant execute only on the read-only allowlist paths.

Guiding invariant for the sugar: **`execute ⇒ ¬writable`, enforced at grant-emission time**, identically on every backend.

---

## 3. Dynamic-library closure — the discoverability gap

An allowlisted binary needs its linked libraries present with the right access, or it won't start.

- **Static closure is discoverable:** `otool -L <bin>` (macOS) / `ldd <bin>` (Linux) enumerate the direct + transitive `NEEDED` libraries at load time. The sugar can walk this closure and grant read(+map/exec) on each.
- **Runtime `dlopen` is NOT discoverable:** a plugin architecture that `dlopen`s a `.so`/`.dylib` by a path computed at runtime (language runtimes, `git` credential/remote helpers, `node` native addons loaded on demand) will not appear in the static closure. `exec:[name]` sugar can grant the static closure **best-effort** but will miss runtime-loaded modules — those surface as a load failure the user resolves with an explicit fs grant.
- **macOS specific:** dyld maps dylibs **in-process** (`file-map-executable` + read), so the lib closure must be **readable**, not merely executable — this is why execute-without-read (§5) can't help the libs. The Seatbelt base already read+maps the system frameworks; a non-system toolchain's out-of-dir libs need an explicit grant.
- **Linux specific:** VM-verified above — a dynamic binary runs only when its **lib directories** carry read+exec. The essential-read set (`/usr`,`/lib*`,`/etc`,…) already covers the standard loader/libc, so the closure gap is mainly non-standard `RPATH`/`LD_LIBRARY_PATH` locations.

**The sugar owes:** grant the resolved binary + its **static** lib closure; **document** that `dlopen`/plugin paths and non-standard lib dirs need an explicit fs entry. Do not pretend the closure is complete.

---

## 4. Config-path access — `exec:[name]` alone won't grant it

Tools read config from paths the exec grant does not cover: `git` reads `~/.gitconfig`, `/etc/gitconfig`, `$GIT_CONFIG`; `node` reads `.npmrc`, `NODE_OPTIONS`; many read `/etc/…` and XDG dirs. `exec:[name]` grants *execute + lib closure* only — it grants none of these.

Behavior when config is blocked is **tool-specific**: most tools (git, node) **degrade to defaults** rather than break when a config file is simply *absent/unreadable* — a read denial reads as "no such config." But a tool that *requires* a config (a wrapper needing a credential file, a tool that errors on an unreadable-but-existing config) will break. **The sugar owes nothing here beyond honesty:** exec-allowlisting a tool does not implicitly grant its config; the user adds fs read grants for the config paths they need. Auto-granting a hardcoded per-tool config set is a maintenance/brand trap (a per-tool knowledge base) and should be avoided — keep exec sugar to the *binary + lib* closure, and let the fs axis own config.

---

## 5. "Run but don't read" — execute-without-read

The maintainer's idea: grant execute WITHOUT read so the process can run a binary but can't read/copy/modify it or its libs.

- **Linux — NOT achievable. VM-verified.** `FS_EXECUTE` without `FS_READ_FILE` on the target → **`execve` fails with `EACCES`**, even for a **statically-linked** binary (no libs involved). Adding `FS_READ_FILE` (read+exec) → runs. Landlock requires the file be readable to execute it; there is no execute-only file access. The lib closure independently needs read (loader opens + mmaps each `.so`).
- **macOS — NOT clean.** Seatbelt `process-exec*` gates the *act* of exec by path, but the binary + every dylib must be `file-read*`-able for dyld to map them in-process. You cannot run a dynamically-linked tool with its image unreadable. (You *can* deny `file-write*` on it — tamper protection — but not read.)
- **Windows — NOT clean.** `FILE_EXECUTE` is a distinct NTFS right, but the image loader must read the PE to map it; stripping read while keeping it launchable is not a supported, robust configuration.

**Verdict:** execute-without-read is not a portable primitive. What IS achievable and worth offering instead is **execute + read but NOT write** — the binary and its libs are runnable and readable but **tamper-proof** (no overwrite, no trojaning). That is the honest version of "run but don't let them modify it," and it falls straight out of the existing read/rw split (grant read+exec, never rw, on allowlist paths).

---

## 6. Attack-surface analysis — does an allowlist add risk?

**Key point, confirmed:** the exec'd subprocess **still runs under the full sandbox** — the same fs/net/env policy applies to it (Seatbelt/Landlock/AppContainer restrictions are inherited, §1). The allowlist governs only *which* binaries run, never *what they can do once running*. A binary that isn't on the allowlist simply cannot start; one that is, is boxed exactly like its parent. So the allowlist **reduces** surface (fewer reachable binaries) and introduces **no new trust sink** — it is not a capability grant, it's a capability *restriction*.

Residuals to be honest about:

- **PATH-poisoning** — **closed** by resolving `["git"]`→concrete path at **nub-load time** and grant/gating on the resolved path (both backends filter on resolved paths, not names). A later `PATH` change in the child can't smuggle a different binary in, because the allow is keyed to the resolved path, not the name.
- **Binary-swap TOCTOU** — a resolved path whose file is replaced between resolve and exec. **Closed optionally** by pinned-hash integrity (§7): verify the binary's digest at exec. Without it, the residual is "someone with write access to the allowlisted path swaps the binary" — but writing that path already requires a write grant the sandbox controls.
- **Trusted binary with latent capability** — an allowlisted `bash`/`node`/`git` can do anything *the sandbox lets it* (it's still boxed), but e.g. `git` with a config-injected `core.sshCommand`, or `node -e <arbitrary>`, is a broad tool. This is inherent to allowlisting powerful interpreters, not specific to the mechanism; the fs/net boxing is what actually contains it. Document that allowlisting an interpreter (`sh`,`bash`,`node`,`python`) is allowlisting *arbitrary code under the box*, not a narrow capability.
- **Interpreter / `sh -c` / shebang trampolines** — bounded: a shell can only exec allowlisted binaries (host-verified); a shebang needs the **script path** allowlisted (host-verified); a re-exec (`sh`→`bash`) needs the target allowlisted (host-verified). None of these escape the allowlist; they just mean the closure must include the interpreters + re-exec targets you actually use.

---

## 7. Load-time resolution + integrity — feasibility + cost

**Proposed shape:** at nub-load, resolve each `exec:[name]` against `PATH` → a concrete absolute path (reusing the existing `resolve_program` in each backend), then grant execute + the discoverable read/lib closure on that path. Optional: record the binary's content hash and verify it at exec (detect a swap).

- **Resolution — cheap, already have it.** `backend/{macos,linux}.rs` each already resolve a program (absolute / cwd-relative / PATH-search) to a canonical path for the auto-grant of the *target* binary. Extending that to N allowlist names is trivial and reuses proven code. Resolving at load-time (parent side) also neutralizes PATH-poisoning (§6).
- **Static-closure walk — moderate.** Run `otool -L`/`ldd` (or parse the ELF/Mach-O `NEEDED`/`LC_LOAD_DYLIB` directly to avoid the subprocess) once per binary at load. Cost is a handful of ms per allowlisted binary, one-time. Miss = a clear load error, not a silent hole.
- **Pinned-hash integrity — cheap, worthwhile.** Hash the resolved binary at load, re-hash (or compare mtime+size as a fast pre-check) at exec. Verifying at exec on Linux would need a `pre_exec` read of the file (adds an open+hash on the hot path) or a Landlock-independent check parent-side before spawn; a parent-side load-time pin + a re-check just before spawn covers the realistic swap window at near-zero cost. Full TOCTOU-proof verification (hash the exact bytes the kernel maps) is not portably expressible, so treat pinned-hash as **swap-detection**, not a hard guarantee.

---

## 8. Recommendation

**Feasible? Yes on macOS and Linux — as a thin sugar over the fs axis, not a separate enforcement primitive.** The exec grant is *just another fs grant* (execute right on resolved binary + read on its lib closure), so it belongs in the same IR/derivation machinery, keyed off a new surface field.

**Right shape:**
1. **Surface:** an `exec: ["git","node",…]` list under the `sandbox` object (a fourth sibling to `fs`/`net`/`env` in `sandbox-config-spec`). It is *sugar*: the compiler resolves each name→path at nub-load and lowers it into fs execute+read grants — the IR need not grow a full parallel axis, only (a) an `execute` bit on `FsAccess`/the grant kinds and (b) the load-time resolve step. Keep the *surface* an axis (users think "which tools"), keep the *IR* an fs lowering.
2. **Default posture:** when `exec` is **present**, flip the backend to deny-exec-by-default and allow only the resolved closure (macOS `(deny process-exec*)` + per-`literal` allows; Linux add `Execute` to handled + grant the closure). When `exec` is **absent**, keep today's ungated behavior (additivity floor — no new confinement unless asked).
3. **Invariants:** `execute ⇒ ¬writable` at emission (§2); resolve→path at load (§6); grant static lib closure best-effort (§3); read+exec, never write, on allowlist paths (the tamper-proof reading of "run but don't modify", §5).
4. **Windows:** ship **coarse or deferred**, degraded honestly via the existing `Degradation` channel. A real allowlist needs LPAC + full System32 DLL closure — a materially larger effort; do not claim exec-allowlisting on Windows until LPAC lands. (Consistent with the backend's existing "report, never silently claim" contract.)

**Genuinely deliverable now:** the macOS + Linux primitive + the resolve→grant sugar + the `execute⇒¬writable` invariant + optional pinned-hash. This is a bounded change to two backends + the compiler, reusing existing resolution and grant-derivation code.

**Runtime-front-end feature (later):** wiring `exec` through the *runtime* sandbox front-end and the `nub sandbox` CLI, and the per-package `dependenciesMeta.<pkg>.sandbox.exec` grant home.

**Out of scope / honest limits:** execute-without-read (impossible portably, §5); a complete `dlopen`/plugin closure (undiscoverable, §3); auto-granting per-tool config paths (a per-tool knowledge base — leave to the fs axis, §4); a tight Windows allowlist without LPAC (§1).

**Net:** an exec allowlist is a real, inheritance-safe, surface-reducing primitive on the two backends that matter most, expressible as fs sugar with no new trust sink. Recommend taking it **in scope for macOS + Linux**, Windows-degraded, with the dynamic-loading and config limits documented up front. This is a **default/security-posture call the maintainer owns** — recommend-only.

---

## Appendix — fixtures (reproducible)

**macOS (host, `sandbox-exec`):** profile `(allow default)(deny process-exec*)(allow process-exec* (literal "/bin/echo") …)`.
- allowlisted `/bin/echo` → ran; non-listed `/bin/date` → `execvp() … Operation not permitted`.
- inheritance: child `bash` execs listed `/bin/ls` → ran; execs non-listed `/bin/date` → `Operation not permitted`.
- shebang script at non-listed path → `execvp() … Operation not permitted` (script path gated).
- `/bin/sh` re-execs `/bin/bash` → allowlist `sh` alone breaks; must list `bash`.

**Linux (VM `nub@34.41.194.82`, kernel 6.17, Landlock ABI 7):** C harness `ll_exec.c`, handled = `EXECUTE|READ_FILE|READ_DIR|WRITE_FILE`.
- static bin, `FS_EXECUTE` only (no read) → `execv … Permission denied` (**exec-without-read denied**).
- static bin, read+exec → ran.
- dynamic `/usr/bin/date`, exec-only → denied; read+exec (binary + lib dirs) → ran.
- allowlist grants `date`; exec non-granted `/usr/bin/head` → `Permission denied` (**allowlist denies**).

**Windows:** source-analyzed only (no Windows host; not Docker-reachable). Claims marked source-predicted; validate on the `windows-latest` CI leg before shipping any Windows exec behavior.

## Changelog

- 2026-07-10 — Initial write-up.
