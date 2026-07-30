# Build jail on macOS — every approach tried, and why

Canonical ledger of every mechanism attempted for Nub's **build jail** on macOS, one heading per approach. The build jail confines dependency lifecycle scripts during `nub install` and must be **totally unprivileged with no setup command, ever**. macOS is the platform where that constraint costs the least: Seatbelt is unprivileged, `/usr/bin/sandbox-exec` ships with the OS, and there is no setup command to skip. **macOS is therefore where the strongest enforcement lives — and the place where over-claiming it as cross-platform is most tempting.**

**This document exists because approaches keep getting re-proposed after being refuted**, and because macOS carries two capabilities the other platforms do not — deny-inside-allow and genuine per-host egress — which makes it easy to write a claim here that is false on Linux or Windows.

## How to use this document

Each approach carries a status, what it would have bought, the evidence with its measurement tool, and — the field that makes this more than an obituary — **what would have to change for it to become viable again**.

### Status values

| status | meaning |
| --- | --- |
| **ADOPTED** | in the shipping design |
| **DEAD (mechanism)** | the OS primitive cannot do it |
| **DEAD (privilege)** | needs elevation — disqualifying for the build jail |
| **DEAD (compat)** | works and confines correctly, but breaks packages |
| **OPEN** | unresolved; a live blocker, an accepted residual, or a held product decision |
| **REJECTED (design)** | technically available and deliberately not used |

### Measurement tools used

| tool | what it establishes | trap |
| --- | --- | --- |
| raw `sandbox-exec -p <profile>` differentials | SBPL semantics — precedence, operation coverage, evasion closure | **Write rules against `/private/tmp`, not `/tmp`.** Seatbelt canonicalizes, so `/tmp` rules match nothing, the deny goes inert, and every arm "passes". That broken control produced a false confirmation once. |
| the real compiled build jail against a real `nub install` | end-to-end behaviour, broker bypasses included | **Clear `~/.cache/nub/pm/side-effects-v1/` between arms and check marker mtime** — it replays build side effects without re-running the script, and produced a false negative that nearly yielded a wrong conclusion. |
| Seatbelt denial lines (`Sandbox: node(NNNN) deny(1) <op> <path>`) | which operation and path was refused | a message-less abort names nothing; that is what made the stdio `fstat` case hard |
| byte-count diffs against an unconfined control | whether a broker read the real thing | an exit code cannot see it — `defaults read` returned **byte-identical** output under the jail |

---

# The adopted mechanism

## Seatbelt via `sandbox-exec -p` — ADOPTED

**What it is.** The resolved policy IR is compiled to an SBPL profile and enforced by wrapping the child in `sandbox-exec -p <profile> -- <cmd>`. Module doc: `crates/nub-sandbox/src/backend/macos.rs:1-35`. Posture is `(deny default)`; the `MACOS_SEATBELT_BASE` block (ported from Codex/Chromium, `backend/macos_seatbelt_base.sbpl`) is the bootstrap that lets an arbitrary binary dyld-load under a deny-default profile, and Nub then appends the IR-derived read/write/net rules.

**What it buys.** Deny-by-default filesystem confinement, **live evaluation** with no enumeration break, closed path-based evasion, mediated metadata ops, and — uniquely among the three platforms — **genuine per-host egress at zero privilege**.

**Zero privilege, and there is nothing to install.** `/usr/bin/sandbox-exec` is the stock unprivileged entry point and every confined launch goes through exactly that path, which is what makes its presence the only readiness question macOS has (`macos.rs`'s `SANDBOX_EXEC_PATH`, re-exported by `macos_setup::enforceable` rather than restated).

**Live evaluation — no enumerate-to-exclude needed.** A file **created after launch** under a granted subtree reads back fine, and a post-launch `.envlate` is still **denied**. This is why the banned enumerate-to-exclude pattern is unnecessary here, and it is the property Windows' setup-time enumeration and Landlock's fixed ruleset both lack.

**Path-based evasion is closed.** Hardlink, symlink, `cp`, `mv`, `..`, `.`, `//`, and the `/tmp`→`/private/tmp` alias are **all blocked** — Seatbelt canonicalizes before matching.

**One hard ceiling, measured.** The profile is an **argv element** and shares `ARG_MAX` (~1 MiB) with the child's full environment: ~1400 grants ≈ 1.04 MB is fine, 3000 → `E2BIG`. That ceiling was nearly hit once — see [the grant explosion](#the-grant-explosion-under-arg_max--adopted-fix).

## The pure allowlist under `(deny default)` — ADOPTED, and macOS is NOT an exception to it

**What it is.** The build jail on macOS is a **pure allowlist that emits zero deny rules**, exactly as on Linux and Windows. `compiler/preset.rs:424-428` states it directly: *"NO `/etc/shadow` deny here any more. The build jail is a PURE ALLOWLIST — it emits no deny rules at all — so the password-hash files are protected by not being granted … on macOS by the Seatbelt base granting the specific `/private/etc` files it needs instead of the whole subpath."*

**A premise to correct, because it is easy to form and it is wrong.** The generous-read base `(allow file-read* (subpath "/"))` is emitted **only** when `default_effect == Allow` (`macos.rs:12-14`, `:737`), which is the `nub sandbox` / `sandbox: true` shape — **not** the build jail's. **macOS is a pure allowlist on the read axis too**, with the same guarantee shape as Linux and Windows. Anyone reading "macOS uses a generous read base with secrets carved out by deny" is reading the agent-sandbox product, not the build jail.

**What the build jail's read set actually is** (`preset.rs:250-341`, `grant_build_jail_dependency_reads`), and it is measured rather than reasoned — a 34-package read-ladder study plus a 311-package trust-list corpus, of which 217 of 219 passing packages were unaffected by the narrowing:

- The consumer's `node_modules` (**not** the whole project). A lifecycle script's own dependencies are hoisted there, so `node-gyp-build` and `prebuild-install` resolve out of `<project>/node_modules/.bin`; dropping the project read outright fails **27 of 33** packages, and keeping only `node_modules` costs nothing.
- The consumer's top-level `package.json` **as one file, never the directory that holds it**. Two packages at scale crash with an uncaught `ENOENT` without it — `@sentry/capacitor` cross-checks its version against sibling `@sentry/*` entries, and `simple-git-hooks` looks for its own config field.
- Two `NUB_PM_CACHE_PATTERNS` subtrees of Nub's own PM cache — including `<cache>/nub/pm/tools/node-gyp`, **a toolchain grant wearing a cache-directory name**, which under Nub is the ONLY node-gyp a confined script can reach. The other 15 `$tooldirs` patterns (`~/.cargo/registry`, `~/.m2/repository`, the pnpm/yarn/bun stores) were reached by **no package in either corpus**.
- The `node_modules` the package **actually** sits in, which is not always the project's — aube's hoisted planner is per-importer, so a workspace member's dependency resolves through `<root>/packages/<m>/node_modules/.bin`, outside `<project>/node_modules` entirely. Missing it reproduces exactly the 27-of-33 failure, **but only in workspaces**, which is how it would have escaped a single-project corpus.

**What the narrowing bought.** Under the old `"./"` grant a dependency's install script could read the consumer's source, config, `.git/hooks/` and `.github/workflows/`.

## Deny-inside-allow — available at zero privilege, REJECTED (design) for the build jail

**What it is.** Grant a subtree and deny one file inside it. **It works, and it is strong**: a single regex deny expresses the whole `.env*` family, and **last-match-wins is exact** — reversing the order re-allows, measured both directions.

**Why the build jail refuses it anyway, uniformly including on macOS.** A policy that passes on the author's Mac and fails on CI Linux is the worst available outcome. Neither unprivileged Linux nor unprivileged Windows can express it — bwrap's mount-masking is the outlier, not the norm — so it is not in the claim, and the build jail's own shape has nothing for a deny to sit inside.

**The scoping nuance, so this is not over-corrected.** The `.env*` secret floor **remains load-bearing for `nub sandbox`**, which is generous-read-minus-secrets and therefore genuinely must deny a file inside a granted tree. That is precisely what its escalation buys. **The error was applying it to the build jail**.

**One concrete cost of the uniformity rule.** `deny_shadows_grant` fail-closes any policy carrying a deny whose `literal_prefix` is `""` — `**/.env*` normalises to `""`, and six floor globs trip it — so **putting a deny back into the shared IR immediately re-breaks Windows.** The build jail emitting zero denies is what keeps all three backends consistent.

## The loopback egress proxy with port pinning — ADOPTED, and the only genuine per-host enforcement anywhere

**What it is.** Egress is permitted to **exactly** the proxy's loopback port — `(allow network* (remote ip "localhost:{port}"))` at `macos.rs:719`, deliberately **not** `localhost:*`, with a test asserting that (`macos.rs:2583-2596`). Every packet must traverse Nub's proxy, and **a raw socket cannot bypass it.**

**What it buys.** Real per-host containment at zero privilege — the one platform where the build jail's host allowlist is an actual boundary rather than a hint.

**End-to-end under a real `nub install`, macOS 26.5.2 arm64**. A jailed `postinstall` sees `HTTP_PROXY`/`HTTPS_PROXY`/`http_proxy`/`https_proxy`/`npm_config_proxy`/`npm_config_https_proxy = http://<per-session-token>@127.0.0.1:<port>` (`macos.rs:135` → `set_proxy_env`), and the cells separate cleanly:

| cell | jail OFF | jail ON |
| --- | --- | --- |
| `CONNECT nodejs.org:443` via the proxy *(in `$downloads`)* | n/a | **HTTP 200 Connection established** |
| `CONNECT example.com:443` via the proxy | n/a | **HTTP 403 Forbidden** |
| `CONNECT webhook.site:443` via the proxy | n/a | **HTTP 403 Forbidden** |
| direct dial to the ALLOWLISTED host by raw IP:443 | CONNECTED | **EPERM connect** |
| direct dial to a NON-allowlisted raw IP:443 | CONNECTED | **EPERM connect** |
| direct dial to a live loopback listener ≠ the proxy port | CONNECTED | **EPERM connect** |

**⇒ Honouring the proxy is required for FUNCTIONALITY, never for SECURITY** — ignoring it yields EPERM, not a bypass.

**The claim boundary this creates, and it is the most over-claimed fact here.** Per-host egress filtering is enforced on **macOS only**. On Linux the same policy collapses to coarse deny (no proxy env reaches the child, no netns exists, the seccomp family ceiling EPERMs everything) and on Windows it is coarse deny by construction. **The cross-platform floor is coarse on/off**, and the load-bearing defense is PACKAGE IDENTITY — no catalog entry means no network at all. Per-host is defense in depth on macOS and **nothing** on Linux and Windows. **A doc or catalog note promising per-host enforcement cross-platform would be false.**

## The `TmpMode::Private` per-run scratch — ADOPTED

**What it is.** A fresh per-run tmp directory granted read-write, with the shared host tmp hidden. Set by the `$tmp` surface key (`preset.rs:385-387`), which sets the MODE rather than emitting an ordinary fs rule.

**One shipped bug, and it is an instance of the precedence footgun below.** The private-dir grant was emitted in a different SBPL node from the shared-tmp deny it had to override, so the grant was a silent no-op. Fixed on `fix/tmpmode-private-writable` `5788301649` — cypress went to exit 0. The general lesson is the [SBPL precedence rule](#the-sbpl-precedence-rule--the-footgun-that-shipped-twice).

**A design consequence users hit.** A project under `/tmp` is **unbuildable**: the tmp-confinement deny covers `/private/tmp` wholesale and nukes the project grant. By design, but it silently presents as *"the jail grants nothing."*

**A residual recorded, not fixed.** clang emits `couldn't open cache file '…/xcrun_db'` on every jailed compile (non-fatal, both arms) — the `Private` tmp mode carves the confstr dir out of the *deny* but the tight build-jail read set never *grants* it, so `emit_tmp`'s documented carve-out does not hold under `build-jail`.

---

# The broker class — what a path allowlist cannot bound

## The `cfprefsd` broker bypass — ADOPTED fix, by grant REMOVAL

**What it was.** `macos_seatbelt_base.sbpl:124` carried an explicit `(allow user-preference-read)`. Under the real compiled build jail, `defaults read -globalDomain` returned **7917 bytes, byte-identical to unconfined**, while the underlying `~/Library/Preferences/.GlobalPreferences.plist` was refused at the file layer.

**Why no amount of path granting or withholding reached it.** `cfprefsd` is a separate, **unsandboxed** process that resolves `HOME` from `getpwuid` and reads on the child's behalf. Domain enumeration was already closed, so it was guess-the-domain — trivial for any known app.

**The fix is a grant removal, and removal is all that is needed.** Unlike `process-info*`, `user-preference-read` is **not** allowed-by-default, so deleting the grant drops it to `(deny default)`. Measured with the plist path denied so the file route could not confound the result:

| profile | result |
| --- | --- |
| grant present | **7916 bytes, rc=0** |
| grant removed | **0 bytes, rc=1** |
| grant removed **plus** an explicit `(deny user-preference-read)` | **identical, 0 bytes rc=1** |

**That equality is why this is a grant removal rather than a deny** — the explicit deny buys nothing, the build jail emits zero denies, and an SBPL deny here would be a rule the Landlock and AppContainer backends have no way to mirror. `backend/macos_seatbelt_base.sbpl:119-141` carries the full record; branch `sandbox/jail-secret-channels` `16ba78916b`.

**Compat verified, not assumed.** `clang`, `make`, `perl`, `python3`, `git`, `xcrun`, `tar`, `curl` and `node` all behave identically with and without it — **node included down to its resolved `Intl` locale**, which is the one consumer that plausibly needed it. The mach port and the shared-memory segment stay granted because CF connects during startup; only the operation that returns a domain's **contents** is gone.

**⇒ Generalize the lesson: any Mach-service broker can read outside the file allowlist. A path-only audit will not find this class.**

## The complete broker surface, enumerated — ADOPTED as the audit method

**What it is.** `(deny default)` covers `mach-lookup`, so the profile's **24 `global-name` entries ARE the whole broker surface.** That is what makes this class auditable at all rather than open-ended. Control establishing the enumeration is complete: keychain fails with `SecKeychain*` parameter errors because `com.apple.SecurityServer` is not listed.

**Verdicts, one per broker:**

| broker | verdict |
| --- | --- |
| `cfprefsd` | was open; **now closed** by grant removal |
| `logd` / `diagnosticd` / `analyticsd` / `system.logger` | closed **by Apple** — `log show` returns *"Cannot run while sandboxed"* |
| `trustd` | **oracle only** — returns a verdict, not bytes |
| `notification_center`, `bsd.dirhelper` | name/path only, no payload |
| `securityd` / keychain | **not listed, denied** |
| `opendirectoryd` | **open, deliberately** — see below |

**What would change this.** A new `global-name` entering the base profile. Any addition to that list is a broker-surface change and needs the same treatment `cfprefsd` got.

## The `opendirectoryd` residual — OPEN, accepted deliberately

**What it is.** With `/var/db/dslocal` file-denied, `id` and `dscacheutil -q user` still return the **entire local user database** — every account name, uid, gid, home, shell, gecos real name — plus group membership.

**Why it is accepted, and removal was measured rather than assumed.** **Password hashes are masked, so this is RECON, not a credential.** Removing it breaks things and does not even close the leak: Node's `os.userInfo()` **hard-throws** `ERR_SYSTEM_ERROR: uv_os_get_passwd returned ENOENT`, `whoami` degrades to a bare `501`, **and group membership still leaks anyway.** Symmetric with Linux, which grants `/etc/passwd` + `/etc/group` outright for the same reason.

**Do not re-open this as a finding.** It is a decided accepted residual with the removal arm already measured.

---

# SBPL semantics — three footguns, each of which shipped a silent no-op

## The SBPL precedence rule — the footgun that shipped twice

**The rule, measured for both operation families on darwin 25.5**:

> **Across operation nodes the MORE SPECIFIC NODE WINS REGARDLESS OF POSITION. Within ONE node, position governs (last-match-wins).**

| profile | result |
| --- | --- |
| `(deny file-write* P)` → `(allow file* P/sub)` | **denied** |
| `(allow file* P/sub)` → `(deny file-write* P)` | **denied** |
| `(deny file-write* P)` → `(allow file-write* P/sub)` | **allowed** |
| `(allow file-write* P/sub)` → `(deny file-write* P)` | **denied** ← the sharp edge |
| `(deny file-read* P)` → `(allow file* P/sub)` | read **denied** |
| `(deny file-read* P)` → `(allow file-read* P/sub)` | read **allowed** |

**Row 4 is the trap: a more specific PATH does NOT win if it sits before the deny.** Specificity resolves the *node*; order resolves within it.

**⇒ A re-grant must be emitted in the SAME node as the deny it must override.** An `(allow file* …)` after a `(deny file-write* …)` is a silent no-op. **This shipped twice** — `emit_tmp`'s per-run tmp grant and `darwin_compiler_cache_files()` — **both carrying a comment asserting the opposite.**

**The corollary already seen in the wild.** A `file-read-metadata` allow beats a `file-read*` deny at **any** position, which is why the stdio grant must withhold policy-denied paths or it punches a stat-shaped hole through the floor. See [the stdio metadata grant](#the-stdio-file-read-metadata-grant--adopted).

**⚠️ When probing this, write rules against `/private/tmp`, not `/tmp`.** Seatbelt canonicalizes, so `/tmp` rules match nothing, the deny goes inert, and every arm "passes". That broken control produced a false confirmation once.

## A `(deny file* …)` rule is silently inert for reads

**What it is.** `(deny file* …)` does **not** deny reads, while `(allow file* …)` does grant them. `(deny file-read* …)` is the working form. Invalid op names *are* rejected, so this is not silent-ignore in general — just this shape.

## Metadata reads are evaluated against an fd's vnode

**What it is.** `file-read-metadata` is evaluated against an fd's **vnode** on `fstat()`, even for a descriptor the process never opened by path. **Only WRITE-ONLY fds are affected** — an `O_RDWR` stdio fd stats fine ungranted. That is why an interactive terminal and a pipe pass and only a `>` redirect aborted Node — see the next section.

---

# Defects found and fixed

## The stdio `file-read-metadata` grant — ADOPTED

**The defect.** Without it, **every Node under a confining profile dies with SIGABRT and no diagnostic.** Node's `PlatformInit` stats fds 0/1/2 before its own error machinery is up and reads `if (errno != EBADF) ABORT()`, so an ungranted stdio path turns EPERM into a message-less abort inside `InitializeOncePerProcessInternal`. The denial line that finally named it: `Sandbox: node(3101) deny(1) file-read-metadata /private/tmp/.../out.log`. **Node is only the loudest victim; any program that stats its own stdio hits the same wall.** `macos.rs:467-528`.

**Why it presents so erratically.** Only a **write-only** fd is affected, which is why an interactive shell survives and a `>` redirect — the shape a log-capturing harness and every CI job produce — does not. That asymmetry is what made it look like a contention flake.

**Scope, kept minimal and verified rather than assumed.** Metadata only — verified that a bare metadata grant yields `statSync` and `access` but EPERM on read/open/readlink/readdir — on the exact resolved path, **never a parent directory**. A pipe or socket has no vnode, `F_GETPATH` fails, and nothing is granted.

**The policy-deny check is load-bearing and position is not.** Every deny in a compiled profile is `file-read*` (both the policy's own via `emit_fs`, and `emit_tmp`'s shared-tmp deny), and a `file-read-metadata` allow beats a `file-read*` deny **at any position** — measured, both orders boot Node — because the leaf operation outranks the group. **So withholding the path is the only thing that closes it**; a broader stdio grant, or one that did not consult the policy, would silently punch a stat-shaped hole through the `.env` / `~/.ssh` floor that `compiler::defaults` promises no later allow can reopen.

**Not to be confused with the other SIGABRT.** A separate jail-specific `SIGABRT` in `InitializeOncePerProcessInternal` is seen **only under heavy host load**, with contention not ruled out, and is marked "do not chase". The stdio case above is the one with a mechanism and a fix. Related work: branch `jail-stdio-abort` `549564f36b` concluded the remaining stdio abort is **not worth fixing** because it is unreachable under the build jail, and shipped a `LIMITATIONS.md` note instead.

## Native builds failing with `spawn EPERM` on `make` — ADOPTED fix, and the stated hypothesis was refuted first

**The defect.** Python discovery and Makefile generation succeeded, then `gyp.spawn('make', argv)` failed with `spawn EPERM`.

**Strike the recorded lead — it is wrong.** An earlier round hypothesised that Seatbelt's `file-read*` implies `file-map-executable`, on the observation that `/Applications` gets composite `file-read*` while `/usr/bin` gets only `file-read-data`+`file-read-metadata`. A three-arm `sandbox-exec` differential showed `file-read-data`+`file-read-metadata` alone **execs fine**, and `/usr/bin/make` runs under the *unmodified* base profile. **The grant asymmetry is not the cause. Do not re-chase it.**

**The real mechanism, isolated to one variable.** Seatbelt denies an ungranted **symlinked** PATH entry with `EPERM`, and **`posix_spawnp` treats `EPERM` as FATAL — aborting the entire PATH search** instead of skipping that entry. The same directory reached by its *real* path is skipped harmlessly. The decisive measurement, same process, same PATH, same dirs:

```text
libuv (Node child_process): ERR EPERM ← aborts the search
libc execvp (/usr/bin/env): rc=0 ← skips the entry, finds /usr/bin/make
```

**It is libuv-specific** — a plain C `posix_spawnp` does not reproduce it, independently confirmed on review. On the test host `/opt/homebrew/opt/openjdk/bin` — a Homebrew `opt` symlink — sat at PATH entry 10 of 56 and **masked `/usr/bin` at entry 32**. That is why Python *discovery* appeared to succeed while `make` failed.

**The fix: canonicalize absolute PATH entries handed to the jailed child.** Two-arm differential on a real `nub` binary, cache cleared, marker mtime checked, **and the jail verified live in BOTH arms** (`~/.zshrc`, `~/.ssh`, `$HOME` write all still blocked, so the fix did not loosen confinement):

| | bare `make` | bare `sh` | bare `cc` | `/usr/bin/make` |
| --- | --- | --- | --- | --- |
| fix OFF | ERR EPERM | ERR EPERM | ERR EPERM | rc=0 |
| fix ON | rc=0 | rc=0 | rc=0 | rc=0 |

**The absolute-path column is a built-in control** — unchanged across arms, proving the change is specific to PATH *search*.

**The same canonicalization rule binds the child's `PATH` generally**, because a PATH is a path list the child hands back to the kernel (`macos.rs:29-35`, `canonicalize_path_var`). And the IR matchers must be firmlink-resolved on their literal prefix, since Seatbelt checks the **canonical** path — a `/tmp/…` allow that was not canonicalized is **inert, silently denied**.

**Landed on `fix/macos-build-jail`, folded to `4f64e230d0`.** One honest caveat: the committed PATH test asserts the child's observed PATH, not end-to-end bare-program resolution — that arm needs a real libuv spawner and the test file has no node; verified manually twice against the real binary. A `tests/<probe>/` harness would close it.

## The missing descriptor sweep — ADOPTED fix, and the leak was real

**The defect.** `grep -c pre_exec backend/macos.rs` returned **0**. Linux runs `close_range(3, u32::MAX, CLOSE_RANGE_CLOEXEC)`; macOS relied entirely on CLOEXEC-by-construction in mio and socket2. *"One mistake wide"* — a `dup()`ed socket or a `socket2::Socket::new_raw` would leak.

**The fix.** A `pre_exec` sweep marking fds ≥ 3 CLOEXEC, enumerated via `PROC_PIDLISTFDS` — macOS has no `close_range`, and Nub raises `RLIMIT_NOFILE` to ~1M, so a blind loop would cost ~1M syscalls per spawn.

**The test's negative control demonstrates the escape**: without the sweep, a confined child read a file the policy denies. **Pairs with the Linux fd-egress measurement — both backends were leaking.**

## The grant explosion under `ARG_MAX` — ADOPTED fix

**The defect.** Image grants were emitted for every speculative path whether or not it existed, producing **~211 KB of SBPL — about 21% of a shared `ARG_MAX` budget** on a stock pyenv+Homebrew Mac.

**The fix.** Filter image grants on `is_file()`: **352 unique → 8 on disk**, measured. Linux already skipped the phantoms via `FsOrigin::Speculative`; **macOS emitted every one because the SBPL loops filter on `Effect`, never on `origin`.**

**One adjacent over-grant closed in the same commit.** `symlink_hop_dirs`' `components().count() > 2` guard admitted `/Users/<user>` (3 components), so a symlinked interpreter under `$HOME` granted read on the **entire home directory**, and the ancestor-collapse then swallowed every other entry into it. Fixed by rejecting any hop dir at or above `$HOME`, in a single `grantable` predicate that also refuses `/`, one-level-below-root, and any surviving `..` — which `canonical`'s raw-path fallback would otherwise let collapse to `/` inside the policy compiler. Unit-tested.

## A PATH-searching shim defeats a caller-side allowlist — ADOPTED fix, and the lesson generalizes

**The defect.** A guarded Python-candidate allowlist was defeated because the chosen shim **re-searched the same PATH and ran the planted script anyway, still unconfined.** Three layers were required, and the middle one is what actually closed it: candidate eligibility, **the probe's own PATH filtered to the same rule**, and re-gating the interpreter the probe reports back before it becomes either the named interpreter or a grant. Plus **`-I`** for a separate `sys.path[0] == cwd` hijack — a `ctypes.py` in the dependency's package dir, reproduced and blocked by `-I`, with the four reported values byte-identical across three interpreters.

**Control:** pre-guard build **escaped** (`pwned` written, `ssh=20` read, unconfined, full env); at `42914756dd` **refused**, no file written.

**⇒ A shim that re-execs a PATH search defeats a caller-side allowlist. Guarding which candidate you *choose* does nothing if the chosen binary re-runs the same search.**

## Claims that failed to reproduce — REFUTED, recorded so they are not re-filed

- **`file:` deps bypass the jail entirely.** The premise was simply wrong. Both `file:` shapes are fully jailed (tarball and `file:../dir`; all escape probes blocked EPERM, no escape file). **`file:` deps never reach `RootProvenance`** — they go through `run_dep_hook`, which sandboxes unconditionally under the Nub embedder (`vendor/aube/.../lib.rs:1612`); `RootProvenance` is reached only from the git-dep nested install. Fork-discipline risk avoided entirely.
- **The CAS-store `.mk` write is a macOS sandbox defect.** Confirmed as a real break but **misfiled**: it is a **linker** bug and **not macOS-specific**. `require('node-addon-api').gyp` returns a path that climbs out of the project because Nub symlinks registry deps to the global CAS, and gyp re-anchors it under `build/`. There is no acceptable jail-side fix — the target is an arbitrary ancestor-relative phantom tree outside the project, so granting write there would be a filesystem-wide hole. The correct fix materializes registry packages into `node_modules/.store/<name>@<ver>/node_modules/<name>` by hardlink/clonefile. **The layout is platform-independent, so Linux hits it too** — tracked in [`build-jail-linux.md`](build-jail-linux.md)..
- **Both of the code audit's "REAL" pen-test findings** failed to reproduce as breaches under a running install (macOS Seatbelt arm,).

---

# Open items

## Node-gyp Python discovery — OPEN, and it is a held product decision

**The defect.** Not fixed by PATH canonicalization, and canonicalization **cannot** help: `~/.pyenv/shims` is a **real, ungranted** directory that genuinely contains `python3`, so `posix_spawnp` finds it and the exec is denied — `rc=126 … /Users/…/.pyenv/shims/python3: Operation not permitted`. `/usr/bin/python3` works.

**Two fixes, and they trade against each other:**

| option | preserves | costs |
| --- | --- | --- |
| Grant the resolved Python's toolchain tree (mirroring the existing `npm_config_nodedir` pattern at `build_jail.rs:79-84`) | compat — the user's own interpreter builds their addon, as under npm/pnpm | a real read+exec expansion over a user-managed tree, and **a half-grant is worse than none** (pyenv shims and Homebrew pythons each need their whole stdlib prefix) |
| Set `PYTHON` set-if-absent to a known-good interpreter | a tiny, closed read set | it silently changes **which** Python builds the addon, which can break a project that pinned one deliberately |

**Left undecided on purpose** — "which Python builds your addon" is a product call, and it expands the jail's read surface.

## macOS nesting — DEAD (mechanism)

**What it was.** Compose a second Seatbelt profile inside an already-confined process, so a nested `nub` invocation could tighten further.

**Verdict.** Impossible; the broker / parent-launcher pattern is the only shape available. **Not needed for the build jail** — no nesting, settled. It is `nub sandbox`'s concern.

## Grants that are cheap on macOS and not portable

Two recorded traps where a macOS-only measurement produced a broken cross-platform grant:

- **`projectCwd` was load-bearing on Seatbelt and a NO-OP on Landlock** (`chdir` is not a Landlock-handled access) — *"which is exactly how the Prisma entry shipped broken after being measured only on macOS."* The field was removed from the settled grant schema.
- **`@prisma/client`'s grant is recorded `macos-arm64` and does not work on Linux** — the differential `DIFFERS` identically with and without it.

**⇒ Never ship a build-jail grant measured only on macOS.** macOS is the most permissive platform to measure on and the least representative.

## Contradictions in the record, unresolved

1. **The generous-read premise.** Any statement that the macOS build jail uses a generous read base with secrets carved out by deny is **false** — that is the `nub sandbox` shape (`default_effect == Allow`), while the build jail is a pure allowlist on the read axis (`preset.rs:424-428`, `macos.rs:737`). The confusion is easy because the same backend serves both products and the module doc describes both in one paragraph.
2. **Two different SIGABRTs are recorded under one name.** The stdio `fstat` abort has a mechanism, a denial line and a fix (`emit_stdio_grants`); the load-dependent abort is a separate, unexplained item marked "do not chase". Reading the second alone leads to the conclusion that the fixed one is still open.
3. **A per-host claim that only holds here.** Grouping Linux and Windows together as best-effort egress via `HTTP(S)_PROXY` is accurate for Windows (via the userland net gate) and **false for Linux**, where no proxy env reaches the child at all — so the macOS/not-macOS split is binary, not a three-way gradient. Cross-referenced in both sibling documents.

## Changelog

- 2026-07-30 — Moved into tracked `research/design/` so code comments can link here, and scrubbed of pointers into untracked documents. Every measurement, table and verdict is unchanged.
- 2026-07-29 — Initial consolidation.
