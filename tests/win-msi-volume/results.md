# Results

Append-only per run. Every claim is labelled MEASURED or INFERRED, and every cell here has its
controls named — a table without them is not a result.

## Run 1 — 30513466701, both images, 2026-07-30

`windows-latest` (Server 2025 Datacenter, 10.0.26100, AMD64, `admin=True`, `session-id=2`) and
`windows-11-arm` (Win 11 Enterprise, 10.0.26200, ARM64, same privilege context). `verdict-selftest`
green on Linux first, so both verdicts were shown to discriminate before a runner minute was spent.

### Question 1 — the MSI DACL is exactly as reported, and it does NOT stop the exec

- **MEASURED — `nodejs/node#63590`'s DACL claim reproduces byte-for-byte on a real MSI install**
  (`node-v24.18.1-x64.msi` from `nodejs.org`, `msiexec /qn` exit 0). `C:\Program Files\nodejs` has a
  **protected** descriptor (`AreAccessRulesProtected = True`) with exactly four ACEs and no
  `S-1-15-2-*` among them:

  | trustee | rights | inheritable | inherited |
  | --- | --- | --- | --- |
  | `NT AUTHORITY\Authenticated Users` | `ReadAndExecute, Synchronize` | yes | no |
  | `NT AUTHORITY\SYSTEM` | `FullControl` | yes | no |
  | `BUILTIN\Administrators` | `FullControl` | yes | no |
  | `BUILTIN\Users` | `ReadAndExecute, Synchronize` | yes | no |

  `node.exe` and `node_modules\npm` inherit exactly those four and nothing else. The parent
  `C:\Program Files` **does** carry inheritable `ALL APPLICATION PACKAGES` and `ALL RESTRICTED
  APPLICATION PACKAGES` `ReadAndExecute` ACEs — so the protected descriptor is precisely what stops
  them propagating in, as the issue says.

- **⭐ MEASURED — AND THE JAIL EXECUTES IT ANYWAY. This is the finding that dissolves the fear.**
  `ac-exec-progfiles` (real LowBox, zero capabilities, no ace anywhere on `C:\Program Files`,
  `node -e` so nothing but `node.exe` and the System32 DLLs is read): **`exec-inline=OK`**, `rc=0`,
  `isAC=1`, on BOTH images. Controls green: the unconfined baseline ran the identical command line,
  the gate was live (`stat-c-root=ERR` in both table-reaching arms), and the positive control passed
  (System32 readable).
  **INFERRED (mechanism):** `CreateProcessW` opens the image file in the CALLING process's security
  context, so the LowBox token's lack of rights on the image cannot block process creation. Not yet
  measured directly.

- **MEASURED — but the confined child cannot READ that tree.** Same run, `ac-read-progfiles` (a
  working interpreter from the user's own store, so the cells exist independently of the exec answer):
  `read-progfiles-node-exe=ERR`, `stat-progfiles-node-exe=ERR`, `readdir-progfiles-dir=ERR`,
  `read-npm-package-json=ERR`, `readdir-npm-tree=ERR`. Same child read `C:\Windows\System32\drivers\etc\hosts`
  fine, so this is the DACL and not a dead child.
  ⚠️ **CONSEQUENCE NOT YET MEASURED:** whether a confined script can therefore still run the bundled
  `npm` (`C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js`) — exec of `node` works, but the
  npm entry script is a FILE READ. Run 2 measures it.

- **MEASURED — nub's own provisioned-Node shape is unaffected.** A `node.exe` copied under
  `%USERPROFILE%\…\node\<version>\` with an ace runs confined and reaches the full operations table.
  nub's store is `<cache_dir>/node/` = `%USERPROFILE%\.cache\nub\node\<version>\`
  (`crates/nub-core/src/node/discovery.rs:1104`), which the user owns and nub can ACE.
  **INFERRED FROM CODE, not measured:** with no pin and a Node on `PATH`, `discover_node` takes the
  PATH node (`discovery.rs:367` "Fast path: PATH … already satisfy the pin (or there's no pin)"), so a
  typical user's ambient MSI Node *is* what the jail would launch.

- **⚠️ HARNESS BUGS FOUND, both caught by the raw per-ACE dump rather than by the derived boolean:**
  1. `has-all-application-packages` came back `False` for `C:\Program Files` even though the printed
     ACE list clearly shows both app-package ACEs. `IdentityReference.Translate([SecurityIdentifier])`
     is not producing SIDs. So the **sibling control's `0/12` is a harness artifact, not a fact**, and
     `msi-control-sibling-under-program-files-has-the-ace=FAIL` must not be read as "no sibling has
     the ace". The `nodejs` rows are unaffected — they are true by direct reading of the dump.
  2. `windows-11-arm` did **not** install an MSI at all (`no release in index.json lists
     win-arm64-msi`), so its DACL rows describe the runner IMAGE's pre-existing directory. The arm64
     MSI does exist (`node-v24.18.1-arm64.msi` → HTTP 200) but `index.json`'s `files` array never
     lists `win-arm64-msi`. The ARM descriptor happens to be identical to the x64 MSI's, but that is
     luck, not attribution. (The `msi-install-actually-succeeded` control was added *after* this run,
     which is exactly why it is needed.)

- **⚠️ THE UNPRIVILEGED-REPAIR ANSWER IS INVALID AS MEASURED — do not read it either way.**
  `deelev/write-on-program-files-nodejs = OK` with `admin-under-impersonation = False`, i.e. the
  admin-stripped token appeared to rewrite the DACL. It is an artifact: `CreateRestrictedToken`'s
  `DISABLE_MAX_PRIVILEGE` **disables** privileges rather than deleting them, and .NET's `Set-Acl` path
  re-enables `SeSecurityPrivilege`/`SeRestorePrivilege`/`SeTakeOwnershipPrivilege` when it needs them.
  So the token still held the authority. Run 2 re-measures with true privilege DELETION.
  **This sharpens a caveat that applies to MECHANISM-FACTS §5h's de-elevated lane too**, which uses
  the same `DISABLE_MAX_PRIVILEGE` construction.

### Question 2 — traverse holds on a VHD volume and a subst drive; SMB is unreachable for a different reason

Anchor green in the same run: §5h's local-`C:` deep read reproduced (`OK 41B`), gate live, positive
control passed. Per-volume DACL read-backs all `Modify, Synchronize` on the deep file; every ungranted
sibling denied. **Both images byte-identical.**

| volume | device `Characteristics` | `0x20000` set | reachability control (whole chain granted) | deep read, ancestors ungranted |
| --- | --- | --- | --- | --- |
| local `C:` NTFS | `0x00020020` | yes | OK | **OK** |
| VHD (diskpart, NTFS, `V:`) | `0x00020120` | yes | OK | **OK** |
| `subst` drive (`S:` → a dir on `C:`) | `0x00020020` | yes | OK | **OK** |
| SMB `\\localhost\<share>\` | `0x00000010` | **no** | **ERR** | ERR |

- **⭐ MEASURED — the shipping backend's assumption survives on a mounted VHD volume and on a `subst`
  drive.** `windows.rs`'s traverse model ("standard local NTFS volumes carry
  `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL`… Traverse would only be enforced on the rare device
  LACKING the volume flag") holds for both, with the ace on the deep directory only and `V:\` / `S:\`
  / `C:\` / `C:\Users` all ungranted. A developer with a project on a mounted volume or a mapped
  `subst` drive is fine.
- **MEASURED — the flag decode is grounded, not recalled.** The Windows SDK header is not on either
  runner image (`sdk-const-source = (not found)`), so the probe fell back to `0x20000`. Cross-checked
  against Microsoft's own generated metadata:
  `windows-sys-0.61.2/src/Windows/Wdk/System/SystemServices/mod.rs:4326` —
  `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL: u32 = 131072u32` = `0x20000`. The observed pattern (set
  on all three local-disk volumes, clear on the SMB redirector, which instead carries `0x10`
  `FILE_REMOTE_DEVICE`) is what that constant predicts.
- **MEASURED — an SMB path is unreachable from an AppContainer, and it is NOT the traverse question.**
  The reachability control fails: with an inheritable ace at the SHARE ROOT (nothing ungranted above
  the target), plus `internetClient` **and** `privateNetworkClientServer` capability SIDs, plus a
  **successful** `CheckNetIsolation LoopbackExempt -a` (`OK.`), every open on the UNC path returns an
  NTSTATUS libuv cannot map (`ERR UNKNOWN`). The unconfined baseline reads the same path fine. So the
  UNC row is a **real compat gap** — a project on a network share cannot be built in this jail — but
  it is attributable to AppContainer network isolation of the SMB redirector, **not** to traverse
  enforcement. The volume-flag hypothesis is therefore *consistent* with, but not *confirmed* by, this
  row.
  **INFERRED:** a `net use Z: \\server\share` mapped drive is the same redirector device, so it
  carries the same result.
- **⚠️ THE PRIVILEGE DIFFERENTIAL IS INCONCLUSIVE, and the property that "passed" is a weak pass.**
  `cn-kept` (identical `CreateProcessAsUserW` + `CreateRestrictedToken` path, privilege retained,
  read back off the running child as `privs=[SeChangeNotifyPrivilege:on,…]`) read deep fine.
  `cn-deleted` (one variable: `SeChangeNotifyPrivilege` deleted, read back as
  `privs=[SeIncreaseWorkingSetPrivilege:off]`) returned **`rc=0xC0000022` (`STATUS_ACCESS_DENIED`)
  with a ZERO-BYTE log** — the process was created (its token was readable) but died in
  initialization before Node emitted a line. So the deep read was never attempted.
  `deleting-changenotify-breaks-the-deep-read` passed only because its predicate was `-ne 'OK'`, which
  conflates `MISSING-OP` with a denial. **It does not answer the mechanism question and must not be
  cited as if it did.**
  **INFERRED, and worth having anyway:** a LowBox process cannot complete startup without
  `SeChangeNotifyPrivilege` at all — consistent with the loader losing the ability to traverse to
  `C:\Windows\System32` for its own DLLs, which would make the privilege load-bearing for traverse.
  Indirect. Run 2 splits the control so this reads as inconclusive rather than as a result.

## Runs 2 and 3 — 30514541825 and 30514708871, both images, 2026-07-30

Run 2 carried the four harness fixes run 1's own output exposed; run 3 fixed the one arm run 2 was
still getting wrong. Every headline cell below is **identical on `windows-latest` (Server 2025
Datacenter 10.0.26100, AMD64) and `windows-11-arm` (Win 11 Enterprise 10.0.26200, ARM64)**, both
`admin=True`, `integrity-level = High Mandatory Level (S-1-16-12288)`, `session-id=2`.

### Question 1 — settled

- **MEASURED — the MSI DACL defect is real and `nodejs` is the outlier.** With the SID-translation bug
  fixed, **43/44** `C:\Program Files\*` siblings on `windows-latest` and **34/35** on
  `windows-11-arm` carry `S-1-15-2-1` (`dotnet`, `Git`, `PowerShell`, `Microsoft Visual Studio`, …).
  `nodejs` is the one that does not. `msi-install-actually-succeeded=PASS` on both images this time,
  so those rows describe an installer we ran (`node-v24.18.1-{x64,arm64}.msi`, `msiexec /qn` exit 0)
  and not the runner image.
- **⭐ MEASURED, 6/6 image-runs — the AppContainer EXECUTES the MSI-installed `node.exe` anyway.**
  `exec=OK`, `rc=0`, `isAC=1`, zero capabilities, no ace anywhere on `C:\Program Files`, with the
  unconfined baseline running the identical command line and the gate live.
- **⭐ MEASURED — but the bundled `npm` does not run, and that is the real bite.** One variable, same
  interpreter: unconfined prints `11.16.0`; confined dies
  `Error: Cannot find module 'C:\Program Files\nodejs\node_modules\npm\bin\npm-cli.js'`, `rc=1`.
  Starting `node.exe` is a parent-context image open; the npm entry script is an ordinary read by the
  confined child, and reads of that tree are all denied (`node.exe`, `node_modules\npm\package.json`,
  `readdir` — all `ERR`, while the same child reads System32).
- **MEASURED — CI provisioning did NOT hide anything on this axis.** `actions/setup-node`'s unzipped
  toolcache tree ALSO lacks `S-1-15-2-1` (`sids = S-1-5-11,S-1-5-11,S-1-5-18,S-1-5-32-544,S-1-5-32-545`)
  and execs just the same. The exec result does not depend on the image DACL at all, so the
  CI-vs-MSI difference is not the thing that concealed this — the earlier probe copying `node.exe`
  into a granted directory was.
- **MEASURED — nub cannot repair the DACL unprivileged.** Impersonated admin-stripped token
  (`Administrators` deny-only, every privilege but `SeChangeNotifyPrivilege` DELETED, read back as
  `privs=[SeChangeNotifyPrivilege:on]`, `admin-under-impersonation=False`): the write on
  `C:\Program Files\nodejs` fails and **the descriptor read-back shows `ace-on-disk=none`**, while the
  control write inside `%USERPROFILE%` succeeds under the same impersonation. Owner is
  `NT AUTHORITY\SYSTEM`; no ACE in the MSI descriptor grants `WRITE_DAC` to Users or Authenticated
  Users.
  *(.NET reports `PrivilegeNotHeldException: SeSecurityPrivilege` rather than an ACCESS_DENIED on
  `WRITE_DAC`; that is the privilege .NET's persist path demands for the section set it is writing, so
  a hand-rolled `SetNamedSecurityInfo(DACL only)` might surface a different error. The outcome —
  nothing lands — is MEASURED; the specific gate is INFERRED.)*
- **⚠️ AND THE RUN-2 EXPLANATION OF THIS WAS WRONG, recorded because it nearly shipped.** Run 2 still
  reported the write as succeeding, and I attributed it to `DISABLE_MAX_PRIVILEGE` merely *disabling*
  privileges that .NET then re-enables. Run 3 refuted that: the real cause was that the probes run
  under `$ErrorActionPreference = 'Continue'`, where a refused `Set-Acl` is a **non-terminating**
  error — it printed, the function returned normally, and the caller's `try/catch` recorded `OK`. The
  `disable` variant fails identically once `-ErrorAction Stop` is set, so the privilege theory was
  never needed. The tell was there in run 1: the residue check found no ace on `Program Files`
  afterwards.
- **⚠️ ONE OBSERVED FLAKE, bounded and unexplained.** `ac-exec-toolcache` on `windows-latest` run 3
  **timed out at 60 s with a zero-byte log** (`rc=0xffffffff TIMED-OUT`, `isAC=1`) after returning
  `OK` in run 2 and on ARM. The process was created and produced nothing. Not attributed — noting it
  because it is an AppContainer child hanging with no output, which is the shape of the sibling lane's
  piped-hang class, and because the probe's timeout bound is what kept it from eating the job.

### Question 2 — settled for local volumes, inconclusive on mechanism

Run 2 reproduced run 1 cell-for-cell on both images, with the tightened privilege-differential
controls now reading correctly.

| volume | `Characteristics` | `0x20000` set | reachability control | deep read, ancestors ungranted |
| --- | --- | --- | --- | --- |
| local `C:` NTFS (anchor) | `0x00020020` | yes | OK | **OK** |
| VHD, diskpart NTFS, `V:` | `0x00020120` | yes | OK | **OK** |
| `subst` drive `S:` | `0x00020020` | yes | OK | **OK** |
| SMB `\\localhost\<share>\` | `0x00000010` | **no** | **ERR** | ERR |

- **⭐ MEASURED — `windows.rs`'s traverse assumption holds on a mounted VHD volume and a `subst`
  drive.** Ace on the deep directory only; `V:\` / `S:\` / `C:\` / `C:\Users` all ungranted; DACL
  read-back `Modify, Synchronize` on the deep file in every arm; every ungranted sibling denied.
- **MEASURED — an SMB path is unreachable from an AppContainer, and it is NOT the traverse question.**
  Even with an inheritable ace at the SHARE ROOT (nothing ungranted above the target), `internetClient`
  **and** `privateNetworkClientServer` capability SIDs, and a **successful**
  `CheckNetIsolation LoopbackExempt -a` (`OK.`), every open returns an NTSTATUS libuv cannot map
  (`ERR UNKNOWN`); the unconfined baseline reads the same path fine. Real compat gap, wrong cause to
  cite.
- **⚠️ MECHANISM STILL NOT SEPARATED, and the differential that would have done it cannot run.**
  `cn-kept` (identical `CreateProcessAsUserW` + `CreateRestrictedToken` path, privilege retained, read
  back as `privs=[SeChangeNotifyPrivilege:on,…]`, 11 log lines) reads deep. `cn-deleted` (one
  variable) returns **`rc=0xC0000022` with a ZERO-BYTE log** on both images: the process is created
  (its token is readable, `isAC=1`, `privs=[SeIncreaseWorkingSetPrivilege:off]`) and dies in
  initialization before Node emits a line, so the read is never attempted.
  `cn-deleted-child-started-at-all=FAIL` ⇒ the differential is **INCONCLUSIVE**, which is what run 2's
  tightened predicate now reports (run 1's `-ne 'OK'` accepted `MISSING-OP` and read as a positive).
  **INFERRED, not measured:** a LowBox process cannot complete startup without
  `SeChangeNotifyPrivilege` — consistent with the loader losing traverse to `C:\Windows\System32` for
  its own DLLs, which would make the privilege load-bearing. Indirect, and it also means the
  privilege can never be absent in practice: the LowBox token inherits it from the launching parent,
  and a parent lacking it produces a child that never starts.
- **MEASURED — the flag decode is grounded.** The Windows SDK header is on neither runner image
  (`sdk-const-source = (not found)`), so the probe used `0x20000`; cross-checked against Microsoft's
  own generated metadata, `windows-sys-0.61.2/src/Windows/Wdk/System/SystemServices/mod.rs:4326` —
  `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL: u32 = 131072u32`.

### Unmeasured, named explicitly

- A **second physical** volume (only a VHD was available on a runner) and a **filter-driver /
  redirector** device other than SMB.
- A `net use Z:` mapped drive. **INFERRED** to match the SMB row — same redirector device, same
  `chars=0x10`.
- Whether the traverse skip survives on a volume that lacks `0x20000` **and** is reachable under an
  AppContainer. No such device was found on either image, so the volume rows cannot separate the two
  mechanisms either.
- Whether nub's lifecycle-script path actually reaches for the ambient `npm` (INFERRED FROM CODE only:
  `discovery.rs:367` takes the PATH node when unpinned, so the ambient Node *is* what would be
  launched).
