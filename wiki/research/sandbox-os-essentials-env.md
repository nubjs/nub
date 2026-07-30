# OS-essential env on the strip-all floor

## The question

When nub's sandbox env axis is set to **strip-all** (deny all user/ambient env), the constructed child environment must still carry the handful of **OS-mechanism** variables a process needs merely to *exist* — Windows `SystemRoot` (winsock/loader), `LOCALAPPDATA` (AppContainer profile dir), POSIX terminal/locale/temp basics. Today nub's per-OS floor is **empirically VM-pinned** (`crates/nub-sandbox/src/compiler/defaults.rs`, commit `bd52bfe2b8`): Windows = `{SystemRoot, LOCALAPPDATA}`, POSIX = `{}`. This doc grounds that floor in what **production sandboxing / isolation / privilege-drop systems actually keep**, so the set is defensible-by-citation rather than hand-guessed.

Two distinct sets are in play and must not be conflated:

- **The strip-all FLOOR** — "what a child needs before it can run *at all*." This is the deny-all posture; it must stay as small as possible. It is the subject of this doc.
- **The curated BASELINE** (`sandbox: true` / `env: ["..."]`, `BASELINE_ENV_EXACT` in `defaults.rs`) — "what a build child needs to *operate usefully*" (PATH + HOME/USER + locale + `npm_config_*` build hints). Larger, and a separate design surface. This doc informs it as a secondary output but does not redefine it.

## TL;DR recommendation

| OS | Strictly-required-to-START floor | Recommended strip-all floor (adopt) | Rationale |
|---|---|---|---|
| **macOS** | none | **none** (unchanged) | empty-environ `execve` starts `node`/`sh`/`true` at rc=0; `os.tmpdir()` falls back to `/tmp`. Verified empirically. |
| **Linux** | none | **none** (unchanged) | same as macOS — POSIX exec has no env prerequisite. |
| **Windows** | `SystemRoot`(≡`windir`), `SystemDrive`, `TEMP` (libuv's own "essential" set); `LOCALAPPDATA` for the AppContainer path | **`{SystemRoot, SystemDrive, TEMP, TMP, LOCALAPPDATA}`** (widen from today's `{SystemRoot, LOCALAPPDATA}`) | grounded in **libuv's `required_vars[]`** — Node's own runtime dependency's documented list of what a Windows child needs. See below. |

Net change from today: **POSIX stays empty** (already correct and now cited). **Windows** gains `SystemDrive`, `TEMP`, `TMP` — a citation-backed superset of the current `{SystemRoot, LOCALAPPDATA}`, still all non-secret OS-mechanism path pointers.

Everything recommended is a **path / topology pointer, never a credential.** The floor injects the *real ambient value* only for these whitelisted names; all other ambient vars (including anything secret-bearing) stay withheld.

---

## Why POSIX needs nothing (macOS + Linux)

Empirically confirmed on macOS (this host), and structurally true on Linux — an absolute-path `execve` with an empty `environ` starts fine; nothing in the POSIX process-creation contract reads an env var:

```
env -i node t.js            # → "ok" (process.env.PATH === undefined), rc=0
env -i /bin/sh -c 'echo x'  # → x, rc=0
env -i /usr/bin/true        # → rc=0
env -i node -e 'os.tmpdir()'# → /tmp   (fallback when TMPDIR/TMP/TEMP all unset)
```

Node's boot path reads **no** env var as a hard prerequisite on POSIX (verified against `node`):
- `os.tmpdir()` → libuv `uv_os_tmpdir` checks `TMPDIR` → `TMP` → `TEMP`, else hardcodes `/tmp` (`deps/uv/src/unix/core.c`, `lib/os.js`). Never throws.
- ICU/locale init consults only `NODE_ICU_DATA` / `--icu-data-dir` / compiled-in full-icu — **`LANG`/`LC_*` are not read for ICU** (they matter to the *terminal*, not V8). (`src/node.cc` ICU bootstrap.)
- `NODE_OPTIONS`, `NODE_PATH`, `PATH` — all optional-with-safe-defaults; `PATH` is read only inside libuv's `child_process` spawn path, never at `node file.js` boot.

So the POSIX floor is legitimately empty. The corroborating prior art: Firecracker's jailer **wipes** the environment entirely (`docs/jailer.md`), and nsjail **denies by default** (`keep_env=false`) — the strictest systems assume the child brings nothing, and it works.

**macOS / Seatbelt note:** the sandbox is applied to the already-spawned process; there is no macOS-specific env var required to *start* a process that the POSIX analysis doesn't already cover. SRT (Anthropic's sandbox-runtime) confirms this indirectly — its macOS path uses a *denylist* (`env -u <secrets> sandbox-exec …`, keep the rest), never a from-empty allowlist, because macOS imposes no startup-env floor.

---

## Why Windows needs a small floor — the authoritative grounding

### libuv's `required_vars[]` — the load-bearing citation

The single best-grounded answer is **libuv's own list of Windows-essential env vars**, because it is *Node's own runtime dependency* stating what a Windows child needs — not third-party guesswork. In `deps/uv/src/win/process.c` (verified in `node`):

```c
/* Windows has a few "essential" environment variables. winsock will fail
 * to initialize if SYSTEMROOT is not defined; some APIs make reference to
 * TEMP. SYSTEMDRIVE is probably also important. We therefore ensure that
 * these get defined if the input environment block does not contain any
 * values for them.
 * Also add variables known to Cygwin to be required for correct
 * subprocess operation in many cases: ... */
static const env_var_t required_vars[] = { /* keep me sorted */
  E_V("HOMEDRIVE"), E_V("HOMEPATH"), E_V("LOGONSERVER"), E_V("PATH"),
  E_V("SYSTEMDRIVE"), E_V("SYSTEMROOT"), E_V("TEMP"), E_V("USERDOMAIN"),
  E_V("USERNAME"), E_V("USERPROFILE"), E_V("WINDIR"),
};
```

`make_program_env()` **back-fills any of these 11 vars from the parent's real environment** whenever `child_process.spawn(..., {env})` supplies a stripped/custom block — regardless of what the caller intended. In other words, when Node itself launches a Windows child with a scrubbed env, libuv silently re-injects this set. This is the closest thing to ground truth for "what a Windows child needs."

Two tiers within that list:
- **Strict OS-essential (the comment's own three):** `SYSTEMROOT` (winsock `WSAStartup` fails without it), `TEMP` (some APIs reference it), `SYSTEMDRIVE`. `WINDIR` ≡ `SYSTEMROOT` (Microsoft documents them as literal aliases).
- **Cygwin-subprocess-compat (the rest):** `HOMEDRIVE`, `HOMEPATH`, `LOGONSERVER`, `USERDOMAIN`, `USERNAME`, `USERPROFILE`, `PATH` — added for correct MSYS2/Cygwin subprocess behavior, **not** native-loader necessity. These carry identity info (`USERNAME`, `LOGONSERVER`, `USERDOMAIN`) — see the secret/privacy note below.

### The AppContainer wrinkle — `LOCALAPPDATA`

Independently of libuv, nub's **enforcing** Windows backend runs the child in a LowBox **AppContainer**, which resolves its per-container profile dir (`%LOCALAPPDATA%\Packages\<moniker>\AC`) from the environment. nub's own VM sweep pinned this exactly (`defaults.rs` doc comment): `{SystemRoot}` and `{SystemRoot, USERPROFILE}` both fail `CreateProcessW` with `ERROR_ENVVAR_NOT_FOUND` (203); `{SystemRoot, LOCALAPPDATA}` is the smallest set that starts. `LOCALAPPDATA` is **not** in libuv's list (libuv doesn't run children in an AppContainer), so it must be kept in addition to the libuv-grounded set. Microsoft: [Implementing an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer).

### Why not the strict-minimum `{SystemRoot, LOCALAPPDATA}` nub ships today?

Today's floor starts a *native* `node.exe` (which tolerates a near-empty block) and satisfies the AppContainer resolver — but it is "starts only where the OS incidentally tolerates it." The moment the child does anything real:
- **winsock / any networking** fails without `SystemRoot` — covered — but a managed (`powershell.exe`, .NET) child also references `SystemDrive`, and libuv considers it essential.
- **`os.tmpdir()` on Windows** is a Node JS re-implementation (`lib/os.js`): `TEMP || TMP || (SystemRoot||windir)+'\\temp'`. With none of `TEMP`/`TMP` set it silently produces `<SystemRoot>\temp` — writable-by-nobody in an AppContainer, so temp-file work breaks. Keeping `TEMP`/`TMP` (the real per-container temp path, which the AppContainer reroutes under its `AC` dir) fixes this. `GetTempPath2`'s documented fallback is `TMP → TEMP → USERPROFILE → Windows dir`.

So widening to **`{SystemRoot, SystemDrive, TEMP, TMP, LOCALAPPDATA}`** is the defensible floor: libuv's three strict-essentials + the temp pair Node's own `os.tmpdir()` needs + the AppContainer essential. It is a citation-backed superset of today's set, still entirely non-secret path pointers, and matches "what Node/libuv themselves inject."

> **Empirical caveat (flag for follow-up):** the exact Windows subset was pinned on one VM against nub's two spawn paths. The widen to add `SystemDrive`/`TEMP`/`TMP` is grounded in libuv's documented list + `os.tmpdir()` source, not re-pinned on the VM. Before landing, re-run the AppContainer + native subset sweep (via the `ci-adhoc-test` Windows leg) to confirm the added vars are inert-or-beneficial and never *break* a start. Adding them can only help a start (they are values Windows/libuv expect), but the empirical confirmation keeps the floor honest.

### Windows vars deliberately NOT on the floor

`ComSpec`, `PATHEXT`, `USERPROFILE`, `APPDATA`, `HOMEDRIVE`, `HOMEPATH`, `NUMBER_OF_PROCESSORS`, `PROCESSOR_ARCHITECTURE`, `ProgramData`, `ProgramFiles`, `ALLUSERSPROFILE` are **essential-to-FUNCTION, not essential-to-START** — none are read by the loader or Node's boot path. They belong in the curated *baseline* (where nub already lists most of them), not the strip-all floor. Keeping them on the floor would be over-injection. (`PATHEXT` default, if ever needed: `.COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC` — per the Microsoft `start` command page; codex synthesizes it when absent.)

---

## Prior-art survey (what each system keeps when scrubbing)

### The directly-comparable strip-all allowlist: OpenAI codex

codex is the closest production analogue — a coding agent that constructs a scrubbed child env from an explicit **Core** allowlist (`ShellEnvironmentPolicyInherit::Core`), with per-OS lists. From `codex/codex-rs/protocol/src/shell_environment.rs`:

- **`UNIX_CORE_ENV_VARS`** = `PATH, SHELL, TMPDIR, TEMP, TMP, HOME, LANG, LC_ALL, LC_CTYPE, LOGNAME, USER`
- **`WINDOWS_CORE_ENV_VARS`** = `PATH, PATHEXT, SHELL, COMSPEC, SYSTEMROOT, SYSTEMDRIVE, USERNAME, USERDOMAIN, USERPROFILE, HOMEDRIVE, HOMEPATH, PROGRAMFILES, PROGRAMFILES(X86), PROGRAMW6432, PROGRAMDATA, LOCALAPPDATA, APPDATA, TEMP, TMP, TMPDIR, POWERSHELL, PWSH`
- Plus: synthesize `PATHEXT=.COM;.EXE;.BAT;.CMD` on Windows if absent; and a **default-exclude** denylist over the whole set: case-insensitive `*KEY*`, `*SECRET*`, `*TOKEN*`.

Note codex's "Core" is a **useful-shell** set (closer to nub's *baseline* than to a strip-all floor) — it deliberately keeps `PATH`/`HOME`/locale so a shell command works. But its Windows list independently corroborates the OS-mechanism names (`SYSTEMROOT`, `SYSTEMDRIVE`, `TEMP`, `TMP`, `LOCALAPPDATA`) and its `*KEY*/*SECRET*/*TOKEN*` default-exclude mirrors nub's secret-scrubbing discipline. Source: `shell_environment.rs`.

### sudo — `env_reset` (the classic authoritative minimal-env)

Default-on `env_reset` runs the command in a **minimal environment**: `TERM PATH HOME MAIL SHELL LOGNAME USER SUDO_*`, then adds anything matching `env_keep` / `env_check`. Compiled-in defaults (`plugins/sudoers/env.c`):
- **`env_keep`:** `COLORS DISPLAY HOSTNAME KRB5CCNAME LS_COLORS PATH PS1 PS2 XAUTHORITY XAUTHORIZATION XDG_CURRENT_DESKTOP`
- **`env_check`** (kept only if value has no `%`/`/`): `COLORTERM LANG LANGUAGE LC_* LINGUAS TERM TZ`
- **`secure_path`** replaces `PATH` with a fixed safe value (off upstream by default; distros ship `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`).

Sources: [`sudoers(5)`](https://man7.org/linux/man-pages/man5/sudoers.5.html), [`plugins/sudoers/env.c`](https://raw.githubusercontent.com/sudo-project/sudo/main/plugins/sudoers/env.c).

**sudo's `initial_badenv_table` is a directly-reusable NEVER-KEEP list** (see the secret/injection section below) — it is the most authoritative published enumeration of env-injection vectors.

### systemd — `systemd.exec(5)`

"The general philosophy is to expose a small curated list." A system service gets: fixed **`PATH`** (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin`), **`LANG`** (from `locale.conf`), **`USER`** (unconditional with `User=`), **`HOME`/`LOGNAME`/`SHELL`** (with `User=`), **`TERM`** (only if tty-attached), plus systemd-internal `INVOCATION_ID`/`XDG_RUNTIME_DIR`/`*_DIRECTORY`. `DefaultEnvironment=`/`PassEnvironment=` are **empty by default** — a system service does NOT inherit PID 1's env. Source: [`systemd.exec(5)`](https://man.archlinux.org/man/systemd.exec.5.en).

### OpenSSH — deny-by-default

Server `AcceptEnv` and client `SendEnv` both default to **accept/send nothing** (only `TERM` is protocol-forced when a pty is requested). Distros layer `AcceptEnv LANG LC_*` / `SendEnv LANG LC_*` as a packaging choice. PAM/`pam_env` seeds `/etc/environment` + `/etc/security/pam_env.conf` independently at session setup. Sources: [`sshd_config(5)`](https://man7.org/linux/man-pages/man5/sshd_config.5.html), [`ssh_config(5)`](https://man.openbsd.org/ssh_config).

### login / PAM floor

`login(1)` sets `HOME USER SHELL PATH LOGNAME MAIL` from `/etc/passwd`+`login.defs` (`ENV_PATH=/bin:/usr/bin`, `ENV_SUPATH=/sbin:/bin:/usr/sbin:/usr/bin`); `TERM` preserved if present. Sources: [`login(1)`](https://man7.org/linux/man-pages/man1/login.1.html), [`login.defs(5)`](https://man7.org/linux/man-pages/man5/login.defs.5.html).

### Containers / bwrap / firejail / nsjail / Firecracker

| System | Env posture | PATH | Note |
|---|---|---|---|
| **Docker / moby** | **sets** a fixed default | hardcoded `/usr/local/sbin:…:/bin` | + `HOSTNAME` always; `TERM` only with `-t`. Never host-inherited. |
| **OCI runtime-spec** | no default (spec silent) | caller-supplied | each runtime decides |
| **bwrap** | **inherits** full parent env | whatever caller has | `--clearenv` is opt-in (keeps only `PWD`) |
| **flatpak** (on bwrap) | pass-through **minus a denylist** | always overridden | strips `LD_*`/`PYTHONPATH`/`PERLLIB`/`TMPDIR`/… ; remaps `XDG_RUNTIME_DIR`; `DISPLAY`/`WAYLAND` via portal |
| **firejail** | editing primitives only (`--env`/`--rmenv`) | n/a | no documented clear-by-default |
| **nsjail** | **deny by default** (`keep_env=false`) | must be set explicitly | strictest allowlist |
| **gVisor/runsc** | mirrors OCI | caller-supplied | not a distinct policy |
| **Firecracker jailer** | **wipes** everything | n/a | guest init's env is out of scope |
| **Chromium/Firefox sandbox** | undocumented | n/a | zygote-forked; no authoritative env spec found — do not cite a list |

Sources: [OCI config.md](https://github.com/opencontainers/runtime-spec/blob/main/config.md), [moby `CreateDaemonEnvironment`](https://pkg.go.dev/github.com/moby/docker/container), [bwrap(1)](https://manpages.debian.org/testing/bubblewrap/bwrap.1.en.html), [flatpak-run(1)](https://man.archlinux.org/man/extra/flatpak/flatpak-run.1.en), [firejail(1)](https://man7.org/linux/man-pages/man1/firejail.1.html), [nsjail config.proto](https://github.com/google/nsjail/blob/master/config.proto), [Firecracker jailer.md](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md), [Chromium zygote docs](https://chromium.googlesource.com/chromium/src/+/HEAD/docs/linux/zygote.md).

### The cross-system pattern

- **`PATH` is the only near-universal var** — but everyone either **force-sets it to a fixed safe value** (Docker, systemd, sudo `secure_path`) or **requires it explicitly** (nsjail). Nobody treats raw host-`PATH` pass-through as safe. **This does not contradict nub's empty POSIX floor:** those systems inject `PATH` because they run a *shell/command lookup* (a useful environment); the strip-all floor's contract is narrower — "start at all," not "run a shell." Node's own boot needs no `PATH`. `PATH` belongs in nub's *baseline*, not the floor.
- **The strict startup floor is genuinely tiny on POSIX (empty) and small on Windows** — the OS-mechanism vars only. Everything else systems keep (`HOME`, `TERM`, `LANG`, `SHELL`) is *usefulness*, which is the baseline's job.

---

## Secret / injection risk — what must NEVER ride the floor

None of the recommended floor vars is secret-bearing (all are path/topology pointers). But the survey surfaces a directly-reusable **never-keep** list — sudo's `initial_badenv_table` + the config/module-search-path injection vectors. nub's strip-all floor is an *allowlist* (only whitelisted names admitted), so these are excluded by construction — but they matter for the *baseline* and any future denylist tier, and are worth encoding explicitly:

- **Dynamic-linker injection:** `LD_*` (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`), `DYLD_*` (macOS), `LDR_*`/`LIBPATH` (AIX), `_RLD*`. sudo deletes all of these.
- **Interpreter/tool hijack:** `BASH_ENV`, `ENV`, `PS4`, `GLOBIGNORE`, `SHELLOPTS`, `NODE_OPTIONS`, `NODE_PATH`, `PYTHONPATH`/`PYTHONSTARTUP`/`PYTHONINSPECT`, `PERL5LIB`/`PERL5OPT`/`PERLIO_DEBUG`, `RUBYLIB`/`RUBYOPT`, `CLASSPATH`, `JAVA_TOOL_OPTIONS`/`_JAVA_OPTIONS`, `GIT_SSH_COMMAND`, `GIT_CONFIG_*`, `GCONV_PATH`.
- **Secret-name patterns** (codex + nub already do this): case-insensitive `*KEY*`, `*SECRET*`, `*TOKEN*`, `*PASSWORD*`, `*CREDENTIAL*`, plus `AWS_*`, `GITHUB_TOKEN`, `GH_TOKEN`, `NPM_TOKEN`, npm registry-auth (`npm_config_*_auth*`, `_password`, `email`).
- **Auth-material *paths* sudo keeps but a generic sandbox should NOT copy uncritically:** `XAUTHORITY` (X11 cookie file), `KRB5CCNAME` (Kerberos credential cache). sudo keeps them for interactive-desktop usability; they point at credential material and have no place on a strip-all floor.
- **`HOME` caveat (sudo's own warning):** "Preserving HOME … has security implications … adding HOME to env_keep … is strongly discouraged." Keeping the *caller's* HOME lets a program pick up attacker-controlled config/data files. nub's floor keeps no HOME; the baseline should keep it only deliberately.

**Privacy note on the Windows floor:** `LOCALAPPDATA` embeds the OS username. This disclosure is **redundant** — the child runs *as* that user and can read its own username via its token/`whoami` — so it leaks nothing new, and it is empirically required to start an AppContainer child. Injecting the real value is the minimal correct choice (a synthetic non-disclosing value needs a real writable scratch dir + fs grant for zero privacy gain). The libuv Cygwin-compat vars (`USERNAME`, `USERDOMAIN`, `LOGONSERVER`) carry the same class of identity info with the *same* redundancy — but nub does **not** need them to start (they're Cygwin-subprocess-compat, not loader-essential), so they stay off the floor.

---

## Recommendation for nub

1. **POSIX (macOS + Linux): keep the floor EMPTY.** Now cited (empty-environ exec works; Node reads no boot-required var; Firecracker jailer / nsjail corroborate). Update the `defaults.rs` doc comment to cite this doc rather than only the VM pin.
2. **Windows: widen `OS_ESSENTIAL_ENV` from `{SystemRoot, LOCALAPPDATA}` to `{SystemRoot, SystemDrive, TEMP, TMP, LOCALAPPDATA}`** — libuv's three strict-essentials (`SYSTEMROOT`, `SYSTEMDRIVE`, `TEMP`) + `TMP` (Node's `os.tmpdir()` fallback pair) + `LOCALAPPDATA` (AppContainer). All non-secret path/topology pointers; a citation-backed superset of today's set that can only help a start. **Gate the widen behind a `ci-adhoc-test` Windows subset re-sweep** confirming the added vars never break a start (recommend-only; this is a sandbox-security-posture call for the maintainer).
3. **Do NOT add the libuv Cygwin-compat vars** (`USERNAME`, `USERDOMAIN`, `LOGONSERVER`, `HOMEDRIVE`, `HOMEPATH`, `USERPROFILE`, `PATH`) to the floor — they're subprocess-compat, not start-essential, and three carry identity info. They belong in the baseline if anywhere.
4. **Encode sudo's `initial_badenv_table` as an explicit never-keep constant** for the baseline / any denylist tier — it is the authoritative published injection-vector list and is currently only implicit in nub's allowlist model.

This is research/recommend-only. The floor widen (item 2) and the never-keep encoding (item 4) are the two code follow-ups; both touch the sandbox security posture and are maintainer-sign-off items, not autonomous lands.

## Sources surveyed

- nub current impl: `crates/nub-sandbox/src/compiler/defaults.rs` (commit `bd52bfe2b8`, PR #408).
- **codex** strip-all allowlist: `codex/codex-rs/protocol/src/shell_environment.rs`.
- **libuv** Windows required vars: `node/deps/uv/src/win/process.c` (`required_vars[]`, `make_program_env`); Node boot reads in `node/src/node.cc`, `lib/os.js`, `deps/uv/src/unix/core.c`.
- **SRT** (Anthropic sandbox-runtime) macOS/Windows env handling: `sandbox-runtime/src/sandbox/{macos,windows}-sandbox-utils.ts` (denylist model, `SystemRoot ?? C:\Windows`).
- **sudo**: [`sudoers(5)`](https://man7.org/linux/man-pages/man5/sudoers.5.html), [`plugins/sudoers/env.c`](https://raw.githubusercontent.com/sudo-project/sudo/main/plugins/sudoers/env.c).
- **systemd**: [`systemd.exec(5)`](https://man.archlinux.org/man/systemd.exec.5.en), [`systemd-system.conf(5)`](https://man.archlinux.org/man/systemd-system.conf.5.en).
- **OpenSSH**: [`sshd_config(5)`](https://man7.org/linux/man-pages/man5/sshd_config.5.html), [`ssh_config(5)`](https://man.openbsd.org/ssh_config).
- **login/PAM**: [`login(1)`](https://man7.org/linux/man-pages/man1/login.1.html), [`login.defs(5)`](https://man7.org/linux/man-pages/man5/login.defs.5.html), [`pam_env(8)`](https://man7.org/linux/man-pages/man8/pam_env.8.html).
- **Containers/sandboxes**: [OCI config.md](https://github.com/opencontainers/runtime-spec/blob/main/config.md), [moby container pkg](https://pkg.go.dev/github.com/moby/docker/container), [bwrap(1)](https://manpages.debian.org/testing/bubblewrap/bwrap.1.en.html), [flatpak-run(1)](https://man.archlinux.org/man/extra/flatpak/flatpak-run.1.en), [firejail(1)](https://man7.org/linux/man-pages/man1/firejail.1.html), [nsjail config.proto](https://github.com/google/nsjail/blob/master/config.proto), [Firecracker jailer.md](https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md), [Chromium zygote docs](https://chromium.googlesource.com/chromium/src/+/HEAD/docs/linux/zygote.md).
- **Windows/Microsoft**: [DLL search order](https://learn.microsoft.com/en-us/windows/win32/dlls/dynamic-link-library-search-order), [Recognized environment variables (USMT)](https://learn.microsoft.com/en-us/windows/deployment/usmt/usmt-recognized-environment-variables), [GetTempPath2A](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-gettemppath2a), [Implementing an AppContainer](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer), [`start` command / PATHEXT+COMSPEC](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/start).

## Changelog

- 2026-07-10 — Initial write-up.
