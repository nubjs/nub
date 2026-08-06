# Build jail on Linux — every approach tried, and why

Canonical ledger of every mechanism attempted for Nub's **build jail** on Linux, one heading per approach. The build jail confines dependency lifecycle scripts during `nub install` and must be **totally unprivileged with no setup command, ever, including inside a container** — that constraint is what decides almost every row below, and it is the single reason bubblewrap is not the answer here.

**This document exists because approaches keep getting re-proposed after being refuted.** The most-repeated Linux mistake is proposing bubblewrap for the build jail, which is on record as already-burned; the second is proposing a Landlock deny rule, which no ABI has.

## How to use this document

Each approach carries a status, what it would have bought, the evidence with its measurement tool, and — the field that makes this more than an obituary — **what would have to change for it to become viable again**.

### Status values

| status | meaning |
| --- | --- |
| **ADOPTED** | in the shipping design |
| **DEAD (mechanism)** | the kernel primitive cannot do it; no privilege or tuning helps |
| **DEAD (privilege)** | works, but needs root, a capability, or a setup command — disqualifying for the build jail |
| **DEAD (compat)** | works and confines correctly, but breaks packages |
| **OPEN** | unresolved; a live blocker or an accepted-but-unclosed residual |
| **REJECTED (design)** | technically available and deliberately not used |

### Measurement tools used

| tool | what it establishes | trap |
| --- | --- | --- |
| a real `nub install` with a `file:` dep carrying a `postinstall`, jail toggled by `dependenciesMeta.<pkg>.sandbox`, with a jail-OFF control | end-to-end behaviour of the shipped policy | none, and this is the strongest evidence class here |
| `/proc/<pid>/ns/net` inode sampled on the live child | whether a network namespace exists | a *dead* child's inode says nothing — sample the live pid |
| `strace -e trace=%file` under an emptied `/etc` | which paths the kernel was actually asked for | distro-shaped; a Debian-only corpus misses the RHEL TLS layout |
| run-log mining (`ENOENT`/`EROFS`/`getaddrinfo` naming the path or host) | broad coverage across hundreds of packages | only works where the denial names the thing — Landlock's `EACCES` is less informative than bwrap's `ENOENT` |
| `bwrap --unshare-user` on a freshly created VM, with the sysctl printed beside the result | whether an unprivileged userns is usable | **`unshare --user true` is a BROKEN control** — see the bubblewrap section |
| a real Landlock enforcement test inside Docker `--cap-drop=ALL` | whether the mechanism survives a container | Docker returns `ENOSYS` for unlisted syscalls, so "no Landlock" and "seccomp blocked it" are indistinguishable without a control |
| the dev-only catalog override (`src/catalog_override.rs`) | which grants an arm actually ran with, without a rebuild between iterations | it REPLACES the compiled catalog rather than merging over it, and a build without the `build-jail-catalog-override` cargo feature REFUSES a set `NUB_BUILD_JAIL_CATALOG` rather than ignoring it — so an arm cannot silently measure the shipped tables under an override's name |

---

# The adopted mechanism

## Landlock plus seccomp — ADOPTED

**What it is.** `landlock_restrict_self` for the filesystem axis and a seccomp socket-family filter for the network axis, both applied pre-`execve` with no namespace of any kind. Module doc: `crates/nub-sandbox/src/backend/linux_landlock.rs:1-19`. The rule set is derived from the **same** `compile_mount_plan` the bubblewrap backend consumes, so the two mechanisms cannot drift on which paths a policy grants. Selection happens in exactly one place — `backend/linux.rs:213-238` `preflight()`: *"THE BUILD JAIL'S ONLY MECHANISM. There is no bubblewrap arm below this for a build-jail policy."*

**What it buys.** Grant-only filesystem confinement and coarse egress denial at **zero privilege**, on any kernel 5.13+, **including inside a container**.

**Why it is expressible at all.** Landlock rules UNION; there is no deny primitive at any ABI, so "deny inside allow" cannot be written. The build jail is a **pure allowlist that emits zero deny rules** (`preset::enforce_pure_allowlist`), so the objection that once disqualified Landlock does not bind here. It still binds `nub sandbox`, which is why that product keeps bubblewrap and its escalation.

**Measured.** ABI 4 / kernel 6.8 unless noted; Enforcement verified inside Docker `--cap-drop=ALL --security-opt=no-new-privileges`, in both classic `docker build` and BuildKit `RUN` — Docker's default seccomp permits syscalls **444–446**. Kernel floor is 5.13 for filesystem rules (`MIN_FS_ABI = 1`).

**Where the ABI stands, and it moves without changing anything here.** Upstream now documents ABI **10**, which added UDP support and `LANDLOCK_ADD_RULE_QUIET`; the intervening versions added `REFER` (2), `TRUNCATE` (3), TCP network rules (4), `IOCTL_DEV` (5), scope flags (6), logging control (7), `RESTRICT_SELF_TSYNC` (8) and `RESOLVE_UNIX` (9). **No version at any point introduced a deny or subtract primitive**, so every verdict in this document is unchanged by the movement — the number is what was stale, not the mechanism. Nub declares the filesystem rights through ABI 5 (`handled_access_fs` in `linux_landlock.rs`) and sizes the ruleset attribute by probed ABI (`RulesetAttr::size_for`, whose open arm already accepts a later kernel's struct), so a newer kernel is used, not merely tolerated.

**Cost is a non-issue.** 100,000 rules accepted at ~2.3 µs/rule; jail construction **0.168 ms** versus bwrap's **1.917 ms** (~11×).

**End-to-end, on four distros.** Real `nub install`, identical on Ubuntu 22.04.5 / 24.04.4 / 26.04 / Debian 13 (trixie):

| cell | jail OFF *(control)* | jail ON |
| --- | --- | --- |
| read `~/.npmrc` (outside the grant) | READ 51 B | **EACCES errno=-13 open** |
| DNS `nodejs.org` | RESOLVED | **EAI_AGAIN errno=-3001 getaddrinfo** |
| allowlisted `nodejs.org` by raw IP:443 | CONNECTED | **EPERM errno=-1 connect** |
| non-allowlisted `1.1.1.1:443`, proxy env deleted in-process | CONNECTED | **EPERM errno=-1 connect** |
| live loopback listener | CONNECTED | **EPERM errno=-1 connect** |
| UDP bind | BOUND | **EPERM errno=-1 bind** |

**It still confines on a userns-restricted host** — on Ubuntu 24.04 and 26.04 with `kernel.apparmor_restrict_unprivileged_userns=1` the FS denial is unchanged. Landlock is a separate LSM with no namespace dependency, and this is the measurement that says so rather than the expectation. **The build jail has no distro-dependent silent failure.**

**Fail-closed below the floor.** Below Landlock 5.13 the jail **refuses** rather than running a dependency's install script unconfined — *"the code the jail exists to contain is the last thing to run free because the kernel is old."* The only escape is the internal differential pin `NUB_SANDBOX_MECHANISM=bubblewrap` (`linux_landlock.rs:599-606`), which is not a user knob.

**And it does not fail closed ABOVE the floor — audited 2026-07-30, clean, do not re-audit.** The hazard worth naming: a backend that requests a right its kernel lacks and then demands *full* enforcement refuses to confine at all, so a newer right silently becomes a hard kernel floor. That is a live trap in `rust-landlock`, whose `BestEffort` downgrade reports `PartiallyEnforced` — measured directly, by forcing the ABI probe to report 2 on a 6.8 host, where the degraded ruleset still enforced every v2 restriction but `RulesetStatus` came back `PartiallyEnforced`. A `!= FullyEnforced` check there would refuse to run on **every** 5.19–6.1 LTS kernel (Ubuntu 22.04, Debian 12, RHEL 9). **Nub cannot reach that state by construction.** It does not use `rust-landlock`'s compatibility layer at all: it issues raw syscalls, probes once via `probe_abi()`, and derives both `handled_access_fs(abi)` and every grant's rights *from that number* — so it only ever declares rights the running kernel has, and there is no `RulesetStatus` or `FullyEnforced` comparison anywhere in the crate. Its only refusal is the deliberate `MIN_FS_ABI = 1` floor above. This is a checked-and-clean result, not an absence of evidence; the per-ABI derivation is what makes it true, so preserve that shape rather than introducing a "require the newest right" check.

## The wholesale system read floor — ADOPTED, deliberately coarse

**What it is.** `/usr`, `/bin`, `/sbin`, `/lib*` granted **wholesale** as read, with `/etc` enumerated leaf by leaf and `/opt` absent entirely. `backend/linux.rs`'s `ESSENTIAL_READ_PATHS`, shared verbatim by the Landlock backend.

**Why wholesale rather than per-path, and this is a rejected alternative not an oversight.** A native build is not 39 binaries, it is a compiler: `cc1plus`/`collect2` live under a version- and triple-keyed `/usr/lib/gcc/<triple>/<major>/`, and GCC OPENS far more than it execs — `specs`, `crt*.o`, the linker scripts, and the whole `/usr/include/**` header tree (**one `node-gyp rebuild` measured 595 opens under `/usr/include` alone**). A positive per-path grant would have to track every GCC major, arch triple and distro forever, and buys little: **there is no credential class under those roots.**

**Why `/etc` and `/opt` are different.** `/etc` is where the credentials actually are, so it is enumerated. `/opt` is third-party software — **~11 GB of it on a GitHub Actions runner** (`hostedtoolcache`, `az`, `pipx`, `microsoft`, `google`) — never a system floor. An interpreter that happens to live under `/opt` is bound by its own policy grant or as the entry program, never by this floor.

**The `/etc` set is measured, not guessed.** A 34-package real-postinstall corpus re-run under an **emptied `/etc`** with `strace -e trace=%file`, so the entries are the paths the kernel was actually asked for.

**And the measured set alone is not portable — a distro-shape correction.** The corpus ran on Debian only. On the RHEL family `/etc/ssl/certs` is a symlink to `/etc/pki/tls/certs` whose entries are **absolute** symlinks into `/etc/pki/ca-trust/…`, and `OPENSSLDIR` is `/etc/pki/tls` — so binding the Debian paths alone yields a directory of **dangling** symlinks and every OpenSSL verify fails with "unable to get local issuer certificate" (reproduced on `rockylinux:9` against a wholesale-`/etc` control). `/etc/ssl/cert.pem` is musl/Alpine's default `SSL_CERT_FILE`. **Node bundles its own CA and is unaffected, which is exactly why a Node-only corpus stayed green** — but `curl`, `git clone https://` and python `requests` inside the jail are not.

**Every TLS entry is named as a subpath, never `/etc/ssl` or `/etc/pki` wholesale** — `/etc/ssl/private` and `/etc/pki/tls/private` are mode-700 private-key directories and admitting either would undo the tightening.

## Coarse seccomp socket-family denial — ADOPTED as the network security boundary

**What it is.** The network axis is carried by a **seccomp socket-family filter**, not by Landlock's network rules. For a package the catalog does not admit, the denied family list is the full twelve — `AF_INET`, `AF_INET6`, `AF_UNIX`, `AF_PACKET`, `AF_VSOCK`, `AF_XDP`, `AF_BLUETOOTH`, `AF_RDS`, `AF_CAN`, `AF_TIPC`, `AF_IB`, `AF_NFC` — plus `io_uring_setup`/`_enter`/`_register` so a socket cannot be created off the family filter. Mismatch action is `Errno(EPERM)` (`linux.rs:2650-2705`, `:2770`).

**An admitted package lifts exactly two families out of that list and nothing else.** The ceiling is now driven by an `IpEgress` enum derived from the compiled net axis rather than the hard-coded `per_host: false` this section originally described; `AF_INET`/`AF_INET6` come out, and `AF_UNIX`, `AF_VSOCK`, the buses and io_uring stay denied. **That asymmetry is the point** — `AF_UNIX` is a path to host daemons like `docker.sock` that no Landlock rule scopes, so a coarse egress grant must not carry it.

**Measured** as the four-distro table above. Two rows carry more than they look:

- **EPERM against a LIVE loopback listener, not ECONNREFUSED** ⇒ `socket()` itself fails, so a local-forwarder bypass is closed too.
- **UDP is denied here even though Landlock cannot restrict UDP.** Do not read "UDP entirely unrestricted" as a hole in the build jail; that sentence is about Landlock's *network rules*, and the jail does not rely on them.

## Prefetch — ADOPTED as the compatibility lever the coarse model requires

**What it is.** Pre-placing an artifact so a lifecycle script never opens a socket. `apply_landlock`'s own comment states the dependency directly: *"a package that genuinely needs a remote artifact is served by prefetch before the jail starts"* (`linux.rs:2980-2988`).

**Why it is structural rather than a nicety.** Since the honest per-package choice on Linux is network-on or network-off, prefetch is what lets a package that would need "on" run with "off" — it is simultaneously a compatibility mechanism **and** the thing that keeps the boundary coarse (.4 corollary).

**Measured scope.** Of 230 source-read corpus packages, 85 (37%) need no network at all; of the 145 that do, five hosts cover 92% — `nodejs.org` 81 pkgs, `github.com` 42, `registry.npmjs.org` 6, `raw.githubusercontent.com`+`raw.github.com` 8, `storage.googleapis.com` 3. `nodejs.org` is contacted **only when node-gyp's header cache is cold**, so compiling from cached headers is a toolchain-only grant.

**One found-not-fixed defect undercuts the foundation.** Every `prebuild-install` / `node-pre-gyp` package is broken independent of the jail: from a pristine cache, `better-sqlite3@11.10.0`'s peer symlink points at a **hashed** store key while the store contains the **unhashed** one, and the generated `.bin` shim targets the unhashed key. **It reproduces with confinement entirely off**, so it is a PM/linker defect predating this work — but the dominant native-addon download mechanism being broken undercuts prefetch's per-family contract, and it explains why the end-to-end bar had to use a hand-written N-API addon.

---

# Bubblewrap and the namespace routes

## Bubblewrap plus userns plus netns — ADOPTED for `nub sandbox`, REJECTED (design) for the build jail

**What it is.** A private mount and network view constructed by `bwrap`, with read-only and writable binds keeping their original absolute paths. Module doc: `backend/linux.rs:1-5`.

**What it buys that Landlock cannot.** A real **deny** primitive via mount-masking, path **hiding** (a denied path is absent rather than refused), a PID namespace for whole-tree reap, mediation of metadata ops through the mount shape, and — via an empty netns plus the UDS bridge — genuine per-host egress.

**Why it is disqualified for the build jail — three independent reasons, any one sufficient.**

1. **It requires `--unshare-user`** (`linux.rs:668`), and unprivileged userns is denied by default on Ubuntu 23.10–25.04 with `apparmor_restrict_unprivileged_userns=1`.
2. **It is impossible inside Docker.** `cap_sys_admin` is absent, so **even root** cannot create a namespace: measured `rc=1`, *"No permissions to create new namespace"*. No capability change helps; it needs `--security-opt seccomp=unconfined --security-opt systempaths=unconfined` on the container, which the workflow author controls, not nub.
3. **The design forbids it, explicitly and not as a preference.** Not as a preference, not as a fallback, not "where available": a design or a selection function that proposes bwrap on the build-jail path is wrong.

**The Ubuntu question, resolved 2026-07-29 and no longer contested.** Measured on freshly created GCE VMs, one variable apart, with the sysctl read beside each result:

| host | kernel | `apparmor_restrict_unprivileged_userns` | `bwrap --unshare-user` | `bwrap --unshare-user --unshare-net` | `/etc/apparmor.d/bwrap-userns-restrict` |
| --- | --- | --- | --- | --- | --- |
| Ubuntu 22.04.5 | 6.8.0-1064-gcp | **0** | rc=0 | rc=0 | absent (restriction off anyway) |
| **Ubuntu 24.04.4** | 6.17.0-1021-gcp | **1** | **rc=1 `setting up uid map: Permission denied`** | **rc=1 `loopback: Failed RTM_NEWADDR`** | **ABSENT** |
| Ubuntu 26.04 | 7.0.0-1008-gcp | **1** | rc=0 | rc=0 | **present** (`aa-status`: `bwrap`, `unpriv_bwrap`) |
| Debian 13 (trixie) | 6.12.96+deb13 | *sysctl not present* | rc=0 | rc=0 | absent (no restriction) |

**The split is the exemption profile, not the sysctl.** 24.04 restricts userns and ships **no** bwrap exemption ⇒ denied; 26.04 restricts and ships one ⇒ allowed. Both the "requires elevation on 24.04" position and the "profile is inert on 24.04" finding are correct.

**⚠️ The broken control that produced the contested reading — do not repeat it.** `unshare --user true` and `unshare --user --net true` return **rc=0 on all four hosts**, including the 24.04 where bwrap fails. `unshare(1)` writes no uid map, and **writing the uid map is precisely what AppArmor denies**, so a green `unshare --user` says nothing about whether a userns is USABLE. The earlier report of "`bwrap --unshare-user` succeeding" on an unverified VM almost certainly measured this instead. **Always probe with a uid-map write (real `bwrap`), and always print the sysctl beside the result.**

**And the path-keyed hypothesis is now the correct model, reversing a note that called it refuted.** AppArmor profiles are keyed to a binary PATH: 24.04 has no `bwrap` profile at all, so even the SYSTEM `/usr/bin/bwrap` is denied, and Nub's digest-pinned `nub-resources/bwrap` is unprofiled on every release, so it is denied wherever the restriction is enforcing — 26.04's `bwrap-userns-restrict` covers `/usr/bin/bwrap`, **not** Nub's copy. The reason this hypothesis looked refuted was that the observation it had to explain (a *success* on 24.04) was itself the broken control above.

**Why this no longer blocks anything.** The build jail does not use bubblewrap. Confirmed end to end: on the same 24.04 box where `bwrap --unshare-user` fails, a real `nub install` build jail confines the filesystem identically to 22.04, and the jailed child's netns inode equals the host's. **Do not re-open it as a build-jail blocker.**

**What would change the verdict.** Universal unprivileged userns — i.e. every distro shipping an exemption profile **and** containers granting `cap_sys_admin`. Neither is coming.

## Shipping a Nub AppArmor profile in the distro — DEAD (privilege / feasibility)

**What it was.** Get an AppArmor profile for Nub's own digest-pinned `bwrap` copy into Ubuntu and Debian, so the path-keyed restriction exempts it the way it exempts `/usr/bin/bwrap` on 26.04.

**Why it is dead.** Getting a profile into Ubuntu and Debian is a multi-year lead time, and the build jail needs to work on the machines people have now. **Not a path.**

**What survives.** On a restricted-userns host the only route for `nub sandbox` is the local one-time `sudo nub setup-sandbox`, which installs a root-owned helper plus a path-keyed profile (D5). That is `nub sandbox`'s escalation and is never the build jail's.

## The one-time root setup helper — ADOPTED for `nub sandbox` only

**What it is.** `sudo nub setup-sandbox` installing a root-owned helper and a path-keyed AppArmor profile, once per machine. Convergent with prior art: podman, flatpak, Chrome and Codex all use the same per-binary AppArmor profile approach (four prongs).

**Why the build jail cannot use it.** Zero setup is the build jail's defining property and the prerequisite for default-on. A three-prong investigation — kernel source, an empirical 24.04 VM, and Landlock — established that a **capability-bearing** userns is deliberately root-gated with no unprivileged path. The premise was corrected along the way: the gate **strips capabilities**, it does not deny `unshare`.

**One CI consequence worth carrying.** Installing bwrap alone is **insufficient** on `ubuntu-latest`: measured 2 passed / 36 failed in 0.00 s before setup, 38 passed in 6.17 s after `sudo nub setup-sandbox --all-users`, with `ENFORCING=yes` established by a userns-inode differential.

## The per-host egress bridge — DEAD (privilege) for the build jail

**What it was.** A `--unshare-net` child in an **empty** netns, with a filesystem-path AF_UNIX socket as the one channel crossing the netns wall — a parent `UnixListener` splicing each connection to a fresh `TcpStream` to the parent-loopback `EgressProxy`, which authenticates every tunnel and enforces the host allowlist. Module doc: `backend/linux_net_bridge.rs:1-21`; sandbox-visible mount point `/dev/.nub-sandbox/net`.

**What it buys.** Genuine per-host egress on Linux — the empty netns IS the boundary, so nothing reaches off-box except through the bridge, and abstract AF_UNIX is netns-scoped while a PATH socket is mount-ns scoped, which is what makes the crossing possible at all.

**Why it is unavailable to the build jail.** It needs a netns, which needs the userns bubblewrap needs. So the capability is not merely unenforced here — it is **absent**. The policy no longer claims it either: per-host egress was withdrawn from the jail on every platform, so nothing in the compiled net axis promises a host gate this bridge would have had to deliver.

**Measured proof that no netns exists on the build-jail path** — the live `postinstall` pid's `/proc/<pid>/ns/net` inode was sampled and **equals the host's on every distro, in BOTH arms**: 22.04 `net:[4026531840]`, 24.04 `net:[4026531833]`, 26.04 `net:[4026531833]`, Debian 13 `net:[4026531840]`. **Direct evidence, not inference.**

**What would change the verdict.** The same universal-userns condition as bubblewrap. Not coming.

**One production bug from this route, fixed, worth not reintroducing.** `spawn_in_netns_bridge()` forked the in-netns bridge and released the target before it `bind`s its listener, so a resumed target's first connect could beat the bind — intermittent "allowed host unreachable" on a loaded host. Fixed with a pipe-based readiness handshake: the child writes one byte after `bind`, the parent polls ready-byte / EOF / 2 s deadline before release, and every outcome preserves fail-closed-via-empty-netns. **200/200 green under escalating contention versus 36/60 pre-fix.** recorded as FLAKE #2.

---

# Filesystem approaches that are dead

## Landlock deny rules — DEAD (mechanism), at every ABI

**What it was.** Express "grant this subtree, deny this file inside it" — the shape `nub sandbox` needs and the `.env*` secret floor was originally written against.

**Measured refutation** — probe `landlock_deny.c`, on ABI 7. `allowed_access = 0` → **`ENOMSG`**. `EXECUTE`-only and `WRITE_FILE`-only rules are **accepted with zero restricting effect**. Rules UNION; they never subtract. **The only deny is a non-grant.**

**Still true at the current ceiling.** ABI 8, 9 and 10 have since landed (`RESTRICT_SELF_TSYNC`, `RESOLVE_UNIX`, UDP plus `LANDLOCK_ADD_RULE_QUIET`), and **none of them adds a deny or precedence primitive** — the quiet flag suppresses audit records for a denial, it does not create one. Re-reading the ABI list is not a way to reopen this.

**Consequence, and it is the design's central fact.** The build jail is grant-only and emits **zero** deny rules; a secret is protected by **not being granted**. Denies inside a broader allow grant need escalation, which is the separate `nub sandbox` product's concern and not the build jail's. So the question is never "can this deny be expressed?" but "what is the smallest grant set that works?"

**What would change the verdict.** A Landlock ABI adding a deny or precedence primitive. Upstream has shown no sign of one, and the union semantics are deliberate.

## Granting `/proc/self` — REJECTED (design), and it is a banned pattern

**What it was.** Widen `PROC_READ_PATHS` to cover per-process `/proc` entries so a build tool can read its own `/proc/self/...`.

**Why it is refused, two stated reasons** (`linux_landlock.rs:85-99`): the ruleset is built **pre-`fork`**, so `/proc/self` would resolve to **Nub's** PID rather than the child's; and granting per-process entries would expose every same-uid process's `environ` and `cmdline` — the user's shell, editor and other tools, environment variables included. That is *"a secret-disclosure channel the bubblewrap path did not have, and it is a strictly worse trade than the build breakage avoided."*

**⇒ Any `/proc/self` dependency must be fixed in the code that reads it, never in the grant set.** Do not propose widening `PROC_READ_PATHS`.

### The refusal stands, but the FIRST reason is stronger than recorded and the SECOND is partly wrong

Both halves were measured directly in 2026-08. Full write-up and probe sources: [`../research/procfs-read-floor.md`](../research/procfs-read-floor.md).

**Reason one understates the problem, and the understatement matters.** The stated obstacle is that the ruleset is built pre-`fork`. The real obstacle is deeper and no amount of re-ordering fixes it: **a Landlock rule pins the INODE of the path it names**, and every `/proc/<pid>` is a distinct inode. Measured — a parent adds a rule on `/proc/self/stat`, enforces, and then the parent reads its own file fine (inode 57316) while a forked child is **denied** its own (inode 59398) and can read the *parent's* without trouble. Building the ruleset after `fork` would therefore cover only the direct child; the processes that actually break are grandchildren. And there is no glob to reach for: the ABI has exactly two rule types, `PATH_BENEATH` and `NET_PORT`.

**Reason two overstates the exposure.** Granting `/proc` does **not** expose `environ`. Three-way control against a non-descendant same-uid process, kernel 6.17 / ABI 7:

| | unconfined | current allowlist | whole `/proc` granted |
| --- | --- | --- | --- |
| `environ`, `maps`, `io` | OK | denied | **still denied** |
| `cmdline`, `status`, `cgroup`, `limits`, `mountinfo`, `comm` | OK | denied | OK |

Under one identical grant `cmdline` succeeds while `environ` fails, so the filesystem rule cannot be the discriminator; unconfined `environ` succeeds, so confinement is what flips it. The mechanism is Landlock's own `hook_ptrace_access_check` (`security/landlock/task.c`), which returns `-EPERM` for `PTRACE_MODE_READ` against any task outside the reader's domain hierarchy. The kernel documentation states it: *"thanks to the ptrace restrictions, access to such sensitive `/proc` files are automatically restricted according to domain hierarchies."*

### The four-direction `environ` gate — MEASURED, and it found two leaks nobody had listed

The table above tested only a **non-descendant** process, which is not the case the threat model cares about. A dedicated probe modelling nub's real shape — P0 unjailed holding a token, P1 forks and enforces the ruleset, P2 the jailed script — read `environ` in every direction, on kernel 6.17 / ABI 7 with the Ubuntu-default `yama/ptrace_scope=1`. Every cell is a real `open()` + `read()`:

| grant shape | unjailed nub parent | own jailed parent | sibling jailed script | unrelated non-descendant | own descendant |
| --- | --- | --- | --- | --- | --- |
| unconfined (positive control) | READ | READ | READ | READ | READ |
| status quo (8 global files) | denied | denied | denied | denied | denied |
| enumerated safe globals | denied | denied | denied | denied | denied |
| `/proc/<jailed-root-pid>` dir rule | denied | **READ** | denied | denied | denied |
| `/proc` subtree, any rights | denied | **READ** | **READ** | denied | **READ** |

**The headline threat stays closed under every shape, including a wholesale `/proc` grant: a jailed lifecycle script cannot read the unjailed Nub parent's `environ`.** Mechanism (INFERRED, consistent with the result): the confined child is strictly more restricted than its unjailed parent, so Landlock's ptrace hook refuses `PTRACE_MODE_READ`.

### ⛔ THE `environ` LEAK IS REAL, AND IT IS WHY BOTH USEFUL SHAPES ARE REJECTED

The two rows that would otherwise repair the failing packages both leak, and the non-descendant control could not see either. This is the disclosure the whole jail exists to prevent, so it is stated here as a named finding rather than a caveat:

- **A `/proc/<pid>` DIRECTORY rule on the jail root exposes that process's own `environ` to its descendants.** Same Landlock domain, so the ptrace hook permits it. Any lifecycle script deeper than the jail root reads the environment of the shell nub handed the script — which is where a lifecycle environment's tokens sit.
- **A `/proc` SUBTREE grant additionally exposes SIBLING jailed scripts' environs** — one dependency's install script reading another's — **and the unjailed parent's `cmdline`** (24 bytes read, MEASURED). A credential passed as an argv element is readable.

⇒ **Neither shape may be adopted, and no amount of "but it fixes six packages" changes that.** A grant that hands one dependency's install script another's environment is the exact Shai-Hulud-shaped disclosure the build jail is for. The four global files added to `PROC_READ_PATHS` sit in the third row — denied in all five directions — which is the only row that is safe, and it repairs nothing.

Any future `/proc` proposal is judged against the whole table above, never against the non-descendant row alone. That row was the entire basis of an earlier, wrong recommendation to grant `/proc` wholesale.

### Per-descendant `/proc/<pid>/stat` is inexpressible, and the safe globals do not repair anything

Five forms attempted, all MEASURED, all dead: a `/proc/self/stat` rule built pre-`fork` leaves the child denied its own (inode pinning); a `/proc/<parent-pid>` directory rule leaves the child denied its own *and* leaks the parent's `environ`; `open("/proc/*/stat", O_PATH)` is `ENOENT` because only `PATH_BENEATH`/`NET_PORT` rule types exist and there is no glob; a pid-range grant cannot name a not-yet-existing pid directory (`ENOENT`); and a second `restrict_self` after `fork` **adds the rule successfully (rc=0) yet the read stays denied**, because Landlock domains only ever intersect and a later ruleset cannot re-grant.

And the enumerated globals do not fix the failing packages. Real Node, the exact syscall `@nuxt/components@2.1.0` dies on, single variable between the first two rows:

| grant | `node -e 'process.memoryUsage()'` |
| --- | --- |
| status quo | `EACCES`, `syscall: 'uv_resident_set_memory'`, rc=1 |
| enumerated safe globals | `EACCES`, `syscall: 'uv_resident_set_memory'`, rc=1 |
| `/proc` subtree (positive control) | `MEMUSAGE_OK rss=44380160`, rc=0 |

⇒ **The refusal stands.** Widening to the safe globals is a strict improvement worth making on its own terms, but it repairs no package; the `process.memoryUsage()` family stays at `write:"disk"` unless a `/proc` subtree is granted, and that trade is worse than the earlier framing suggested.

**What the refusal costs, quantified.** Of the eight Linux packages in the corpus that land at `write:"disk"`, four have a measured fatal refusal and three of them are **one path**: `/proc/self/stat`, reached by libuv's `uv_resident_set_memory` behind `process.memoryUsage().rss`. In yarn v1 that runs inside `initPeakMemoryCounter` on *every* invocation before any package work, so any lifecycle script shelling out to yarn v1 dies identically. `@nuxt/components@2.1.0` printed the stack itself:

```
Error: EACCES: permission denied, uv_resident_set_memory
    at process.memoryUsage (node:internal/process/per_thread:221:5)
    at ConsoleReporter.checkPeakMemory (/usr/lib/node_modules/yarn/lib/cli.js:33423:40)
    at ConsoleReporter.initPeakMemoryCounter (/usr/lib/node_modules/yarn/lib/cli.js:33414:10)
```

These packages do **not** need whole-disk write. `write:"disk"` "repairs" them only because `relax_fs_to_full_disk` clears the ruleset and flips the default to `Allow`, which incidentally exposes `/proc` — it fixes a *read* refusal by abandoning filesystem confinement entirely. Recording them as needing disk write reads the symptom for the cause.

**Separately, and independent of any decision above:** several `/proc` files carry no per-process content at all and are denied on every jailed Node startup today — `/proc/version_signature`, `/proc/version`, `/proc/mounts`, `/proc/filesystems`, `/proc/sys/kernel/pid_max`. Granting those is a strict improvement with zero disclosure surface. The cgroup denials (`/proc/self/cgroup`, `/sys/fs/cgroup/**/memory.max`) are quiet rather than harmless: `process.constrainedMemory()` and `availableMemory()` return **0** instead of the real limit, so a script sizing a worker pool inside a memory-limited container divides by zero.

**What is granted instead.** Eight global `/proc` files a toolchain actually reads — `cpuinfo`, `meminfo`, `stat`, `uptime`, `loadavg`, `sys/vm/overcommit_memory`, `sys/kernel/osrelease`, `sys/kernel/ostype`.

**Two real defects this created, both fixed by moving the code rather than the grant.**

- **Nub's own runtime capture reads `/proc/self/maps`** to pin the monitor's runtime identity, and that read is denied under the jail with no grant possible. Failing the capture eagerly **aborted every Nub process nested inside a jail before its work began** — including the node shim every lifecycle script invokes. Fix: the capture stays eager (observing the pristine process image before any application code runs *is* the anti-substitution property), and only the **failure** is deferred to the one consumer that needs the authority — the bubblewrap launch. The Landlock path materializes no runtime image at all, so nothing is weakened silently. Commit `f2e7e6c5bd`; regression test pins a Nub binary completing its earliest bootstrap inside a real Landlock jail, paired with that same jail withholding a home secret while the package's own file reads back, so a ruleset that restricted nothing could not satisfy it.
- **The `/proc/self/maps` parse itself was wrong** — fixed to `split(maxsplit=5)`.

## Granting `/dev/tty` — REJECTED (design)

**What it was.** Add `/dev/tty` to `DEVICE_PATHS` so an interactive build tool can reach its controlling terminal.

**Why it is refused.** It is the process's controlling terminal, and handing it to a dependency's install script is the **TIOCSTI injection vector** that `setsid` exists to close — granting the node back would reopen it. Bubblewrap never exposed the host tty either; its `--dev` supplied a fresh devtmpfs. `linux_landlock.rs`'s `DEVICE_PATHS` comment.

## Adding `chmod` to the seccomp filter — REJECTED (design)

**What it was.** Close the metadata-write residual (below) by denying `chmod` in seccomp, since Landlock cannot mediate it.

**Why it is refused.** The seccomp filter **has no path filtering**, so it would break every legitimate `chmod +x` on build output — which native builds do constantly. **The cost of the mitigation vastly exceeds the benefit.**

**Now measured, twice, rather than argued.** A denial matrix over real installs breaks **4 of 5** packages with `npm error code EPERM / syscall chmod` — `sqlite3` (on `prebuild-install/bin.js`), `esbuild` (`bin/esbuild`), `simple-git-hooks` (`cli.js`), `bufferutil` (`node-gyp-build/optional.js`); only `better-sqlite3` survives. Ubuntu 24.04, kernel 6.8, `Errno(EPERM)`, non-root, against a 5/5-green no-denial control. A separate 344-package survey puts chmod-from-a-reachable-install-entry at **48/344 (14%)**, realistically ~20% counting `tar-fs` and `adm-zip`.

**The breakage is INVERTED relative to intuition, and this is the most useful thing anyone has learned here.** The chmods you would most want to stop — writing `+x` into `<project>/.git/hooks` — **fail gracefully** (3 of 6 hook installers catch and warn). The entirely benign ones — making your own downloaded binary executable inside your own package dir — **abort the install**. And **81% of chmod-using packages are fully covered by "inside your own package dir"**, the out-of-grant remainder being one archetype of ~11–13 git-hook installers.

**So a path-scoped chmod grant would be nearly free — and path-scoping is exactly what seccomp cannot express.** The crux was never compatibility; it is that the mechanism cannot say the thing we mean. `seccomp_unotify` does not rescue it either: `include/uapi/linux/seccomp.h` states outright that *"the seccomp notifier _cannot_ be used to implement a security policy!"*, because `USER_NOTIF_FLAG_CONTINUE` has an inherent TOCTOU on pointer arguments — and conditional-allow on a path is precisely what a path filter needs. **Do not reopen this expecting a different answer; it needs a kernel-side hook, not a cleverer filter.**

## Denying the `chown` and `setxattr` families in seccomp — ADOPTED

**What it is.** `chown`/`lchown`/`fchownat` and `setxattr`/`lsetxattr`/`removexattr`/`lremovexattr` → `Errno(EPERM)`. Carried by the `deny_metadata` dimension of `build_seccomp` (`backend/linux.rs`), enabled by `apply_landlock` — the Landlock backend, which is the build jail's only mechanism.

**Scoped to the build jail, deliberately.** `nub sandbox` runs commands the **user** chose, where a refused `chown` is a surprise with no threat model behind it; a dependency's install script has no comparable claim. So the bubblewrap/nesting path (`linux_monitor.rs`) passes `deny_metadata: false` and is byte-identical to before.

**The same denial also exists in `vendor/aube/crates/aube-scripts/src/linux_jail.rs`, which never engages under Nub** — `jail_enabled()` is `!embedder_owns_lifecycle_sandbox && …` and Nub's embedder profile sets that flag true, so Nub interposes `nub-sandbox` instead. That copy is correct for standalone aube and changes nothing about Nub; do not read it as the one that binds.

**Measured free, on a real kernel, at both uids.** Ubuntu 24.04 / 6.8, five packages (`better-sqlite3`, `sqlite3`, `esbuild`, `simple-git-hooks`, `bufferutil`): 5/5 pass as uid 1000 **and** 5/5 as root, against a 5/5-green control. The `chmod` arm above is the arm-effect control proving the filter genuinely reaches the install rather than silently not loading.

**The root case specifically, because it was the open question.** As root, node-tar's `preserveOwner` default flips on, so the chown traffic is real rather than hypothetical: a cold-cache root install of `sqlite3` issues **6 `fchownat` calls**. Under the filter all six return `-1 EPERM` and **the install still exits 0** with a loadable `node_sqlite3.node` — the extractors treat chown as best-effort and swallow the failure. The only behavioural delta is that files stay owned by the installing uid instead of being chowned to the uid recorded in the tarball, which is benign and arguably preferable.

**`utimensat` was proposed for the same list and dropped — it is not free.** It breaks `sqlite3` and `bufferutil` (2 of 5) in either form, path or fd. The proposal came from an strace sample showing zero calls; a wider run falsified it. **Absence of calls in a sample is not absence of use** — anything added to this list needs the denial matrix re-run, not just a trace.

**Be honest about what it buys: very little.** The two families that are safe to deny are safe *because* nobody uses them, and nobody uses them because they do nothing — `chown` to another uid already fails under DAC, and `user.*` xattrs are inert. The one with real teeth, `chmod`, is the one packages genuinely need. **The partial is cheap precisely because it denies operations nobody uses, which is the same reason it closes nothing.** The residual below survives it completely intact.

---

# Accepted residuals — measured, decided, not defects to re-chase

## The `stat` and `access` reconnaissance gap — OPEN, accepted

**What it is.** Landlock has no `stat` right, so ungranted paths are **refused-but-visible** (`EACCES`, not `ENOENT`). Landlock governs `open`, **not `access`/`stat`**, and `chmod`/`chown`/`utime`/`setxattr`/`ioctl`/`fcntl` are unmediated at every ABI — the filesystem rights cover file content and directory operations only, and that is as true at the current ABI 10 as it was at 7.

**Quantified, and more precise than "ungranted paths are visible"** — pen-test PT3. `access()`/`stat()` unmediated ⇒ **existence and mode of any path the script can NAME leaks** (`~/.ssh/id_rsa` → `mode=600`, `/etc/shadow` → `640`); but `readdir`/`opendir` **are** mediated (home, `/home`, `/proc` all `EACCES`). ⇒ **an attacker can CONFIRM A GUESS but cannot ENUMERATE or DISCOVER. Content never leaks.** macOS Seatbelt is stricter — even `stat` is `EPERM`.

**The same gap is what makes the realpath ancestor walk succeed on Linux.** Landlock ignoring `stat` is the free pass that Windows gets from bypass-traverse, and it is why the Windows `resolveMainPath` blocker has no Linux twin.

## The errno-shape compat cost — DEAD (compat) for ~0.5% of packages, accepted

**What it is.** THE defining Landlock-versus-bubblewrap behavioural difference. Same denial, different errno, opposite outcome — confirmed at syscall level, both arms, same VM:

| | bubblewrap | Landlock |
| --- | --- | --- |
| how it denies | path **absent** from the mount view | path **visible**, access refused |
| `~/.npmrc` open | `ENOENT` (errno -2) | `EACCES` (errno -13) |
| project write | `EROFS` | `EACCES` |

**Neither mechanism ever leaked content** — `~/.npmrc` is denied under both. An earlier claim that bubblewrap "permits" it was retracted; there is no bubblewrap secret leak.

**Why it breaks packages.** The common ecosystem idiom tolerates only `ENOENT`. `libnpmconfig`'s `maybeReadIni()` is the canonical shape: `catch (err) { if (err.code === 'ENOENT') return ''; else throw err }`. Under bwrap the optional file is "absent" and the package carries on; under Landlock it is "forbidden" and the package hard-crashes. Confirmed for `@pact-foundation/pact-node` and `@aws-amplify/cli`.

**Landlock cannot present `ENOENT`** — there is no stat right to withhold, so this is inherent, not a grant bug. **Measured cost: ~0.5% of a 367-package corpus (2 packages).** Treat as an accepted compat cost to document, not a defect to chase.

**A second, sharper instance of the same difference — and this one WAS a real break.** Under bubblewrap Nub's own preload was never loaded: the runtime dir sits outside the child's mount view, so preload discovery found nothing and ran unaugmented. Under Landlock the same dir is **visible but ungranted**, so discovery **succeeded**, Nub injected `--import …/preload.mjs`, and **Node died on the unreadable file.** Fix (`6253f4b3c6`): stamp `NODE_COMPAT=1` on the constructed lifecycle env so the shim skips preload injection outright, making both mechanisms agree on the behaviour bubblewrap already had. Correct on its own terms too — *"a published postinstall never asked for Nub's augmentation, and keeping Nub's runtime — including the dlopen'd native addon — out of untrusted code removes surface instead of widening the allowlist to grant it in."* Landlock P0, branch `shim-maps-fix`: 0/20 → 20/20, corpus 121 → 2 → **0 genuine**.

**A `test`-before-`read` script takes the read path and crashes.** `test -r $PROJECT/.env` reports **readable** and `[ -e ]` reports **exists**, while `open()` then returns `EACCES`, where bwrap's `ENOENT` made the same probe skip honestly. Not fixable by any grant.

## The metadata-write residual — OPEN, accepted, and demonstrated reaching a home secret

**What it is.** A jailed script `chmod`'d an ungranted `~/.ssh/id_rsa` from **600 to 777** on Linux, verified on disk. `unlink` of an ungranted file was `EACCES` (directory ops mediated, pure-metadata ops not). macOS blocks the same chmod.

**Why it is accepted.** Landlock cannot mediate metadata ops at any ABI, and the only other lever — [adding `chmod` to seccomp](#adding-chmod-to-the-seccomp-filter--rejected-design) — has no path filtering.

**This is a known upstream gap with no owner, not an oversight on our side.** Landlock has no LSM hook on the metadata path; upstream tracks it as [`landlock-lsm/linux#11`](https://github.com/landlock-lsm/linux/issues/11), open since 2024 and blocked on an LSM hook signature change nobody has picked up. **No ABI — 1 through the current 10 — has ever added a metadata right, and none is queued.** Treat "a later kernel will fix this" as false until that issue moves.

**What the partial seccomp denial does and does not change.** [Denying `chown` and `setxattr`](#denying-the-chown-and-setxattr-families-in-seccomp--adopted) removes two of the four primitives for build-jail launches. **The residual is untouched by it**: `chmod 0777` on anything the jailed uid owns, host-wide, plus arbitrary mtime rewriting. Those are the two with teeth and both remain.

**Honest sharp edge, do not flatten it.** 600→777 makes the key world-readable to **other** processes. It is still not a read BY the jailed script.

**One adjacent symlink-following `chmod` was real and is closed.** `aube-linker/src/sys.rs:210-212` now requires `symlink_metadata(target).is_ok_and(|md| md.is_file())`. Requiring a **regular file** (not merely "not a symlink") also keeps the chmod off a dir/fifo/device and is behaviour-preserving, since `symlink_metadata` examines only the final component — so a workspace package linked into `node_modules` as a symlinked *directory* still gets its bins chmod'd, where a naive "reject any symlink" would have broken workspaces. **Neither npm nor pnpm contains at this point** — both contain only lexically at the manifest layer (`npm-normalize-package-bin`, pnpm's `isSubdir`), and pnpm's own test names this exact threat model (`bins/resolver/test/index.ts:172`). Both-directions test: with fix 9 passed; reverted to `target.exists()` → FAILED `left: 493 (0o755), right: 384 (0o600)`. Branch `sandbox/binlink-symlink-chmod` @ `7bce5307b1`.

## No PID namespace, so whole-tree reap is best-effort — OPEN

**What it is.** Landlock has no PID namespace. A `setsid` daemon survives (mitigable unprivileged via `PR_SET_CHILD_SUBREAPER`); `kill -TERM` on any same-uid process needs **kernel ≥ 6.12** to close, and **24.04 LTS is 6.8**.

**What ships instead.** The `pre_exec` hook calls `setsid`, so the child is a session leader and its PGID equals its PID — enough to signal and sweep the group. Gated on a flag because every other path shares Nub's own process group, where a negative target would signal Nub itself. Commit `e7f483a544`.

**Weaker than the namespace it replaces, and stated as such:** a script that calls `setsid` itself leaves the group and survives. `build_jail.rs`'s comment claimed a whole-tree reap that was **never true on this path**; corrected rather than left describing bubblewrap.

## Landlock audit records are unusable for diagnostics — DEAD (privilege)

**What it was.** Use Landlock's audit records to tell a user exactly which grant a failing build needed.

**The record content is ideal** — symbolic blocker, **full path**, dev/ino, and a `domain=` id per `restrict_self` call. **But reading it needs `CAP_AUDIT_READ`**, and **`LANDLOCK_RESTRICT_SELF_LOG_NEW_EXEC_ON` defaults OFF**, which is exactly Nub's post-`execve` case.

**What would change the verdict.** An unprivileged per-domain audit read. Until then, diagnostics come from run-log mining and the `access`/`stat` gap.

## Ten silent Landlock degradations — OPEN

Ten cases where a jailed run exits 0 with a denial logged. Unclosed, and directly relevant to the corpus measurement problem: **a green install proves nothing on its own** (the SILENT class — 31 of 230 source-read packages, 13.5%, fail silently).

---

# Porting invariants — each of these broke something when missed

## Read must render as READ plus EXECUTE

**What it is.** `FsAccess::Read` must expand to `ACCESS_READ_FILE | ACCESS_EXECUTE`. A read-only bind grants exec for free under bwrap; **Landlock does not.**; stated and tested at `linux_landlock.rs`'s access-bit constants *"HERE, where a future reader looking at a broken native build will land."*

## Grants must name resolved leaves, not parent directories

**What it is.** Landlock evaluates the **resolved** path against the ruleset, while bubblewrap resolves a symlink at bind time. `/etc/resolv.conf` is a symlink to `/run/systemd/resolve/stub-resolv.conf` on every systemd host, so a grant on the `/etc` **directory** leaves the real file outside every rule and **silently kills DNS for the whole network-allowed tier**. Opening each leaf with `O_PATH` (which follows symlinks) keys the rule on the target inode instead — which is why `ESSENTIAL_READ_PATHS` must name files, not their parent.

## Directory-only rights must be masked off file grants

**What it is.** Landlock rejects a rule with `EINVAL` when a directory-only right is attached to a file `parent_fd`, and the jail grants individual files (`<project>/package.json`, `/etc/resolv.conf`, the interpreter binary) — so every access set is masked through `FILE_ONLY_RIGHTS` before `add_rule`.

## The descriptor sweep must be re-implemented, and it must run first

**What it is.** `close_range(3, u32::MAX, CLOSE_RANGE_CLOEXEC)`. **An inherited fd was measured egressing through both Landlock and seccomp**, so this is a demonstrated leak, not a hardening nicety. Ordering matters: the sweep runs **first**, so its own `/proc` fallback still works (`890476426c`).

## Four protections bubblewrap supplied through the namespace that the Landlock port silently lost

All four closed in `890476426c`, listed because each is the kind of thing a port drops invisibly:

- **`setsid`, not `setpgid`.** `--new-session` was the only defence against TIOCSTI injection into the launcher's shell, **which the repo measured landing without it**, and seccomp cannot catch it.
- **Drop every capability.** `--cap-drop ALL` under a user namespace is gone, and Landlock mediates no metadata syscall at any ABI, so a root-in-container script keeping `CAP_DAC_OVERRIDE` could rewrite host ownership and modes.
- **Enumerate the ReadWrite rights** instead of aliasing the handled set, which had conferred `MAKE_CHAR`/`MAKE_BLOCK` — **device-node creation**.
- **Grant the private tmp dir and export `TMPDIR`, grant the entry program, grant the global `/proc` files.** No mount namespace means no `/tmp` rebind, so the child is pointed at the per-run scratch dir by its real host path (`linux.rs:3008-3012`).

## The same IR renders differently here, and two recent grants are where that shows

Both were found on macOS and both compile on Linux; neither means the same thing.

**The project-root cwd grant is a macOS necessity that Landlock renders as a no-op.** Seatbelt gates `getcwd(2)` on `file-read-data` of the cwd's **own** directory node, so a confined process at an ungranted cwd cannot learn where it is at all — five distinct signatures, one cause, and it broke any lifecycle script whose real work runs at `INIT_CWD`. The fix grants the project root **node alone, never the subtree**, which is `(literal …)` on Seatbelt and `MountAccess::ListOnly` here (`65c51530cb`). **Landlock handles neither `chdir` nor `getcwd`**, so the macOS defect cannot occur on Linux and the grant buys a Linux child only the ability to list the root's top-level entries. That is *reasoned from the handled-access set, not measured* — the deciding differential would be a confined process at an ungranted cwd on a real Landlock host, calling `getcwd` with and without the rule.

**Node-versus-subtree is expressed by RIGHT here and by PATH on Seatbelt.** A node-only grant on Landlock is the rule carrying `READ_DIR` alone; on Seatbelt it is a `(literal …)` term rather than a `(subpath …)`. A port that copies the *path* spelling across without the rights mask widens a node grant into a subtree read.

**Landlock is unaffected by the fs-polarity fix, and that was measured rather than assumed.** `emit_fs` on macOS had been mapping `(Allow, Read)` to a `(deny file-write* …)` term, so a read grant silently revoked write from everything it enclosed; the fix makes only `Effect::Deny` subtract on the fs axis. **Landlock unions its rules and has no deny primitive at any ABI, so it never had the defect** — confirmed on 6.8.0-136-generic ABI 4 at `0d5dc51381`, where a package dir nested under an enclosing node-only project rule keeps the rights its own rule grants. Bubblewrap is the one Linux mechanism that *does* order-shadow, since it layers binds in authored order and a later enclosing `--ro-bind` shadows an earlier nested `--bind` — but **the build jail never reaches it**, because `linux::preflight` is Landlock-or-refuse for a build-jail policy. The invariant to carry forward: `enforce_pure_allowlist` binds the **backends**, not only the IR. Stripping every deny from the IR is worth nothing if a backend synthesizes one back.

---

# The network policy the jail actually applies

## The `$downloads` host allowlist on Linux — DEAD (mechanism), and the jail now consults it on no platform

**What it was.** The IR emitted `net: ["$downloads"]` on non-Windows, and `fold_net` (`fold.rs:621`) turned the list into `NetPolicy { enforce: true, rules: <one Allow per host> }`. The intent was a curated per-host allowlist.

**What actually happened here, traced end to end plus measured**, `sandbox/integration` @ `58973b881a`:

- `backend/mod.rs:906` `proxy_needed()` was TRUE, so `mod.rs:1188` **did** start an `EgressProxy` and `mod.rs:1224` **did** try a `linux_net_bridge`. **Both were then discarded.**
- `apply_landlock` (`linux.rs:2989`) took **no proxy port** and hard-coded `per_host: false`, so the full twelve families were denied.
- **No proxy env is injected.** `insert_proxy_env` has exactly one Linux caller, inside `target_environment()`, reached only from the **bubblewrap** path. `apply_landlock` builds env from `policy.env.constructed` alone.

**The measurement that settled it.** The **allowlisted** and **non-allowlisted** cells were **bit-identical** (`EPERM errno=-1 connect` both), and no proxy env vars reached the child in either arm, so no proxy-mediated fetch was even attemptable. **The host list bought a package nothing.**

**⇒ On Linux the real primitive is coarse on/off, OS-enforced, needing no privilege.** Per-host egress filtering is now enforced **nowhere** in the build jail: `build_jail_net` is a per-package boolean that references `$downloads` on no platform (`compiler/preset.rs:602-671`), and the macOS proxy route — the one mechanism that genuinely could enforce a host list unprivileged — was withdrawn from the jail deliberately, because a list that gates one platform and is provenance on two is a compatibility liability rather than a defense. **Do not document per-host enforcement as cross-platform, and do not restore a host list here as a missing feature.** `$downloads` still serves `nub sandbox`.

**The per-package boolean IS now delivered on Linux — this is the half that changed.** It used to be read by one consumer, `net_gate_node_options` (`compiler/defaults.rs:929`), stamped into `NODE_OPTIONS` only by the Windows branch of `build_jail.rs`, so a package the catalog GRANTED network still got none at the OS layer here. `933915dd43` closed that: `apply_landlock`'s hard-coded `per_host: false` became an `IpEgress` enum derived from the compiled net axis, so **an Allow rule lifts `AF_INET`/`AF_INET6` out of the socket-family ceiling and a deny-all base keeps the full ceiling.** The rest of the ceiling — `AF_UNIX`, `AF_VSOCK`, the buses, io_uring — is unchanged in both directions. The same change stopped the Landlock arm starting an egress proxy it cannot route a child through, which had been leaving an unusable loopback listener on every granted package's lifecycle spawn.

**Linux is the one platform whose admitted spelling is not `true`.** It is `["*"]`, a catch-all Allow naming no host. `enforce = false` would re-permit `AF_UNIX` — a path to host daemons like `docker.sock` that neither Landlock nor a netns scopes — plus `AF_VSOCK`, `AF_PACKET` and io_uring's socket-creation side door, because `build_seccomp` gates the whole ceiling on that one flag. Keeping `enforce = true` with a catch-all Allow is the coarse grant **with the non-egress protections intact**, and zero *authored* hosts is what keeps `mode` off the proxy-starting path.

**Reasoned, not measured, on this platform.** The uniform per-package contract is verified on Windows only (run 30612421934, de-elevated on a real AppContainer). The Linux half is read from the code. **Linux is the least-evidenced platform on the network axis**, and the differential that would settle it is an admitted package against an unadmitted one on a real Landlock host, checking that `AF_INET` separates while `AF_UNIX` stays denied in both arms.

**A cross-platform claim to correct.** Describing Linux and Windows together as best-effort egress via `HTTP(S)_PROXY` is wrong about both. **No proxy env reaches the Linux child at all** (measured). Windows' net gate does stamp those variables, but at `http://127.0.0.1:1` — a closed port, so the effect is a refusal for a proxy-honouring non-Node child, not a filtered route. Nor does macOS proxy a jailed script any longer.

**One consequence already recorded elsewhere.** A corpus arm measured **only `prisma` recovered** by `$downloads` out of 8 network-tier breaks, with `wasm-pack` unrescuable by any host allowlist because it ignores proxy env entirely — part of why the host list was retired rather than repaired.

## The per-host seccomp carve-out — REJECTED (design) without a netns

**What it was.** `PER_HOST_PERMITTED = [AF_INET, AF_INET6]` — re-permit just the IP families so the child can reach the in-netns bridge, keeping every global-bus family denied (`AF_VSOCK` reaches the hypervisor CID-addressed and not netns-scoped; `AF_BLUETOOTH`/`AF_RDS`/`AF_CAN`/`AF_TIPC`/`AF_IB`/`AF_NFC` are global buses). `linux.rs:2650-2672`.

**Why it must never be used on the Landlock path**, stated in the code: *"it exists only because a netns bounds the IP families it re-permits, and there is no netns here."* Re-permitting `AF_INET` without a namespace would let a script dial any host directly and ignore the proxy, **making the allowlist decorative** — a strict weakening of coarse-deny.

**The carve-out's own justification is worth preserving.** Without it, per-host would have been a strict weakening of coarse-deny even *with* a netns, because the netns alone does not confine every family.

---

# Open Linux defects

## Elevated `nub install` breaks when Nub lives under `$HOME` — OPEN, bubblewrap-only

**The defect.** `bwrap: Can't find source path /proc/self/fd/204: Permission denied` at euid 0, **even with AppArmor disabled**. fd 204 is the runtime image pinned from `/proc/self/exe` via `--ro-bind-fd`. bwrap maps only uid 0, and a capability in a non-initial userns does **not** override DAC for a file owned by an unmapped uid; `/home/<user>` is 0750 (Ubuntu `HOME_MODE` since 21.04). Confirmed by `chmod 755 $HOME` flipping it. Affects the curl installer's `~/.nub/bin/nub` and every dev build.

**`--ro-bind-data` does not transplant** — it copies bytes, which is wrong for an executable.

**A misattribution on top.** `is_namespace_denial` matches bare `"permission denied"`, so users are told to run setup, **which cannot fix it**, and the nag appears even with the restriction off.

## The `--ro-bind-fd` TOCTOU class — OPEN, bubblewrap-only

**What it is.** `--ro-bind-fd` is **not** a descriptor bind. It stores the literal string `/proc/self/fd/N` (`bubblewrap.c:1736`) and `realpath()`s it later (`:1402`), so it binds **whatever that name points at then**. Latent instances at `linux_monitor.rs:1110` and `:6317` on `/proc/self/exe` and `/proc/self/maps`: **if the Nub binary is unlinked while running — which an in-place `nub upgrade` would do — every confined launch dies.**

**Related bwrap facts worth keeping.** An overmount cannot revoke an already-open `struct file`, and `fstat(2)` performs no permission check, so inherited stdio is unaffected by masks. Masks are construction-time snapshots: a file created afterwards in a masked dir is not covered. And `DENY_WALK_SKIP_DIRS = ["node_modules", ".git"]` — bwrap **deliberately does not** walk those, for cost.

**One bwrap capability that was wrongly thought impossible.** Deny-then-reallow nested **is** expressible; the guard claiming otherwise was over-broad. Working argv: `--bind $P $P` · `--perms 111 --tmpfs $P/private` · `--bind $P/private/reopened …` · `--remount-ro $P/private`. It works because bwrap creates its own mountpoints (`ensure_dir`/`ensure_file` + `mkdir_with_parents`, `bubblewrap.c:1000-1006`) and applies ops **in argv order**. Verified identical on 0.8.0 / 0.9.0 / 0.10.0 / 0.11.2; no `--overlay` needed.

## The npm `npmrc` floor entry is inert on Linux — OPEN

**The defect.** A live escape was confirmed: a jailed script read `/opt/homebrew/lib/node_modules/npm/npmrc`. Fixed on macOS; Linux misses it twice — `deny_search_roots` covers only the package dir, and `DENY_WALK_SKIP_DIRS` skips `node_modules`.

**Hazard on the obvious fix.** An exact-path deny risks tripping Windows' `deny_shadows_grant` and **fail-closing every Windows install**. Verify before committing. (And note the build jail is grant-only, so a deny is the wrong shape regardless — this wants a narrowed grant.)

## The jail read-grants the machine-global PM store — OPEN, lower severity

A jailed script can read every package **any** project installed, including private-registry ones (`preset.rs:271`). Compounds the known node_modules-secrets and allowlisted-CDN residuals. Noted, not reproduced.

## Cross-project store poisoning is closed in practice but not in the registry path — OPEN

**Measured not reproduced**, and the reason is precise: `break_cas_hardlinks` (`lifecycle.rs:706`) runs unconditionally before every jailed build, so project B got a fresh benign copy at a different inode — on **APFS and ext**. Cross-project approval bypass is also closed (project B gets nothing, `WARN_NUB_IGNORED_BUILD_SCRIPTS`).

**The caveat.** The repro used `file:`-tarball deps (project-local `.store`). The **GVS registry path** (`.aube` symlinked into the global store) is guard-covered in code but **not empirically closed** — no malicious registry package could be fabricated offline.

## Node-gyp writes into a sibling store cell — OPEN, and it is a linker bug, not a jail defect

**What happens.** `require('node-addon-api').gyp` returns a path that climbs **out of the project**, because Nub symlinks registry deps to the global CAS. gyp then re-anchors it under `build/`:

```text
jailed: PermissionError [Errno 1]: './build/../../../../../../.cache/nub/pm/store/…/nothing.target.mk'
control: gyp info ok (unjailed, same dir, same node-gyp)
```

**There is no acceptable jail-side fix** — the target is an arbitrary ancestor-relative phantom tree outside the project, so granting write there would be a filesystem-wide hole. **The correct fix is in the linker**: materialize registry packages into `node_modules/.store/<name>@<ver>/node_modules/<name>` by hardlink/clonefile instead of symlinking to the global CAS. That also fixes a **non-sandbox** bug (`require('node-addon-api').include_dir` resolving outside the project). **The layout is platform-independent, so this is not a macOS item** even though it was first filed as one..

**A related and genuinely upstream defect, resolved as a doc caveat.** The escaping-`base_path` sibling write in `gyp/pylib/gyp/generator/make.py:2434` — `output_file = os.path.join(options.depth, options.generator_output, base_path, base_name)` with `depth="."` and `generator_output="build"` — prepends `build/` in a way that cancels only the FIRST `..`, so the write lands one level short. **Reproduces byte-for-byte under real pnpm's `.pnpm` virtual store**, so it is not nub-specific and pnpm has not special-cased it. **~29% of a 35-package sample use the vulnerable idiom** (`bcrypt`, `sqlite3`, `ffi-napi`, `ref-napi`, `node-pty`, `tree-sitter`) — it is node-addon-api's **own documented boilerplate**; `include_dirs`/`.include_dir` is the safe form. Flag-injection is **not viable** (`--depth`/`--generator-output` are hardcoded in node-gyp's `configure.js`, and a package running `node-gyp rebuild` directly resolves via bare PATH, bypassing `npm_config_node_gyp`). Root-cause fix is genuinely infeasible in Nub, so the documented workaround is correct under the prefer-root-cause rule's stated exception. Doc shipped on `main` @ `dc27843e26`.

## Stale comments that produced wrong conclusions — CLOSED

Three separate rounds drew wrong conclusions from four comments that described the code as it had stopped being. All four now read correctly, verified against the tree:

- The `BootstrapSpec::per_host` comment (`linux_monitor.rs:~692`) states that the socket-blocking seccomp **stays installed** under per-host and narrows to `AF_INET`/`AF_INET6`, rather than being dropped.
- The same comment states that **`AF_UNIX` stays denied** under per-host, matching `PER_HOST_PERMITTED` (`linux.rs:2671`), which names exactly two families.
- The module doc (`linux_monitor.rs:8-9`) states that the production confined-Linux path **constructs and adopts the launcher unconditionally**, replacing "remains deliberately uninstalled".
- The env-band comment (`fold.rs:~96`) says **two** env-deny bands, agreeing with the band-by-band emission record at `:278-292`.

**Kept as a section rather than deleted**, because the defect class is the point: each of these was a comment that outlived the code it described, and each cost a round. The check that catches the next one is reading the symbol, not the sentence above it.

## CI gates that do not gate — mostly CLOSED, one residual

The gates were the structural cause of breaks shipping undetected in both directions, and three of the four holes are now shut in `ci.yml`:

| hole | state |
| --- | --- |
| Linux enforcement tests all hit `skip_without_bwrap()` and silently returned | **closed** — a `bwrap-resource` job builds the digest-pinned helper, the `test` job downloads it, pins `NUB_BWRAP_VERSION`/`NUB_BWRAP_SHA256` before every `cargo` invocation that compiles `nub-sandbox`, and runs `nub setup-sandbox --all-users` to repair the runner's restricted userns |
| no require-gate, so a hollow "N passed in 0.01 s" read as success | **closed** — `NUB_SANDBOX_REQUIRE_BWRAP: "1"` is set job-wide, turning a skip into a hard failure; the workflow records the repair as taking the Linux enforcement suite from 2 of 38 running to 38 of 38 in ~6 s |
| clippy was Ubuntu-only, so an OS-specific `#[cfg]` arm was invisible | **closed** — the clippy job is a matrix over `ubuntu-latest`, `windows-latest` and `macos-14`, with `nub-native` linted separately from inside the crate |
| the `unused variable: d` at `tests/ir.rs` that the Ubuntu-only gate missed | **closed** — the binding carries `#[cfg_attr(target_os = "windows", allow(unused_variables))]`, with the comment stating it is genuinely unused on that target rather than accidentally dropped |

**The residual, and it is the designated done-gate.** `sandbox-conformance.yml` fires on `workflow_dispatch`, on a `push` to `sandbox-primitives`, and on a path-filtered `pull_request` against `main`. **None of those is a push to this branch**, and prerelease work here opens no PRs — so the cross-platform conformance matrix, the one gate that pairs every axis with a negative control, still does not run on the branch the feature is built on. Dispatch it by hand or widen the push filter; do not read a green `ci.yml` as covering it.

**One gate-shaped trap worth keeping.** **`docker … | tail` returns TAIL's exit status**, so a container gate written that way silently reports success on a failing build — redirect to a file, capture `$?`, `exit $RC`. That one caught a real Linux break on its first correct use.

---

## Pen-test result — no breach on Landlock

Recorded because a negative needs its positive controls named. Against `sandbox/integration` `9036dc62eb`:

- **Shai-Hulud-shaped replay: the sink received 0 bytes.** Every stop-point held with a paired jail-OFF control **proving each attack is otherwise live** — the control exfiltrated the key. Sharp IMDS distinction preserved: jail-ON `EPERM` (socket blocked) versus jail-OFF `ECONNREFUSED` (socket allowed, no server).
- **Escape matrix held.** The ruleset survives spawn/exec; `REFER` and hardlink-then-rename out of a granted subtree are both `EACCES`; the fd sweep is clean (only node-internal pipes on fds 3–39, and `/proc/self/fd` readdir itself `EACCES`).
- **Both of the code audit's "REAL" findings failed to reproduce as breaches under a running install.**

## The Linux corpus arm — RUN, aggregation not landed

**Placeholder, deliberately carrying no numbers.** A Linux corpus sweep completed on 2026-07-31; its aggregation has not landed, so **this document states no Linux corpus result.** Do not fill this section from the macOS arm's figures — the whole reason a Linux arm exists is that the two backends render the same IR differently, and the macOS numbers are themselves a re-score of truncated runs pending a re-run. Anything quoted here before the aggregation lands is invented.

**What the arm is expected to settle**, stated in advance so the result is not read backwards into it: whether the `EACCES`-versus-`ENOENT` errno-shape cost changes the break set relative to macOS, and whether the wholesale `/usr` read floor plus the enumerated `/etc` set covers the same packages Seatbelt's tighter set does. Both are Linux-specific by construction.

## Contradictions in the record, unresolved

1. **Linux per-host is described as best-effort via `HTTP(S)_PROXY`, and no proxy env reaches the child at all** (measured). The honest Linux statement is coarse deny, with the host list retired on every platform and the per-package boolean now delivered here.
2. **Corpus figures disagree by a wide margin and by construction.** The Landlock P0 row reports "corpus 121→2→**0 genuine**"; a later corpus run reports "**~26% of measurable P2 breaks** under the production policy" and "default-on is NOT supportable on this evidence". Different arms and different dates, and the reconciliation is: **every published corpus figure predates the curated carve-outs entirely** (Linux numbers measured 07-28 16:46–18:38; `curated.rs` grants landed 22:24, catalog wired 23:52). Neither figure is quotable without its arm.
3. **A harness defect makes the above hard to settle retroactively.** `PROVENANCE.txt` records only `version: v0.6.0` — no git sha, no preset, no catalog state — which is why a run's configuration had to be inferred from file mtimes after the fact. Every result artifact must stamp the sha, the arm, and whether curated grants were compiled in. **Half of that is now available rather than needing to be built**: the catalog override prints a banner naming the file it loaded and its entry counts, or naming the rejection and the fallback, so a run's catalog state is a line the harness can capture instead of a mtime it has to reconstruct. The sha and the preset still are not stamped.

## Changelog

- 2026-07-31 — Scrubbed a residual host-allowlist claim: the egress-bridge section said the absent per-host capability was one "the policy still claims", which stopped being true when per-host was withdrawn on every platform. The compiled net axis promises no host gate, so the bridge's absence costs nothing that is still promised.
- 2026-07-31 — **REVERSAL:** the per-package egress boolean is no longer inert on Linux. `apply_landlock`'s hard-coded `per_host: false` became an `IpEgress` derived from the compiled net axis, so an admitted package lifts `AF_INET`/`AF_INET6` out of the socket-family ceiling while `AF_UNIX`, `AF_VSOCK`, the buses and io_uring stay denied; the Landlock arm also stopped starting a proxy it cannot route through. The `$downloads` host list is now consulted on **no** platform, macOS included, so per-host enforcement is enforced nowhere in the jail. Recorded how the project-root cwd grant and the fs-polarity fix render here — both are macOS findings that are no-ops on Landlock, the first reasoned from the handled-access set rather than measured. Added an explicitly empty placeholder for the Linux corpus arm.
- 2026-07-30 — Recorded the Landlock fail-shut audit as checked-and-clean: Nub derives rights from a runtime `probe_abi()` and never compares against `FullyEnforced`, so a newer right cannot silently become a hard kernel floor. Measured against the `rust-landlock` trap it avoids. Also recorded the metadata-mitigation measurements: `chown` and `setxattr` denial measured free at both uids (5/5 packages, non-root and root, kernel 6.8) and adopted as the `deny_metadata` dimension of `nub-sandbox`'s `build_seccomp`, enabled for the Landlock backend only (`nub sandbox` is unchanged). The same denial is mirrored in aube's own jail, which — verified in code — never engages under Nub. `utimensat` proposed and dropped: it breaks `sqlite3` and `bufferutil`. `chmod` exclusion promoted from argued to measured (4/5 break with EPERM), plus the inverted-breakage finding — the harmful chmods fail gracefully while the benign ones abort, 81% are in-package-dir, so a *path-scoped* grant would be nearly free and path-scoping is exactly what seccomp cannot express. Named the upstream blocker (`landlock-lsm/linux#11`) so the residual reads as a tracked gap rather than an oversight.
- 2026-07-30 — Reconciled against the tree. Corrected the ABI ceiling: upstream now documents ABI 10 (UDP plus `LANDLOCK_ADD_RULE_QUIET`), and since no version has ever added a deny or subtract primitive, no verdict here moves — the number was stale, the mechanism is not. Closed the "stale comments" section (all four now read correctly) and most of the CI-gate section (`ci.yml` builds and stages the pinned bwrap, repairs the runner's userns, sets the require-gate, and lints on three OSes), leaving the conformance workflow's branch coverage as the one live residual. Recorded that the per-package `packageNetwork` table reaches only the Windows-stamped net gate, so neither catalog network table varies a Linux run, and added the dev-only catalog override.
- 2026-07-30 — Moved into tracked `research/design/` so code comments can link here, and scrubbed of pointers into untracked documents. Every measurement, table and verdict is unchanged.
- 2026-07-29 — Initial consolidation.
