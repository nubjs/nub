# Build jail on Windows — every approach tried, and why

Canonical ledger of every mechanism attempted for Nub's **build jail** on Windows, one heading per approach. The build jail confines dependency lifecycle scripts during `nub install` and must be **totally unprivileged with no setup command, ever** — that constraint is what kills most of what follows. The separate `nub sandbox` product is allowed a one-time elevation and is covered here only where a route was tried for the build jail and survives there.

**This document exists because approaches keep getting re-proposed after being refuted.** A preload realpath patch was proposed, refuted, and proposed again. The `--preserve-symlinks` flag was proposed despite a formal rejection already on record. An expensive detour into restricted tokens happened because a probe using `AccessCheck` concluded AppContainer could not do reads, when the traverse model at `crates/nub-sandbox/src/backend/windows.rs:412-423` already said leaf grants suffice — and `AccessCheck` **cannot model bypass-traverse by construction**. Read this before proposing a Windows mechanism.

## How to use this document

Each approach carries a status, what it would have bought, the evidence with its measurement tool, and — the field that makes this more than an obituary — **what would have to change for it to become viable again**.

### Status values

| status | meaning |
| --- | --- |
| **ADOPTED** | in the shipping design |
| **DEAD (mechanism)** | the OS primitive cannot do it; no amount of privilege or tuning helps |
| **DEAD (privilege)** | works, but needs admin or a setup command — disqualifying for the build jail |
| **DEAD (compat)** | works and confines correctly, but breaks packages |
| **OPEN** | unresolved; a live blocker or an unexplored candidate |
| **REJECTED (design)** | technically available and deliberately not used |

### Measurement tools, and why the distinction is load-bearing

| tool | what it establishes | what it CANNOT see |
| --- | --- | --- |
| `AccessCheck` | the OS's own evaluator against one real token and one real security descriptor | **bypass-traverse, by construction** — it evaluates ONE descriptor, so it can only say whether a path fails **as a target**, never whether a deep open **through** it fails. It also cannot say whether a process can be LAUNCHED with the token. |
| a real `CreateProcessW` / `CreateProcessAsUserW` launch | the whole check in situ, traverse skip included, plus startup viability | costs a window station (so not runnable over SSH — see [Session 0 cannot launch an AppContainer](#session-0-cannot-launch-an-appcontainer)) |
| `Get-Acl` / `GetNamedSecurityInfoW` descriptor read | what a DACL actually contains | nothing about whether the check passes |
| `RtlIsCapabilitySid` / `NtCreateLowBoxToken` | whether a SID is a capability, and whether the kernel will accept it | — |
| the dev-only catalog override (`src/catalog_override.rs`) | which grants an arm actually ran with, without a rebuild between iterations | it REPLACES the compiled catalog rather than merging over it, and a build without the `build-jail-catalog-override` cargo feature REFUSES a set `NUB_BUILD_JAIL_CATALOG` rather than ignoring it — so an arm cannot silently measure the shipped tables under an override's name |

**This is the distinction that cost the multi-hour detour.** Every Windows arm before run 30506129146 was an `AccessCheck` model, so it established only that `lstat`/`readdir`/`chdir` **ON** `C:\` and `C:\Users` fail. It never established that a deep open **THROUGH** them fails — and the real launch showed it does not. Do not draw a read verdict from `AccessCheck` alone.

---

# The adopted mechanism

## AppContainer (LowBox token) — ADOPTED

**What it is.** A per-run AppContainer profile whose SID is granted inheritable ACEs on exactly the allowed leaves, launched via `CreateProcessW` + `STARTUPINFOEX` carrying `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`. Module doc: `crates/nub-sandbox/src/backend/windows.rs:1-51`. A LowBox token reaches an object only where the object's ACL names its AppContainer SID, a held capability, or `ALL APPLICATION PACKAGES` — everything else is denied by default, with no per-file deny needed.

**What it buys.** Deny-by-default reads, deny-by-default writes, and coarse egress denial, all in one token at zero privilege.

**Measured** — `windows-latest` (Server 2025 Datacenter 10.0.26100, AMD64) and `windows-11-arm` (Win 11 Enterprise 10.0.26200, ARM64), four concordant runs, `FAILURES = 0`, every number identical on both images. Harness `tests/win-bypass-traverse/` on branch `sandbox/win-preserve-main-only` (also `sandbox/win-bypass-traverse`); workflow `win-bypass-traverse-probe.yml`. Controls: a `plain` baseline arm passing all 35 cells; every AC arm denied on `C:\` (gate live); `System32` granted in a confined arm (gate passable, so a column of errors is about DACLs and not a dead child); an ace-withheld arm denied; ungranted siblings denied; and **a per-arm DACL read-back proving the inheritable ACE reached the deep file** — without which a propagation slip is indistinguishable from a kernel denial. `verdict.ps1` is validated against six synthetic worlds including `grant-never-landed`, which is read-cell-identical to a real denial.

**Verdict.** Viable, and settled as the route (2026-07-29). **Two blockers remain, both Node/libuv interaction problems rather than DACL or privilege problems:** [the `resolveMainPath` realpath walk](#nodes-realpath-walk-opens-every-ancestor-as-a-target--open-and-it-is-blocker-1) and [the piped-spawn hang](#piped-child_process-stdio-hangs-indefinitely--open-and-it-is-blocker-2). The jail is **not shippable** until both are closed.

**What would change the verdict.** Nothing about the mechanism; it is the two Node problems.

## Bypass-traverse — leaf-only grants, no ancestor ACE — ADOPTED

**What it is.** Granting the AppContainer SID read/modify on only the allowed **leaves**, and relying on the object manager to skip the access check on intermediate path components. A LowBox token retains `SeChangeNotifyPrivilege` (Bypass Traverse Checking) enabled, and standard local NTFS volumes carry `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL` on the volume device object. Traverse model recorded at `windows.rs:412-423`.

**What it buys.** It dissolves the problem that four separate sections of the record treated as fatal: Nub never needs `WRITE_DAC` on `C:\` or `C:\Users`, and never touches them.

**Measured** by a real AppContainer launch. With one inheritable grant at `%USERPROFILE%\<project>`, a confined child five components down at `%USERPROFILE%\<proj>\data\proj\node_modules\dep\index.js` succeeded on `readFileSync` (812 B), `require()`, `statSync`, `readdirSync`, `writeFileSync`, `process.chdir` plus a relative read, `node <that file>` as the entry point, and a **bare** specifier resolved through `_nodeModulePaths` probing every ancestor's `node_modules`. **Nothing was written on `C:\` or `C:\Users`** — every ACE went inside `C:\Users\runneradmin\…`, which the invoking user owns.

**And the mirror image, in the same child.** One `findup-walk` line carries the whole shape: `…\proj\node_modules\dep=OK | …\node_modules=OK | …\proj=OK | …\data=OK | …\<proj-root>=ERR:EPERM | C:\Users\runneradmin=ERR:EPERM | C:\Users=ERR:EPERM | C:\=ERR:EPERM`. The traverse skip is exactly what its documentation claims — **intermediate components only**. An ancestor opened as a TARGET is still refused, which is what Node's realpath does.

**Reversal recorded.** Every earlier analysis drew its verdict from `AccessCheck` rows showing `C:\` and `C:\Users` denied, and concluded that unprivileged Windows forces a choice between confined reads and coarse egress denial. The real launch shows that premise false: one token gives both halves. The capability finding those analyses rested on stands; the verdict drawn from it does not.

**Open portability caveat.** Which mechanism performs the traverse skip is **not established** — `windows.rs:412-423` credits `SeChangeNotifyPrivilege` **and** the volume-device flag, and both predict this observable. Only a local `C:` volume was tested. The volume-flag hypothesis predicts traverse **would** be enforced on a device lacking the flag: a network drive, a mounted volume, a filter-driver or redirector device. *(That prediction is INFERRED; no such device was probed.)*

## Withholding the `internetClient` capability — ADOPTED

**What it is.** Coarse egress denial by simply not granting the AppContainer the `internetClient` capability. `windows.rs:24-27`.

**What it buys.** OS-enforced, zero-privilege, all-or-nothing egress deny **including loopback** — the layer-1 security boundary of the network axis.

**Measured** in the same real launch as bypass-traverse, in the same token. At capability count zero: `connect` to a literal IP gives `EACCES 1.1.1.1:443`, DNS gives `ENOTFOUND`, and **loopback gives `ETIMEDOUT 127.0.0.1:135`**. The `plain` arm reaches all three.

**Why this row matters beyond itself.** It is what refuted the earlier claim that the two halves are mutually exclusive.

**It is now the per-package lever, not a blanket one.** When this row was written `build_jail_net()` returned `json!(false)` on Windows unconditionally, making the jail deny-all by construction; `933915dd43` changed it to return `json!(true)` for a name the catalog admits (`compiler/preset.rs:662`), which becomes `net.enforce = false` and so `allow_internet` on the launch. **Measured de-elevated on a real AppContainer** (run 30612421934, both principals): all 8 admitted names connect to `1.1.1.1:443`, the granted arm resolves `github.com` and connects, and an unadmitted name gets `WSAEACCES` with `getaddrinfo` failing as well. The paired controls are an unjailed connect (so a refusal is not a runner with no egress) and a `token` launch per policy (so a refusal is not a jail that cannot start a child).

## The userland preload network gate — ADOPTED as the network tier, and NOT a boundary

**What it is.** A `NODE_OPTIONS=--import data:…` preload (`backend/net_gate_shim.js`, delivered by `compiler/defaults.rs`'s `net_gate_node_options`) that patches `net.Socket.prototype.connect`, `dns`, `dgram` and the `child_process` seams inside the confined Node, keyed on a per-package boolean from package identity.

**What it buys.** The shape the threat actually has. Shai-Hulud grew by publishing a new lifecycle hook into packages that never had one, phoning home with plain `https.get`/`fetch`/`axios` — all of which this denies for any package the catalog does not name, at both Node tiers. Against Nub's 344-package corpus the preload reaches 178 of the 179 packages that contact any host; the exception is a POSIX `.sh` that does not run on Windows at all.

**It is NOT the only enforcement path the `packageNetwork` table has — on Windows the capability is a second and stronger one.** This row previously said it was, and `933915dd43` made that false. Two call sites read the table: `net_gate_node_options` (`compiler/defaults.rs:942`) stamps the shim, and `build_jail_net` (`compiler/preset.rs:662`) decides `net.enforce`, which the Windows backend turns into the `internetClient` capability. So an unadmitted package is refused by the OS here whether or not it is Node — measured with a native probe child carrying no `NODE_OPTIONS` at all, which still got `WSAEACCES` (run 30612421934). The division of labour: the capability is what makes a denial a boundary; the shim denies the same unadmitted package one layer up, inside Node, where the error names the host and the API. **Neither layer narrows a GRANTED package** — the shim's first statement is `if (POLICY.allow === true) return`, so an admitted package is unpatched, and it carries no host list to narrow with (`backend/net_gate_shim.js`: *"There is deliberately NO host filtering"*).

**What that leaves per platform.** The contract is now uniform — no entry means no egress, an entry means coarse egress — spelled per platform because `enforce` carries more than egress on Linux: macOS and Windows get `true`, Linux the catch-all `["*"]` that lifts only `AF_INET`/`AF_INET6` out of its seccomp ceiling (`compiler/preset.rs:602-671`). The jail no longer consults `$downloads` on any platform; the catalog's per-package `hosts` arrays are provenance, never a gate. Only the Windows half is measured by the run cited here. Do not restate the old "Windows-only" reading, and do not restore a host list as a missing feature.

**How a non-Node child is covered, and it is a blackhole rather than a proxy.** The gate re-stamps its own `NODE_OPTIONS` across a spawn so a child `node` stays gated, and sets `http_proxy`/`https_proxy`/`ALL_PROXY` (both cases) to `http://127.0.0.1:1` while deleting `NO_PROXY`, so `curl`, `wget`, `git` and anything built on `proxy-from-env` turn egress into a connection failure. Nothing is routed or filtered — the address is a closed port. **Do not describe this as proxy-mediated egress**; it is a dead end deliberately placed where a proxy-honouring client will look.

**Named residuals, all accepted.** A native addon opening a raw socket bypasses it entirely, and so does any client ignoring proxy env — `curl --noproxy '*'`, a static binary, or Windows PowerShell 5.1, which reads HKCU proxy settings rather than the environment. No corpus package uses PowerShell as a lifecycle entry.

**Verdict.** Adopted, and **must never be described as an OS-enforced guarantee.** Windows is the one platform whose OS lever needs a userland companion at all: Linux carries the per-package grant in its seccomp socket-family ceiling and macOS in `(allow network*)` versus coarse deny, both of which reach a non-Node child that the shim cannot. **Every one of the three is on/off** — the shim is a second expression of the same boolean here, not a finer tier above it.

**A cross-platform claim this makes precise.** Grouping Linux and Windows together as best-effort egress via `HTTP(S)_PROXY` is wrong in both directions, and adding macOS as the well-behaved third is wrong too. Linux stamps **no** proxy env at all (measured). Windows stamps it, but at a closed port, so the effect is a refusal rather than a filter. **macOS stamps none either** — the jail starts no proxy on either arm of the per-package boolean, so no jailed child on any platform is pointed at something that answers. The proxy variables that name a live listener belong to `nub sandbox`.

## A nub-owned staged interpreter copy — ADOPTED

**What it is.** Lifecycle scripts run on a **copy** of the project's own Node, staged under `<cache>/jail-bin/<version>-<arch>`, rather than on the ambient interpreter. `crates/nub-cli/src/pm_engine/jail_bin.rs` keeps every PM decision (which interpreter, the cache key, what counts as a complete previous stage, the env rewrite); `crates/nub-sandbox/src/backend/windows_jail_bin.rs` does the mechanism — grant the empty dir, copy, rename. Called from `build_jail.rs` before anything reads `npm_node_execpath`. Commits `d016eeefc6` + `f72cdec843` (branch `sandbox/win-jail-interp`, folded into `sandbox/integration`).

**Two independent reasons the ambient interpreter is unusable**, which is what makes this necessary rather than an optimisation. Either one alone is sufficient.

- **The read-grant ACE cannot be written where the stock MSI installs.** A leaf read grant is an ACE, and an ACE needs `WRITE_DAC` on the target. Measured de-elevated on a restricted token (`admin-authority=false`, IL 8192, privileges cut to `SeChangeNotifyPrivilege`) against the REAL `C:\Program Files\nodejs\node.exe` with its DACL unmodified — `Users:(RX)`, `Authenticated Users:(RX)`, `Administrators:(F)`, `SYSTEM:(F)`, **no `ALL APPLICATION PACKAGES` entry**, `WRITE_DAC` for Administrators and SYSTEM only — the launch FAILED with `installing read grant ACE on C:/Program Files/nodejs/node.exe failed: Access is denied. (os error 5)`. A nub-owned staged copy in the **same run** LAUNCHED, `code=0`. `C:\hostedtoolcache` behaves the same, and the already-granted-to-AppContainers skip does not rescue either: that skip needs an **inheritable** ACE, which no file object can carry.
- **Widening that DACL would not have made a nested spawn work even where Nub can write it.** `CreateProcessW` opens the image in the **caller's** context, so once the caller is itself inside the AppContainer, opening the ambient `node.exe` by absolute path is a confined open and is refused — measured `Access is denied.` confined against the identical command line succeeding unconfined.

**The constraint that shapes it: the staged copy must be the project's OWN Node version.** `prebuild-install/util.js:18-19` defaults the ABI it fetches for to `process.versions.modules` of the **running** Node (`node_abi` likewise), and `node-gyp-build/node-gyp-build.js:10` does the same — so a version-mismatched interpreter makes that whole family fetch a wrong-ABI prebuild that then fails to load. Enforced **structurally** rather than by convention: the cache key is `<version>-<arch>` derived from the source interpreter, and the tree is a byte copy.

**Cost, measured, and the ORDER is the finding.** Granting the app-package SID on an **empty** directory costs **24 ms**, against **426 ms** re-granting across the populated tree, because children inherit the ACE at creation. So grant, then populate — never the reverse. Payload ~100 MiB / 2,435 entries.

**Do NOT populate by hard link.** Measured: an NTFS hard link shares one MFT record and therefore one security descriptor, so an ACE written on the link lands on the **original** too. Copy.

**A companion finding, and it fails silently.** `PATH` must be **REPLACED**, not prepended. Stock `npm.cmd` carries `IF NOT EXIST "%~dp0\node.exe" SET "NODE_EXE=node"` — a PATH re-search fallback. An un-ACE'd directory left on `PATH` is simply skipped with no error: measured, the MSI dir listed first, and the child's own `process.execPath` was the staged copy.

**Verdict.** Adopted, and it is what makes the Windows build jail start at all for a standard user with an all-users Node.

**What would change the verdict.** Nothing reachable. The first reason needs Windows to let a standard user rewrite `%ProgramFiles%` DACLs; the second is `CreateProcessW`'s documented behaviour, not a gap.

## Writing the ancestor traverse ACE with `SetKernelObjectSecurity` — ADOPTED

**What it is.** The primitive the ancestor repair writes its non-inherited traverse ACE with: `SetKernelObjectSecurity` over a **hand-built** descriptor (`InitializeSecurityDescriptor` + `SetSecurityDescriptorDacl`) rather than `SetEntriesInAclW`'s convenience wrapper. `windows.rs:1355-1460`, commit `c85fcb7a61`. `SetEntriesInAclW` still does the ACL merge — it only assembles an ACL in memory and never propagated anything — and `SE_DACL_AUTO_INHERITED` / `SE_DACL_PROTECTED` are carried across so writing a fresh descriptor cannot clear them on a directory Nub does not own.

**What it replaced, and why BOTH predecessors failed.** `SetNamedSecurityInfoW` re-applies inheritable ACEs to every descendant on any DACL rewrite; on an ancestor like the user profile or a tool cache that is minutes of I/O per launch, and it wedged a 20-minute CI step. The handle-based `SetSecurityInfo` was tried next and **narrowed nothing** — both `Set*SecurityInfo` entry points run advapi32's user-mode propagation pass before returning, and for file objects it still walks existing children. The chain includes `%TEMP%`. `SetKernelObjectSecurity` goes straight to `NtSetSecurityObject`: it writes the object's own descriptor, and there is no propagation pass.

**Measured** against a probe-local copy of the old writer — same trustee, same path, same run, one variable:

| fixture | `SetKernelObjectSecurity` | the writer it replaced |
| --- | --- | --- |
| empty directory | 209 / 205 µs | 370 / 400 µs |
| **4,000-entry tree** | **131 / 130 µs** | **534,378 / 616,863 µs** |
| re-grant, ACE already present | 140 µs | — |
| real `%TEMP%` on the runner | **630 / 654 µs** | **minutes** |

The old writer blows up **~1,444×** with tree size; the new one is flat. The re-grant row is the same fact from another angle — 140 µs on a path already carrying the ACE is a descriptor write, not a walk.

**The product-level differential is stronger than the microbenchmark, and it is what settles it.** Two baseline runs on the old writer, 8.5 h apart, both die at `watchdog-stalled-at=run_jailed`, exit 97, at the **first** repair-on launch. With the fix the watchdog fired **zero** times and every step completed — the `win-jail-repairs-probe` workflow's **first green run in 20 attempts** (17 failures, 3 successes, all three being this change's own commits).

**Effect verified, not just the call returning rc=0.** The ACE lands as `0x001000a1` with flags `0x00`, and the non-inherited scope holds: no inheritance flags, no ACE on the child directory, and a sibling read refused `PermissionDenied raw=5` from inside the jail.

**One adjacent change rides with it.** A leaf grant is **skipped** where the target already publishes an inheritable `ALL APPLICATION PACKAGES` read — `%ProgramFiles%` carries `ReadAndExecute` inheritably on 43 of 44 children — and the teardown list is now driven from `grant_leaf_ace`'s single return value, so a skipped path is never recorded and revoke cannot strip an ACE this launch did not create.

**Supersedes** the "Also unresolved and now moot" note under [writing traverse ACEs](#writing-traverse-aces-on-the-ancestor-chain--dead-privilege), which recorded this primitive as the next move *if the mechanism had survived*. The full-chain goal is still dead on privilege; the **writable** half — `%USERPROFILE%` and below — survived, is what ships, and is what this primitive made affordable.

**What would change the verdict.** Nothing. It is strictly cheaper than both predecessors at identical effect.

## Is the ancestor repair necessary at all — the ACE half is INERT unprivileged; DELETION RECOMMENDED, not taken

**The question, and it was a deletion question.** The ancestor repair is the least elegant mechanism in the Windows backend, and the defect it was built for acquired a second, cheaper fix: [the realpath preload](#nodes-realpath-walk-opens-every-ancestor-as-a-target--open-and-it-is-blocker-1) ships a userland walk that tolerates a refused component when it is a strict ancestor of a granted root — and it covers `C:\`, which no ACE can. So: given the preload, does removing the repair change anything?

**Why nobody had answered it.** Every previous matrix varied the repair with NO preload, or stamped the preload only in the repaired arm. The cell that decides deletion — repair-**OFF** *with* the preload, beside repair-**ON** with the same preload, one fixture, one run — had never been run.

**Measured** — run 30571090527, `win-deelevated-jail-probe`, branch `sandbox/win-ancestor-necessity`. Both principals, the realpath term stamped in every arm that claims to measure the shipping configuration, `NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR` the only variable between the two compared arms. Group `ancestor_necessity` in `tests/windows_deelevated_jail.rs`. Cells: an absolute entry point whose body `require()`s an absolute path; a **bare specifier through a store-cell junction** (nub's default `Isolated` shape); the distribution's own `npm-cli.js` as an absolute entry; and seven non-Node cells — `cmd /c dir /b`, `cmd /c cd`, `where.exe node`, `powershell`, `git --version`, `git rev-parse`, `python -c`.

| | control<br>no preload, no repair | repair ON<br>+ preload | repair OFF<br>+ preload |
| --- | --- | --- | --- |
| **DE-ELEVATED** — IL 8192, no admin authority | | | |
| absolute entry → absolute `require()` | `EPERM lstat 'C:\'` | OK | OK |
| bare specifier through a junction | `EPERM lstat 'C:\'` | OK | OK |
| `npm-cli.js` as an absolute entry | `EPERM lstat 'C:\'` | `-4048` | `-4048` |
| the seven non-Node cells | — | 7 cells | **byte-identical, 7/7** |
| **ELEVATED** — IL 12288, admin authority | | | |
| absolute entry → absolute `require()` | `EPERM lstat 'C:\'` | OK | OK |
| bare specifier through a junction | `EPERM lstat 'C:\'` | OK | OK |
| `npm-cli.js` as an absolute entry | `EPERM lstat 'C:\'` | **OK** (`10.9.8`) | **`-4048`** |
| `cmd /c dir /b` | — | **OK** (listing) | **`Access is denied.`** |
| `where.exe node` | — | **OK** (resolves) | **`Could not find files`** |

**THE VERDICT. De-elevated — the principal the build jail is specified for — the ancestor repair changes NOTHING.** Every Node outcome is identical and the non-Node transcript is byte-identical across all seven cells. Elevated, three cells still depend on it: `where.exe`'s PATH search, `cmd`'s working-directory enumeration, and npm's own deep entry point — all ancestor opens as TARGETS above the profile, which only an elevated token can re-ACE.

**So the mechanism is deletable for every unprivileged user, and its entire remaining value is making an ELEVATED run behave better than an unprivileged one.** That is an argument for removing it, not keeping it: those three operations already fail for every normal user, and the repair's only effect is to hide that on CI. **The deletion is NOT taken here** — whether an elevated run may keep a widening an unprivileged one cannot is a posture call, and taking it would change behaviour other lanes currently observe green. What would go: `ancestor_chain`, `set_ace_on_object`, `TRAVERSE_MASK`, the `AceGuard.objects` teardown revoke, `windows_object_traverse_ace` and its re-exports, the `NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR` seam, and the `ace_cost` / `ancestor_repair` groups in `windows_jail_repairs.rs`.

**WHAT ACTUALLY MADE IT INERT, and it was a defect in the preload, not a property of the jail.** Until run 30571090527 the repair still looked load-bearing de-elevated, on the bare-specifier cell. The cause, once the `data:` frame was stripped from the confined child's stderr: the shim threw `EPERM … lstat 'C:\Users\runneradmin'` from `lstatOrTolerate` — refusing to tolerate a component that IS a strict ancestor of a granted root. **The tolerance rule is a lowercased string prefix test, and Windows hands one process two spellings of the same directory:** `%TEMP%` arrives 8.3-SHORT (`C:\Users\RUNNER~1\…`) while the working directory and a junction's `readlink` target arrive LONG (`C:\Users\runneradmin\…`). Whichever spelling the roots carried, the walk met the other one.

Measured in both directions on one fixture (run 30569197328), which is what made it a finding rather than a guess:

| roots stamped as | absolute entry | bare through junction | `npm-cli.js` | thrown on |
| --- | --- | --- | --- | --- |
| SHORT only (`%TEMP%`-derived) | OK | **REFUSED** | `-4048` | `lstat 'C:\Users\runneradmin'` |
| LONG only (canonicalized) | **REFUSED** | **REFUSED** | **REFUSED** | `lstat 'C:\Users\RUNNER~1'` |
| **BOTH** (the fix) | OK | OK | `-4048` | — |

`realpath_shim_node_options` now stamps every root in both spellings (`with_alternate_spellings`). Adding a spelling cannot widen the jail — the tolerance only ever asserts that a component the OS refused to interrogate is a plain directory, and both spellings name the same directory. The `\\?\` verbatim prefix is a THIRD spelling of this same bug, already special-cased inside the shim (`stripLongPrefix`) after it measured as `native-longpath-granted=ERR`; the fix is applied where roots are chosen so the comparison keeps one rule instead of accreting per-spelling cases. **That fix is sound and it is asymmetric** — it produces a spelling PAIR only when the root arrives short, and it is one entry in a wider survey of what canonical form is available and where it can be computed: see [canonicalizing a path before matching it against the granted roots](#canonicalizing-a-path-before-matching-it-against-the-granted-roots).

**THE CONTROL FOR THAT FIX, gated in the same run.** An arm carrying a SINGLE spelling must still lose cells, or the expansion is not what changed the outcome: `anc-control-single-spelling-roots-still-lose-a-cell` = PASS, `(false, false, false)` against the fixed arm's `(true, true, false)`.

**Gated, so the arms are admissible.** Two rounds of Windows conclusions were retracted for being measured on launches reproducing none of nub's repairs, so every arm reports the shims' own `globalThis` sentinels from inside the confined child: `true` in both preload arms, `false` in the control. The control reproduces the defect verbatim — `EPERM: operation not permitted, lstat 'C:\'` at `realpathSync` ← `toRealPath`. And `--preserve-symlinks-main`, which rides the same term, is ruled out as the thing doing the work: `realpath_unavailable_resolution.rs` already measures it alone leaving every `require()` dying `EPERM`.

**Two near-misses worth recording, because each would have produced a WRONG published answer.**

- **A cell that failed in the CONTROL too.** The first run reported the junction cell failing in all six arm-instances, which reads as a clean deletion verdict. No junction had ever been created: the `mklink` command line went through `Command::arg` — Rust's quoting, applied on the way to a shell that does not parse it that way — and its output was discarded. *Failing in the control is the tell that a cell is measuring its own fixture.* Fixed with `raw_arg` plus printing the result.
- **An unreadable error.** A stack thrown inside a `data:` preload puts ~15 kB of base64 ahead of the message, so the 240-character log excerpt was pure payload and the cause was invisible for two runs. The excerpt now drops `data:` frames before truncating — that one change is what turned "the shim threw" into the spelling defect above.

**Corroborating fact, and a live demonstration of a known-unsound proxy.** `can_write_dacl` reports **0 of 13** sampled ancestors writable in BOTH principals, including `C:\`, `C:\Users` and `%USERPROFILE%`. It is wrong in both — the repair demonstrably lands in each. Same unsoundness above medium IL already recorded under [fail-soft leaf grants](#fail-soft-leaf-grants--adopted), now observed at medium IL too. **Key on the differential, never on the proxy.**

**The reason the elevated column was not empty, now repaired.** `npm-cli.js` failed `-4048` (`UV_EPERM`) de-elevated in every preloaded arm, with an EMPTY stderr. This section previously read that shape as "not a realpath refusal, therefore a missing grant in npm's own startup" — **that reading is REFUTED, and no grant closes it**: it is `lstat 'C:\'` again, on the one resolver the preload did not reach. Cause, evidence and the repair are one section down, under [the realpath shim did not reach Node's ESM resolver](#the-realpath-shim-did-not-reach-nodes-esm-resolver--repaired-at-the-fs-binding): the shim now patches `binding.lstat` as well as the `fs` property, which the destructured copy reads at call time. While it was open it was the clearest instance of the deletion case above, because the defect was real for every unprivileged user and the ancestor repair only hid it on an elevated runner; with the resolver repaired in both principals, that column no longer carries the argument either way.

**The capability half went unconditionally** — see [harvesting the AppSilo capability SID](#harvesting-the-appsilo-capability-sid-that-c-already-carries--dead-mechanism).

**One assumption this group refuted in passing: the 32,767-character `CreateProcessW` environment ceiling does not exist.** The full production `NODE_OPTIONS` stamp measured **56,010 characters** and the whole environment block **56,790** — 1.73× the supposed cap — and it LAUNCHED, with the confined child reporting all three preload sentinels (`realpath`, `stdio`, `net-gate`) true off its own `globalThis`, in both principals, across four runs. The 32,767 figure attaches to `lpCommandLine` and to `SetEnvironmentVariable`'s per-variable maximum, not to the `lpEnvironment` block nub passes through under `CREATE_UNICODE_ENVIRONMENT`. **Honest residual: what that establishes is that 56,790 STARTS, not that no cap exists above it.**

**The stamp has since shrunk by a third, so the ceiling is no longer the binding number.** All three shims are now run through `strip_js_comments` before encoding — whole lines only, prose and blank lines, with the no-multiline-string-literal precondition machine-checked rather than promised — which took the composed stamp from **~55.6k to ~33.8k characters**. Line-leading comments were 55–62% of every shim, which is why the sources stay densely commented while the delivered payload does not carry the prose. The budget test is `stamped_node_options_fits_the_env_block` (36,000, sized just over the Windows composition, where `with_alternate_spellings` can double the roots). It tracks the PAYLOAD, not the ceiling: a budget pinned near 56,790 would leave ~22k of slack and guard nothing. **The cheap reclaim is now spent** — if a future edit trips that budget, the question is what grew, not whether to raise it.


## Fail-soft leaf grants — ADOPTED

**What it is.** A refused **read** grant is skipped rather than fatal: `grant_leaf_ace` returns `io::Result<bool>` and the call site's `.map_err(…)?` became a `match` (`windows.rs:1762-1780`), with the drop-only seam `NUB_SANDBOX_WIN_FAIL_CLOSED_READ_GRANTS` restoring the old behaviour so a probe can measure both directions in one run. **Write** grants stay fatal — every one is Nub's own tmp or the package dir being built. Commit `d016eeefc6`.

**Why it is right rather than a loosening.** It mirrors the contract the ancestor repair two dozen lines below it already carried — *"a refused ACE write is therefore skipped, not fatal"* — and follows the project principle that a residual is acceptable while packages breaking is not: a grant is a REDUCTION from the unconfined lifecycle spawn's complete access, so skipping one leaves the child with **less** reach, never more. A read grant may legitimately name a toolchain the user holds no `WRITE_DAC` on, and aborting every lifecycle script over one unreachable toolchain is the loudest possible failure for the mildest cause.

**Causally measured**, de-elevated, one variable — only the leaf-grant loop's behaviour differs:

| arm | `cmd.exe` cells | rc |
| --- | --- | --- |
| fail-**soft** | **7/9** | 0 |
| fail-**closed** | **0/9** | **−101** |

with the fail-closed arm's launcher naming its own refusal: `installing read grant ACE on C:\Windows\system32\cmd.exe failed: Access is denied. (os error 5)`. Mechanism: `resolve_program` auto-grants the program FILE itself so the LowBox child can exec it, and de-elevated a standard user holds no `WRITE_DAC` in System32.

**A trap this closes, and it is the expensive kind.** An earlier round reported `cmd.exe` as **broken under confinement** on the strength of 19/19 absent cells. That was the fail-closed **launch abort** — the process never started — not a confinement failure. **An empty transcript at ~100 ms alongside `Access is denied.` is a launch that did not happen.** Read the launcher's error before reading the absent cells.

**The corollary that makes the mechanism legible.** busybox survived even fail-closed *because it is nub-owned* and staged into the fixture: `leafgrant-staged-busybox=writable` against `cmd-exe=refused:Some(5)`. A nub-owned program's leaf grant always installs; a System32 binary's never does de-elevated. That is the same fact the [staged interpreter](#a-nub-owned-staged-interpreter-copy--adopted) exploits.

**The hazard it introduces, stated because removing the guard reopens it.** A launch can now succeed while quietly running the **ambient** interpreter. That is why the interpreter probe asserts the child's own `process.execPath`; deleting that assertion as redundant re-opens a silent failure.

**Do NOT overstate the account — it is incomplete, and the gaps are measured.** `pwsh` ran de-elevated (`code=Some(0)`) *despite* an equally-refused grant, and `powershell.exe` started too, where fail-closed should have aborted both. So "fail-closed aborted everything whose grant was refused" is **not** a universal account. Only the `cmd` case is established causally.

**Two harness corrections inside the same round**, recorded so they are not re-derived. Two of the nine cells (`CELL-FOR`, `CELL-SET-AND-EXPAND`) first read as "cmd cannot do this" because the probe over-escaped `%` in a Rust literal (`%%%%V` reaches the `.cmd` as `%%%%V`, not `%%V`) — the probe's own quoting, not cmd's. And an earlier property keyed on `can_write_dacl`, which is an **unsound proxy**: it returns false in the ELEVATED arm on `System32\cmd.exe` while the real grant there plainly succeeds, because an elevated token's DACL-write authority does not come from the file's DACL and an access-checked open cannot see it. Key on the mechanism — launch a trivial `exit /b 0` fail-closed and read the launcher's error — never on the proxy.

**What would change the verdict.** Nothing on the read axis. Restoring fail-closed aborts the jail on any toolchain the user cannot re-ACE, which is the ordinary case for a system-installed one.

## Grant polarity — Windows is additive by construction, and that is now an enforced invariant

**What it is.** `windows::derive_grants` accumulates a read set and a write set with **no ordering between them**, so an fs `Allow` can only ever add reach here. That was already true and is recorded now because it stopped being true everywhere: `emit_fs` on macOS had been mapping `(Allow, Read)` to a `(deny file-write* …)` term, and under Seatbelt's last-match-wins a read grant silently revoked write from everything it enclosed. **Three of the four renderings were additive already — Seatbelt was the outlier** (`f43aab575f`).

**Why it belongs in this ledger rather than only in the macOS one.** `enforce_pure_allowlist`'s invariant binds the **backends**, not only the IR: stripping every deny from the IR buys nothing if a backend synthesizes one back. Windows is the backend where that is structurally impossible — a LowBox token reaches an object only where an ACE names it, so there is no polarity to invert — and preserving that property is what keeps `derive_grants` from acquiring an ordering rule later.

**The cost is shared.** *"Readable but not writable inside a writable grant"* is now inexpressible on every backend, Windows included. Removing access is a `Deny`, which removes read too.

## The store-entry root a native build writes — ADOPTED, and unmeasured here

**What it is.** node-gyp reaches one directory outside the package dir by arithmetic — `build/` absorbs exactly one `..` from `node-addon-api`'s relative `.gyp` path — landing on the package's store-entry root. `store_entry_write_root` grants that root read-write, guarded on the candidate's parent being a virtual store the engine materializes into (`46661af07c`). **The derivation is in the shared compiler, not a backend, so Windows gets the grant too.**

**Reasoned, not measured, on this platform.** The three-package differential that established it (`@vscode/sqlite3`, `cmark-gfm`, `drivelist`, each producing a real `.node` only with the grant) ran on macOS. Nothing here has been run on Windows, and the grant has a Windows-specific cost the other platforms do not pay: it is another inheritable-ACE tree walk per lifecycle spawn, on a directory that is populated rather than empty — the regime where [the per-launch ACE cost](#the-per-launch-ace-cost--adopted-measured) is ~426 ms rather than ~24 ms.

## Bundled busybox as the Windows lifecycle shell — ADOPTED

**What it is.** Dependency lifecycle scripts on Windows run through Nub's bundled `busybox-w32` `sh` instead of `cmd.exe`. Mechanism: a new `EngineContext::default_script_shell` embedder seam, copied into `ScriptSettings::default_shell` by the settings pass and read by the spawn only when the user set no `script-shell` — precedence **user `script-shell` → embedder default → platform default**, so `None` reproduces aube's own behaviour exactly. Nub fills it on Windows with `busybox.exe sh -c`, the **applet-name** form (a multi-call binary dispatches on `argv[0]` or a leading applet name, and `busybox.exe -c` selects no shell), resolved through the same `resolve_bundled_busybox` — byte-identical to what `nub run` on Windows already passes. A missing sidecar warns and leaves `cmd.exe` rather than failing read-only verbs that never spawn a script. Commit `38c169f19a`, branch `busybox-lifecycle-shell`, since **folded into `sandbox/integration`** — `apply_lifecycle_script_shell` (`crates/nub-cli/src/pm_engine/mod.rs`) resolves the bundled busybox and sets `default_script_shell` on the engine context, and the aube side reads it in `commands/script_settings.rs`.

⛔ **IT COLLIDED WITH THE JAIL'S VERBATIM GUARD, AND THE COLLISION WAS INVISIBLE FOR AS LONG AS THE SIDECAR WAS ABSENT.** Measured 2026-08-04 on a real Windows box: with `busybox.exe` staged beside `nub.exe` — the configuration `release.yml` actually ships — **every** dependency lifecycle script failed to spawn with `the build jail could not confine a dependency's install script … a verbatim command line is only accepted for the Windows command interpreter, not …\busybox.exe`.

The cause was a guard reading the wrong field. aube emits a pre-encoded `cmd.exe` command line (`verbatim_tail`) because `cmd.exe` alone does not implement the `CommandLineToArgvW` rules, and the jail fails closed on a verbatim tail whose program is not `cmd.exe` (`crates/nub-sandbox/src/backend/mod.rs`). That guard tested only `script_shell` — the **user** override — while this feature sets `default_shell`, the **embedder** default, so a cmd.exe encoding was produced for a busybox spawn and then correctly refused. Fixed in `1b4a5488c1` by gating on **both** shell fields, so the encoding is emitted only when the `cmd.exe` platform default genuinely applies. The jail's check is deliberately left alone: widening it would hand busybox a line encoded for a different parser, which is the same bug facing the other way.

**Two things kept it hidden, and both are worth remembering.** A bare `cargo build` output has **no `busybox.exe` beside it** — only `nub.exe` and `nub-sandbox-probe.exe` — so CI and every local probe silently took the `cmd.exe` fallback that the encoding *is* correct for, exercising the wrong path entirely. And the `jailed` flag threaded into `verbatim_tail` is aube's **own** jail, which stays false when an embedder confines through the lifecycle spawn hook, so it never suppressed anything here. Consequence for measurement, and it is not small: **the entire Windows build-jail corpus was collected through the `cmd.exe` fallback rather than the busybox shell that ships** — the corpus harness references busybox nowhere.

**Grounds, all measured — and note the first is a plain correctness bug independent of any sandbox.** `cmd.exe` **exits 0 while writing the wrong bytes** for `echo "MARK=${VAR:-default}"`, because it has no POSIX parameter expansion. That is a silently wrong result in the shipped status quo, not a jail concern at all.

| ground | measurement |
| --- | --- |
| no compat cost | **0 of 363** corpus lifecycle script bodies use cmd-only syntax — no `%VAR%`, `%~dp0`, `if exist`, `copy`/`del`, `>nul`, `call`, `set VAR=`, caret escapes, or `.cmd` invocation |
| the corpus already leans POSIX | **144** use forward-slash paths; POSIX `sh` parses **all 363** |
| it FIXES packages | `detox-recorder` and `svf-lib` invoke a shipped `./*.sh` and fail under `cmd` today |
| it is jail-friendlier than what it replaces | confined with **zero** capabilities, busybox's `cd`, glob, redirect, read and its entire spawn battery are byte-identical to unconfined — and it needs **no ancestor repair for itself**, where `cmd.exe` depends on a repair that is best-effort by design |

**One residual to record.** busybox's `/dev/null` redirect is **denied on Windows build 26100** and fine on **26200**, matching a Microsoft note about a feature that shipped between those builds.

**And one retraction from the same round.** `cmd`'s `>NUL` was initially reported denied. That was **withdrawn** — an `%ERRORLEVEL%`-staleness artifact, since a builtin does not reset `%ERRORLEVEL%` on success. Do not cite it.

**A claim elsewhere in this document that this makes stale.** [The userland preload network gate](#the-userland-preload-network-gate--adopted-as-the-network-tier-and-not-a-boundary) says the one corpus package its preload misses is "a POSIX `.sh` that does not run on Windows at all". With busybox as the lifecycle shell, POSIX `.sh` bodies **do** run on Windows — that is one of this change's stated wins. Re-derive that exception before citing it.

**What would change the verdict.** A corpus package that genuinely needs cmd semantics. The seam already takes it: user `script-shell` wins the precedence.

## Dedicated local account plus WFP — ADOPTED for `nub sandbox`, DEAD (privilege) for the build jail

**What it is.** A separate local principal created by a one-time elevated setup, plus persistent WFP filters over a pre-authorized loopback port window. Module doc: `backend/windows_account/mod.rs:1-43`.

**What it buys.** The whole grammar the allowlist cannot express: generous-read-minus-secrets, deny-inside-allow (a separate principal is not an AppContainer, so DACL denies bind), and per-host egress.

**Why it is dead for the build jail.** Account creation needs `NetUserAdd`, which returns `ERROR_ACCESS_DENIED` unelevated (`srt`'s `user.rs:108` records the same). All WFP is admin-gated by BFE (`wfp.rs:92-93`). The build jail's non-negotiable property is zero privilege including no one-time setup. **This exact route was once proposed for the build jail and is on record as an already-burned mistake.**

**What would change the verdict.** Nothing. The privilege requirement is structural, and the build jail cannot pay it.

---

# Making `C:\` and `C:\Users` readable — six attempts, all dead

Every arm before the real launch believed the ancestor chain had to be made openable. It does not (see [bypass-traverse](#bypass-traverse--leaf-only-grants-no-ancestor-ace--adopted)). These are recorded because each was expensive and each keeps looking attractive.

## Writing traverse ACEs on the ancestor chain — DEAD (privilege)

**What it was.** A non-inherited ACE carrying exactly traverse + read-attributes on each granted path's ancestors, written per launch. Still present in code at `windows.rs:1550-1594` (`ancestor_chain`), gated by the drop-only differential seam `NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR`.

**What it would have bought.** An openable ancestor chain, which would have fixed Node's realpath walk directly.

**Measured** by `Get-Acl` read-only across three images including a genuine workstation, 31 s:

| image | edition | build.UBR | type | `lstat C:\` w/o grant | `lstat C:\Users` | std-group `WRITE_DAC` |
| --- | --- | --- | --- | --- | --- | --- |
| `windows-11-arm` | Win 11 Enterprise 25H2 | 26200.8875 | **workstation** | NO | NO | NO |
| `windows-latest` | Server 2025 Datacenter 24H2 | 26100.32995 | server | NO | NO | NO |
| `windows-2022` | Server 2022 Datacenter 21H2 | 20348.5386 | server | NO | NO | NO |

A developer desktop behaves identically to the server images, so this is **not** a CI-harness artifact. `C:\` is owned by `NT SERVICE\TrustedInstaller` and `C:\Users` by `NT AUTHORITY\SYSTEM`, and neither grants `WRITE_DAC` to `BUILTIN\Users`, `Authenticated Users` or `Everyone` — measured `WRITE_DAC`-refused de-elevated, run 30464397422.

**The structurally important part.** The usual escape hatch — "when something breaks, LOOSEN THE GRANT" — does not apply here, because **on Windows granting IS a DACL write**, so the loosening mechanism is itself what is blocked. Windows is the one platform where "just grant more" is not free.

**The writable half DID survive, and it ships.** This section's verdict is about `C:\` and `C:\Users` only. Everywhere the user *does* hold `WRITE_DAC` — `%USERPROFILE%` and below — `ancestor_chain` writes the traverse ACE and that is the whole shipping ancestor repair. What made it affordable is the writer, not the mechanism: see [`SetKernelObjectSecurity`](#writing-the-ancestor-traverse-ace-with-setkernelobjectsecurity--adopted). *(Supersedes this section's former "Also unresolved and now moot" note, which recorded `SetNamedSecurityInfoW`'s subtree re-propagation — a measured ~20-minute stall, unfixed by the handle-based `SetSecurityInfo` — and named `SetKernelObjectSecurity` as the next move "if the mechanism had survived". It survived; the primitive is in.)*

**What would change the verdict.** A Windows release where a standard user holds `WRITE_DAC` on `C:\` — which would be a security regression in Windows, so treat this as permanently closed.

## Harvesting the AppSilo capability SID that `C:\` already carries — DEAD (mechanism)

**What it was.** Rather than write an ACE, **request** the capability Windows already granted on those paths. `C:\` carries `(A;;0x1000a1;;;S-1-15-3-65536-1888954469-…-700089176)` and `C:\Users` carries a different one at `(A;;0x100021;;;S-1-15-3-65536-4045685566-…-191844675)`. Harvest them off the DACLs at launch, in a `harvest_capability_sids` that no longer exists (see the deletion below), and pass them in the capability array.

**Why it looked right.** The SIDs are real, `RtlIsCapabilitySid` returns True for them, they carry the exact traverse+read-attributes mask, they sit on the exact paths the error names, and requesting a raw capability SID is unprivileged — Nub already does it for `internetClient`.

**Measured refusal, and it is surgical rather than a shape check** — `RtlIsCapabilitySid` + `NtCreateLowBoxToken` on both images, harness `tests/win-both-gates/probe.ps1`, workflow `win-both-gates-probe.yml`, run 30504494371. `65536 = 0x10000 = SECURITY_CAPABILITY_APP_SILO_RID`:

| capability SID offered ALONE | `RtlIsCapabilitySid` | `NtCreateLowBoxToken` |
| --- | --- | --- |
| `S-1-15-3-1` (well-known) | True | **ACCEPTED** |
| `S-1-15-3-12` (well-known) | True | **ACCEPTED** |
| `S-1-15-3-1-2-3-4-5` (5 subauthorities) | True | **ACCEPTED** |
| `S-1-15-3-1024-1-2-3-4-5-6-7-8` (hash-named class) | True | **ACCEPTED** |
| `S-1-15-3-65537-1-2-3-4-5-6-7-8` (class **+1**) | True | **ACCEPTED** |
| `S-1-15-3-65536-1-2-3-4-5-6-7-8` (AppSilo, arbitrary) | True | **refused `0xc000000d`** |
| the three REAL harvested AppSilo SIDs | True | **refused `0xc000000d`** |

**`65537` passes and `65536` does not.** This is a deliberate kernel block on the AppSilo class, and it is coherent: that capability is precisely what grants an isolated Win32 app baseline OS read, so letting any process hand it to its own LowBox token would defeat Win32 App Isolation's own filesystem boundary.

**A trap this closes.** `RtlIsCapabilitySid` returning True is **not sufficient**. A documented-looking claim that "the Nt layer accepts any `S-1-15-3-*`" is true of the predicate and false of the syscall.

**A second, independent fact closes it twice over.** `C:\Users`' capability ACE mask is `0x100021` = `FILE_LIST_DIRECTORY | FILE_TRAVERSE | SYNCHRONIZE` — **no `FILE_READ_ATTRIBUTES`** — so even a holdable capability could not satisfy an `lstat` there.

**Cross-checked against Chromium.** `app_container_base.cc:317-348` exposes only `FromNamedCapability` (name → `S-1-15-3-1024-…`), `FromKnownCapability` (well-known small subauthorities), and raw `AddCapabilitySddl`. **It never harvests an existing ACE's SID and requests it** — consistent with the kernel refusal. And `AddCapabilitySddl` is `ConvertStringSidToSid` into this same array, so raw SDDL buys nothing extra.

**THE CODE IS GONE (2026-07-30).** It was retained for a while as live-but-inert: `harvest_capability_sids` ran on every launch, `CreateProcessW` refused, and a fail-soft retry dropped the set and launched again — `capability-fallbacks=1` in **both** principals, so the half was inert regardless of privilege and never once widened a launch. Inert code that runs on every lifecycle spawn is pure cost, and a mechanism that cannot succeed cannot be load-bearing for anything, so it was deleted rather than kept as a hedge: `harvest_capability_sids`, `allow_ace_capability_sids`, `capability_sids_sddl`, the `CAPABILITY_FALLBACKS` counter, the `windows_ancestor_capability_sids` / `windows_capability_fallbacks` diagnostics and their re-exports, the capability-array append, and the second `CreateProcessW` attempt the drop existed for. The launch is now a single call. Commit `40e7e85c30`.

**Two code comments that asserted the opposite were corrected first** (2026-07-30), and are now moot for `windows.rs`: the `2b. THE ANCESTOR CHAIN` comment described the capability half as the working unprivileged answer for `C:\`/`C:\Users`, and `compiler/defaults.rs`'s "WHAT SHIPS INSTEAD" paragraph called the capability "reachable unprivileged". `windows.rs` now states plainly that nothing repairs those two roots. The `windows.rs` half was authored on `sandbox/win-deelev-shell` at `9703ecdf37` and reused verbatim.

**Two MORE describe the deleted half from a distance, and that is the general lesson.** A symbol removal does not touch prose that referenced the mechanism from elsewhere: `realpath_shim_node_options`'s doc (`compiler/defaults.rs`) describes the ancestor repair as making the chain "openable from both ends", and the busybox rationale in `crates/nub-cli/src/pm_engine/mod.rs` credits "the capability SIDs it harvests off each ancestor's DACL". **Neither is true — nothing repairs the chain above the user profile** — so read both sentences against this section rather than trusting them. **Grep for the mechanism's PROSE, not just its symbol, when deleting one:** the compiler finds the symbol and never the sentence.

**What would change the verdict.** A Windows kernel that stops special-casing the AppSilo RID — which would weaken Win32 App Isolation, so do not expect it.

## Holding the well-known capabilities — DEAD (mechanism)

**What it was.** Grant the LowBox token all 12 documented well-known capabilities in case one of them satisfies the ancestor chain.

**Measured** — the `wellknown-caps` arm (12 held, confirmed on the token) is **cell-for-cell identical** to the zero-capability arm on every path, on both images. Exactly as the DACLs predict, since none of them appears on the ancestor chain.

**What would change the verdict.** A future Windows adding a well-known capability to the `C:\` / `C:\Users` DACLs. Detectable by re-reading those descriptors, so re-check the SDDL before re-testing the token.

## Deriving the capability by name — DEAD (mechanism)

**What it was.** If the SID cannot be harvested and offered, derive it from its capability NAME via `DeriveCapabilitySidsFromName` and offer the derived form.

**Measured** — every capability name the machine knows was enumerated from its own registry (`CapabilityClasses\AllCachedCapabilities` = 358 entries, `CapAuthz\ApplicationsEx` = 128 subkeys) plus all 13 documented `isolatedWin32-*` names: **530/530 derived on arm and 456/456 on x64, 1060 distinct SIDs, ZERO match** for any of the three harvested AppSilo SIDs. The `CapabilityClasses` taxonomy has six entries — `capabilityClass_{DevUnlock,DevUnlock_Internal,Enterprise,General,Restricted,Windows}` — and **no AppSilo class**. The three SIDs are byte-identical across both images, so they are image-baked constants, not per-machine.

**Harness note worth not rediscovering.** `DeriveCapabilitySidsFromName` must be resolved from **`kernelbase.dll`**. Binding it to `userenv.dll` (what the docs say) returns false for every name, which reads as "nothing derived" rather than "wrong module" — a first revision reported 0/530 that way.

## Offering the harvested SID as the PACKAGE SID — DEAD (mechanism)

**What it was.** Pass the harvested AppSilo SID as the AppContainer's own package SID rather than as a capability.

**Measured** — refused; `RtlIsPackageSid` is **False** for it. Also refused as capabilities: `S-1-15-2-1`, `S-1-15-2-2`, the user SID, and `BUILTIN\Users` (`RtlIsCapabilitySid` False for each).

## Turning a restricted token into an AppContainer in place — DEAD (mechanism)

**What it was.** Build a restricted token (which reads everywhere), then call `SetTokenInformation(TokenAppContainerSid)` on it to add AppContainer identity — hoping to get the egress lever without the read gate.

**Measured** — **refused, err 87 (`ERROR_INVALID_PARAMETER`)**. Every other construction path is closed too: the `NtCreateLowBoxToken` capability array (above), and `CreateProcessW`'s `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` with a harvested SID.

---

# Restricted-token routes — the mechanism AppContainer arguably should have been, and both variants are dead

## Restricted token plus low integrity level — DEAD (mechanism) as a read jail

**What it was.** Derive a restricted token from Nub's own token via `CreateRestrictedToken`, drop it to low integrity, and launch with `CreateProcessAsUserW`. Reference implementation read: `srt`'s `token.rs`.

**What it would have bought.** Reads with no ACE anywhere — which dissolves the ancestor problem outright, since `realpathSync`, `process.cwd()`, `find-up`/`pkg-dir`/`cosmiconfig` upward walks and `_nodeModulePaths` probing are all reads.

**Measured — the good half, in situ rather than modelled.**, disposable GCE `nub-win-rtok`, Windows Server 2025, harness `tests/win-restricted-token/{Jail.cs,stage.ps1}`. De-elevation measured not asserted: every arm ran as `probe2`, a local account in `Users` only, `elevated=False`, `in_admins=False`, integrity `S-1-16-8192`, and **exactly two privileges** (`SeChangeNotify`, `SeIncreaseWorkingSet`).

- `CreateProcessAsUserW` **is unprivileged** — launched at none/medium/low/untrusted IL. The documented `CreateRestrictedToken` exemption from `SE_ASSIGNPRIMARYTOKEN` holds.
- `CreateProcessWithTokenW` **fails `err=1314 ERROR_PRIVILEGE_NOT_HELD`** at every IL — it needs `SeImpersonatePrivilege`, which a standard user lacks.
- An unprivileged **owner can lower an object's integrity label**: `SetNamedSecurityInfoW(..., LABEL_SECURITY_INFORMATION, ...)` with `S:(ML;OICI;NW;;;LW)` returned rc=0 on the project dir, Nub store and Nub cache. **Negative control: `ERR=5 ACCESS_DENIED` on `C:\` and `C:\Windows`** — owner/DACL-derived, exactly the grant scope wanted.
- The write allowlist works end to end: labeled project dir **WROTE**; `%USERPROFILE%` and `C:\` **Access is denied**.
- **Low IL does not break TLS**, refuting `srt`'s stated reason for choosing Medium (`token.rs:45-47`). On Server 2025 + Node 24 at low IL: `registry.npmjs.org` 200 TLSv1.2, `nodejs.org` 200 TLSv1.3 (327,277 B), `github.com` 200 TLSv1.3 (591,653 B), all byte-for-byte matching baseline. Also fine at low IL: `dns.lookup`, named-pipe round trip, child spawn, `cmd.exe`, `whoami /groups` (LSA), `os.userInfo()`, HKCU reads, HKLM reads.
- **Real packages pass: 10/11 lifecycle scripts rc=0, identical to baseline**, judged by artifact not exit code (`sqlite3` `build\` 1,980,416 B; `keytar.node` 707,584 B; `bufferutil` prebuilds 341,880 B; `esbuild.exe` 11,670,528 B). `cpu-features` fails identically in both arms (no MSVC on that box).
- **From-source MSVC compilation works too** (GCE `nub-win-msvc`, Server 2022, VS Build Tools 17.14 / MSVC 14.44.35207 / SDK 10.0.26100, harness `tests/win-restricted-token/{Jail.cs,gyp.ps1,vm-startup.ps1}` on branch `win-msvc-jail`, log `gyp-findings.txt`). Four packages compiled by real MSBuild with **every `.node` byte-identical to baseline**: `better-sqlite3` 1,891,328 B, `cpu-features` 174,080 B, `keytar` 163,840 B, `gc-stats` 121,856 B, with matching `build\` file and `.obj` counts.

**Why it is dead anyway: reads are UNCONFINED.** The token keeps the user's own SID, so every DACL granting the user still applies. `AccessCheck`, six arms one variable apart, identical on `windows-11-arm` and `windows-latest`:

| token | AppContainer? | read `C:\` | read `C:\Users` | read project | write project |
| --- | --- | --- | --- | --- | --- |
| own token *(baseline control)* | no | GRANTED | GRANTED | GRANTED | GRANTED |
| restricted, medium IL | no | GRANTED | GRANTED | GRANTED | GRANTED |
| restricted, **low IL** | no | **GRANTED** | **GRANTED** | GRANTED | DENIED |

Confirmed in situ: at low IL without a restricting set, `~\.npmrc` and `~\.ssh\id_rsa` read **in full**. That is the one property the whole exercise exists to get, and this route does not deliver it. It is **write-only confinement**.

**Medium IL is not a jail at all**, shown in situ as well as modelled: writing into `%USERPROFILE%` **succeeds** at `il=none` and `il=medium` and only `il=low` denies it. Low IL is what confines; Medium is the user's own access.

**The only low-IL breaks are write-ups — the mechanism working — and each is a grant:**

| broken at low IL | why | remedy (measured) |
| --- | --- | --- |
| `%TEMP%` write (`EPERM`) | `%LOCALAPPDATA%\Temp` is Medium-labeled | label a low temp dir and point `TEMP`/`TMP` at it. Windows' own `…\Temp\Low` convention, **measured ABSENT** on the image, so Nub must create it. |
| `prebuild-install` / `node-pre-gyp` on a **cold** cache | stages the download into `%APPDATA%\npm-cache\_prebuilds`, Medium-labeled | label the PM cache dir the scripts inherit via `npm_config_cache` |
| node-gyp **devdir** on a **cold** header cache | `%LOCALAPPDATA%\node-gyp` is Medium-labeled | label it; grant `%USERPROFILE%\.node-gyp` too for version-independence (the ≤ 8 location measured at **0** entries) |
| HKCU **write** (`ACCESS_DENIED`) | HKCU carries a Medium label | no file-label fix; same API on a registry object. Narrow exposure, and **not needed for the compile path** — all four MSVC packages compile byte-identically with HKCU writes denied |

**One misleading failure mode worth mapping if this ever revives.** Withhold `%TEMP%` and node-gyp does not report a permission error — **it reports that MSVC is not installed** (`gyp ERR! find VS could not use PowerShell to find Visual Studio 2017 or newer` / `Failure details: undefined`). `find-visualstudio` shells to PowerShell `Add-Type`, which compiles a C# assembly into `%TEMP%`. A field report of this reads as a broken toolchain.

**What would change the verdict.** Nothing on the read axis by itself — see the two follow-ons: [the unique restricting SID](#restricted-token-plus-a-unique-restricting-sid--dead-mechanism) (which fixes reads and cannot boot) and [a NO_READ_UP label](#a-no_read_up-mandatory-label-on-the-secret-paths--open) (which fixes reads as a denylist).

## Restricted token plus a unique restricting SID — DEAD (mechanism)

**What it was.** Chromium's mechanism (`restricted_token.h:84-87`): restricting SIDs force the access check to run **twice**, once on your SIDs and once on the restricting set, and access must be granted in **both**. A restricting set containing a SID that appears in no DACL is therefore deny-by-default, with an ACE for that SID re-opening a path.

**Measured — the access model is exactly what was wanted.**, GCE `nub-win-usid`, Windows Server 2022, Node v24.9.0, identity `probe2` (`Users` only, `elevated=False`, exactly two privileges). Harness `tests/win-restricted-token/{Jail.cs,usid.ps1,setup-usid.ps1,ops-usid.js}` on branch `win-unique-restricting-sid`, raw output `usid-log.txt`.

The setup gate passed all three checks with **zero registration**: `AllocateAndInitializeSid` with `SECURITY_NULL_SID_AUTHORITY` + 4 random subauthorities yields a SID the OS maps to no account (`LookupAccountSidW` → `err=1332 ERROR_NONE_MAPPED`) and it is usable anyway; `SetNamedSecurityInfoW` **accepts a DACL ACE naming it, rc=0** (negative control `ERR=5` on `C:\` and `C:\Windows`); and `CreateRestrictedToken(SidsToRestrict=[unique])` is accepted. No registry write, no SAM entry, no capability database, no first-run state.

| path | `il=low`, no restrict *(the low-IL design above)* | `{unique}` | **`{unique, BU}`** |
| --- | --- | --- | --- |
| `C:\` | GRANTED | DENIED | GRANTED |
| `C:\Users` | GRANTED | DENIED | GRANTED |
| **`%USERPROFILE%`** | GRANTED | DENIED | **DENIED** |
| **`~\.ssh`, `~\.ssh\id_rsa`, `~\.npmrc`** | **GRANTED** | DENIED | **DENIED** |
| project dir, ACE'd for `unique` | GRANTED | GRANTED | **GRANTED** |
| sibling under the same parent, not ACE'd | GRANTED | DENIED | **DENIED** |
| `C:\Windows`, `System32`, `ntdll.dll` | GRANTED | DENIED | GRANTED |

**The whole mechanism in one line, from the measured DACLs.** `%USERPROFILE%` is `O:BA G:SY D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;<user-sid>)` — the only non-admin trustee is the user's **own** SID, which the second check does not see unless it is in the restricting set. `C:\`, `C:\Users`, `C:\Windows`, `System32`, `ntdll.dll` and `node.exe` all grant `BU`. **The boundary falls exactly between machine content and this user's private data.** Do not call it "deny-by-default" without that qualifier.

Bypass-traverse covered the ancestors here too, in situ: with an ACE on `…\jail\project` and nothing written on `%USERPROFILE%` or `…\jail` (both DENIED for traverse), a low-IL restricted child read `project\package.json` **and** `project\node_modules\nested-pkg\a\b\c\probe.js`. It also composed with the low-IL write label — **gotcha: the ACE mask must include write (`0x1301bf`)**; with a read-only mask (`0x1200a9`) the write is denied even on a Low-labeled dir, because the label satisfies the mandatory check while the discretionary second check still needs write. A first pass read that as "the two mechanisms do not compose"; they do.

**The blocker: that token cannot start a process tree Nub does not own.**

| restricting set | `cmd.exe` (console only) | `whoami.exe` / `node.exe` |
| --- | --- | --- |
| `{unique}` | `0xC0000022` ACCESS_DENIED | — |
| `{unique, restricted}` or `{unique, world}` | **runs** (builtins fine) | fails |
| `{unique, world, restricted, users}` | **runs**, reads System32 | **`0xC0000142` DLL_INIT_FAILED** |
| `{unique, world, restricted, users, self}` | runs | **runs** — and confines nothing |
| `{self}` alone | `0xC0000022` | — |
| baseline, no restricting set | runs | runs |

**Only the user's own SID unblocks startup, and eight substitutes were bisected — all fail:** `INTERACTIVE`, `AUTHENTICATED USERS`, `THIS ORGANIZATION`, `NTLM AUTHENTICATION`, `LOCAL ACCOUNT`, `LOCAL`, `CONSOLE LOGON`, `PRINCIPAL SELF`, every one `0xC0000142`. HKCU was the prime suspect and **is not it**: `SetNamedSecurityInfoW(SE_REGISTRY_KEY)` on `CURRENT_USER` granting the unique SID `KEY_READ` returned rc=0 and changed nothing. `cmd.exe` survives because it is a pure console app; the failure tracks `user32`-touching images.

**This is Chromium's documented behaviour, not a tuning gap.** `broker_services.cc:296-305` creates the target **suspended** with the lockdown token then calls `SetThreadToken(&temp_thread, tokens.initial_.get())`, commented: *"Change the token of the main thread of the new process for the impersonation token with more rights. **This allows the target to start; otherwise it will crash too early for us to help.**"* The target then calls `TargetServices::LowerToken()` **itself**. Every Chromium sandboxee is Chromium's own binary. **`node.exe` will never call that.**

**The parent-driven equivalent was measured and gets further than expected without arriving.** `SetThreadToken` on the child's main thread **is allowed unprivileged**, and so is `SetThreadToken(thread, NULL)` from the parent afterwards, so the revert is mechanically available without `SeImpersonatePrivilege`. Two findings on top: impersonating a **Medium**-IL token on a **Low**-IL child gives `0xC00000A5 STATUS_BAD_IMPERSONATION_LEVEL` (the initial token must be built at the **same** IL as the lockdown token, which is what Chromium does); and with Chromium's exact shape, node gets **past the loader** into `node::InitializeOncePerProcessInternal` and aborts on **`Assertion failed: ncrypto::CSPRNG(nullptr, 0)`** — OpenSSL RNG seeding fails under the restricted primary token, identically with and without the revert.

**The decisive one, independent of everything above: no exe can be spawned from inside the jail.** Under `{unique, world, restricted, users}`, `cmd /c 'whoami & echo AFTER_WHOAMI'` printed **only** `AFTER_WHOAMI`; the `il=none` baseline printed `nub-win-usid\probe2`. A grandchild inherits the restricted primary token with nobody to impersonate for it, so it dies at DLL init. **npm lifecycle scripts are process trees** (`cmd` → `node` → `node-gyp` → `cl.exe`), so this fails "packages breaking is not acceptable" at the first spawn.

**There is no middle ground on this axis: the one SID that fixes startup is the one SID whose absence was doing the confining.**

**Cost and residue, recorded in case the route ever revives.** Recursive grant over a real 1,737-entry `node_modules` is **1,289 ms** first pass, 1,225 ms steady state (~0.74 ms/entry), but the root ACE is `OICI` so anything created afterwards inherits it — a tree Nub creates itself costs one ACL write. Revoke restores the DACL exactly as found (13 entries / 10 ms). A per-run unique SID would accumulate one dead ACE per run toward the ACL size limit; a fixed nub-specific constant SID is equally safe, since holding it in a token confers nothing a process already running as that user does not have *(INFERRED, not measured)*.

**What would change the verdict.** A parent-side way to let an arbitrary uncooperative image finish DLL init under a restricting set — i.e. Windows shipping the broker step Chromium does in-process. Not a tuning knob.

## Composing LowBox on a restricted base — DEAD (mechanism)

**What it was.** `NtCreateLowBoxToken` takes the base token as a parameter, so build the restricted token first and then LowBox it (or the reverse), hoping the base's user SID rescues reads while AppContainer identity supplies the egress lever.

**Measured — the composition is structurally available and useless.** `AccessCheck`, both construction orders, both images:

| token | AppContainer? | read `C:\` | read `C:\Users` | read project | write project |
| --- | --- | --- | --- | --- | --- |
| LowBox on own base *(gate control)* | yes | DENIED | DENIED | DENIED | DENIED |
| **restricted → LowBox, low IL** | **yes** | **DENIED** | **DENIED** | **DENIED** | **DENIED** |
| **LowBox → restricted, low IL** | **yes** | **DENIED** | **DENIED** | **DENIED** | **DENIED** |

Chromium's `app_container_unittest.cc:231-244` asserts the result keeps the base's user SID while still reporting `IsAppContainer` — both true. The AppContainer **second gate** (the DACL must grant the user AND a package SID or held capability) is not bypassed by the base token's SID. Order does not matter. The `lowbox-gate-is-modelled` control passed, so `AccessCheck` really was applying the gate.

**A reversal inside this row, and it is the one most likely to be repeated.** The restricted token's advantage was originally explained as "it keeps the user's own SID whereas a LowBox token's brand-new profile SID appears in no DACL". **That is wrong** — a LowBox token retains the user SID too, asserted by Chromium's own test and confirmed by the composed arms still being denied. The real difference is the presence of the second gate, full stop. The SID framing misled an earlier round, and summaries elsewhere still reproduce it ("keeps the user SID, so every DACL granting the user applies"). Correct it wherever it survives.

**One further flaw in that modelling, since closed.** Every `AccessCheck` arm above ran with a **zero** capability array, so its DENIEDs could not distinguish "the gate is inherently unpassable" from "an empty array passes nothing". A later positive control — a zero-capability LowBox token **GRANTED** read on `C:\Windows\System32`, which carries `(A;;0x1200a9;;;AC)` + `(A;;0x1200a9;;;S-1-15-2-2)` — shows the gate **is** passable. A table of DENIEDs is a statement about the ancestor chain's DACLs, not about the gate.

## A NO_READ_UP mandatory label on the secret paths — OPEN

**What it is.** Keep the proven low-IL token (which boots everything) and close the read gap by **mandatory policy** instead of a restricting set: write `S:(ML;;NRNWNX;;;ME)` on the secret paths so a low-IL child cannot read up to them.

**Measured working, with all three controls**. Before: the shipped low-IL token read `~\.npmrc` and `~\.ssh\id_rsa` in full. The label write is unprivileged **as the owner**: `S:(ML;;NRNWNX;;;ME)` on `.npmrc` and `S:(ML;OICI;NRNWNX;;;ME)` on `.ssh` → rc=0, read back `type=17 policy=0x7 sid=S-1-16-8192` (`NO_WRITE_UP|NO_READ_UP|NO_EXECUTE_UP` at Medium). **Negative control `ERR=5` on `C:\Windows`.** After, same token and same command: both paths `Access is denied.`, project still readable. **Collateral control: a Medium-IL child still reads them both**, so ordinary applications are unaffected.

**Why it is held back, deliberately.** It is a **DENYLIST rather than deny-by-default**, which the build jail's design forbids — **and it MUTATES THE USER'S REAL FILES**: the label persists if Nub dies mid-run and affects every low-IL process on the machine. Open questions for whoever picks it up: which paths to enumerate, whether persistently relabelling a user's real `~\.ssh` is acceptable, and how to restore on uninstall.

**What would make it the answer.** The AppContainer realpath fix failing. It is the recorded fallback and nothing else, per.

## The `srt` reference implementation — REJECTED (design) as a template

**What it was.** Reading `srt` (`.repos/srt/vendor/srt-win-src`) as a model for the unprivileged build jail.

**What the read established**. It is the **elevated account model**, not an unprivileged jail: it runs the child at Medium integrity on purpose (`token.rs:48`), writes **zero** integrity labels or SACLs (one `SetTokenInformation(IntegrityLevel)` call at `token.rs:182`, no `LABEL_SECURITY_INFORMATION` anywhere), gets its confinement from a separate local account that costs elevation (`user.rs:108`), has all WFP admin-gated (`wfp.rs:92-93`), and has **no capability model at all** (zero hits for AppContainer / LowBox / `internetClient`).

**Two things worth carrying forward anyway.** Its stated reason for Medium IL — Schannel/LSA/registry edge cases at Low IL — **did not reproduce** (measured TLS 1.2 and 1.3, DNS, named pipes and LSA lookups all working at Low IL on Server 2025), so do not cite `srt` as evidence Low IL is unusable. And it hits the same ancestor wall with a different principal: `ci/smoke-exec.ps1:449-457` records its sandbox user unable to enumerate the real user's un-ACE'd profile, with bypass-traverse being what lets it open a deep path anyway — independent confirmation that the wall is "the principal is in no DACL", not "AppContainer is special".

---

# Egress levers other than the capability

## The `\Device\Afd` DACL via `SidsToRestrict` — DEAD (mechanism)

**What it was.** Put a restricting SID in the token so the second access check fails on the AFD device object, denying socket creation while leaving the filesystem alone.

**Why the premise is false.** Socket creation does **not** access-check `\Device\Afd`. `socket()` opens `\Device\Afd\Endpoint`, and absent `FILE_DEVICE_SECURE_OPEN` the device DACL is not consulted; a live socket handle's descriptor is a fresh one from the token's default DACL, not AFD's.

**The descriptors were read anyway, precisely so the separation is not re-mistaken for a lever.** `\Device\Afd` = `O:BAG:SYD:(A;;0x1201bf;;;WD)(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;RC)` — it grants **Everyone** and **RESTRICTED (`S-1-5-12`)**, and **not `BUILTIN\Users`**. So a restricting set of `{BUILTIN\Users, self}` is granted on `C:\` + `C:\Users` + profile while denied on `\Device\Afd` (measured, arm `R2`) — a clean separation **that only matters if the device descriptor were consulted, which it is not**. Also measured: `\Device\{Tcp,Tcp6,Udp,Nsi,RawIp}` are denied to every restricted and LowBox arm.

**Independent corroboration.** Chromium's NULL-restricting-SID `USER_LOCKDOWN` token still has network.

**What would change the verdict.** Windows setting `FILE_DEVICE_SECURE_OPEN` on AFD. Not something to plan around.

## Job objects — DEAD (mechanism) for egress, ADOPTED for reaping and process count

**What it was.** Use a Job Object to deny the confined tree network access.

**Measured** — the only network-bearing info class is `JOBOBJECT_NET_RATE_CONTROL_INFORMATION`: `MaxBandwidth` + `DscpTag`, flags ENABLE / MAX_BANDWIDTH / DSCP_TAG. **Bandwidth shaping and packet tagging; no deny.** (`JobObjectSecurityLimitInformation` exists in the enum but is unsupported post-Vista — *not independently verified here*.)

**What Jobs ARE used for, and it is adopted.** `KILL_ON_JOB_CLOSE` so the whole tree dies when the job handle closes, plus `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` bounding a fork bomb at zero privilege (`windows.rs:120-141` `active_process_cap`). The cap is sized from what a legitimate build needs: node-gyp emits no `-j` so `make` runs serial, and the measured structural ceiling of a parallel native build is `2 * cores + 5` (23 at 8 cores, 69 at 32); `8 * cores` with a 64 floor is ~4× that headroom. Over-cap failure is an observable spawn error in the child (`ERROR_NOT_ENOUGH_QUOTA`, 1816), not a kill of the tree.

**Still open.** Job-object confinement of a **restricted-token** child was never measured. Moot unless a restricted-token route revives.

## Server silos — DEAD (privilege)

**What it was.** Use a silo (the container primitive underneath Windows Containers) to give the confined tree its own network compartment.

**Why it is dead.** Silo creation requires administrative authority. The build jail's zero-privilege requirement disqualifies it outright, and nothing about it is worth a re-measurement.

## Windows Filtering Platform — DEAD (privilege)

**What it was.** Per-host or per-path egress filtering via WFP filters keyed on the confined principal.

**Measured/read** — `FwpmEngineOpen0` and `FwpmFilterCreateEnumHandle0` are **admin-gated by BFE** (`srt`'s `wfp.rs:92-93`); installing **and reading** filters both need admin.

**Verdict.** This is why per-host network is not in the build jail's claim anywhere, on any platform: granular network on Windows definitively requires elevation, and the claim is the intersection. WFP is `nub sandbox`'s, bought with its one-time setup.

## The AppContainer loopback exemption — DEAD (privilege), and doubly wrong

**What it was.** Register a machine-wide loopback exemption for the per-run AppContainer SID via `NetworkIsolationSetAppContainerConfig`, so the child can reach Nub's loopback egress proxy as its sole egress and get per-host filtering through it. Implemented as `WinNetPlan::Tier1` (`windows.rs:363-377`), reachable only when Nub is already elevated.

**Why it is dead for the build jail.** The registration needs admin (`windows.rs:356-362`). And it is wrong on a second axis: **the available exemption exposes every loopback listener, including local forwarders**, so a script could stand up its own forwarder and bypass the hostname gate (`windows.rs:26-27`, `preset.rs:443-447`). Deny-all is the stricter posture, so the Windows divergence loses a capability and never enforcement.

**Stale symbol, now CLOSED.** That doc comment named `WinNetPlan::PerHostUnsupported`, a variant the enum never had — the unelevated arm is `WinNetPlan::FailUnelevated` (`backend/windows.rs:376`). The comment was rewritten when per-host egress was withdrawn and no longer names any `WinNetPlan` variant; `PerHostUnsupported` appears nowhere in the tree.

**What would change the verdict.** An unprivileged, per-SID loopback exemption scoped to one port. Windows offers no such thing today.

---

# Canonicalizing a path before matching it against the granted roots

Windows hands one process several spellings of the same directory, and the realpath preload's tolerance rule decides whether a refused component is a strict ancestor of a granted root by comparing strings. Four spelling defects have now been fixed one at a time — 8.3 short vs long, an `execPath` compared by spelling rather than identity, a `\\?\` prefix special-cased at the comparison, and the trailing-dot form below. This section is the survey that stops the fifth being fixed the same way: what canonical form is available, where it can be computed, and what containment check to run against it.

**The governing fact, and it sets the bar for everything that follows: the tolerance rule is not the security boundary.** It never grants access. It asserts only that a component the OS refused to interrogate is a plain directory, so the userland walk can continue; the open that follows is still checked by the kernel against the LowBox token. A tolerance decision that is wrong in the permissive direction produces a walk that continues to an open that is then denied — never a widened jail. A tolerance decision that is wrong in the restrictive direction produces `EPERM` on a path the jail granted, which is the failure mode every one of the four defects actually took. **So the bar here is compatibility, not soundness**, and a cheap lexical rule is the right instrument. The same reasoning inverts for a deny list, where a missed spelling IS a bypass — Chromium's sandbox builds a long-to-short name map of the loaded modules so a blocklisted DLL that is loaded under its 8.3 alias is still unloaded in the child (`sandbox/policy/win/sandbox_win.cc`, `GetShortNameModules` and `BlocklistAddOneDll`).

## Lexical normalization in the child, filesystem canonicalization in the launcher — ADOPTED as the model

**What it is.** The split BuildXL uses, which is the closest production analogue to this jail: a Detours-based filesystem filter that decides an allow/deny policy for every path operation of every build process on Windows.

Its in-process enforcement path canonicalizes with `GetFullPathNameW` and nothing else (`Public/Src/Sandbox/Windows/DetoursServices/CanonicalizedPath.cpp` in `microsoft/BuildXL`), whose own header states the contract: *"Immutable, typed, and canonical path string. The represented path is absolute, free of .. and . traversals, redundant path separators, etc."* That is a purely lexical transform. It opens no handle, touches no filesystem, expands no 8.3 name, resolves no symlink, and works identically on a path that does not exist. That same call is what `Path.GetFullPath` wraps, and Microsoft's own enumeration of what normalization does — identify the path, apply the current directory, canonicalize separators, evaluate `.` and `..`, trim trailing periods and spaces — contains no step that reads the disk.

Handle-based canonicalization exists in BuildXL but lives entirely on the engine side, out of the hot path, and carries a warning against using it on a whole path: *"We cannot call GetFinalPathNameByHandle on the whole path because that function resolves junctions to their target paths"* (`Public/Src/Utilities/Native/IO/Windows/FileSystem.Win.cs`). Bazel records the cost directly: *"GetFinalPathNameByHandleW is slow so avoid calling it if we can"* (`src/main/cpp/util/file_windows.cc`).

**Why it transfers.** Nub's preload runs inside the jail, where no filesystem canonicalizer is reachable at all (the two sections below). The launcher runs outside it, unconfined, and already builds the root set. Canonicalization belongs there; the child gets a lexical rule.

## The `GetFinalPathNameByHandleW` route inside the jail — DEAD (mechanism)

**What it would buy.** A single canonical form. It is the one Windows API that returns a fully resolved path, and it is what both `fs.realpathSync.native` and Rust's `std::fs::canonicalize` are built on — libuv opens the target with zero desired access and `FILE_FLAG_BACKUP_SEMANTICS`, then calls it with `VOLUME_NAME_DOS` and strips the `\\?\` prefix (`deps/uv/src/win/fs.c`, `fs__realpath_handle`).

**Measured refutation** — run 30460192608, already on record under [redirecting realpath at its native twin](#redirecting-realpath-at-its-native-twin--dead-mechanism): the call is refused under this jail on a file the jail GRANTED and Node reads successfully in the same script. Two further disqualifications hold even where it is not refused. It requires a HANDLE, so it cannot answer for a path that does not exist yet — the common case for a build. And Microsoft documents an ancestor-permission failure of its own over SMB: *"the function splits the path into its components and tries to query for the normalized name of each of those components in turn. If the user lacks access permission to any one of those components, then the function call fails with ERROR_ACCESS_DENIED."*

**What would change the verdict.** A Windows/Node combination where the call succeeds on a leaf handle under an AppContainer. That is the same condition already tracked on the native-twin section, and one probe arm re-tests it.

## The `GetLongPathNameW` route inside the jail — DEAD (mechanism)

**What it would buy.** The narrower fix — expand 8.3 components without resolving symlinks, so a store-cell junction is left intact.

**Why it cannot run inside the jail, from the API contract rather than a measurement.** Microsoft states the requirement up front: *"To use this function, the caller must have the following permissions on the specified path and parent directories: List Folder, Read Data, Read Attributes."* Those are precisely the ancestor permissions the jail withholds, and the same page names the resulting failure: *"It is possible to have access to a file or directory but not have access to some of the parent directories of that file or directory. As a result, GetLongPathName may fail when it is unable to query the parent directory of a path component to determine the long name for that component."* Expanding a short name means reading the parent directory, so the API fails in exactly the situation the preload exists to repair.

**And it fails on a path that does not exist:** *"If the function fails for any other reason, such as if the file does not exist, the return value is zero."* Bazel carries the same limitation as an open TODO on its own wrapper — *"update GetLongPath so it succeeds even if the path does not (fully) exist"* (`src/main/native/windows/file.h`).

**One correctness note worth keeping even though the API is unusable here.** The tilde is a heuristic, not a guarantee: *"do not assume that you can skip calling GetLongPathName if the path does not contain a tilde (~) character."* Any future short-name detector built on `~` is a cheap filter, not a decision.

## Component-wise containment instead of a string prefix test — ADOPTED as the rule to keep

**The question this settles.** Whether the tolerance rule carries the classic sibling-prefix bug, where an allowlisted `C:\foo` also matches `C:\foobar`.

**It does not.** The predicate requires a path boundary after the shared prefix — `isSep(r[c.length]) || isSep(c[c.length - 1])` — and that check is load-bearing rather than decorative. Measured by running the predicate verbatim out of `windows_realpath_shim.js` against an adversarial table:

| roots | candidate | result | |
| --- | --- | --- | --- |
| `C:\foobar\pkg` | `C:\foo` | `false` | sibling prefix, correctly rejected |
| `C:\foo\pkg` | `C:\foo` | `true` | true ancestor |
| `C:\foo\pkg` | `C:\` | `true` | volume root |
| `C:\foo\pkg` | `C:\foo\pkg` | `false` | strict ancestor only |
| `C:\foo\pkg` | `C:\foo\pkg\sub` | `false` | descendant |
| `C:\foo\pkg` | `C:/foo` | `true` | forward slashes |
| `C:\foo\pkg` | `c:\FOO` | `true` | case |
| `C:\foo\pkg` | `C:\foo\.\` | `true` | dot segment and trailing separator |
| `C:\foo\pkg` | `\\?\C:\foo` | `true` | verbatim candidate |
| `\\?\C:\foo\pkg` | `C:\foo` | `true` | verbatim root |
| `\\srv\share\pkg` | `\\srv\share` | `true` | UNC |
| `\\?\UNC\srv\share\pkg` | `\\srv\share` | `true` | verbatim UNC root |

**Prior art nonetheless decomposes into components rather than comparing strings, and the reason is worth carrying.** BuildXL's allowlist is a trie of path components searched one component at a time (`PolicySearch.cpp`), and its subtree test walks both paths element by element, tolerating duplicate separators and either separator flavor (`IsPathWithinTree` in `StringOperations.cpp`). The boundary condition then cannot be got wrong, because there is no boundary to check — the comparison never sees a partial component. The string form here is equivalent given the boundary check, and it is cheaper than splitting on every probe; the recommendation is to keep it, with the boundary check documented as the thing that makes it equivalent so nobody removes it as redundant.

## Stamping every root in both Windows spellings — ADOPTED, and asymmetric

**What it is.** The shipped fix, on record above under [the ancestor-repair verdict](#is-the-ancestor-repair-necessary-at-all--the-ace-half-is-inert-unprivileged-deletion-recommended-not-taken): `with_alternate_spellings` emits each root as built, plus `std::fs::canonicalize` of it where that succeeds.

**It should stay, and it is not sufficient.** Rust's `canonicalize` on Windows is `CreateFileW` with zero access rights and `FILE_FLAG_BACKUP_SEMANTICS`, then `GetFinalPathNameByHandleW(VOLUME_NAME_DOS)` (`library/std/src/sys/pal/windows/fs.rs`). It always returns the LONG form. So a root that arrives SHORT yields the pair `{short, long}` and the walk matches whichever spelling it meets — the measured case, where the project root is `%TEMP%`-derived on a GitHub runner. A root that arrives LONG canonicalizes to itself and yields one spelling, and a walk that meets the SHORT form of that same ancestor still fails. That is not hypothetical: it is the arm gated as `anc-control-single-spelling-roots-still-lose-a-cell`, which must keep failing for the fix's own control to hold.

Three further properties bound what the current call can do. It resolves symlinks and junctions, so a root that IS a junction is replaced by its target rather than paired with it. It requires the root to exist, and silently keeps only the as-built spelling when it does not. And it runs on the launcher's view of the filesystem, which is correct here only because the launcher and the child see the same volume.

## Canonicalizing the roots through nub's existing non-existent-path canonicalizer — RECOMMENDED

**The replacement, and nub already ships it.** The path matcher carries `canonicalize_including_nonexistent` (`crates/nub-sandbox/src/matcher/path.rs`), which resolves the longest existing prefix through the OS — collapsing symlinks, firmlinks and Windows 8.3 names — and then re-applies the remaining components with `.` and `..` collapsed lexically. It is the same walk-up-to-an-existing-prefix technique Bazel uses when short-name expansion fails on a path about to be created: *"walk up in the path until we find a prefix that exists and can be shortened, or is a root directory. Save the non-existent tail in wsuffix, we'll add it back later"* (`src/main/cpp/util/path_windows.cc`).

**The change is to route `with_alternate_spellings` through it instead of bare `canonicalize`, and to keep emitting the as-built spelling beside the canonical one.** That closes the non-existent-root case and makes the canonical member of the pair well-defined for every input, without touching the child's comparison rule. It does not close the remaining direction — a root that arrives long while the walk meets the short spelling of the same ancestor — because the only API that would generate the short member is the one rejected immediately below. That direction is closed by owning the environment instead.

**What is deliberately NOT recommended: a short-form spelling generated with `GetShortPathNameW`.** It fails when 8.3 generation is disabled on the volume, which Bazel documents as *"common in containers"* with the upstream reports to match, so it would produce a root set whose contents depend on volume configuration.

## Owning the child's `TEMP` so the short spelling never enters — OPEN, and it is the root cause

**Where nub's short spelling actually comes from.** The Windows environment floor passes the ambient `TEMP` and `TMP` through verbatim (`OS_ESSENTIAL_ENV` in `crates/nub-sandbox/src/compiler/defaults.rs`, which keeps *"the ambient's actual cased key + real value"*). On a GitHub-hosted Windows runner that value is 8.3-short, so the short spelling enters the confined child through nub's own floor, and any root derived from it is short while the working directory and a junction's `readlink` target are long. It reaches the policy on a second path too: the `$tmp` substitution symbol is `std::env::temp_dir()` (`build_jail.rs`), which reads the same ambient value, so the write anchor carries whatever spelling the environment happened to hold.

**BuildXL closes this at the source rather than reconciling it downstream.** Its build parameters carry `TEMP` and `TMP` on a `DisallowedTempVariables` list annotated *"these environment variables should not be read from config, since they refer to temporary directories that we reserve the right to redirect"*, and the pip environment overrides both to a build-owned `RestrictedTemp` on top of a nine-name inherited allowlist (`Public/Src/Engine/ProcessPipExecutor/PipEnvironment.cs`). A confined process never sees the user's temp directory, in any spelling.

**Why this is the right shape for nub too.** The jail already confines writes to *"a private per-run tmp"* (`build_jail.rs` module doc) and already overwrites `NODE_OPTIONS` unconditionally on the same grounds — that leaving an ambient value in place turns an allowlisted name into an injection channel. Pointing `TEMP` and `TMP` at a nub-owned directory removes the only spelling nub does not choose, which is a smaller and more durable surface than reconciling spellings at the comparison. It is filed OPEN rather than adopted because it changes what lifecycle scripts see, and packages that write to the user's temp directory and expect it to persist would notice.

**What would change the verdict.** Evidence that a real package depends on inheriting the ambient temp directory. None is on record.

## Suppressing short-name discovery inside the child — REJECTED (design)

**What BuildXL does.** It hides short names from the confined process outright. The `FindFirstFile` family is detoured and the alternate name zeroed, with the rationale stated in the source: *"We want to hide short file names, since they are not deterministic, not always present, and we don't canonicalize them for enforcement"* (`DetouredFunctions.cpp`, `ScrubShortFileName`). A dedicated test asserts no surviving path contains a tilde.

**Why it does not transfer.** It needs a Detours-style API interception layer, which nub does not have and will not acquire — the augmenter posture restricts mechanism to Node's own extension surfaces. It also addresses discovery, and nub's short spelling is inherited through the environment before the process starts, so interception would not have caught the measured case.

**The transferable half is the design position, and it is adopted above:** decide one spelling, and stop the others from entering the child, rather than teaching the comparison about each one as it appears.

## Disabling 8.3 generation on the volume — DEAD (privilege)

**What it would buy.** The whole problem, removed. BuildXL's short-name test says so directly in its header: *"These tests should pass trivially if the test volume has short name generation disabled."*

**Why it is disqualified.** The build jail must be totally unprivileged with no setup command. Changing `NtfsDisable8dot3NameCreation` or running `fsutil 8dot3name set` is machine-wide or per-volume administrative configuration, and it is not retroactive — new names stop being generated, existing ones remain, so it does not fix a machine that already has them. Stripping the existing ones is destructive, and Microsoft's own warning is unambiguous: *"Permanently removing 8dot3 file names and not modifying registry keys that point to the 8dot3 file names may lead to unexpected application failures, including the inability to uninstall an application."*

**Worth knowing as an environment variable rather than a lever.** A volume with generation disabled has no short spellings at all, so a probe run there cannot reproduce the class. Any future 8.3 test must assert that the fixture actually has a short name before trusting a green result.

## Cost, and what the prior art caches — the current rule is already cheap enough

**What the rule costs today.** The tolerance predicate runs only on the refusal path, and only for the handful of components above every grant. The walk itself does the ordinary work: one `lstat` per component, memoised through Node's own `realpathCacheKey` cache, which the shim reads back by symbol description precisely to keep that memoisation. The root set is normalized once at install time, not per probe. There is no filesystem access in the comparison and no allocation beyond the normalized candidate.

**So the recommended changes add nothing to the hot path.** Every one of them lands in the launcher, on a root set of three or four entries, once per confined spawn.

**The one caching idea from prior art that nub does not have, recorded in case the shape changes.** BuildXL keeps a `ResolvedPathCache` keyed case-insensitively over normalized paths, caching the reparse-point resolution — the part that costs I/O — and takes the lock with `try_to_lock` so a contended probe degrades to redoing the work rather than blocking: *"Using the cache is best effort, as this is faster than waiting on locks."* Bazel's equivalent is to make normalization a property of the interned path object rather than of each comparison, and to gate the expensive step behind a scan for a tilde and then a cheap 8.3 regex before any filesystem call (`WindowsOsPathPolicy.java`). Neither is worth building for a per-component `lstat` walk over a few ancestors; both are the right answer if the tolerance rule ever moves to a per-operation interception layer.

## Residual spelling divergences — OPEN, and every one fails closed

**Two spellings the rule does not reconcile, and one it deliberately does not chase.** Measured on the predicate as shipped, and recorded so the next one is recognised as a member of this class rather than a new bug.

A trailing dot or trailing space is a fourth spelling. Windows normalization trims both — *"if the path doesn't end in a separator, all trailing periods and spaces (U+0020) are removed"* — while Node's `path.win32.resolve` preserves them, so `C:\foo.` does not match a root of `C:\foo` and the tolerance does not fire. The direction is `EPERM` on a path that should have been tolerated, never a widening. It has not been observed, and the components it could affect are the ones above every grant, where a trailing dot is not a shape that occurs.

Relative segments inside a verbatim path resolve differently in Node than in Windows. Microsoft states that the `\\?\` prefix *"turns off automatic expansion of the path string"* and therefore *"allows the use of `..` and `.` in the path names"*, which makes them literal component names. Node collapses them anyway: `path.win32.resolve` turns `\\?\C:\foo\..\bar` into `\\?\C:\bar`. So a verbatim candidate carrying `..` is compared as a path the OS would not open. The consequence is bounded to the same tolerate-or-throw decision. It is worth naming the general class anyway: where a lexical canonicalizer IS the security check, this divergence is a documented bypass, because the NT object manager treats `..` as an ordinary object name while `GetFullPathNameW` collapses it, so a containment check and the subsequent open can disagree about which file is named. That is not the situation here, and keeping it that way is why the boundary is the token and not the string.

**Case folding is a third candidate and is left alone deliberately.** BuildXL folds to UPPERCASE with a stated reason — *"It converts to uppercase rather than lowercase because it preserves certain characters which cannot be round-trip converted between locales"* — alongside an admission that no user-mode fold reproduces the filesystem's behavior: *"there is no way to accurately model the case insensitive behavior of the file system."* Chromium narrows the problem instead and folds ASCII only. The shim folds to lowercase. Measured against the shipped predicate, the boundary arithmetic survives every length-changing and context-sensitive fold tried, because the same fold applies to both sides of the comparison and the boundary check requires a separator where the candidate ends. No change recommended, and no evidence either fold is wrong here.

---

# Standing defects and blockers

## Node's realpath walk opens every ancestor as a target — OPEN, and it is blocker 1

**The defect.** Node's JS `realpathSync` walks a path component by component and, on Windows, `lstat`s the volume root first. Bypass-traverse exempts intermediate components of a single open; it does **not** make an ancestor openable as a target. So an unflagged confined `node` dies before user code runs.

**Measured** — run 30506129146, all six arms, both images, byte-identical `Error: EPERM … lstat 'C:\'` at `realpathSync` ← `toRealPath` ← `_findPath` ← `resolveMainPath`. Also measured: `fs.realpathSync` on a **GRANTED** deep file is itself `EPERM … lstat 'C:\'` — it is realpath **as a call** that is unavailable, not that file.

**A targeted realpath fix is sufficient**, and that is a measurement rather than a hope: everything else passes in the same child — `stat`, `readdir`, `require`, `chdir`, bare-specifier resolution, deep writes. An earlier objection that "if realpath fails, a bunch of other stuff fails too" is refuted by that table.

**All four escape routes are closed.** See the four sections below. Any fix must come from somewhere other than the preserve-symlinks flags.

### Redirecting realpath at its native twin — DEAD (mechanism)

**What it was.** Preload a shim replacing `fs.realpathSync` with `fs.realpathSync.native`, on the theory that the native implementation opens only the leaf handle.

**Measured refutation** — run 30460192608: `fs.realpathSync.native` is refused under this jail too, with `EPERM … realpath` **on a file the jail GRANTED and Node `readFileSync`s successfully in the same script**. Its single `GetFinalPathNameByHandleW` needs more than the leaf handle the jail allows. Retained as a probe differential arm at `compiler/defaults.rs`'s `windows_native_realpath_shim_node_options`, deliberately kept as the SAME string that was rejected so a future Node granting that call under an AppContainer can be re-tested directly.

**Both realpath implementations are unavailable, which leaves only NOT CALLING one.**

**What would change the verdict.** A Windows/Node combination where `GetFinalPathNameByHandleW` succeeds on a leaf handle under an AppContainer. Re-testable in one probe arm.

### The `--preserve-symlinks-main --preserve-symlinks` pair — DEAD (compat)

**What it was.** Stamp both flags into `NODE_OPTIONS` so module resolution never realpaths: the main flag clears `resolveMainPath`'s call (`internal/modules/run_main.js:38-41`), the non-main flag clears the ones in `_findPath` and the ESM `finalizeResolution` (`loader.js:601,845,858`).

**It works.** Measured on Windows CI as a one-variable differential in the same run: `ac-noflags` produced **0 ops, rc=1**, dying in realpath; the same grants **with** the flags produced **30 ops**. Run 30463527647 separately confirms the entry point runs, dependency `require`s resolve, and a lifecycle script body completes under the real build-jail policy.

**And it is disqualified, because it silently binds the wrong version.** Nub's default node linker is `Isolated` (`aube-linker/src/lib.rs:185`), which materialises each package in its own store cell and wires dependencies as symlinks. With `--preserve-symlinks` a dependency resolves under its **link** path, so the parent-directory walk from `node_modules/<pkg>` skips the store cell holding that package's private dependencies and lands on the project's top-level `node_modules`. Against a fixture mirroring the real `.aube/<dep_path>/node_modules/<name>` layout, a package whose private dependency is `bar@2.0.0` resolved **`bar@1.0.0`** and threw nothing. Standing regression test `crates/nub-sandbox/tests/preserve_symlinks_isolated_layout.rs`; record at `compiler/defaults.rs`'s `windows_realpath_node_options`, retained explicitly as **NOT SHIPPED**.

**A lifecycle script that builds against the wrong dependency version and exits 0 is worse than one that cannot start.**

**Two prior rejections of this flag that predate the jail entirely.** The flag was ruled out for hidden-tree hoisting because it flips Node's resolution semantics tree-wide, and rejected again as a default on 2026-07-05. It also breaks the pnpm/isolated layout outright — see `pnpm/pnpm#244` and `pnpm/pnpm#496`. **Anyone proposing this flag is proposing something already rejected four times.**

**What would change the verdict.** A default node linker with no symlinks — `--node-linker hoisted` makes the flag semantically inert while still removing every `toRealPath` call, and `preserve_symlinks_hoisted_layout.rs` exists for that arm. That is a linker-default change, not a jail change, and it is not on the table for the default install.

### The `--preserve-symlinks-main` flag alone — DEAD (mechanism)

**What it was.** The obvious next move after the pair was disqualified: ship only the main flag, whose wrong-version hazard lives in the **non**-main flag's sites.

**Note first what cannot be used as evidence.** **Every `OK` in the AppContainer operations table above was produced with BOTH flags set.** Run 30506129146 (no flags) produced 0 ops on every arm, so `require-deep-granted = OK` and "bare specifier resolved from inside it = OK" say **nothing whatever** about main-only. The two questions had to be separated.

**The clean half.** The wrong-version hazard **is** attributable to `--preserve-symlinks` alone — measured, `preserve_symlinks_isolated_layout`, 4 arms, one fixture, Node 26.5.0: control `bar@2.0.0` · `--preserve-symlinks-main` **`bar@2.0.0`** · `--preserve-symlinks` `bar@1.0.0` · both `bar@1.0.0`. The prior note that "the pair mis-resolves" was correct but too coarse to act on.

**A second hazard that bounds the claim.** Main-only has its **own** wrong-version hazard when the entry point is reached through a symlink (measured): `node node_modules/foo/build.js` where `node_modules/foo` is a store-cell link → main-only silently binds `bar@1.0.0`. Mechanism, also measured via `__filename`: skipping the main realpath leaves the entry on its **link** path, and the entry's resolved path is the ROOT of the `node_modules` walk, so it moves dependency resolution transitively. **Does not fire under Nub** — `materialized_pkg_dir` (`aube/src/commands/install/bin_linking.rs:24-49`) gives a dep's lifecycle script a store-cell **real** path for `current_dir` (`install/lifecycle.rs:484`) and gives the Windows `.cmd` shims real-path targets (same file:164), so no lifecycle entry arrives through a link. *(INFERRED FROM CODE — not measured against a real install.)*

**The killer, and it is structural: main-only does not fix the jail at all.** `Module._findPath` realpaths **every non-main resolution** unless `--preserve-symlinks` is set — read out of the running binary, not recalled: `if (!isMain) { if (--preserve-symlinks) resolve else toRealPath } else if (--preserve-symlinks-main) resolve else toRealPath`. No cache rescues it: `toRealPath` memoises only what a **successful** walk populated, and here none succeeds. Measured against a process with realpath refused for every path — the jail's own condition, since `realpathSync` was already measured failing on a granted deep file: control dies in `resolveMainPath` with the same stack as run 30506129146; **main-only reaches user code and then every `require()` dies `EPERM`**; both flags resolve fine. Standing regression test `crates/nub-sandbox/tests/realpath_unavailable_resolution.rs` (branch `sandbox/win-preserve-main-only`).

**So the only configuration that WORKS is disqualified, and the only configuration that is not disqualified does not WORK.**

### Repairing the ancestor chain so realpath succeeds — DEAD (privilege) as a FULL fix, and superseded

As a complete answer to this blocker it is closed: the ACE write needs `WRITE_DAC` nobody has on `C:\` or `C:\Users` (see [writing traverse ACEs](#writing-traverse-aces-on-the-ancestor-chain--dead-privilege)), and the capability request is kernel-refused (see [harvesting the AppSilo SID](#harvesting-the-appsilo-capability-sid-that-c-already-carries--dead-mechanism)) and has since been deleted. **What actually closes the realpath walk is the preload**, which tolerates a refused strict ancestor of a granted root and therefore covers `C:\` too — measured, both principals.

**The ACE half is now INERT for an unprivileged principal** — measured, every cell identical with it disabled — and survives only on three ELEVATED-only cells. See [is the ancestor repair necessary at all](#is-the-ancestor-repair-necessary-at-all--the-ace-half-is-inert-unprivileged-deletion-recommended-not-taken) for the differential and the deletion recommendation.

## Piped `child_process` stdio hangs indefinitely — OPEN, and it is blocker 2

**The defect.** `child_process.spawnSync` with piped stdio **never returns** under the AppContainer.

**Independently reproduced, twice.** In run 30506477831 it swallowed every op after it and timed the arm out (`0xffffffff`); moving it after the completion marker recovered the table, and in run 30507134879 it is `MISSING-OP` in every AC arm while `plain` returns normally. It matches `crates/nub-sandbox/tests/windows_realpath_ancestors.rs` — "BLOCKS INDEFINITELY … in libuv's named-pipe setup, before the timer arms" — runs 30460192608 and 30461823852.

**Root cause is the object namespace, not a permission.** Under one policy and one grant set, `\\.\pipe\LOCAL\…` was **created** while `\\.\pipe\…` was **refused** (run 30473523088). Global NPFS is closed to a LowBox token, the AppContainer's private namespace is open, and libuv spells only the former. **No filesystem rule reaches `\Device\NamedPipe`** — a maximally loose policy still hung — so there is nothing to grant. Record at `compiler/defaults.rs`'s `windows_build_jail_node_options`.

**Every npm lifecycle script spawns piped children, and an indefinite hang is a worse failure mode than a refusal.**

**The shipped repair, host-verified and NOT jail-verified.** `backend/windows_stdio_shim.js`, delivered via `NODE_OPTIONS=--import data:…` and gated on Node ≥ 20.6 (an unknown flag there aborts Node at startup, turning a missing repair into a broken install). It patches `ChildProcess.prototype.spawn` — the single seam beneath `spawn`/`execFile`/`exec`, which all call a module-local `spawn` — plus the `*Sync` family. `stdio_shim_semantics.rs` is green **on macOS** across `execFile`, streamed `spawn`, `execFileSync`, `spawnSync`, `execSync`, non-zero exit carrying `.status`/`.stderr`, and a failed spawn reaching both `error` and `close`. **In-jail verification never ran**.

**Documented residuals of the shim:** `fork`/IPC (fail-fast `ERR_NUB_SANDBOX_NO_IPC`, naming the `dependenciesMeta.<pkg>.sandbox:false` opt-out), streaming `spawn` delivering bytes at exit, Node < 20.6, a child conversing over stdin, and `maxBuffer` ceasing to apply.

**One hypothesis refuted along the way, worth not re-forming.** The stall is **not** the `NODE_OPTIONS` payload size — it reproduces with an **11-character** `NODE_OPTIONS`. (The arm that first suggested otherwise reasoned from an `env-block-chars=27428` reading against a 32,767 ceiling that has since been [refuted outright](#is-the-ancestor-repair-necessary-at-all--the-ace-half-is-inert-unprivileged-deletion-recommended-not-taken); the 11-character reproduction is the part that carries.)

**What would close it.** Either libuv learning to spell the AppContainer's private pipe namespace, or the stdio shim verified in-jail.

## The realpath shim did not reach Node's ESM resolver — REPAIRED at the fs binding

**The defect, and it was blocker 3.** Node's ESM resolver binds `realpathSync` by DESTRUCTURING it out of `fs` when `internal/modules/esm/resolve.js` is first required, and that happens before any `--import` preload evaluates. The realpath shim replaces the property `fs.realpathSync`. The CJS resolver reads that property at CALL time (`toRealPath` in `internal/modules/helpers.js`), so it picks the shim up; the ESM resolver holds the ORIGINAL and calls it forever. So every `import()` under the jail dies `EPERM … lstat 'C:\'` — the blocker-1 defect, on the one resolver the blocker-1 repair does not cover.

**Node's own source, per version.** `lib/internal/modules/esm/resolve.js` — `const { realpathSync } = require('fs')` at v18.19.0:27, v20.19.0:27, v22.15.0:28, v22.23.1:28, v23.11.0:28 and v25.9.0:28; `const fs = require('fs')` with `fs.realpathSync(...)` at v24.17.0:28 and on current `main`. **It is destructured across the whole support band except v24 and the current line, and it flip-flopped, so a version check is not a fix.**

**Measured, and the sequence is what proves the mechanism** — run 30574281568, `win-deelevated-jail-probe`, branch `sandbox/win-npm-grant`, Node 22 on `windows-latest`.

| arm | outcome |
| --- | --- |
| `npm-cli.js --version`, de-elevated | `-4048`, **empty stdout and stderr** |
| the same, driving `Npm.load()` from `-e` so the rejection is caught | `LOAD-ERR code=EPERM errno=-4048 syscall=lstat path=C:\` |
| its stack | `realpathSync (node:fs:2749)` ← `finalizeResolution (node:internal/modules/esm/resolve:280)` ← `moduleResolve` ← `defaultResolve` ← `ModuleLoader.import` |
| the same cells, elevated with the ancestor repair on | `10.9.8`, `LOAD-OK` |

The frame is `realpathSync (node:fs:2749)`, NOT the shim's `Object.sync (<preload>)` — which is the direct evidence that the resolver is calling the unshimmed function. In the same run, an in-child refusal tracer recorded the CJS side reaching `lstatOrTolerate` in the preload and TOLERATING `C:\` and `C:\Users`, so both behaviours are observed side by side in one process.

**Why the failure is silent, which is why it read as a missing grant for two rounds.** `Display.load` opens with `await Promise.all([import('chalk'), import('supports-color')])` and only afterwards calls `log.resume()` / `output.flush()`. npm buffers every log and output event until then, and `getExitCodeFromError` (`lib/utils/error-message.js`) takes the process status from `err.errno` — so an fs refusal anywhere before that line exits `-4048` having printed nothing at all. The empty transcript is npm's buffering, not evidence about the cause.

**`--require` WOULD close it, and cannot be used.** Measured on Node 22.23.1 with one fixture and one variable — the same `fs.realpathSync` replacement delivered two ways, then `import('chalk')`: via `--require`, the ESM resolver calls the replacement (5 hits, the whole chalk graph); via `--import`, 0 hits. So `--require` runs before the destructure and `--import` after. But `--require` takes a specifier the CJS resolver must resolve, and `Module._findPath` realpaths any non-main resolution — under this jail that is `lstat 'C:\'` before the shim exists. The `data:` channel the shim ships on exists precisely because `defaultResolve` short-circuits `data:` without touching the filesystem, and `--require` has no such spelling. Chicken-and-egg, not an oversight.

**No grant closes it.** The refused object is `C:\`. Every route to making it openable as a target is already recorded DEAD on privilege or mechanism — see [making `C:\` and `C:\Users` readable](#making-c-and-cusers-readable--six-attempts-all-dead). This is a defect in nub's own repair, not a gap in the grant catalog.

**THE REPAIR, and it is one layer below where the search had been looking.** The premise that closed off every candidate above — "the resolver holds a copy of `realpathSync`, so only a resolution-level seam can reach it" — is true of the `fs` PROPERTY and false one level down. `realpathSync` does not do the walk itself; it calls `binding.lstat` once per path component (`lib/fs.js` ~3273/3315/3357), and it looks that property up on the binding object **at call time**. So patching `process.binding('fs').lstat` is observed by the destructured copy, which never had to be reachable — and by the snapshotted one too, which is why this also covers the v25.7–26.0 band where the resolver sits in the V8 startup snapshot and no preload of any kind reaches it. `module.registerHooks()` is not needed, so neither its v22.15 floor nor its v24 re-entrancy is a constraint any more.

The shim now installs both layers (`windows_realpath_shim.js`): the `fs.realpath*` property replacements, unchanged, plus a `binding.lstat` wrapper carrying the same ancestor-tolerance rule and a `binding.realpath` wrapper delegating to the same walk. It is an ADDITION, not a swap — `fs.realpathSync.native` and `fs.promises.realpath` reach `binding.realpath`, a different native function the `lstat` patch does not touch, and the property replacements are what serve them.

**Two scoping facts the repair turns on**, both of which a careless implementation gets wrong silently:

- The walk calls `binding.lstat(base, **true**, …)` once per component **and** `binding.lstat(root, **false**, …)` for the volume-root probe. That second call is `lstat 'C:\'` — the defect's primary spelling. Scoping the tolerance to the bigint form alone therefore leaves the headline case refused, and does so invisibly on any non-Windows simulation, because the root probe is guarded by `if (isWindows)`. The tolerance is keyed on both shapes and on the third argument being absent (an `FSReqCallback` means the async form, `kUsePromises` the promise form; a realpath walk is only ever the sync one), so an ordinary `fs.lstatSync` of the same refused path still throws.
- v18 reports through a ctx object carrying a libuv `errno` and no `code`; v20+ throws and passes `throwIfNoEntry`, a boolean, in that same argument position. Handling one convention and not the other leaves half the support band unrepaired with nothing in the output to say so.

**Measured layer by layer, against a fs-layer-only stamp rather than against nothing** — because on v24.17+ and v26.1+ the property replacement resolves ES modules on its own (nodejs/node#62835 restored the namespace read there), so an arm run only on a modern interpreter cannot tell the two layers apart and will go green with the binding seam deleted.

| interpreter | property replacement alone | with the binding seam |
| --- | --- | --- |
| 18.19, 20.19, 22.15, 22.23, 24.14, 25.9, 26.0 | bare `import` dies `EPERM` | resolves, private version bound |
| 24.17, 26.1, 26.5 | resolves | resolves |

**Confirmed in the real jail**, which is the measurement none of the above substitutes for — run 30598802736, `win-deelevated-jail-probe`, de-elevated principal, staged interpreter v22.23.1. All eleven cells pass, and every control fires in the same arm: unrepaired, the ESM entry is dead AND the CJS `require` is dead AND `fs.realpathSync.native` / `fs.promises.realpath` both throw `EPERM`; under the stamp all four answer, and `--preserve-symlinks` still binds the unrelated top-level `bar@1.0.0` so the fixture is shown able to produce the wrong answer it exists to detect. The unrepaired-CJS control also settles a question a simulation cannot: a `--require`-delivered simulation warms Node's CJS `realpathCache` before its own patch installs, so a simulated CJS cell passes with no repair at all and proves nothing — in the real jail it dies, so the `fs`-layer replacement is what repairs CommonJS.

So the seam is what carries the whole 18.19–24.16 / 25.x / 26.0 band, **including the v22 LTS line, to which the upstream fix was never backported**. Independently of version it also carries any caller holding a binding captured before the preload ran: a builtin's ESM named exports are snapshotted when its facade is first instantiated, so `import { realpathSync } from "node:fs"` is the pre-patch function and reaches `binding.lstat` directly — measured refused without the seam and working with it on 20.19, 22.15 and 26.5, as is `realpathSync.native` off that same snapshot, which reaches `binding.realpath`.

**Consequence for the ancestor repair.** The elevated column in [is the ancestor repair necessary at all](#is-the-ancestor-repair-necessary-at-all--the-ace-half-is-inert-unprivileged-deletion-recommended-not-taken) was entirely this defect: elevated, the traverse ACE makes `lstat 'C:\'` succeed, so the ESM resolver never notices it is unshimmed. With the resolver repaired for the unprivileged case as well, the two columns are expected to agree, and the ACE half no longer has this defect to mask. The deletion question is decided on that run's own arms, not here.

**What is not fixed by it.** An entry point reached THROUGH a symlink still binds under the link path, because `--preserve-symlinks-main` runs before any `--import` preload and the binding seam cannot get in front of it. That is unchanged from what ships today, measured identical with the flag alone and with the previous stamp on 20.19, 22.15 and 26.5, and it is tracked separately. A dependency that registers its OWN `module.register` loader also still fails: that loader runs on a real worker thread with its own realm, which no `--import` preload reaches.

**What would change the verdict.** `process.binding` is deprecated (DEP0111); its removal would take the seam with it. The shim guards the call and degrades to the property-only repair rather than aborting the child, so the failure mode would be the loud EPERM that ships today, not a silent wrong resolution. `node:vfs` (nodejs/node#61478) is the sanctioned successor surface if it lands.

## A per-file deny ACE is inert against its own child — DEAD (mechanism), and it is a design constraint

**What it was.** The original denylist design: grant broadly, then deny specific secrets with an explicit deny ACE.

**Measured 9/9 cells including LPAC.** The deny sat **first** in the ACL and covered `FILE_READ_DATA` (`0x001200a9`), and **the child read the file anyway**. The clean cells carry no `ALL APPLICATION PACKAGES` ACE at all and the deny is defeated there too, so an explicit deny naming the per-run AppContainer SID is **simply inert against that AppContainer's own child** — AAP or no AAP, LPAC or no LPAC. Cause: access is checked at **handle-open** and the granted mask is cached in the handle; later operations never re-consult the DACL.;. Recorded in `crates/nub-sandbox/tests/windows_residuals.rs`, where the question was originally posed.

**A reversal worth naming.** The backend's allowlist-only rationale previously rested on an assertion whose own code comment said it "may be backwards", and the result sat recorded nowhere. It is now measured, and the comment's suspicion was indeed backwards — the deny is weaker than feared, not stronger.

**⇒ A per-file denylist is not expressible on Windows.** Note the symmetry with Landlock: **neither unprivileged platform can express deny-inside-allow**; bwrap's mount-masking is the outlier, not the norm.

**Consequence the build jail must respect.** `deny_shadows_grant` **fail-closes any policy carrying a deny whose `literal_prefix` is `""`** — `**/.env*` normalises to `""`, and **six** floor globs trip it. This is why the build jail must emit **zero** denies: putting one back into the shared IR immediately re-breaks Windows.

## The `ALL APPLICATION PACKAGES` clean-root precondition — ADOPTED after correction

**What it was.** `verify_clean_root` (`windows.rs:956`) walked the working directory's ancestors and refused the launch if any of them granted `ALL APPLICATION PACKAGES`, on the theory that an inherited AAP grant would widen the allow-set past the per-run SID.

**It blocked the Windows build jail entirely.** Measured on `windows-latest`: `build-jail could not be applied (fail-closed): … \\?\D:\ grants ALL APPLICATION PACKAGES access`. Not drive-specific (same on `C:\nubfx`), byte-identical at the parent commit, and `--sandbox-admin setup` did not rescue it because the check ran before the account route was selected.

**Measured per-ancestor on `windows-latest`**:

| path | AAP rights | AAP **inheritable** | `SE_DACL_PROTECTED` |
| --- | --- | --- | --- |
| `C:\` | `0x00000000` | – | no |
| `D:\` (work volume) | `0x001200a9` | **no** | no |
| `C:\Users` | `0x001200a9` | **no** | yes |
| `C:\Users\<name>` | `0` | – | **yes** |
| `D:\a\nub\nub\…` | `0` | – | no |

Two facts: drive roots are **not uniform** (`C:\` grants AAP nothing, `D:\` and `C:\Users` do), and **every AAP ACE found is non-inheritable**, so it governs one directory object and can never reach the tree the child runs in. The check was rejecting on grants that were not a hazard. Fixed on `fix/windows-clean-root`, folded to `456117767b`; the account route is now exempt because a separate local principal is never an AppContainer (`windows.rs:478-491`).

**Unresolved contradiction in the record.** A later descriptor read found **no AAP or ARAP ACE on `C:\` or `C:\Users` on any of three images** — only the AppSilo capability SID — which contradicts the table above reporting `C:\Users` AAP rights `0x001200a9`, and a third reading that `C:\Users` grants AAP **nothing** on Server 2022. **These do not reconcile as stated.** Most likely image-dependence plus one survey misreading a capability SID as AAP, which one of them did in its own first revision. Treat any AAP claim as image-specific and re-read the descriptor.

## The AppContainer `%LOCALAPPDATA%` is destroyed per launch — ADOPTED cost, and a reversed claim

**The reversal, recorded because it was first reported wrong.** It was reported as a known structural weakness making Windows protection "materially thinner". **That is false.** Windows **redirects** the known folder for a LowBox token: the child resolves `…\AppData\Local\Packages\nub_sbx_<pid>_<nonce>_<ctr>\AC`, never the real one. Against the real `%LOCALAPPDATA%` every jailed probe was DENIED with unjailed controls succeeding — a depth-2 walk yielded **42 MB / 53 dirs / 42 files unjailed** (including a DPAPI credentials blob and a GitHub account store) **vs 0 bytes jailed**. Reproduced on two branches.

**The real gap is persistence, not reachability.** `ProfileGuard::drop` calls `DeleteAppContainerProfile`, so jailed `%LOCALAPPDATA%` is fresh, empty and destroyed **per LAUNCH** — two container names were observed within one probe run, so not even per-package. Windows tooling that caches there gets **zero reuse, ever**. The `USERPROFILE` jail-home *is* persistent. **This is a COMPAT cost, not a security hole.**

**The code comment that asserted otherwise has been corrected** (`compiler/preset.rs:~556-566`): it now records that `LOCALAPPDATA` is deliberately NOT redirected because the LowBox launch resolves its profile directory from it, that the OS redirects the known folder so the real one is denied outright, and that the resulting gap is zero reuse rather than a reachability hole — with the `USERPROFILE` jail-home named as the axis that does persist.

## Profile registration is required per launch — ADOPTED, with the caveat stated

**What was hoped.** That `DeriveAppContainerSidFromAppContainerName` — a pure hash of the name that writes nothing — would let a launch skip profile registration entirely, making the zero-setup claim absolute.

**Measured**. `DeriveAppContainerSidFromAppContainerName` returns **the same SID** as `CreateAppContainerProfile` (`derive-equals-create = True`) and writes nothing: no HKCU mapping, no `%LOCALAPPDATA%\Packages` dir. `CreateAppContainerProfile` adds **exactly one** subkey under `HKCU\…\CurrentVersion\AppContainer\Mappings` (46→47) plus `%LOCALAPPDATA%\Packages\<name>`, and **`DeleteAppContainerProfile` removes both** (back to 46, dir gone) — measured, not inferred. ACE residue after all eight arms: **0**; `PurgeAccessRules` by trustee leaves nothing naming a dead per-run SID.

**But a profile-less launch does not work.** The `ac-derive-only` arm — hash-derived SID, no profile, ACEs written and confirmed on the deep file — failed `CreateProcessW **err=2**` (`ERROR_FILE_NOT_FOUND`) on both images, while every profile-registered arm launched from that identical path in the same run. One variable. **Registration is required per launch; the state is transient and self-cleaning, not absent.**

**And it needs no administrative authority** — a separate de-elevated run confirms it (run 30424798156, branch `sandbox/win-deelevated-jail`). Paired one-variable differential on `windows-latest`, `EnableLUA=1`: arm A (`elevated=1 admin=1 il=12288`) 8/8 PASS; arm B (`route=restricted-token+medium-il`, `elevated=1 **admin=0** il=8192`) **8/8 PASS**, including `profile-create-and-launch=PASS (child exit 0; IsAppContainer=1)`, `acl-grant-allow=PASS`, `acl-grant-deny=PASS`, and `teardown=PASS (profiles 0→1→0, aces Some(3)→Some(7)→Some(3))`. Concluding line: `WINDOWS BUILD JAIL HOLDS WITH NO ELEVATION`.

**Methodology note worth keeping.** The gate is an ACCESS CHECK (`admin=`), **not** `TokenIsElevated` — `CreateRestrictedToken` *copies* that flag, so a de-elevated token still reports `elevated=1` while holding no admin authority (measured, run 30423750288). **Never gate on the flag.**

**Residuals so this is not over-read.** The de-elevated arm used a **synthesized** principal (`CreateRestrictedToken`, `Administrators` deny-only, `DISABLE_MAX_PRIVILEGE` keeping only `SeChangeNotifyPrivilege`, dropped to medium IL) — **not a separate standard-user account**; `windows-latest`'s `runneradmin` is `TokenElevationTypeDefault`, so `TokenLinkedToken` fails with 1312 and there is no standard-user half to borrow (run 30424255514). It strips admin authority and IL but not the user SID, so HKCU and `%LOCALAPPDATA%` are still an admin's profile; closing that needs a genuine account (`tests/win-restricted-token/gce-startup.ps1`'s `probe2`). `DeriveAppContainerSidFromAppContainerName` writing nothing was measured **elevated only**. And a UAC prompt cannot manifest on CI at all, so "no prompt" is **INFERRED** from "no elevation required".

## The per-launch ACE cost — ADOPTED, measured

**Measured** on a 4,000-entry / 3,880-file fixture:

| operation | windows-latest | windows-11-arm |
| --- | --- | --- |
| inheritable `Modify` grant, whole 4,000-entry tree | 617–1,102 ms | 452–453 ms |
| revoke (purge by trustee), same tree | 754–844 ms | 395–468 ms |
| grant on a single 97-entry leaf dir | 20–24 ms | 13–29 ms |
| revoke, same leaf | 20–102 ms | 12–17 ms |

The shipping backend writes **leaf** grants, so the per-launch figure is the ~20 ms row. Inheritance (`OICI`) means new files pick the ACE up **at creation**, so the recursive pass only covers pre-existing content — **scope grants to the package/project dir**. A 50k-entry monorepo root would cost roughly 45 s per launch, applying the per-entry figure below.

**A cost figure that was corrected.** An earlier reading took 11 entries / 77 ms as ~7 ms/entry; that was fixed cost. The real per-entry rate on a real post-build MSBuild tree is **~0.8–0.9 ms**, linear, `errors=0`, stable across two passes: 124/122 ms, 283/348, 464/455, 539/424, 624/553, **1,451/1,293**. And the jail adds **no measurable overhead to the compile itself** — 27.1 s vs 24.4 s (`better-sqlite3`), 16.5 vs 17.4, 13.1 vs 12.7, 9.5 vs 9.3 on a contended box. The jail's per-launch cost is the labeling/ACE pass, not the build.

## Environment keys containing `=` fail the launch closed — OPEN

**The defect.** `backend/mod.rs:1132` rejects any env key containing `=`, but **cmd.exe injects hidden `=C:` / `=ExitCode` variables**, so `apply()` fail-closes. All three of Nub's own `windows_enforcement` failures on `windows-latest` are this, on `env.enforce == false` control arms. **The build jail is unaffected** — its env is allowlist-scrubbed. Pre-existing, not from the sandbox branches.

## A space-bearing Windows profile path cannot prefetch — OPEN

**The defect.** `Url::from_directory_path` percent-encodes a space and `build_prefetch.rs:777` declines on any `%`, so a profile at `C:\Users\First Last\` cannot prefetch — and `build_jail_net()` returns deny-all on Windows, so such a profile can neither prefetch **nor** fetch. Space-bearing Windows profile paths are common. **Not measured end to end** (no such profile on either host).

## Output redaction is not wired on the confined path

The confined launch owns spawn+wait internally and cannot hand back piped stdio, so secret redaction does not apply there. Interacts with the stdio shim work above.

## Session 0 cannot launch an AppContainer

**Not a defect — a measurement-environment fact that shapes every Windows probe.** OpenSSH puts you in the `services` session 0, which has no window station a LowBox token can attach to, so **every launch returns `0xC0000142 STATUS_DLL_INIT_FAILED`**. Proven environmental by a sharp control: Nub's own CI-proven `windows_enforcement` harness fails identically there (27/35, every one `-1073741502`) versus 32/35 for the same binary on `windows-latest`. And now measured rather than inferred: `session-id = 2` on both GitHub runners, which is why CI works.

**⇒ Windows sandbox work goes through `ci-adhoc-test` (branch-scoped, no PR), not the standing VM.** The restricted-token routes were measurable over plain SSH precisely because they are **not** AppContainers, which is the one operational advantage they had.

---

## Contradictions in the record, unresolved

Surfacing these is part of this document's job.

1. **AAP on `C:\Users` — three incompatible readings.** One descriptor survey found no `ALL APPLICATION PACKAGES` or `ALL RESTRICTED APPLICATION PACKAGES` ACE on any of three images, only the AppSilo capability SID. Another reports `0x001200a9`, non-inheritable. A third reports nothing at all on Server 2022. These cannot all be true as stated, and the first survey's own first revision misread a capability SID as AAP — an easy mistake. Treat any AAP claim as image-specific and re-read the descriptor.
2. ~~**A stale symbol.** The `build_jail_net` doc comment in `preset.rs` names `WinNetPlan::PerHostUnsupported`; the enum has `FailUnelevated`.~~ **CLOSED** — that comment was rewritten when the net axis became a per-package boolean, and `PerHostUnsupported` no longer appears anywhere in the tree.

## Changelog

- 2026-07-31 — Scrubbed the cross-platform half of the net-gate rows, which still described macOS as pinning jailed egress to a proxy — it starts none on either arm, so no jailed child on any platform is pointed at a listener that answers. Corrected the division of labour between the capability and the shim: the shim returns early for an ADMITTED package (`POLICY.allow === true`) and carries no host list, so neither layer narrows a granted package. Closed the stale-symbol note, since `PerHostUnsupported` is gone from the tree and the comment that named it was rewritten. Refreshed four `preset.rs` line references.
- 2026-07-31 — Recorded the Windows halves of two cross-cutting compiler changes: `derive_grants` is additive by construction, which is now an enforced invariant rather than an incidental property (Seatbelt was the one backend synthesizing a deny from an `Allow`), and the store-entry-root grant a confined native build needs is derived in the shared compiler, so Windows gets it unmeasured and pays a populated-tree ACE walk for it. Closed the stale-`WinNetPlan` contradiction. The egress rows are unchanged — the capability-as-per-package-lever correction landed separately and already reads correctly.
- 2026-07-31 — Blocker 3 is REPAIRED, one layer below where every earlier candidate looked. `realpathSync` does not walk the path itself; it calls `binding.lstat` per component and looks that property up on the binding object at CALL time, so patching `process.binding('fs').lstat` reaches the copy the ESM resolver destructured — and the copy baked into the V8 startup snapshot on v25.7-26.0, which no preload reaches at all. `module.registerHooks()` is therefore not needed and its v22.15 floor and v24 re-entrancy stop being constraints. The seam is an ADDITION to the `fs.realpath*` replacements, not a swap: `fs.realpathSync.native` and `fs.promises.realpath` reach `binding.realpath`, a different function. Measured against a property-replacement-only stamp on 13 interpreters — the seam is what carries 18.19-24.16 / 25.x / 26.0, including the v22 LTS line the upstream fix (nodejs/node#62835) was never backported to. Two scoping facts are load-bearing and recorded in the section: the volume-root probe calls `binding.lstat` with bigint FALSE, so scoping on the bigint form alone silently leaves `lstat 'C:\'` refused; and v18 reports through a ctx object where v20+ throws.
- 2026-07-30 — Reconciled against the tree. Recorded that all three stamped shims now go through a whole-line comment stripper, taking the composed `NODE_OPTIONS` from ~55.6k to ~33.8k characters, and that the budget test (`stamped_node_options_fits_the_env_block`, 36,000) tracks the payload rather than the refuted 32,767 ceiling. Made the net gate's reach precise: it is the ONLY consumer of the catalog's `packageNetwork` table on any platform, and its proxy variables point at a closed port rather than a filtering proxy. Busybox as the lifecycle shell is folded into `sandbox/integration`, and the `preset.rs` `%LOCALAPPDATA%` comment has been corrected; two further comments that outlived the capability-SID deletion are recorded as still describing it.
- 2026-07-30 — **REVERSAL:** npm's `-4048` was recorded as a missing grant in npm's own startup; it is not, and no grant closes it. It is `EPERM lstat 'C:\'` raised by Node's ESM resolver, which destructures `realpathSync` out of `fs` before any `--import` preload evaluates and therefore never sees the realpath shim — new section, [blocker 3](#the-realpath-shim-did-not-reach-nodes-esm-resolver--repaired-at-the-fs-binding), with the per-version source survey, the `--require`-beats-`--import` differential, and why npm reports nothing. The probe grew two durable diagnostics that produced it: an in-child refusal tracer and a bisect that drives `Npm.load()` directly so the rejection is caught rather than buffered. The ancestor repair's ACE half is therefore NOT deleted — elevated it still resolves this cell, which is the mask the deletion argument is about.
- 2026-07-30 — Added the path-canonicalization survey, after four spelling defects had been fixed one at a time. Establishes that no filesystem canonicalizer is reachable inside the jail — `GetFinalPathNameByHandleW` is measured refused, and `GetLongPathNameW` requires the ancestor permissions the jail withholds and fails on a path that does not exist — so canonicalization belongs in the launcher and the child keeps a lexical rule, which is the split BuildXL's Detours sandbox uses. Records that the tolerance rule carries NO sibling-prefix boundary bug (measured against the shipped predicate), that the both-spellings fix is asymmetric and should route through the non-existent-path canonicalizer nub already ships, and that the root cause of the 8.3 case is the environment floor passing the ambient temp directory through verbatim.
- 2026-07-30 — Ran the arm every previous matrix left out: repair-OFF **with** the realpath preload, beside repair-ON, both principals. Deleted the ancestor repair's capability half (kernel-refused, never once widened a launch). Found and fixed the reason the ACE half still looked load-bearing — the preload's roots and the walked components arrive in different Windows spellings (8.3 short vs long), so its tolerance rule silently never fired; roots are now stamped in both. With that fixed the ACE half is INERT de-elevated and deletion is recommended but not taken. Also refuted the 32,767 `CreateProcessW` environment-block ceiling: a 56,790-character block launches with every preload active.
- 2026-07-30 — Moved into tracked `research/design/` so code comments can link here, and scrubbed of pointers into untracked documents. Recorded four newly settled approaches, all ADOPTED: the nub-owned staged interpreter copy, `SetKernelObjectSecurity` as the ancestor-ACE writer, fail-soft leaf grants, and bundled busybox as the Windows lifecycle shell. Corrected the capability-SID comments in `backend/windows.rs` and `compiler/defaults.rs`, which described the AppSilo capability as reachable unprivileged; both now state the measured kernel refusal.
- 2026-07-29 — Initial consolidation.
