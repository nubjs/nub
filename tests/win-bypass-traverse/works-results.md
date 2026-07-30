# Does a tool that STARTS inside an AppContainer actually do its job?

`tools.ps1` asked "does the process start and print a marker" and got `cmd.exe`, `powershell.exe`,
`pwsh.exe` and busybox `sh` all green off `echo`. That contradicted Microsoft's own host-prep doc
(`.repos/mxc/docs/host-prep.md:62`), which names those shells as failing inside an AppContainer
without a one-time host-wide ace on `C:\`. **Microsoft is right; the startup test was too weak.**

## READ THIS FIRST: what these probes measure, and what they do not

**`tools.ps1`, `works.ps1` and `gyp.ps1` launch a BARE AppContainer** — `SECURITY_CAPABILITIES` with
`CapabilityCount = 0`, no ancestor repair, no preload. Their results are facts about **raw
AppContainer physics**, and they must NOT be read as facts about nub. nub's production launch carries
three repairs none of those arms had:

1. **The ancestor repair** (`crates/nub-sandbox/src/backend/windows.rs:1555-1595`). A non-inherited
   `0x001000a1` traverse + read-attributes ace where nub holds `WRITE_DAC`, and where it does not —
   `C:\`, `C:\Users` — a **request for the capability sid Windows already granted on that exact
   path** (`C:\` carries `(A;;0x1000a1;;;S-1-15-3-65536-…)`). Requesting a raw capability sid is
   **unprivileged**, so this is the unprivileged equivalent of the privileged ace the `ac-croot` arm
   below needed.
2. **The stdio shim** (`crates/nub-sandbox/src/compiler/defaults.rs:648-692`).
   `windows_stdio_shim.js` delivered as `--import data:text/javascript;base64,…` on a STAMPED
   `NODE_OPTIONS`. Its header documents the piped-spawn spin measured below and calls `execFile`
   "exact" under the repair.
3. **The realpath shim** (`windows_realpath_shim.js` + `--preserve-symlinks-main`) — the shippable
   form of the `--preserve-symlinks --preserve-symlinks-main` pair these probes work around with.

`prod.ps1` is the probe that reproduces all three with a bare arm as its control. **Until it has a
result, no product-level conclusion below is settled.**

Runs: `30518533077`, `30519126454` (x64 leg cancelled on the job timeout), `30522944978`,
`30524441158` (`prod.ps1`). Images: **windows-latest** = Server 2025, build **26100**, AMD64;
**windows-11-arm** = Windows 11 Enterprise, build **26200**, ARM64.

## The reconciliation with Microsoft's doc

Microsoft's claim is about an API, not a process: an AppContainer cannot read the system-drive root's
metadata. `probe.ps1` had already measured that (`lstat 'C:\'` → `EPERM`). What differs per tool is
the CONSEQUENCE — `node` throws and dies, `cmd.exe` carries on and lies, PowerShell carries on and
cannot spawn. Both named APIs reproduce, and only under the token:

| API | unconfined | bare AppContainer |
| --- | --- | --- |
| `[IO.File]::GetAttributes('C:\')` | `Hidden, System, Directory` | `ERR MethodInvocationException` |
| `([IO.DirectoryInfo]'C:\').GetAccessControl()` | `NT SERVICE\TrustedInstaller` | `ERR MethodInvocationException` |

The filed upstream form is **PowerShell/PowerShell#27253**, root-caused to `GetFileAttributesEx("C:\")`
returning `ERROR_ACCESS_DENIED` and .NET reporting it as "does not exist". Fix PR #27266
(`SafeDoesPathExist`) shipped in **7.7.0-preview.1 and the 7.6.2 backport only** — not 7.5.x, not the
7.4 LTS. mxc's own harness works around the residue (`tests/scripts/T3-Workloads.ps1:368`: `git -C`
"avoids pwsh's `Set-Location` which walks ancestor paths and trips"). The runners carry pwsh 7.6.3 /
7.6.4 and Windows PowerShell 5.1, so every PowerShell row here is POST-fix residue.

## Raw physics: what breaks in a bare AppContainer, and which grant tier repairs it

`ac-ancestors` = non-inheriting `ReadAndExecute` on the fixture root and `%USERPROFILE%` (unprivileged
but **not** what production does — no capability request). `ac-croot` = plus `C:\` and `C:\Users`,
needing `WRITE_DAC` the user does not hold.

| op | plain | ac (bare) | ac-ancestors | ac-croot |
| --- | --- | --- | --- | --- |
| `if exist <granted FILE>` | yes | yes | yes | yes |
| `if exist <granted DIR>` | yes | **no** | yes | yes |
| `if exist C:\` / `C:\Windows` / `%ProgramFiles%` | yes | **no** | **no** | yes |
| `cd <abs granted dir>` | ok | **FAIL** | **FAIL** | ok |
| `cd <relative>` | ok | ok | ok | ok |
| `dir <abs granted dir>` / `dir .` | ok | **FAIL** | **FAIL** | ok |
| `type <abs granted file>` | ok | ok | ok | ok |
| `>NUL` / `2>NUL` | ok | ok | ok | ok |
| PowerShell `Get-Location` | real cwd | **`C:\`** | **`C:\`** | real cwd |
| PowerShell `Test-Path 'C:\'` | True | **False** | **False** | True |
| PowerShell `Set-Location <granted>` | ok | **ERR** | **ERR** | ok |

The tier comparison rests on **one image** (arm64); run `30519126454`'s x64 leg was cancelled.

Two failure shapes deserve naming, because no exit-code harness sees them:

- **PowerShell cannot spawn any external program.** Every `& <exe>` dies `DriveNotFoundException: A
  drive with the name 'Microsoft.PowerShell.Core\FileSystem' does not exist`, and `$LASTEXITCODE` is
  **EMPTY** — so a script's own `if ($LASTEXITCODE -ne 0)` reads as success. Measured on Windows
  PowerShell 5.1, the one node-gyp invokes.
- **`Get-Location` reporting `C:\`** means every relative path silently resolves against the wrong
  root.

**Do not conclude from this table that nub needs Microsoft's privileged step.** `ac-croot` shows a
privileged ace suffices; it does not show it is necessary, because no arm here requested the
capability sids production requests. That is `prod.ps1`'s question.

## busybox-w32 needs no repair at all

In the bare `ac` arm its whole battery differs from unconfined by 5 lines: the ungranted sibling (2,
intended), `%TEMP%` (the OS redirects an AppContainer's TEMP to its own `AC\Temp` — benign and
writable), and `[ -d "C:/" ]` → no. `cd`, glob, redirect, read, and the entire SPAWN battery (node,
whoami, a pipe, `$(...)`, a nested `sh`) are **byte-identical confined** with zero capabilities and no
ancestor grant.

`nub run` on Windows already defaults to busybox (`crates/nub-cli/src/cli.rs:3921`; the comment at
3808 rules out a cmd.exe fallback deliberately, and `__NUB_BUSYBOX_EXE` is the CI seam). Only aube's
dependency-lifecycle path still hardcodes `cmd.exe`
(`vendor/aube/crates/aube-scripts/src/lib.rs:337-355`), where a non-default `script_shell` already
takes a POSIX `-c` branch. One hazard for that swap: the default cmd branch uses `raw_arg`
deliberately "so cmd.exe sees the original script bytes", while the `script_shell` branch uses `arg`
— a script body containing quotes may be re-quoted differently.

## `\Device\Null` is build-dependent

Same commit, same battery, run `30518533077`:

| image | busybox `> /dev/null` |
| --- | --- |
| windows-latest (build 26100) | `can't create /dev/null: Permission denied`, rc=1 |
| windows-11-arm (build 26200) | rc=0 |

Matches mxc's note that AppContainers cannot open `\Device\Null` on "downlevel builds that pre-date
the `Feature_AgenticAppContainerBfsSupport` ship", and that the descriptor **resets at every boot** —
so their own `prepare-null-device` fix is not durable. Distinct from the sibling lane's refutation,
which covered an already-open handle inherited from the unconfined parent.

`cmd.exe`'s `>NUL` / `2>NUL` work confined. An earlier reading of those cells as denials was an
artifact: **a cmd BUILTIN that succeeds does not reset `%ERRORLEVEL%`**, so a working redirect
inherited the previous command's failure code. Every op now uses `(cmd) && ok || FAIL`.

## LPAC is a rejected mechanism

One attribute off the ordinary AppContainer (`ALL_APPLICATION_PACKAGES_POLICY = OPT_OUT`) kills
everything, because an LPAC stops honouring the `ALL APPLICATION PACKAGES` aces that blanket
System32: `cmd.exe` 0 bytes rc=1; `powershell.exe` `Cannot open registry key
SOFTWARE\Microsoft\PowerShell. Access is denied.`; busybox `WSAStartup failed, error 18`. Arms
removed; `launcher.ps1` keeps LPAC support so re-measuring is one argument away.

## From-source native builds: two blockers, only one of them nub's

**(a) An UNSHIMMED confined node hangs on a piped spawn.** `node-gyp configure` is
`rc=0xffffffff TIMED-OUT` after 180 s against a green unconfined control on arm64, hanging at its
very FIRST `execFile` (the python search's `py.exe -3 -c …`) — it never reaches VS detection. The
isolated arm confirms it: node `execFile` of `powershell.exe` with piped stdio TIMED-OUT with 0 bytes
on both images while the same arm unconfined returned all three shapes. Python's
`subprocess.run(capture_output=True)` WORKS confined, so this is node/libuv-specific.
**`windows_stdio_shim.js` exists precisely for this** — its header names the same root cause
(`uv__pipe_server` retrying a refused global-NPFS name forever) and the same signature ("cpu_ms 14906
of 15059 wall"). **These arms did not load it**, so this is a probe result, not a product result.

**(b) VS detection is a COM denial — and no shim touches it.** node-gyp's exact `Add-Type -Path
Find-VisualStudio.cs; [VisualStudioConfiguration.Main]::PrintJson()` as the AppContainer's own first
process: `Add-Type` compiles fine, then

```
Exception calling "PrintJson" with "0" argument(s): "Retrieving the COM class factory for component
with CLSID {177F0C4A-1CD3-4DE7-A32C-71DBBB9FA36D} failed due to the following error: 80070005 Access
is denied. (Exception from HRESULT: 0x80070005 (E_ACCESSDENIED))."
```

That CLSID is the VS `SetupConfiguration` COM server. **PowerShell still exits rc=0**, so node-gyp's
`execFile` sees success with empty stdout. The other path is dead too: `Get-Module -ListAvailable
-Name VSSetup` returns null confined (`2.2.16` unconfined). No file grant fixes either — DCOM launch
permission for an AppContainer sid is machine-wide and privileged.

Fix shape, mirroring what nub already does for python: **pre-resolve VS UNCONFINED and inject the
answer** so node-gyp's env short-circuit skips the COM probe. `findVisualStudio` already reads
`VCINSTALLDIR`, `VSCMD_VER` and `WindowsSDKVersion` and assumes the VC packages rather than probing.

## Python: the grant is right; only the write's propagation is expensive

Every ancestor from python's install dir to `C:\` carries **no** AppContainer ace, so the loader
cannot open `python312.dll` beside the exe — hence `0xc0000135 STATUS_DLL_NOT_FOUND`. One inheritable
`ReadAndExecute` ace on the install root fixes it completely: `ssl`, `sqlite3`, `ctypes` import,
`subprocess` with piped stdio works, `os.chdir` works, IO works, and the only residual diffs are the
intended denials.

| shape | windows-latest | windows-11-arm |
| --- | --- | --- |
| empty dir (the O(1) control) | 3 ms grant / 3 ms revoke | 3 / 3 |
| whole install root | 1035 ms | 921 ms |
| RE-grant, identical ace already present | 982 ms | 683 ms |
| revoke | 928 ms | 632 ms |
| narrow: `python.exe` / `DLLs\` / `Lib\` | 3 / 8 / 936 ms | 4 / 7 / 469 ms |
| narrow total | 954 ms | 486 ms |

Three conclusions. The empty-dir control shows the cost is **inheritance propagation**, not the DACL
write. A re-grant costs the same as a fresh one, so per-spawn write-and-revoke cannot be made cheap
by caching the decision. And **narrowing is a dead end**: `Lib\` *is* the tree (6412 entries, 180 MB),
so the narrow set costs the same **and does not work** — `py-narrow-exec` is `0xc0000135`, because
`python3.dll` / `python312.dll` / `vcruntime140*.dll` sit in the install ROOT, not `DLLs\`.

This independently confirms a problem production already documents: `set_ace_on_object`'s comment
(`windows.rs:1160-1169`) says the propagation "wedged a 20-minute CI step", that swapping the named
writer for the handle-based `SetSecurityInfo` "narrowed nothing", and that the genuinely
non-propagating primitive is `SetKernelObjectSecurity` with a hand-built descriptor — "that is the
next move". The empty-dir-vs-populated ratio here (3 ms vs ~1000 ms) is a clean quantification of
that claim.

**The free lever, which is orthogonal and not yet implemented:** `%ProgramFiles%` carries
`ALL APPLICATION PACKAGES:ReadAndExecute` **inheritably** (verbatim on both images), so an all-users
python or node needs NO grant at all — skip the write when the tree already grants an AppContainer
read. The runners' per-user `hostedtoolcache` layout carries `NONE`, which is why it pays. The
backend already has the `GetEffectiveRightsFromAclW` machinery to decide it.

An earlier 3.7 s figure for the same operation was runner contention; ~1 s is the number.

## What this does not establish

- **The whole product-level question.** Every arm outside `prod.ps1` launched a bare AppContainer.
  `prod.ps1` (run `30524441158`) is the one that reproduces production's launch; at hand-off it had
  not finished.
- **`prod.ps1` has a known cost flaw of its own:** it writes its ancestor traverse aces with
  PowerShell `Set-Acl`, the NAMED writer, which propagates — exactly what `windows.rs:1160` warns
  takes minutes on a CI ancestor chain. Its chain includes `%LOCALAPPDATA%`, so the job may exhaust
  its timeout. The fix is the handle-based write production uses.
- **`gyp-configure`'s x64 leg has a red unconfined control** — the runner's bundled node-gyp is
  11.5.0, whose supported-years list predates the VS 18 (2026) on that image, so it cannot find VS
  even unconfined. Only the arm64 leg's control is green.
- **`cmd.exe`'s `>NUL` on build 26100 with the corrected idiom** was never measured; that leg was the
  cancelled one, and it is the image where busybox's `/dev/null` was denied.
- **Why `getattributes-croot` returns a partial `Directory` in the `ac-ancestors` arm** (vs `ERR` in
  `ac` and the full value in `ac-croot`) is unexplained; that tier grants nothing on `C:\`.
- **node-gyp's behaviour once the piped spawn works** is unknown — the hang is upstream of
  everything, so the COM failure is measured only in isolation.
- `python_toolchain_grant`'s Windows path is untested by construction (its whole test module is
  `#[cfg(unix)]`, annotated "the Python-grant cases are POSIX-shaped throughout"). One specific
  untested hazard follows: its local `canonical()` is bare `std::fs::canonicalize`, which per the std
  docs "converts the path to use extended length path syntax" on Windows and "may be incompatible
  with other applications (if passed to the application on the command-line, or written to a file
  another application may read)". That value goes straight into `npm_config_python`, so node-gyp would
  receive `\\?\C:\…\python.exe` and write it into generated build files. The repo already has
  `strip_verbatim_prefix` (`crates/nub-sandbox/src/matcher/path.rs:178`) for this class.
  **Not reproduced** — the code path was read, the breakage was not observed.
