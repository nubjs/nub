# Build jail on macOS — every approach tried, and why

Canonical ledger of every mechanism attempted for Nub's **build jail** on macOS, one heading per approach. The build jail confines dependency lifecycle scripts during `nub install` and must be **totally unprivileged with no setup command, ever**. macOS is the platform where that constraint costs the least: Seatbelt is unprivileged, `/usr/bin/sandbox-exec` ships with the OS, and there is no setup command to skip. **macOS is therefore where the strongest enforcement lives — and the place where over-claiming it as cross-platform is most tempting.**

**This document exists because approaches keep getting re-proposed after being refuted**, and because macOS carries two capabilities the other platforms do not — deny-inside-allow and genuine per-host egress — which makes it easy to write a claim here that is false on Linux or Windows. **The build jail uses neither of them.** Both are platform capabilities that Seatbelt affords and the jail declines, for the same reason in both cases: a rule that only one of three platforms can enforce is a compatibility liability rather than a defense. They survive in `nub sandbox`, which is a different product with a different privilege budget.

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
| the dev-only catalog override (`src/catalog_override.rs`) | which grants an arm actually ran with, without a rebuild between iterations | it REPLACES the compiled catalog rather than merging over it, and a build without the `build-jail-catalog-override` cargo feature REFUSES a set `NUB_BUILD_JAIL_CATALOG` rather than ignoring it — so an arm cannot silently measure the shipped tables under an override's name |

---

# The adopted mechanism

## Seatbelt via `sandbox-exec -p` — ADOPTED

**What it is.** The resolved policy IR is compiled to an SBPL profile and enforced by wrapping the child in `sandbox-exec -p <profile> -- <cmd>`. Module doc: `crates/nub-sandbox/src/backend/macos.rs:1-35`. Posture is `(deny default)`; the `MACOS_SEATBELT_BASE` block (ported from Codex/Chromium, `backend/macos_seatbelt_base.sbpl`) is the bootstrap that lets an arbitrary binary dyld-load under a deny-default profile, and Nub then appends the IR-derived read/write/net rules.

**What it buys.** Deny-by-default filesystem confinement, **live evaluation** with no enumeration break, closed path-based evasion, and mediated metadata ops.

**What it does NOT buy the build jail, despite being able to.** Seatbelt is the only one of the three primitives that can enforce a host list at zero privilege, and the jail's net axis does not use it: an admitted package gets `(allow network*)` and an unadmitted one gets coarse deny, with no proxy started in either arm. That capability is [withdrawn from the jail on purpose](#the-loopback-egress-proxy-with-port-pinning--rejected-design-for-the-build-jail-adopted-for-nub-sandbox) and lives on in `nub sandbox`.

**Zero privilege, and there is nothing to install.** `/usr/bin/sandbox-exec` is the stock unprivileged entry point and every confined launch goes through exactly that path, which is what makes its presence the only readiness question macOS has (`macos.rs`'s `SANDBOX_EXEC_PATH`, re-exported by `macos_setup::enforceable` rather than restated).

**Live evaluation — no enumerate-to-exclude needed.** A file **created after launch** under a granted subtree reads back fine, and a post-launch `.envlate` is still **denied**. This is why the banned enumerate-to-exclude pattern is unnecessary here, and it is the property Windows' setup-time enumeration and Landlock's fixed ruleset both lack.

**Path-based evasion is closed.** Hardlink, symlink, `cp`, `mv`, `..`, `.`, `//`, and the `/tmp`→`/private/tmp` alias are **all blocked** — Seatbelt canonicalizes before matching.

**One hard ceiling, measured.** The profile is an **argv element** and shares `ARG_MAX` (~1 MiB) with the child's full environment: ~1400 grants ≈ 1.04 MB is fine, 3000 → `E2BIG`. That ceiling was nearly hit once — see [the grant explosion](#the-grant-explosion-under-arg_max--adopted-fix).

## The pure allowlist under `(deny default)` — ADOPTED, and macOS is NOT an exception to it

**What it is.** The build jail on macOS is a **pure allowlist that emits zero deny rules**, exactly as on Linux and Windows. `compiler/preset.rs:458-462` states it directly: *"NO `/etc/shadow` deny here any more. The build jail is a PURE ALLOWLIST — it emits no deny rules at all — so the password-hash files are protected by not being granted … on macOS by the Seatbelt base granting the specific `/private/etc` files it needs instead of the whole subpath."*

**A premise to correct, because it is easy to form and it is wrong.** The generous-read base `(allow file-read* (subpath "/"))` is emitted **only** when `default_effect == Allow` (`macos.rs:12-14`, `:738`), which is the `nub sandbox` / `sandbox: true` shape — **not** the build jail's. **macOS is a pure allowlist on the read axis too**, with the same guarantee shape as Linux and Windows. Anyone reading "macOS uses a generous read base with secrets carved out by deny" is reading the agent-sandbox product, not the build jail.

**What the build jail's read set actually is** (`preset.rs:269-360`, `grant_build_jail_dependency_reads`), and it is measured rather than reasoned — a 34-package read-ladder study plus a 311-package trust-list corpus, of which 217 of 219 passing packages were unaffected by the narrowing:

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

## The loopback egress proxy with port pinning — REJECTED (design) for the build jail, ADOPTED for `nub sandbox`

**What it is.** Egress is permitted to **exactly** the proxy's loopback port — `(allow network* (remote ip "localhost:{port}"))` at `macos.rs:719`, deliberately **not** `localhost:*`, with a test asserting that (`macos.rs:2583-2596`). Every packet must traverse Nub's proxy, and **a raw socket cannot bypass it.** It is the only mechanism on any of the three platforms that enforces a host list at zero privilege, and it works.

**The build jail no longer reaches it, and that was a decision rather than a regression.** `build_jail_net` returns a boolean per package and never references `$downloads` (`compiler/preset.rs:602-671`): an admitted package compiles to `true` → `net.enforce = false` → `(allow network*)` with no proxy started, and an unadmitted one compiles to `false` → enforce with no Allow rule, which `proxy_needed` (`backend/mod.rs:906`) reads as coarse deny. **Neither arm of the build jail starts a proxy on macOS.**

**Why it was withdrawn, stated as the reason rather than the history.** A host list that gates one platform and is provenance on the other two is a compatibility liability rather than a defense. macOS is the platform most developers build on, so enforcing per-host *there* means an incomplete list throws errors that Linux and Windows users never see — and confidence in the list is low. The two platforms that cannot follow are blocked on privilege, not effort: Linux needs a network namespace to force the child through the proxy, which needs an unprivileged user namespace; Windows' loopback exemption (`NetworkIsolationSetAppContainerConfig`) is admin-only.

**The measurement that established the mechanism, macOS 26.5.2 arm64, under a real `nub install` when the build jail still routed through it.** It is retained because it is what proves the mechanism works — it now describes the `nub sandbox` shape, not a jailed lifecycle script. A proxied child sees `HTTP_PROXY`/`HTTPS_PROXY`/`http_proxy`/`https_proxy`/`npm_config_proxy`/`npm_config_https_proxy = http://<per-session-token>@127.0.0.1:<port>` (`macos.rs:135` → `set_proxy_env`), and the cells separate cleanly:

| cell | unconfined | confined |
| --- | --- | --- |
| `CONNECT nodejs.org:443` via the proxy *(an allowlisted host)* | n/a | **HTTP 200 Connection established** |
| `CONNECT example.com:443` via the proxy | n/a | **HTTP 403 Forbidden** |
| `CONNECT webhook.site:443` via the proxy | n/a | **HTTP 403 Forbidden** |
| direct dial to the ALLOWLISTED host by raw IP:443 | CONNECTED | **EPERM connect** |
| direct dial to a NON-allowlisted raw IP:443 | CONNECTED | **EPERM connect** |
| direct dial to a live loopback listener ≠ the proxy port | CONNECTED | **EPERM connect** |

**⇒ Honouring the proxy is required for FUNCTIONALITY, never for SECURITY** — ignoring it yields EPERM, not a bypass.

**Do not restore `$downloads` here as a missing feature.** The build jail's net axis is a per-package boolean on every platform; `$downloads` still serves `nub sandbox`, and the catalog's per-package `hosts` arrays are retained as provenance — a package that used to fetch from its own CDN and now reaches elsewhere shows up as a reviewable diff on `data/build-jail-catalog.json` — never as a gate.

## The egress contract is uniform across the three platforms, and it is a per-package boolean

**The rule.** **No catalog entry ⇒ no egress. An entry ⇒ coarse egress.** Denial spells `false` everywhere; the admitted case is coarse everywhere too, but **spelled per platform, because `enforce` carries more than egress on Linux** (`compiler/preset.rs:602-671`):

| platform | admitted spells | why that spelling |
| --- | --- | --- |
| **macOS** | `true` (`enforce = false`) | Seatbelt emits `(allow network*)` and starts no proxy |
| **Windows** | `true` (`enforce = false`) | the AppContainer `internetClient` capability is granted on exactly `!net.enforce`, so coarse-allow is the ONLY spelling that reaches it |
| **Linux** | `["*"]` — a catch-all Allow naming no host | it CANNOT be `true`: `build_seccomp` gates the whole socket-family ceiling and the io_uring triple-block on `net.enforce`, so `false` would re-permit `AF_UNIX`, `AF_VSOCK`, `AF_PACKET` and io_uring's socket-creation side door. A catch-all Allow lifts exactly `AF_INET`/`AF_INET6` out of the ceiling and leaves the rest denied |

**Only the Windows half of that contract is measured** — run 30612421934, de-elevated on a real AppContainer, where all 8 admitted names connect and an unadmitted one is refused `WSAEACCES` by the kernel with a native probe child carrying no `NODE_OPTIONS` at all. The macOS and Linux halves are read from the code, not run.

**The claim boundary this creates, and it is the most over-claimed fact here.** **Per-host egress filtering is enforced nowhere in the build jail.** The load-bearing defense is PACKAGE IDENTITY, which now holds identically on all three platforms — the earlier reading that package identity gated Windows while hostname gated macOS was true of an older tree and is false at HEAD. **A doc or catalog note promising per-host enforcement, on any platform, would be false.**

**A grant is a per-package BOOLEAN, so it restores every host that package talks to.** Two costs are written into the catalog entries rather than glossed: admitting `snyk` also restores its Sentry telemetry POST, and `@pact-foundation/pact-node`'s `.npmrc` read hands a lifecycle script a file that routinely holds a registry token.

## The `TmpMode::Private` per-run scratch — ADOPTED

**What it is.** A fresh per-run tmp directory granted read-write, with the shared host tmp hidden. Set by the `$tmp` surface key (`preset.rs:420-421`), which sets the MODE rather than emitting an ordinary fs rule. The key is `#[cfg(not(windows))]`: the AppContainer backend cannot enforce a private tmp at all, so Windows takes the shared mode by the key's absence.

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

# SBPL semantics — four silent no-ops

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

## The `(trace …)` directive is inert

**What it is.** SBPL's documented policy-authoring aid: `(trace "<path>")` in an `(allow default)` profile is supposed to log every operation the child performs, so a profile can be generated from a real run instead of hand-written. **It writes nothing on darwin 25.5.**

**Measured, with the positive control that makes the negative mean something** — macOS 26.5.2 / darwin 25.5.0, stock `/usr/bin/sandbox-exec`:

| profile | result |
| --- | --- |
| `(allow default)` + `(trace "/private/tmp/…/trace.out")` | rc=0, child runs, **no file created** |
| the same with the `/tmp` spelling | rc=0, **no file created** |
| `(allow default)` + `(deny file-read* (literal …))` | **`Operation not permitted`, rc=1** |

**The deny row is the control**: the same profile shape, loaded by the same binary in the same run, changes behaviour — so the profile is being parsed and applied, and `(trace …)` is inert rather than silently rejected. **Do not plan a trace-driven policy generator on this platform**; the read set here was derived by run-log mining and denial lines instead, which is why the read-ladder study was expensive.

---

# Defects found and fixed

## The stdio `file-read-metadata` grant — ADOPTED

**The defect.** Without it, **every Node under a confining profile dies with SIGABRT and no diagnostic.** Node's `PlatformInit` stats fds 0/1/2 before its own error machinery is up and reads `if (errno != EBADF) ABORT()`, so an ungranted stdio path turns EPERM into a message-less abort inside `InitializeOncePerProcessInternal`. The denial line that finally named it: `Sandbox: node(3101) deny(1) file-read-metadata /private/tmp/.../out.log`. **Node is only the loudest victim; any program that stats its own stdio hits the same wall.** `macos.rs:467-528`.

**Why it presents so erratically.** Only a **write-only** fd is affected, which is why an interactive shell survives and a `>` redirect — the shape a log-capturing harness and every CI job produce — does not. That asymmetry is what made it look like a contention flake.

**Scope, kept minimal and verified rather than assumed.** Metadata only — verified that a bare metadata grant yields `statSync` and `access` but EPERM on read/open/readlink/readdir — on the exact resolved path, **never a parent directory**. A pipe or socket has no vnode, `F_GETPATH` fails, and nothing is granted.

**The policy-deny check is load-bearing and position is not.** Every deny in a compiled profile is `file-read*` (both the policy's own via `emit_fs`, and `emit_tmp`'s shared-tmp deny), and a `file-read-metadata` allow beats a `file-read*` deny **at any position** — measured, both orders boot Node — because the leaf operation outranks the group. **So withholding the path is the only thing that closes it**; a broader stdio grant, or one that did not consult the policy, would silently punch a stat-shaped hole through the `.env` / `~/.ssh` floor that `compiler::defaults` promises no later allow can reopen.

**Not to be confused with the other SIGABRT.** A separate jail-specific `SIGABRT` in `InitializeOncePerProcessInternal` is seen **only under heavy host load**, with contention not ruled out, and is marked "do not chase". The stdio case above is the one with a mechanism and a fix. Related work: branch `jail-stdio-abort` `549564f36b` concluded the remaining stdio abort is **not worth fixing** because it is unreachable under the build jail, and shipped a `LIMITATIONS.md` note instead.

**⛔ THE ABORT DOES NOT REPRODUCE ON GITHUB'S macOS RUNNERS, AND THE THREE OBVIOUS EXPLANATIONS ARE ALL MEASURED DEAD.** The two tests guarding this grant are differentials whose control requires ungranted stdio to abort Node. On hosted runners it does not — Node exits 0 — so both tests fail *on their own control assertion*, saying they cannot verify anything rather than reporting a verdict about nub. Bracketed on one commit (`cargo test -p nub-sandbox --lib stdio`, run `31099804876`, with a guard failing the probe if the control leg unexpectedly passed):

| environment | Darwin | result |
| --- | --- | --- |
| `macos-14` runner | 23.6.0 | 2 passed, 2 failed |
| `macos-15` runner | 24.6.0 | 2 passed, 2 failed — *identical* |
| dev Mac | 25.5.0 | 4 passed |

⇒ **Not a 23→24 boundary, so bumping the runner image does not fix it.** Nor is it stdio SHAPE — all four pass locally from `/dev/null`, to a file, through a pipe, and backgrounded off any tty. Nor is it Node MAJOR — v20.19.0, v22.23.1, v24.17.0 and v26.5.0 all abort on Darwin 25. What remains is a Darwin-25-or-later boundary or an unidentified runner-environment factor; **these are not distinguishable with available hardware, and CI's own Node version has never been measured.** The handling is the same either way.

**The handling, and why it is not an OS gate.** An OS-version gate would encode the refuted premise, and relaxing the assertions would turn a working control into a rubber stamp. Instead the precondition is probed once per run and, when it does not hold, announced on the real stderr and skipped — the contract `skip_without_bwrap_with` already provides for bubblewrap on Linux, `NUB_SANDBOX_REQUIRE_STDIO_ABORT=1` included to convert the skip into a hard failure. ⛔ **Leaving the job red was not the safe option:** the failing step SKIPS every macOS conformance step behind it, so the platform had no effective gate coverage and nothing in the output said so — `219 passed; 2 failed` of 221, with an entire conformance matrix discarded behind it. Note the two tests are treated differently: the second one's security property (`granted.is_empty()` — a policy-denied redirect earns no grant) is decided by nub's own withhold branch rather than by the OS, so it and the per-shape grant-set checks stay unconditional; only the downstream SIGABRT consequence takes the precondition.

## A confined process cannot resolve its own cwd — ADOPTED fix, and depth had nothing to do with it

**The rule, measured.** **Seatbelt gates `getcwd(2)` on `file-read-data` of the cwd's OWN directory node.** Not on traversal of its ancestors, and not on anything a parent process held. The access class matters and splits cleanly:

| grant on the cwd's node | `getcwd` |
| --- | --- |
| `file-read-metadata` | **FAIL** |
| `file-test-existence` | **FAIL** |
| `file-read-data` | **OK** |

**The defect.** Nothing in the build jail's surface named the project root, so any confined process running *there* could not learn where it was — and a lifecycle script acting on the consumer's repository spawns its real work at `INIT_CWD`, which is exactly the project root. Five signatures, one cause: `uv_cwd EPERM` in Node, `getwd: invalid argument` from Go, `fatal: Unable to read current working directory: Operation not permitted` from git, `pwd: .: Operation not permitted` from coreutils, and `shell-init: error retrieving current directory` from bash.

**It presented as a rule about process DEPTH, and that reading is wrong.** The script itself worked and its children did not — because the script's own cwd is its package dir, which is granted. **Depth is irrelevant:** a great-grandchild resolves a granted cwd fine, and an immediate child fails at an ungranted one. **Ancestors are irrelevant too**, measured on an L1/L2/L3 chain and again on a `proj/node_modules/pkg` cwd resolving with both ancestors ungranted. The node grant does not leak sideways either.

**The fix grants the project root NODE alone, never the subtree** (`65c51530cb`). What a confined package gains is the ability to list the root's top-level entries; the project's files stay out. It renders as `(literal …)` on macOS and `MountAccess::ListOnly` on Linux — the same shape `curated::project_cwd` already used per package, now unconditional.

**Differential**, macOS 15, real jail, `@arkweid/lefthook@0.7.7`: without the grant, `Couldn't discover absolute path` / `getwd: invalid argument`; with it, both gone. A synthetic `file:` dependency reproduces git's and bash's variants and clears them the same way.

**Two walls behind this one, unchanged and NOT closed by it.** `.git/HEAD` and `.git/config` are unreadable, so `git rev-parse` still reports `not a git repository`; and `.git/hooks` is unwritable. A package needing those needs its own catalog grant.

## An fs `Allow` must never synthesize a deny — ADOPTED fix, and Seatbelt was the outlier

**The defect.** `emit_fs` mapped `(Allow, Read)` to `(deny file-write* <term>)` and emitted it in the write loop. Because Seatbelt is last-match-wins within a node, **a read grant silently revoked write from everything it enclosed.** `curated::grant_from_table` appends `projectReads` *after* `siblingDirs`, which is exactly that shape: measured on a real Seatbelt profile, widening `siblingDirs` alone wrote 20 entries under `.prisma/client`, and adding `projectReads: ["node_modules"]` alone wrote **0**.

**The argument that settles the polarity, and it is stronger than the measurement.** The synthesized deny had nothing to cap. **Seatbelt's write base is already `(deny default)`, and a generous `default_effect` widens only READS** — so the only thing that deny could ever cancel was another Nub grant. Only `Effect::Deny` subtracts on the fs axis now; an `Allow` renders as permission or as nothing (`f43aab575f`). Fixing it at the mechanism rather than at a call site is what made it stick: the same root cause had been fixed once before at `curated::project_cwd`'s call site alone (`0d5dc51381`), which is why it survived everywhere else.

**It also settles a semantics question the three backends disagreed on.** `FsPolicy`'s contract already says the write-set is the `ReadWrite` allows and that a `Deny` removes both; Landlock unions its rules and has no deny primitive at any ABI; `windows::derive_grants` accumulates a read set and a write set with no ordering at all. **Three of four renderings were additive already. Seatbelt was the outlier**, and `enforce_pure_allowlist` now records that its invariant binds the *backends*, not only the IR — stripping every deny from the IR is worth nothing if a backend synthesizes one back.

**The cost, stated rather than glossed.** *"Readable but not writable inside a writable grant"* is now inexpressible, on every backend. Removing access is a `Deny`, which removes read too. No docs example and no catalog entry depended on the demote, and the `projectReads` guidance in `data/README.md` and on `CuratedGrant` — which said to prefer read as the smaller grant, steering contributors straight into this — now says what is true: the fields compose, and nothing in the catalog subtracts.

**Backends.** macOS is the change. **Landlock is unaffected and was measured so** on 6.8.0-136-generic ABI 4 at `0d5dc51381`: a package dir nested under an enclosing node-only project rule keeps the rights its own rule grants. Windows is unaffected by construction. Bubblewrap layers binds in authored order, so a later enclosing `--ro-bind` does shadow an earlier nested `--bind` there — but the build jail never reaches it, because `linux::preflight` is Landlock-or-refuse for a build-jail policy.

**A test that had been failing since `0d5dc51381` was repaired in the same change.** `macos_moveblock` hand-writes its writable container as a bare path, which now renders `(literal P)` and grants nothing inside it — so both its "a legit write still succeeds" non-regressions and its two documented relocation residuals were measuring a policy that grants no container write at all.

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

## A confined native build needs its own store-entry root — ADOPTED fix

**The defect.** node-gyp reaches exactly one directory outside the package dir, and it reaches it by arithmetic rather than by choice: `node-addon-api` hands its `.gyp` over `..`-relative, and gyp joins `depth(".")` + `generator_output("build")` + `base_path("../../../..")`. **`build/` absorbs exactly one `..`**, so the join collapses one level too shallow and lands on the package's **store-entry root** — scoped and unscoped alike, since a scoped name's extra `..` is cancelled by its extra directory level.

**npm and pnpm compute the same escaping path and both build fine, so the layout is not the fault; only confinement turns it into a failure.** It presented for weeks as an ABI problem because gyp's `EnsureDirExists` is a bare `except OSError: pass` one line above the `open()` that reports, laundering the jail's EPERM into a misleading `ENOENT`.

**The fix grants that root read-write, guarded on store containment** (`46661af07c`). The candidate qualifies only when its parent is a virtual store the engine materializes into — the global `$cache/nub/pm/store` or the project-local `node_modules/.store`. **Under a hoisted linker the identical arithmetic lands on the project root** or a workspace member's root, and the guard declines there *structurally* rather than by luck. The project-local leaf has one definition, in `nub-sandbox`, which `nub-cli` re-exports; a second copy that went stale would decline silently, compiling the grant to nothing and failing the package exactly as before.

**Measured against the real catalog path, isolated cache, one variable between arms:**

| package | control | treatment |
| --- | --- | --- |
| `@vscode/sqlite3` 5.1.14-vscode | break | `vscode-sqlite3.node` **1,886,416 B** |
| `cmark-gfm` 0.9.0 | break | `binding.node` **368,832 B** |
| `drivelist` 12.0.2 | break | `drivelist.node` **128,416 B** |

Each control arm reproduces the corpus string verbatim (`FileNotFoundError … nothing.target.mk`) and produces **no artifact**. A confined probe package confirms that writes through the store entry's own dependency symlinks are still refused with EPERM, as are the store root, a sibling entry, and the project tree.

**Accepted cost.** The store entry also holds `node_modules/.bin` for a package with binary deps, which the grant makes writable. Those shims execute only while this package's own lifecycle scripts run, and the grant does not reach the project's `node_modules/.bin`.

**Residual, unfixed on purpose.** A scoped package under a hoisted linker escapes into `node_modules/@scope/`. Granting the scope dir would hand the build write access to its sibling packages.

**Not the same item as the CAS-store `.mk` write below**, which is a linker bug with no acceptable jail-side fix. This one has a bounded, guarded target and is fixed.

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

- **`projectCwd` is load-bearing on Seatbelt and a NO-OP on Landlock**, because `chdir` and `getcwd` are not Landlock-handled accesses. **Correction to an earlier reading: the field was NOT removed.** It is part of the settled catalog schema (`catalog.rs`'s `PackageGrant.project_cwd`, parsed from `projectCwd`, codegen'd by `build.rs`) and `curated.rs:269-273` turns it into a read rule on the project root — the node alone, never `subtree_globs`, or a cwd grant would widen into a whole-project read. Two entries carry it today, `@prisma/client` and `msw`, and both are recorded `platform: macos-arm64`. **The per-package field is now largely subsumed** by the unconditional project-root node grant that [the cwd defect](#a-confined-process-cannot-resolve-its-own-cwd--adopted-fix-and-depth-had-nothing-to-do-with-it) added, which is the same shape applied to every package.
- **All three catalog `packageGrants` are recorded `platform: macos-arm64`, and that field is provenance rather than a gate.** The parser validates it and drops it; nothing carries it into the generated table, so **every grant applies on every OS** — a known, deliberate schema gap (`data/README.md`, "Platform-conditional entries"). The consequence to hold: a grant measured on macOS is already in force on Linux and Windows, where its evidence does not reach. For `@prisma/client` the Linux differential `DIFFERS` identically with and without it.

**⇒ Never ship a build-jail grant measured only on macOS.** macOS is the most permissive platform to measure on and the least representative.

## Contradictions in the record, unresolved

1. **The generous-read premise.** Any statement that the macOS build jail uses a generous read base with secrets carved out by deny is **false** — that is the `nub sandbox` shape (`default_effect == Allow`), while the build jail is a pure allowlist on the read axis (`preset.rs:458-462`, `macos.rs:738`). The confusion is easy because the same backend serves both products and the module doc describes both in one paragraph.
2. **Two different SIGABRTs are recorded under one name.** The stdio `fstat` abort has a mechanism, a denial line and a fix (`emit_stdio_grants`); the load-dependent abort is a separate, unexplained item marked "do not chase". Reading the second alone leads to the conclusion that the fixed one is still open.
3. **A proxy claim that holds nowhere in the build jail.** No jailed child on any platform is pointed at a filtering proxy. macOS starts none on either arm of the per-package boolean; Linux stamps no proxy env at all (measured); Windows' net gate does stamp `http_proxy`/`https_proxy` on every child, but at `http://127.0.0.1:1` — a **blackhole**, so a proxy-honouring non-Node child fails to connect rather than being filtered (`backend/net_gate_shim.js`, `forceEnv`). Describing the three platforms as a gradient of proxy-mediated egress is wrong in every direction. Anything in the record describing a proxied jailed script on macOS predates the per-package boolean and is describing `nub sandbox`. Cross-referenced in both sibling documents.

## Changelog

- 2026-07-31 — Scrubbed the two places the withdrawal had not reached. The Seatbelt row still listed "genuine per-host egress at zero privilege" among what the ADOPTED mechanism buys the jail, and the document header still introduced per-host egress as a macOS capability without saying the jail declines it. Both now separate what the platform affords from what the jail uses; the capability itself is unchanged and still `nub sandbox`'s.
- 2026-07-31 — **REVERSAL:** the loopback egress proxy is no longer on the build jail's path on macOS. `build_jail_net` is a per-package boolean that never references `$downloads`, so an admitted package gets `(allow network*)` with no proxy and an unadmitted one gets coarse deny; per-host enforcement is withdrawn from the jail deliberately and survives only in `nub sandbox`. Recorded the resulting uniform three-platform egress contract, of which only the Windows half is measured. Added three fixes: the `getcwd` rule (gated on `file-read-data` of the cwd's own directory node — depth-independent, ancestor-independent), the `emit_fs` polarity fix (an `Allow` never synthesizes a deny; Seatbelt was the outlier across four renderings), and the store-entry-root grant a confined native build needs.
- 2026-07-30 — Reconciled against the tree. **REVERSAL:** `projectCwd` was recorded as removed from the grant schema; it is live, carried by two catalog entries, and `curated.rs` emits it as a project-root read node. Recorded that the catalog's `platform` field is provenance and does not scope a grant, so every entry applies on every OS. Made the egress claim precise — the `$downloads` allowlist is flat for every jailed script here, and the per-package `packageNetwork` boolean reaches only the Windows-stamped net gate — and corrected the sibling-platform proxy claim (Linux stamps none, Windows stamps a blackhole). Added the measured `(trace …)` inertness and the dev-only catalog override.
- 2026-07-30 — Moved into tracked `research/design/` so code comments can link here, and scrubbed of pointers into untracked documents. Every measurement, table and verdict is unchanged.
- 2026-07-29 — Initial consolidation.
