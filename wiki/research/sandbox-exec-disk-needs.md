# Exec axis — per-binary disk needs: which common tools FAIL vs degrade with only binary+libs readable

**Status:** research / recommend-only, 2026-07-12. Lands NO product code.
**Question (maintainer):** of the binaries most commonly invoked in coding-agent tool
calls, how many actually **FAIL** (vs merely degrade) when the sandbox grants execute
access to the binary but NOT broad disk access — and exactly what disk paths does each
need to function? This decides whether per-tool config special-casing (auto-including a
tool's known config paths) is worth the brittleness, or whether "grant the binary + its
library closure, user adds config paths explicitly" suffices.
**Parent:** [[sandbox-exec-allowlist]] established the exec primitive (exec-requires-read;
the lib + config-path closure is the honest limit). This doc is the empirical **disk-needs**
data behind that "config-path closure is the honest limit" line.

## Method (empirical, not guessed)

- **Linux VM `nub-linux`** (Ubuntu 24.04, kernel 6.17, Landlock on) — the stricter enforcer.
  True FS enforcement via a **minimal chroot jail** (`sudo chroot` into a root containing
  *only* the resolved binary + its `ldd` closure + minimal `/dev` + a cwd fixture — no
  `/etc`, no `$HOME`, no config, no cache). If the tool runs there, it runs on binary+libs
  alone. Real-work path reads enumerated with `strace -f -e trace=openat -y` in the
  unrestricted env (successes vs `ENOENT`).
- **macOS host** (Darwin 25, arm64) — lib closures via `otool -L`; toolchain data dirs pinned
  empirically (`go env`, `rustup`/`cargo` homes, `pnpm store path`, `npm config`).
- chroot yields `ENOENT` for a denied path where Landlock yields `EACCES`; behaviorally
  identical for "can the tool read it" — a tool that tolerates a missing config also
  tolerates a denied one. Noted where it could matter.

## TL;DR — the headline count (29 binaries)

- **18 of 29 run on binary + lib-closure alone and operate purely on cwd** (class A). No
  config, HOME, cache, or CA needed. The easy majority: all coreutils (`ls cat grep sed awk
  find head tail sort`), `rg`, the shells (`bash sh`), `env`, `which`, `jq`, `tar`, `make`,
  and — the notable one — **`node`** (self-contained; embeds ICU + snapshot; a trivial
  `node -e` runs in a 10-file jail).
- **3 need a small, stable, enumerable config/CA set to do real work** (class B): `git`,
  `curl`, `wget`. git *runs* bare (init/status/log/add/diff all work) but **`git commit`
  hard-fails** (exit 128, "Author identity unknown") without identity config; curl/wget need
  the **CA bundle** + a few `/etc` network-resolution files for HTTPS.
- **8 are toolchain / package-manager dragons** needing their install tree **plus** a mutable
  cache (class C): `python3`, `pip`, `npm`, `pnpm`, `yarn`, `cargo`, `rustc`, `go`. No
  exec-sugar auto-include can fully solve these — they need real fs grants regardless.

**So: per-tool *config* special-casing buys almost nothing for 18 tools (they need nothing),
buys real value for exactly ~2 cases (git identity; the CA/net bundle), and cannot solve the
8 class-C tools at all.** The load-bearing automation is the **library-closure** resolution,
not a config-path database.

## Per-binary table

Class: **A** = binary+libs only, cwd-scoped work · **B** = small stable config/CA set ·
**C** = broad/mutable disk (install tree + cache).

| Binary | Lib closure (Linux → macOS) | Runs bare? | Reads for real work | Class |
|---|---|---|---|---|
| `ls` `cat` `head` `tail` `sort` | libc only (1) | ✅ | cwd only | **A** |
| `grep` | + libpcre2 (2) | ✅ | cwd only | **A** |
| `sed` | + libacl/libselinux/pcre2 (4) | ✅ | cwd only | **A** |
| `awk` (gawk) | + readline/mpfr/gmp/gettext (7) | ✅ | cwd only | **A** |
| `find` | + libselinux/pcre2 (3) | ✅ | cwd only | **A** |
| `which` | — (POSIX shell **script**) | ⚠️ needs `/bin/sh` | cwd/`$PATH` | **A** (script) |
| `env` | libc | ✅ | nothing | **A** |
| `bash` `sh` | + libtinfo (2) | ✅ | cwd; `~/.bashrc` optional | **A** |
| `jq` | + libjq/libonig/libm (4) | ✅ | cwd only | **A** |
| `tar` (bsdtar) | + libacl/selinux/pcre2 (4); macOS libarchive | ✅ | cwd only | **A** |
| `make` | none extra (1) | ✅ | `Makefile` in cwd + toolchain it *spawns* | **A** |
| `rg` (ripgrep) | static (0–1) | ✅ | cwd; `.gitignore`/`.ignore` in tree; `$RIPGREP_CONFIG_PATH` opt | **A** |
| `node` | libstdc++/gcc_s/libm (6) → CoreFoundation/Security (4) | ✅ **(10-file jail)** | cwd-relative: `app.js`, `node_modules/**`, walks up for `package.json` (missing → `ENOENT`, fine); `/etc/ssl/openssl.cnf` opt | **A** |
| `git` | libpcre2/libz (3) → +CoreServices/pcre2/iconv/intl (7) | ✅ for reads; **commit FAILS** | `~/.gitconfig`, `/etc/gitconfig`, `~/.config/git/{config,attributes}`, `/etc/gitattributes`, `/etc/localtime`; identity **required** to commit; repo `.git/` (in cwd) | **B** |
| `curl` | 31 libs (libcurl/ssl/nghttp2/idn2/…) | ✅ `--version`; HTTPS needs disk | **CA bundle** `/etc/ssl/certs/ca-certificates.crt`; net: `/etc/{resolv.conf,nsswitch.conf,hosts,host.conf,gai.conf,passwd}`, `/usr/lib/ssl/openssl.cnf`; NSS libs (dlopen); `~/.curlrc` opt | **B** |
| `wget` | 9 libs (ssl/idn2/psl/…) → 10 (openssl@3/…) | ✅ `--version` | same CA + net set as curl; `/etc/wgetrc`, `~/.wgetrc` opt | **B** |
| `python3` | libm/libz/libexpat (4) | ❌ **HARD FAIL bare** | **stdlib is mandatory**: `/usr/lib/python3.N/**` (`encodings` etc.) — without it: `ModuleNotFoundError: No module named 'encodings'`, fatal. Then site-packages, `$PYTHONPATH` | **C** |
| `pip` | (python module) | ❌ (python + pip pkg) | python stdlib + `pip` in site-packages + cache `~/.cache/pip` (`~/Library/Caches/pip`) + writes site-packages | **C** |
| `npm` | (node **script**) | ❌ **127 bare** | node interpreter **+** its whole package tree `<prefix>/lib/node_modules/npm/**` (thousands of files); `~/.npmrc`, `/etc/npmrc`; cache `~/.npm`; writes project `node_modules` | **C** |
| `pnpm` | (node script, bundled `pnpm.cjs`) | ❌ | node + `pnpm.cjs` bundle + **global store** `~/.local/share/pnpm/store` (`~/Library/pnpm`); `~/.npmrc`; writes `node_modules` | **C** |
| `yarn` | (node script, `yarn.js`) | ❌ | node + yarn dist JS + cache `~/.cache/yarn` / `.yarn/`; `~/.npmrc`/`.yarnrc`; writes `node_modules` | **C** |
| `cargo` | Security/CoreFoundation (rustup shim) | ❌ | **rustup toolchain** `~/.rustup/toolchains/<v>/**`, registry cache `~/.cargo/registry/**`, git-dep cache, project `target/**` (writes) | **C** |
| `rustc` | (rustup shim → real rustc) | ❌ | `~/.rustup/toolchains/<v>/{bin,lib}/**` (compiler dylibs + std `.rlib`s); reads source in cwd; writes `target/` | **C** |
| `go` | libresolv/CF/Security (4) | ❌ | **GOROOT** `<brew>/libexec/**` (std lib + `go` tool), module cache `~/go/pkg/mod/**`, build cache `~/.cache/go-build` (`~/Library/Caches/go-build`); writes both caches | **C** |

## Findings that shape the design

- **The library closure is the non-negotiable, deterministic part — automate it.** Every tool
  is dead without its `ldd`/`otool -L` closure readable+executable (exec-requires-read,
  per [[sandbox-exec-allowlist]]). This is discoverable and stable, so `exec:<tool>` should
  auto-resolve and grant it. Two gotchas:
  - **macOS Homebrew binaries have a WIDE closure** across `/opt/homebrew/opt/*/lib` (git → 7
    dylibs, awk → 6, wget → 10, all under the Cellar). Granting "the binary + its libs" on
    macOS means granting a fan-out of Homebrew paths, not one file. Linux closures are tighter
    (`/lib/x86_64-linux-gnu`).
  - **NSS plugins are `dlopen`ed and invisible to `ldd`.** Any glibc tool that resolves the
    current user (`getpwuid` — git's author fallback, curl) or a hostname loads
    `libnss_files`/`libnss_dns` at runtime. A closure built from `ldd` alone will miss them;
    the resolver silently degrades or errors. The auto-closure must add the `libnss_*` set on
    glibc.
- **`node` is the happy surprise — class A.** It embeds its runtime; a bare `node -e` runs with
  no HOME/config/cache. Its "disk needs" for real work are almost entirely **cwd-relative**
  (`node_modules` lives under the project, `package.json` walk-ups that miss just `ENOENT`
  cleanly). Granting cwd covers the common case. Contrast `python3`, which **cannot start**
  without its stdlib on disk — the single sharpest degrade-vs-fail split in the set.
- **git is the one config case worth special-casing.** It runs bare for reads, but `commit`
  — the most common agent write op — hard-fails without identity, and the path set is tiny,
  stable, and universally known (`~/.gitconfig`, `/etc/gitconfig`, `~/.config/git/`). High
  payoff, low cardinality, near-zero brittleness. This is the *exception that proves per-tool
  sugar is not worth generalizing.*
- **curl/wget disk needs are mostly the NET axis.** Their real-work reads are the CA bundle +
  `/etc/{resolv.conf,nsswitch.conf,hosts}` — network-resolution config, not tool config. When
  the sandbox's **net axis** grants egress it should carry these; they need not be a
  per-tool *exec* concern. (Overlap noted with [[sandbox-net-config-surfaces]].)
- **Class-C tools cannot be solved by exec-sugar and should not pretend to be.** Their needs
  are (install tree) + (machine-specific, mutable, sometimes gigabyte-scale cache) + (write
  targets). A curated auto-include would be perpetually stale (rustup toolchain versions,
  pnpm store relocation via `NUB_CACHE_DIR`/`store-dir`, `GOMODCACHE` overrides) and a wrong
  include is worse than an honest omission. These require **explicit fs grants** — and that's
  the correct, legible contract for them.

## Recommendation

**Hybrid, weighted toward the deterministic half:**

1. **`exec:<tool>` auto-resolves and grants the binary's full library closure** (rx, never on
   a writable path — W^X per parent doc), including the glibc **NSS plugin set** and, on
   macOS, the transitive Homebrew dylib fan-out. This is the load-bearing automation; without
   it exec is useless, and it is fully deterministic. **Worth building.**
2. **Ship a TINY curated known-config table — not a per-tool database.** Only the high-payoff,
   low-cardinality, stable cases earn an entry:
   - `git` → `~/.gitconfig`, `/etc/gitconfig`, `~/.config/git/` (read). The one clear win.
   - a shared **`net-config` bundle** (CA bundle + `/etc/{resolv.conf,nsswitch.conf,hosts}`)
     that the **net axis** owns, so `curl`/`wget`/`node`-fetch inherit it when egress is
     granted — not an exec special-case.
   Keep this table hand-picked and short; resist growing it per-tool. The moment an entry is
   version-specific, machine-specific, or a write target, it does **not** belong here.
3. **Class-C toolchains/PMs get NO config sugar.** Document them as "grant the binary + libs
   via exec; grant their install tree + cache via explicit fs." Provide *documented recipes*
   (e.g. `~/.cargo` + `~/.rustup` + `target/`; the pnpm store + `node_modules`; GOROOT +
   `~/go/pkg/mod` + `~/.cache/go-build`) rather than magic auto-includes.

**Bottom line:** the primitive is **"grant binary + auto-resolved lib-closure via `rx`; user
(or the net axis) grants config/data paths explicitly,"** with a single deliberate exception —
a short curated known-config table whose only strong member today is git identity. General
per-tool config special-casing is not worth the brittleness; the library-closure resolver is
where the engineering should go.

## Class-C dragons — no exec-sugar fully solves these

`python3`/`pip`, `npm`/`pnpm`/`yarn`, `cargo`/`rustc`, `go`. Each needs (a) an install/runtime
tree that is large and version-pathed, and (b) a mutable, often relocatable cache it also
**writes**. An exec allowlist can make them *spawnable*; only real fs grants make them
*functional*. Treat "I granted exec:cargo and it can't build" as expected, not a bug — the fs
axis is the answer.

## Class-C follow-up: global-config-blocked → crash vs degrade

**Maintainer's exact question:** "if these things are blocked from reading their global
configs do they crash outright?" This isolates ONE variable from the class-C set: grant the
binary + libs + cwd + a minimal valid project + all required runtime data + network, and deny
**only the global config** (`~/.npmrc`, `~/.gitconfig`, `~/.cargo/config.toml`,
`~/.config/pip/pip.conf`, `~/.config/go/env`, `~/.yarnrc`, `/etc` variants). Then run a real
command. Empirical on the macOS host (has the full set; the VM lacks pnpm/yarn/cargo/go/pip).
Two deny flavours tested: **absent** (empty `HOME` → config `ENOENT`) and, for the tools with
a real config on this host, **present-but-unreadable** (`chmod 000` → `EACCES`, the flavour a
real Landlock/Seatbelt deny actually produces).

**Answer: overwhelmingly NO — they DEGRADE.** Global config (registry URL, proxy, retry
counts, default flags) is advisory, not required-to-start. One sharp exception (cargo) and one
asterisk (git), below.

| Tool | Representative command | Config **absent** (ENOENT) | Config **unreadable** (EACCES) | Verdict |
|---|---|---|---|---|
| `node` | `node script.js` | ✅ rc=0 | — (no global config) | **DEGRADE** — node has ~no global config |
| `npm` | `npm install --dry-run is-odd` | ✅ rc=0, default registry | ✅ rc=0 (unreadable `~/.npmrc`) | **DEGRADE** |
| `pnpm` | `pnpm config get registry` / `install` | ✅ default registry | — | **DEGRADE** |
| `yarn` | `yarn config get registry` | ✅ default registry | — | **DEGRADE** |
| `pip` | `pip download requests` | ✅ downloaded from PyPI | — | **DEGRADE** (the PEP-668 "externally-managed" block on `pip install` is an env-policy refusal, NOT a config-read crash) |
| `rustc` | `rustc hello.rs` | ✅ rc=0 | — (no global config file) | **DEGRADE** |
| `go` | `go build` (hello) | ✅ rc=0 (`GOENV=off`) | — | **DEGRADE** |
| `cargo` | `cargo build` (hello) | ✅ rc=0 | ❌ **rc=101 HARD CRASH** | **DEGRADE on absent, CRASH on EACCES** |
| `git` | `git commit` | ⚠️ see asterisk | ⚠️ see asterisk | **degrade OR crash — depends on passwd GECOS, not config** |

### The two findings that matter

- **`cargo` is the ENOENT-vs-EACCES dragon — the one real crash.** `cargo build` on a
  hello-world **degrades cleanly (rc=0)** when `~/.cargo/config.toml` is *absent*, but **hard-
  aborts (rc=101, `failed to read configuration file … Permission denied (os error 13)`)** when
  that file *exists but is unreadable*. This is load-bearing for the sandbox because **a real
  Landlock/Seatbelt deny produces `EACCES`, not `ENOENT`.** So merely "not granting"
  `~/.cargo/config.toml` while it exists on disk will **crash the build**. cargo must either
  get read on its config path, or the deny must surface as not-found (path hidden) rather than
  permission-denied. npm and git did **not** crash on the same `EACCES` treatment — this is
  cargo-specific (its config loader treats a read error as fatal, distinct from not-found).
- **`git commit`'s failure is a passwd/GECOS dependency, not a config-read crash.** With global
  config blocked, git tries to *synthesize* identity from the user database (`getpwuid` GECOS
  name + hostname). On a normal account with a full name in passwd (macOS host: "Colin
  McDonnell") it **degrades** — commits with an auto-identity + warning. On a service account
  with an **empty GECOS** (the VM's `nub` user, `nub:x:1001:1002::/home/nub`) it **hard-fails,
  exit 128, "Author identity unknown."** So blocking git's global config alone does not by
  itself crash commit on a real dev box; it crashes only when config **and** a usable passwd
  name are both unavailable. The dependency to grant is really the **passwd/user DB**, not just
  `~/.gitconfig`.

### Implication for the exec design

This *strengthens* the recommendation. Global config being advisory means the exec sugar does
**not** need to chase per-tool global-config paths to keep these tools functional — they
degrade to sane defaults. The two exceptions are not general per-tool-config problems:

1. **cargo's EACCES-fatality is an fs-deny *mechanism* concern, not a config-allowlist one.**
   The engine should prefer a **hide/not-found** deny over a **permission-denied** deny for
   paths a tool may probe (or grant-read cargo's config dir). Track under the fs-axis deny
   semantics, not the exec known-config table.
2. **git wants the passwd/user DB** (already part of the NSS/`getpwuid` closure flagged in the
   main findings), plus — only if commits-as-the-user are desired — the git identity config.

## Changelog

- 2026-07-12 — Initial write-up. Empirical: Linux chroot-jail enforcement + strace on VM
  `nub-linux`; macOS `otool`/toolchain-dir pinning. 29 binaries classified A/B/C.
- 2026-07-12 — Added the class-C "global-config-blocked → crash vs degrade" section. Verdict:
  DEGRADE across the board, with cargo the lone EACCES-vs-ENOENT crash (rc=101 on an
  unreadable-but-present config) and git's commit failure traced to an empty passwd GECOS, not
  config. Empirical on the macOS host (full class-C set) + VM GECOS confirmation.
