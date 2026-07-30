# The "virgin world" build jail — run the install in a pristine OS and copy the result back

**Question (maintainer, 2026-07-29):** instead of confining lifecycle scripts with a fine-grained
allowlist over the real machine, run the install inside a **container / container-like virgin
world** — a pristine filesystem with no credentials in it — give it *"full disk access and full
network access because who cares"*, let it run totally (including writes into the project), and
copy the result back. Framed as a hail mary.

**Two constraint relaxations the maintainer made mid-investigation, both load-bearing:**

1. **The build jail may be OPT-IN and may REQUIRE ELEVATION.** *"This approach is so clean that I
   would be okay making the build jail opt-in and requiring elevation."* This invalidates most of
   the existing corpus, which judged nearly everything at the zero-privilege gate.
2. **A slow one-time spin-up is acceptable**, and the world is **one per INSTALL**, not one per
   lifecycle script. This retires the per-script cost model that killed the microVM family.

**The one constraint that cannot be relaxed:** the world must produce **host-ABI-correct native
artifacts**. `node-gyp` builds `.node` files that must `dlopen` into the user's own Node.

Evidence tags: **MEASURED** (ran it) · **READ-FROM-SOURCE** (read the source/doc) · **INFERRED**.

---

## Verdict in one line

**The idea works on Linux, is SKU-gated on Windows, and is structurally impossible on macOS in its
literal form.** The best cross-platform approximation is a **dedicated local user account** — a
*partial* virgin world that fixes the exact defect an allowlist cannot, at the cost of not being a
separate filesystem.

| platform | virgin **filesystem** possible? | mechanism | privilege | verdict |
|---|---|---|---|---|
| **Linux** | **yes** — real mount namespace | `systemd-nspawn`, youki's `libcontainer` crate, or Docker | one-time root | **alive** |
| **Windows** | partial | process-isolated Windows container (same-OS, emits PE) **or** dedicated account | admin | **narrow coverage** |
| **macOS** | **no — at any privilege level** | dedicated uid + Seatbelt (partial only) | admin | **the hard one** |

---

## 1. The axiom: wrong guest OS is a category error, not a cost problem

**MEASURED** (this host, macOS 26.5.2 / arm64, Node v26.5.0). Same `hello.c`, same `binding.gyp`,
same arm64; the ONLY variable is where the compile ran.

```
# built in docker run --rm node:22-bookworm  (linux/aarch64 guest)
build/Release/hello.node: ELF 64-bit LSB shared object, ARM aarch64
$ node -e "require('./build/Release/hello.node')"
Error: dlopen(...): tried: '...' (slice is not valid mach-o file)

# positive control — identical fixture built natively on the host
build/Release/hello.node: Mach-O 64-bit bundle arm64
$ node -e "console.log(require('./build/Release/hello.node').hello());"
hello from native
```

Every container option on macOS boots a **Linux** guest. No amount of privilege, speed, or snapshot
technology changes the output format. This single fact kills the entire macOS container column and
most of the Windows one.

## 2. Linux: alive, with a known playbook

### 2a. The glibc rule — build old, run new (the manylinux playbook)

**MEASURED** (prong A, pure-N-API C++ probe, arm64, Node 22 throughout):

| built on | → bullseye 2.31 | → bookworm 2.36 | → trixie 2.41 | → alpine musl |
|---|---|---|---|---|
| bookworm 2.36 | **FAIL** | OK | OK | ⚠ "OK" |
| trixie 2.41 | **FAIL** | OK | OK | ⚠ "OK" |
| bullseye 2.31 | OK | OK | OK | ⚠ "OK" |
| alpine musl | **FAIL** | **FAIL** | **FAIL** | OK |

**newer→older FAILS, older→newer WORKS; musl↔glibc is dead both ways.** Real packages are worse
than the toy: `better-sqlite3@11` built on trixie requires `GLIBC_2.38` and fails on bookworm —
**one Debian release** of skew. But `bufferutil@4` on trixie needs only `GLIBC_2.17` and loads
everywhere, so breakage is *package-dependent and intermittent across a dependency tree*, the worst
possible shape for a default.

This is exactly the problem [manylinux](https://github.com/pypa/manylinux) ([PEP 513](https://peps.python.org/pep-0513/),
[PEP 599](https://peps.python.org/pep-0599/)) solved for Python a decade ago: **pin the world's
glibc at or below the oldest supported host.** nub's version is *easier* than manylinux's — the
artifact only has to load on **one known host**, not on every Linux in existence.

Two hazards to design around:
- **glibc-built addon on musl SIGSEGVs (exit 139)** rather than erroring — musl's loader has zero
  `.gnu.version_r` processing (READ-FROM-SOURCE, `ldso/dynlink.c`), so the version check never
  happens. A hard `LOAD_FAIL` is recoverable; a SIGSEGV mid-install is not.
- **`prebuild-install` / `detect-libc` key off the CONTAINER's libc.** MEASURED with `sharp`: a musl
  world selects `@img/sharp-linuxmusl-arm64` and silently hands a glibc host an unloadable tree.

**Good news — Node-version skew is a NON-issue.** A pure N-API addon built on Node 22 loaded on Node
20, 22 and 24 (`modules` 115/127/137). N-API is ABI-stable by contract. The world must match the
host's **libc**, not its Node. One fewer dimension than feared.

### 2b. EXDEV — the kernel constraint that dictates the topology

**MEASURED by the root agent**, one variable (mount topology), same docker volume both arms:

```
two --mount of the same volume:  179 254:1 /store · 180 254:1 /proj  → ln: Cross-device link
one -v of the same volume:       175 254:1 /v                        → HARDLINK_OK links=2
```

Same superblock, same device. Linux `do_linkat()` (`fs/namei.c`) checks `old_path.mnt !=
new_path.mnt` — **per-MOUNT, not per-superblock** (READ-FROM-SOURCE).

nub's `node_modules` is a hardlink (Linux) / reflink (macOS) projection of a machine-wide CAS store
(`aube-linker/src/lib.rs:298`). So to link CAS→`node_modules` inside a world, **store and project
must live under ONE mount.** Naively that mount's nearest common ancestor is `$HOME`, which would
mean mounting the developer's entire home rw — strictly worse than today's jail.

**The topology that dissolves it:** bind-mount **only the project directory**; put a
**project-local CAS store inside it**. Links work (one mount), and **copy-back becomes a no-op**
because the world wrote straight into the real location — which also retires the Prisma-codegen
write-back problem and the merge-conflict problem entirely.

Cost: forfeits machine-wide dedup. Already measured in nub's existing project-local mode —
**571 pkgs / 21,002 files / +1.44 s / 2.97×**. Known and bounded.

### 2c. Do not exec per script

**MEASURED**, `docker exec` into a running container, 5 reps: 404 / 375 / 294 / 293 / 423 ms.
At ~300 lifecycle scripts that is **90–120 s per install** — worse than what it replaces. The world
must run the **entire lifecycle phase** inside itself.

### 2e. MEASURED — the Linux spike (2026-07-30, GCE `nub-linux`, Ubuntu 24.04.4, glibc 2.39)

450-line Rust prototype, plain `rustc`, zero crates. Branch `buildjail-virginworld`
(`158035670b`), `tests/buildjail-virginworld/`. **Every property holds.**

| claim | result |
|---|---|
| reroots | `ls -1 /` inside → 20 entries, all the world's; host's `snap`, `lost+found` invisible |
| **`realpath` / ancestor walks** | **succeed with NOTHING granted** — `realpathSync` through a symlink, `lstat` on all 10 ancestors to `/`, `resolveMainPath` via symlink |
| `$HOME` absent | `entries=0` vs host's 64 incl. `.ssh/id_rsa`, `.aws`. `/home/nub` → **ABSENT**, not `EPERM` |
| native addon | `better-sqlite3` compiled in a **glibc-2.31** world → runs on the **glibc-2.39** host |
| EXDEV | one mount → `HARDLINK OK links=2`; same inode via a second mount → `Invalid cross-device link` |
| cost | **2.0 ms** world setup; install 107 s in-world vs 154 s host control |

**Privilege — a 4-rung ladder, one host, one variable each:**

| rung | state | result |
|---|---|---|
| 1 | `restrict=1`, no profile, non-root | **rc=72** `uid_map write failed: Operation not permitted` |
| 2 | + path-bound AppArmor profile granting `userns` | **rc=0** |
| 3 | profile removed, `restrict=0` | **rc=0** |
| 4 | `restrict=1`, no profile, as root | **rc=0** |

⇒ **ZERO root, ever**, where unprivileged userns is unrestricted (most non-Ubuntu distros,
Ubuntu ≤ 23.10). Where Ubuntu 24.04 restricts it: **one** `apparmor_parser` call installing a profile
scoped to the single binary, leaving the global restriction in force for everything else. nub already
ships this exact pattern at `/etc/apparmor.d/nub-bwrap-userns`. The `unshare -U` exit-0 trap was
confirmed live and the prototype judges on the `uid_map` write instead.

**TWO DESIGN-DETERMINING FINDINGS the prototype existed to surface:**

1. **Map to a NON-ROOT uid.** With uid 0 inside a single-uid map, node-tar believes it is root,
   `fchown`s every entry it extracts, and `EINVAL`s on every unmapped id — so `node-gyp` cannot
   unpack the Node headers and **no native package builds at all**. Control, only `--uid` varied:
   `--uid 0` → `TAR_ENTRY_ERROR EINVAL … fchown` ×~70, fatal. `--uid 1000` → clean build. (The
   `newuidmap` alternative would cost a setuid-root helper; a build world needs only one uid.)
2. **Build the world from inside the new PID namespace.** `CLONE_NEWPID` affects only children, and
   procfs refuses to mount unless the mounter's PID ns is owned by its user ns — so building in the
   parent gets `EPERM` on `/proc`. `fork()` first, then build. (Ordering read from
   `bubblewrap/bubblewrap.c`: relative `put_old`, oldroot `MS_REC|MS_PRIVATE` before
   `MNT_DETACH`.)

**Overlayfs is load-bearing:** 2 ms with it, 1275–1301 ms without (`cp -a` 581 MB into tmpfs) —
**~650×**. Overlay is what makes per-run freshness free. One-time world build: 35 s / 581 MB, and
`apt-get install build-essential python3` ran **inside the rerooted world with no root at all**.

**ABI margin measured, not assumed:** `better_sqlite3.node` requires at most `GLIBC_2.29` /
`GLIBCXX_3.4.20`; built on 2.31, runs on 2.39. The manylinux build-old-run-new rule, concretely.

**⚠️ EGRESS IS UNSOLVED — the one open question blocking a real implementation.** `CLONE_NEWNET`
gives a genuinely empty network (`DNS_FAIL` inside vs `DNS_OK` with host netns, one variable), but an
install needs the registry. The real choice is host netns (no egress control) vs a userspace path
(`slirp4netns` / a proxy). The prototype ships the flag and does not answer the question.

### 2d. Mechanism choice

`systemd-nspawn` deserves a look **before** Docker: it ships on every systemd box, needs only root,
requires no daemon and no image registry, and boots a virgin world from a plain rootfs directory
(Fedora's `mock` uses exactly this). It had **zero mentions** in the prior corpus. For a Rust-native
path, youki's **`libcontainer`** crate (0.7.0, 2026-07-25) is the credible "drive a Linux container
without a Docker daemon" option. Docker works but imposes a hard runtime dependency, and
`docker`-group membership is root-equivalent by Docker's own documentation.

**gVisor stays dead even under the relaxed bar** — its cost is *per-syscall* (3.6× a native C
compile, 1.9× a real `node-gyp rebuild`), so unlike boot latency it never amortizes.

## 3. macOS: no virgin filesystem exists, at any privilege level

Three structural facts, all **MEASURED on this host**:

**(a) No filesystem namespaces, no bind mount.**
```
$ grep -cE '^#define[[:space:]]+SYS_' $SDK/usr/include/sys/syscall.h   → 459
$ grep -icE 'SYS_unshare|SYS_setns' …                                  → 0
$ ls /sbin/mount_nullfs                                → No such file or directory
```
`mount(8)`'s `-o union` is a *global* namespace change, not per-process. Elevation does not create
these primitives; they are absent from XNU.

**(b) APFS copy-on-write is VOLUME-scoped, not container-scoped** — so a separate volume cannot be
cheaply seeded. Single-variable, with a passing control:
```
clonefile /bin/echo (System vol) → /tmp (Data vol) : rc=-1  Cross-device link
clonefile /tmp/src               → /tmp            : rc=0   ok          ← control
```
Both volumes are in the same APFS container `disk3`. ⇒ **"a separate filesystem" and "cheap" are
mutually exclusive on macOS.** A separate volume means a real 12–20 GB byte copy per world; staying
on the Data volume for near-free cloning means it is a *directory*, not a filesystem.

**(c) A redirected `$HOME` is not isolation** — the maintainer's objection, proven:
```
$ HOME=/tmp/fake-home node -e '…'
env.HOME           = /tmp/fake-home
os.homedir()       = /tmp/fake-home
os.userInfo().home = /Users/dev      ← the REAL home, via getpwuid
real ~ readable    = 315 entries
```
One API call defeats it, and the real tree is fully present underneath. **The only thing on macOS
that changes `getpwuid`'s answer is changing the euid** — not an allowlist, not an env scrub, and
not `chroot` (directory-service lookups go to `opendirectoryd` over Mach IPC, which is not
filesystem-scoped).

**`chroot` is dead on four independent grounds:** no bind mount and cross-volume `clonefile` EXDEV
make population a 12–20 GB copy; the dyld shared cache is 6.1 GB on a third volume; it is
empirically broken on sealed-SSV Apple Silicon ([darwin-jail#2](https://github.com/darwin-containers/darwin-jail/issues/2));
and it does not touch the Mach bootstrap port, so a chrooted child still reaches `opendirectoryd`,
`cfprefsd`, `securityd` — making it *strictly weaker* than Seatbelt, which has `(deny mach-lookup)`.

**A VZ macOS guest is the only true virgin macOS filesystem** and is capped by the framework at
**2 concurrent instances**, needs a ~15 GB IPSW plus Xcode inside, and boots in 20–30 s.
*Correction to the prior study:* the macOS SLA is **not** the barrier — it permits two virtualized
copies for *(a) software development*, and the "non-commercial" qualifier attaches only to clause
(d), personal use. The cap and the cost are the barriers.

### 3a. macOS RE-OPENED — `chroot` is gated by AMFI, not by SIP, and the gate may be strippable

Superseding §3's "no fresh disk on macOS." A later prong found the actual gate, and it is narrower
than assumed.

- **`chroot(2)` itself is unrestricted.** READ-FROM-SOURCE, `xnu` `bsd/vfs/vfs_syscalls.c:4600`
  — it needs only `suser()` plus the generic MAC hook. No entitlement, no `csr_check`. (Contrast
  `pivot_root` at :4663, hard-gated to `pid == 1` + `com.apple.private.vfs.pivot-root` —
  unobtainable.)
- **The gate is AMFI's `execve` hook, and it keys on the HARDENED RUNTIME.** VERIFIED by raw-byte
  grep of the live kernel on this host (`BootKernelExtensions.kc`, macOS 26.5.2):
  `hardened runtime not allowed in chroot` and the escape entitlement
  `com.apple.security.cs.allow-in-chroot` (undocumented; zero public hits; found on zero shipped
  binaries).
- **Every Apple platform binary runs with `CS_RUNTIME` implied** even though on-disk `codesign` says
  `flags=0x0` — measured via `csops(CS_OPS_STATUS)`. So `/bin/sh`, `clang` and the official Node
  binary are all in AMFI's chroot kill set.
- **A SECOND, newer wall, VERIFIED BY ME on this host with a one-variable differential:** a plain
  copy of an Apple system binary is SIGKILLed *outside any chroot* by launch constraints —
  `cp /bin/sleep /tmp/x && /tmp/x 1` → **exit 137**; the same bytes after `codesign -f -s -` →
  **exit 0**. This breaks darwin-jail's whole rsync-the-system method on macOS 13+, and is likely
  why nobody reports it working on modern macOS.
- ⚠️ **CORRECTED 2026-07-30 — this hypothesis is BACKWARDS for Apple binaries.** The spike measured
  it: re-signing a *platform* binary demotes it to third-party and AMFI kills it outright
  (`AMFI: '...' is adhoc signed. AMFI: code signature validation failed`). **Plain copies of Apple
  binaries work inside a real jail; re-signed ones die.** Re-signing IS the fix for
  **non-Apple** binaries — the shipped hardened `node` is `Killed: 9` on entry and the ad-hoc
  re-signed one runs (`node re-signed in jail: v24.18.0`, one variable). So the rule is
  per-provenance, not global: **leave Apple's tree alone, re-sign what you bring in.**
- ⚠️ **macOS 14/15 are closed BOTH ways, so this is macOS 26+ only.** Plain copies die to launch
  constraints (Ventura+) and re-signed copies die because system executables are arm64e-only —
  control: on 14/15 the *x86_64 slice of the same file* runs under Rosetta (`rc=0`) while the
  thinned arm64e slice is killed (`rc=137`); on 26 both run. Verify on a SIP-on 14/15 box before
  relying on this.
- ⚠️ **The `chroot /` "deciding experiment" below may be inconclusive.** XNU sets `FD_CHROOT`
  unconditionally (VERIFIED: `fdt_flag_set(fdp, FD_CHROOT)`, no condition, in `chroot()`), but AMFI
  is a closed kext and may instead compare the process root vnode against the global `rootvnode` —
  in which case `chroot /` leaves them equal and never arms the gate. The spike inferred it is a
  no-op, but its evidence came from a SIP-**off** runner where nothing would arm regardless, so
  neither reading is established. **A real reroot is the safe test.**
- Original (now-superseded) hypothesis, retained for provenance: *"`codesign -f -s -` plausibly
  fixes BOTH — it strips the Apple identity (→ no launch constraint, MEASURED) and strips
  `CS_RUNTIME` (→ out of AMFI's chroot kill set, INFERRED)."*
  Cost: 1,791 executables / 357 MB across `/bin /sbin /usr/bin /usr/sbin /usr/libexec` — about a
  minute, one-time. **Not 12–20 GB.** The earlier §3 cost estimate was wrong because it assumed the
  tree had to be *copied*; it can be *mounted*.
- **The sealed system volume IS the virgin disk.** `diskutil apfs listSnapshots /` shows a mountable
  snapshot of `disk3s1s1` (12.6 GB, `Sealed: Yes`), and per-path `df -P` confirms `/usr`, `/bin`,
  `/System` live there while `/Users`, `/private`, `/opt`, `/usr/local`, `/Applications` are
  firmlinked in from Data. **Mount the system snapshot alone and you get a pristine Apple-only tree
  with no user data at all** — zero download, zero copy.
- **Second-mounting an APFS volume is proven** — the OS does it live: `mount` shows `/dev/disk3s5`
  twice, once rw at `/System/Volumes/Data` and once read-only as a Time Machine local snapshot at
  another path (`mount_apfs -s`).
- **`chroot` cannot fix `getpwuid`**, measured cleanly: identity resolution is a Mach round-trip to
  `opendirectoryd`. Denying `/etc/passwd` via Seatbelt still leaks the real user; denying
  `mach-lookup` on `com.apple.system.opendirectoryd.libinfo` gives
  `uv_os_get_passwd returned ENOENT`. ⇒ **Seatbelt is not what chroot replaces; it is what chroot
  needs.** Same for `cfprefsd`, `securityd`/`trustd`.
- **SIP-off is NOT an acceptable price** and is a different category from "elevation" — it is a
  global, permanent, recovery-mode change disabling launch constraints and library validation
  machine-wide. darwin-containers ships that way; nub must not.

**THE DECIDING EXPERIMENT (30 s, needs `sudo`, non-destructive — `chroot /` is a no-op reroot):**
```sh
sudo chroot / /bin/echo hello                                  # expect: Killed: 9
cp /bin/echo /tmp/echo_adhoc && codesign -f -s - /tmp/echo_adhoc
sudo chroot / /tmp/echo_adhoc hello                            # if this prints hello, macOS is open
```
Estimated P(second command succeeds) ≈ 0.6; P(full toolchain works end-to-end, SIP on) ≈ 0.25–0.30.

## 4. The dedicated local account — the cross-platform fallback, and it is partial

This is the one mechanism that appears on all three platforms, produces host-ABI artifacts
everywhere, and enforces isolation in the **kernel** rather than in a policy that must be correct.

**Critically: the prior rejection of a dedicated uid was mostly gated on the two
constraints the maintainer just relaxed.**

| prior objection | gated on | status |
|---|---|---|
| root required at spawn, every time | **privilege** | dissolved — a blessed helper/daemon is now permitted |
| scratch create / chown / copy-back / cleanup need root | **privilege** | dissolved |
| developer cannot kill a runaway build | **privilege** | dissolved — the helper reaps |
| `chmod`/`chown` on outputs unavailable | **privilege** | dissolved |
| §3aa: the bit protecting `~/.ssh` is the bit the build needs | **deployment shape** | **dissolved by the staging shape** — if nothing under `$HOME` is opened, `$HOME` stays 0750 and the contradiction never arises. Measured on Linux: `grep -c "/data/dev" strace.log → 0`, 7/7 native packages compiled |
| `.env*` floor needs a deny primitive | deployment shape | dissolved — *"the secret floor stops being a confinement problem and becomes a copy-manifest problem"* |
| four silent write-loss classes | **neither** | **SURVIVES** |
| no network confinement from DAC | **neither** | **SURVIVES** — compose with Seatbelt |

**What a dedicated uid buys that Seatbelt structurally cannot:**
1. `os.userInfo().homedir` becomes the build account's own empty home — fixes §3(c) at the kernel
   layer instead of by policy.
2. **Broker holes close by construction.** The mechanism record notes that under the
   real compiled Seatbelt jail, `defaults read -globalDomain` returned 7917 bytes byte-identical to
   unconfined, because `cfprefsd` is a separate unsandboxed process resolving `HOME` via `getpwuid`
   — *"no amount of path granting or withholding affects it."* A different euid kills the whole
   class rather than patching it grant-by-grant.
3. Keychain and TCC are per-uid — **absent**, not denied.
4. The developer's home is a DAC wall (0750 `dev:staff`), not a rule.

**The honest residual — it is a PARTIAL virgin world:**
- `/Users/Shared` is `drwxrwxrwt` and sibling homes are 0755; a dedicated uid still reads those.
- **Stale-verdict landmine:** `sysadminctl -addUser` puts the account in primary group **`staff`
  (GID 20)**, and every macOS home is `owner:staff` — so the obvious implementation gets group read
  on every home on the machine, and `dscl . -read /Groups/staff GroupMembership` shows only `root`,
  so an audit reads "safe" while `PrimaryGroupID: 20` has access. Must create a dedicated GID **and
  assert it at run time.**
- **nub's CAS store must move out of `$HOME`** — it resolves to `~/.cache/nub/pm/store/`
  (`pm_engine/identity.rs:171`), inside a 0750 home, unreachable to a non-staff build user. This is
  a **prerequisite, not a detail.**
- Account durability on macOS is genuinely bad: Nix's UID base moved 30000 → 301-332 → 350 and still
  collides ([nix#16155](https://github.com/NixOS/nix/issues/16155)); **macOS 15 Sequoia's installer
  deleted `_nixbld1-4` outright** ([nix#10892](https://github.com/NixOS/nix/issues/10892)).
- The `$HOME`-cache write class (cypress/puppeteer/playwright/electron writing `~/.cache/<tool>`)
  is fixed if the build account's home is **persistent** across installs — at the price of a
  same-uid surface between installs.

**Nix's precedent binds:** release notes for 1.11.10 — *"On other platforms, the 'build user'
mechanism is now disabled."* Nix ships build users only where it can layer a kernel restriction on
top. **The answer is not "uid instead of Seatbelt", it is "uid AND Seatbelt."**

## 5. Windows

**Process-isolated Windows containers are the only same-OS, right-ABI container in the entire
study** — the wrong-guest-OS killer does not apply, and they emit PE via MSVC. But coverage is
narrow (READ-FROM-SOURCE, MS version-compatibility matrix):
- **Windows 10 hosts: process isolation is ❌ for every base image.** Hyper-V isolation only.
- **Windows Home is excluded from the container feature entirely** — a SKU gate elevation cannot fix.
- Build-number matching binds on Server hosts (`0xc0370101` when a host patches ahead of an image).
- *Correction:* the prior study's ~92 GB toolchain image figure is wrong by an order of magnitude —
  `servercore:ltsc2022` ≈ 5.16 GB + VS 2022 C++ workload ≈ 8.1 GB ⇒ **~10–14 GB**, a payable
  one-time pull. Items above are what kill it, not the image.

**Windows Sandbox — the prior headline killer is refuted, a better one replaces it.** Read-write
`MappedFolders` *do* persist after disposal and a `LogonCommand` can return output and a sentinel
exit code, so "no way to retrieve output" is wrong as stated (the real gap is no synchronous launch
API). The actual killer: *"Host-installed software isn't available in the sandbox"* + *"as clean as
a brand-new installation"* ⇒ an ~8 GB MSVC install **per launch**, uncacheable by design.

### 5a. THE WINDOWS ANSWER — application silo + per-silo `bindflt` (found 2026-07-29)

**Windows has a mount-namespace analogue, it ships enabled on Windows 11 Home and Windows 10 1903+,
and it is NOT gated on the Containers optional feature.** A job object promoted with
`JobObjectCreateSilo` can carry `bindflt` mappings that are private to that job's process tree.

VERIFIED in Microsoft's own source (`hcsshim`):
- `internal/jobobject/jobobject.go:480` — *"The binding is only applied and visible for processes
  running in the job, any processes on the host or in another job will not be able to see the
  binding."*
- `PromoteToSilo()` is **only** `SetInformationJobObject(handle, 35, 0, 0)` — no HCS, no Hyper-V, no
  server-silo conversion, no image. `JobObjectCreateSilo = 35` (`internal/winapi/jobobject.go:60`).
- `TestSiloFileBinding` is Microsoft's own **differential test with a control**: create job with
  `Silo: true` → `ApplyFileBinding` → assert `os.Stat(siloPath)` **fails on the host**
  (`t.Fatalf("expected to not be able to see %q on the host")`) → `job.Assign(pid)` → the process
  inside sees it.
- The presence gate is `C:\windows\system32\bindfltapi.dll`, ⇒ user-mode API from **Win10 1903**.
  `bindflt` default start is **Automatic on Home, Pro, Education, Enterprise** (21H2–24H2).

**Why this dissolves all three measured Windows blockers:** the jailed process runs under a
**NORMAL token** — no AppContainer, no restricted token, no ACEs. So `C:\` is a real directory the
token can open (`realpathSync` lives), the libuv piped-`spawnSync` hang is AppContainer-specific and
absent, and MSVC/node-gyp run as the ordinary user against the host toolchain. **Isolation stops
being a grant set and becomes a namespace.** (INFERRED from the token model — this is the
load-bearing inference and the first thing a probe should confirm.)

**What it is NOT:** it cannot reroot `C:\`. A real reroot is a *server*-silo property
(`SILO_OBJECT_ROOT_DIRECTORY_SHADOW_DOS_DEVICES`, class 0x28) and that is the full container with
all its SKU gates. So the shape is **shadow the secrets, keep the toolchain**: bind an empty scratch
dir over `C:\Users` (or `%USERPROFILE%`) inside the silo; System32, Program Files, MSVC and the host
Node stay real and correct. A virgin *world* without a virgin *disk* — and it sidesteps the
~10–14 GB image entirely.

### 5b. MEASURED — the Windows spike (2026-07-30, GCE `nub-win`, Server 2022 build 10.0.20348)

All five open questions answered. Branch `win-silo-bindflt`, probe at `tests/win-silo-bindflt/`.

**Privilege — BOTH published write-ups are wrong, and the fix is a one-variable differential.**

| arm | token | silo promote | `BfSetupFilter` |
|---|---|---|---|
| admin (SSH) | elevated, **no SeTcbPrivilege** | OK | `hr=0x00000000` ×5 |
| SYSTEM | has SeTcbPrivilege | OK | `hr=0x00000000` ×5 |
| stduser | not admin | OK | **`0x80070005`** |
| **stdpriv** | same acct, **SeTcbPrivilege held AND enabled** | OK | **`0x80070005`** |
| **stdadmin** | same acct **+ Administrators** | OK | `hr=0x00000000` ×5 |

`stdpriv` → `stdadmin` differs in exactly one thing. ⇒ **Silo creation needs NO privilege at all**
(Quarkslab's `SeTcbPrivilege` claim does not apply to it), and **the gate is Administrators
MEMBERSHIP on `bindflt`** — `SeTcbPrivilege` is neither necessary (admin arm succeeded without it)
nor sufficient (stdpriv held it, enabled, refused). Deep Instinct/Bitdefender's "admin" is right;
both write-ups misattribute the gate to the silo.

**One-time elevation CONFIRMED.** An elevated helper created the silo + bindings and
`DuplicateHandle`d the job to a standard user's process; the unprivileged client then launched node
into it — `launched=true assigned=true in_job=true exit=0`, with the client's own control proving it
could not have done the bind itself (`own BfSetupFilter hr=0xd0000022`). **Honest caveat:** what is
one-time is the *elevation*, not the helper's involvement — bindings are per-job, so a resident
helper must perform the bind per install.

**THE HEADLINE — the realpath blocker dissolves.** Unflagged `node <deep file>`, five components
down, no `--preserve-symlinks*`:
```
child-reached-user-code = YES
realpathSync(C:\) = C:\        lstatSync(C:\).isDirectory = true    readdirSync(C:\).length = 37
```
Against §5h's byte-identical `Error: EPERM … lstat 'C:\'` under AppContainer. **Isolation as a
namespace does not fight path resolution.** Piped `spawnSync` also returns (`elapsed_ms=80`,
`status=0`) instead of hanging, and grandchildren inherit the view, so npm → node-gyp → MSBuild
keeps the jail.

**It is a real boundary.** Both escape candidates are redirected, not holes — in-silo
`\\?\...\secrets` and `\\?\GLOBALROOT\Device\HarddiskVolumeN\...\secrets` both list only
`MARKER-INSIDE.txt`, while the host reads `TOP_SECRET` from the same paths. `realpathSync` returns
the **virtual** path, so there is no path-identity confusion.

**Cost: ~0.42 ms/jail** (`CreateJobObjectW` 12 µs · `PromoteToSilo` 5 µs · `BfSetupFilter` 405 µs
median n=25) vs the ~20 ms per-launch leaf ACE AppContainer pays — **~48× cheaper**, and it scales
with mapping count rather than tree size.

**CAVEATS.** SKU coverage is **unmeasured** — Server 2022 only; "bindflt Automatic on Home/Pro/Edu/
Ent" remains READ-FROM-SOURCE, and since SKU gating is exactly what killed Windows containers, **a
Windows 11 Home + Pro measurement is the next thing to do and it decides whether this is the Windows
answer or a Server/Pro-only one.** MSVC/node-gyp was NOT run (that box has no MSVC) — it working is
INFERRED from the normal token. Gotcha for anyone re-deriving: `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
must be set before promotion or it fails `err=87`.

**The dedicated account remains the fallback Windows answer** — nub already started it for `nub sandbox`
([#561](https://github.com/nubjs/nub/pull/561)). Its worst measured cost, a 20-minute
`SetNamedSecurityInfoW` inheritance-propagation stall (recorded during the Windows backend work), is an artifact of the
**in-place** deployment shape; a build-account-owned scratch has no host ACL to rewrite. Same
dissolution as macOS's §3aa contradiction. `SetKernelObjectSecurity` (non-propagating) is the named
untried fix.

## 6. Fast start is real and not load-bearing

Firecracker snapshot restore is **<8 ms Intel / <3 ms ARM** vs ~125 ms cold; AWS Lambda SnapStart
measures p50 3.2 ms. Apple `container` is ~0.8–1.2 s warm. Docker on this host measured 1.31–1.33 s
steady-state; on Linux SSD, 568±5 ms.

**But the maintainer already solved this architecturally** by moving to one world per *install*. A
1.3 s boot against a 30 s install is a rounding error. Snapshot machinery would optimize a cost the
design already retired while inheriting real complexity — Firecracker's own docs warn that resuming
a snapshot more than once makes **both** microVMs insecure w.r.t. entropy, identifiers and crypto
tokens, and require identical host hardware/software for restore.

What *does* survive amortization: gVisor's per-syscall tax, the `docker exec` per-script cost (§2c),
and bind-mount I/O across a VM boundary (macOS/Windows only, where the ABI killer already applies).

## 7. Prior art — nobody has shipped this

**No system has shipped a virgin-world build sandbox that is (a) zero-privilege, (b) cross-platform,
and (c) hosts a host-ABI native compile with the user's own toolchain.** Each relaxed something:

| system | privilege | platforms | toolchain | virgin world? | fails closed? |
|---|---|---|---|---|---|
| **Guix** | root install | **Linux only** | owns it to a `hex0` seed | **yes (purest)** | yes |
| **Nix (Linux)** | root daemon + build users | Linux | owns it via the store | yes | **no** (`sandbox-fallback=true`) |
| **Nix (macOS)** | root install | macOS | owns clang, **grants host `/usr/lib` + Frameworks** | **no** | n/a — **default OFF** |
| **Bazel** | none | no Windows | **host toolchain** | **no — `(allow default)`** | **no** (silent downgrade) |
| **Homebrew** | none | macOS/Linux | host Xcode CLT | **no — `(allow default)`** | no |
| **Cargo `build-wrap`** | `sudo`×3 on Ubuntu 24.04 | no Windows | host, `--ro-bind / /` | **no** | yes |
| **BuildXL** (Microsoft) | admin/SYSTEM | **Windows** | host toolchain | **shadowed namespace** — silo + `wcifs` COW layers + `bindflt` | yes |
| **npm/pnpm/Bun/Yarn/Deno** | — | — | host | **no sandbox at all** | — |

> **CORRECTION (2026-07-29):** an earlier draft of this section said "nobody has shipped this" and
> listed Bazel as the Windows gap. **BuildXL is a counter-example** — Microsoft's own build engine
> sandboxes build processes on Windows with exactly the silo + `bindflt` stack described in §5a
> (`WcCreateContainer(jobHandle, description, isServerSilo)`, `WciSetupFilter`, `BfSetupFilter` —
> read in `BuildXL/Public/Src/Utilities/Native/Processes/Windows/NativeContainerUtilities.cs`). It
> is the closest shipped analogue to this proposal on any platform, and it picks exactly the trade
> §7 describes: keep the host toolchain, shadow the rest.

> **The virgin world and the host toolchain are mutually exclusive, and every system has picked one.**
> Those that kept the virgin world (Nix, Guix) paid by owning the entire toolchain — forcing a root
> install and, for Guix, Linux-only. Those that kept the host toolchain (Bazel, Homebrew,
> build-wrap) abandoned the virgin world and independently converged on the identical shape:
> read everything, write to an allowlist, deny network.

**Nix on macOS is the proposal's own strongest counter-example** (both VERIFIED in source):
`globals.cc:97-103` hardcodes `/System/Library/Frameworks`, `/System/Library/PrivateFrameworks`,
`/bin/sh`, `/usr/lib` as permanent impure host deps; `local-settings.hh:393` — *"The default is
`true` on Linux and `false` on all other platforms."* The most hermetic build system ever shipped
cannot make macOS virgin, and ships the macOS sandbox turned off.

**The near-exact prior art is dead.** `gh api repos/phylum-dev/birdcage` →
`{"archived": true, "license": "GPL-3.0", "pushed_at": "2026-07-06"}`. Phylum built a cross-platform
Landlock+Seatbelt sandbox specifically to confine package-manager lifecycle scripts; archived three
weeks ago, and GPL-3.0 against nub's MIT so never adoptable. **No JS package manager ships OS-level
lifecycle confinement** — verified by grep across pnpm, Yarn Berry, npm, Bun, Deno, LavaMoat for
`seccomp|landlock|sandbox-exec|sandbox_init|AppContainer|bubblewrap|CLONE_NEWUSER|pledge`. The
category is empty; npm's [RFC #54](https://github.com/npm/rfcs/blob/main/accepted/0054-make-scripts-install-opt-in.md)
scoped sandboxing explicitly out as *"a harder problem with more compatibility risk."*

Supporting the case that a sandbox beats an allowlist:
[GHSA-379q-355j-w6rj](https://github.com/pnpm/pnpm/security/advisories/GHSA-379q-355j-w6rj) (CVSS
8.8) — pnpm's `onlyBuiltDependencies` allowlist was bypassed because it *"is never consulted during
the fetch phase."* An allowlist protects only the code paths that remember to call it.

## 8. Corrections to the existing corpus

- **The micro-VM feasibility cost model assumes one jail per lifecycle script.** Its
  killer #2 (cost, 1.5–3 orders of magnitude) does **not** survive the one-world-per-install design.
  Killers #1 (privilege) and #3 (wrong OS) do; #3 is the one that actually decides it.
- **Prisma: do not cite "writes only into `node_modules`."** True of the v6 default path only.
  Prisma 7's generator **requires** an output path (`client-generator-ts/src/generator.ts:13-24`)
  and its documented example is `output = "../src/generated"` — **into the project source tree**.
  `prisma@7.9.1` also has a `preinstall`. Codegen into the project is the direction of travel.
- **The ~92 GB Windows toolchain image figure is wrong** — ~10–14 GB (§5).
- **The macOS SLA does not bar this use** — clause (a), software development (§3).
- **Windows Sandbox can return output and files** — read-write `MappedFolders` persist (§5).
- **The mechanism record claims macOS "hides ungranted paths".** Reported unverified:
  it hides ungranted *files*; ungranted *directories* may be enumerable via a `(literal …)
  (vnode-type DIRECTORY)` multi-filter form that SBPL ORs rather than ANDs. **NOT independently
  reproduced** — the root agent's repro had a broken control (the probe binary never executed).
  Needs a clean re-test before being treated as fact.

## 9. Never researched before this thread

`sysbox` (evaluated: solves nested containers, not nub's problem), `criu` / snapshot-restore,
lazy-pull snapshotters (stargz/SOCI/nydus — irrelevant, the rootfs is a local one-time artifact),
`nsjail`, `minijail`, **`systemd-nspawn`**, LXD/Incus, youki/crun as mechanisms, QEMU/UTM,
Sandboxie-Plus, macOS chroot-as-root, APFS ephemeral volumes, and **Windows process-isolated
containers as distinct from Hyper-V-isolated**.

## 10. The open decision for the maintainer

**On macOS a dedicated uid is a PARTIAL virgin world, not a separate filesystem.** It makes
`getpwuid` name an empty home, empties the keychain and TCC, and puts a DAC wall around the
developer's home — but world-readable paths (`/Users/Shared`, 0755 sibling homes) remain visible,
and it must be composed with Seatbelt for the egress axis DAC cannot express. **Is that enough?**

If yes, the first implementation prerequisite is **moving nub's CAS store out of `$HOME`**.

## Provenance

Six parallel prongs (Opus, high effort), 2026-07-29: Linux container mechanics · macOS/Windows
synthetic-HOME · lifecycle-script write taxonomy · Nix/Bazel/Cargo/Homebrew prior art · elevated
same-OS worlds · exhaustive re-sweep + fast-start. The root agent independently re-verified every
load-bearing claim relayed above: the ELF/Mach-O differential, the EXDEV mount finding and its
control, `clonefile` cross-volume EXDEV and its control, the absence of macOS namespaces and bind
mounts, `getpwuid` defeating a redirected `HOME`, Nix's two macOS concessions, birdcage's archival,
`docker exec` latency, Prisma 6 vs 7 codegen targets, and the unjailed-root-script call site.
One relayed claim (the SBPL directory oracle, §8) failed re-verification due to a broken control and
is explicitly marked unconfirmed.

## Changelog

- 2026-07-29 — Initial write-up.
