# DOES A TOOL THAT *STARTS* INSIDE AN APPCONTAINER ACTUALLY DO ITS JOB?
#
# THE QUESTION, and why `tools.ps1` was too weak to answer it. That probe asked "does the process
# start and print a marker", and got `cmd.exe`, `powershell.exe`, `pwsh.exe` and busybox `sh` all
# green off `echo`. Microsoft's own host-prep doc (`.repos/mxc/docs/host-prep.md:62`) names those
# same three shells as tools that "fail with ERROR_ACCESS_DENIED inside an AppContainer" without a
# one-time host-wide ace on `C:\` — a setup step that is DISQUALIFYING for nub's build jail, which
# must work totally unprivileged. Two readings of that disagreement are both consistent with the
# evidence so far, and they have opposite consequences:
#
#   (a) Microsoft describes an API call failing, not a process failing. `probe.ps1` already measured
#       `lstat 'C:\'` -> EPERM from a confined child, so the API claim is CONFIRMED, not refuted.
#       A tool that tolerates the refusal starts anyway; `node` throws and dies. Under this reading
#       `tools.ps1` was simply not exercising the operations that hit it.
#   (b) The failure needs a condition `tools.ps1` never created — LPAC rather than an ordinary
#       AppContainer, a different capability set, or an OS build without the AppContainer traverse
#       behaviour this effort depends on.
#
# Under (a) the dangerous outcome is not a crash. It is a shell that starts, answers a filesystem
# question WRONGLY, and exits 0 — a lifecycle script that "succeeds" having done nothing, which no
# exit-code harness can see. `tools.ps1` already caught three of those by accident and mislabelled
# them: `busybox-node`, `busybox-shshim` and `busybox-cmdshim` each exited rc=0 with their marker
# ABSENT, one of them after printing "Access is denied." So this probe measures OPERATIONS, and its
# unit of evidence is a BYTE-FOR-BYTE DIFF of the confined child's whole log against the unconfined
# child's whole log. A differing line is the finding; an equal log is the only clean pass.
#
# WHY THE DIFF IS THE MEASUREMENT rather than a marker. A marker answers "did anything work". A diff
# answers "did everything work", and it is the only shape that catches a wrong answer — `if exist
# C:\` printing `no`, a glob expanding to nothing, `%ERRORLEVEL%` coming back 0 after a denied
# spawn. Every path the batteries touch is therefore a CONSTANT within a run, so the two logs are
# comparable with no normalisation: same fixture root, same cwd, and `TEMP`/`TMP` pointed at the
# same granted directory in EVERY arm including `plain` (`tools.ps1` set it only for the confined
# arms, which would have made TEMP a second variable here).
#
# THE BATTERIES ARE SPLIT SAFE/SPAWN for one reason: a piped spawn under an AppContainer is known to
# hang indefinitely (MECHANISM-FACTS §5h), and a hang inside a battery costs every op after it. The
# `safe` battery uses shell builtins only, so its table always completes; the `spawn` battery holds
# every child-process and pipe op, and a hang there loses only those.
#
# PART 2 — LPAC. Hypothesis (b) is tested directly: the same batteries run a third arm whose only
# difference is `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY = OPT_OUT`. An LPAC stops
# honouring the `ALL APPLICATION PACKAGES` aces that blanket System32, so if Microsoft's failure
# list reproduces anywhere it should reproduce here. mxc defaults this OFF
# (`sdk/node/src/types.ts:157`), so it is a hypothesis about their harness, not a certainty.
#
# PART 3 — PYTHON, which is 35% of the corpus. `node-gyp` reaches python for 121 of the 344 corpus
# packages, and `tools.ps1` measured confined `python.exe` dying `0xc0000135
# STATUS_DLL_NOT_FOUND` — a LOADER failure, before any python code. The theory on record ("just an
# ungranted install directory") was never verified, so this probe (i) reports the DACL of python's
# install tree, its `DLLs\` subdirectory and the dlls beside the exe so the theory can be falsified
# rather than assumed, and (ii) runs the grant differential that settles it: one inheritable
# ReadAndExecute ace on python's install root, nothing else changed. Microsoft's own harness does
# exactly this (`T3-Workloads.ps1:215` `Test-NeedsAppContainerGrant` auto-grants python's directory
# read-only whenever it is not under `Program Files`), which is corroboration for the shape of the
# fix but not for OUR failure's cause. The grant's WALL-CLOCK COST is measured too, because on
# Windows every grant is written per lifecycle spawn and revoked after.

Set-StrictMode -Off
$ErrorActionPreference = 'Continue'

. (Join-Path $PSScriptRoot 'verdict.ps1')   # W / Fact / Prop / $script:fails
. (Join-Path $PSScriptRoot 'launcher.ps1')  # [Bt]::CreateProfile / DeleteProfile / LaunchEx

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)

W '== host =='
Fact 'os' ((Get-CimInstance Win32_OperatingSystem).Caption)
Fact 'os-version' ([Environment]::OSVersion.VersionString)
Fact 'os-build' ((Get-CimInstance Win32_OperatingSystem).BuildNumber)
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'whoami' (whoami)
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
Fact 'elevated' ($pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))
Fact 'session-id' (Get-Process -Id $PID).SessionId
Fact 'node-version' (& node --version 2>&1)
# THE VERSION BOUNDARY, load-bearing for reading the PowerShell rows. PowerShell/PowerShell#27253
# ("FileSystem PSDrives not mounted when PowerShell runs inside an AppContainer") is the filed
# upstream form of Microsoft's host-prep claim, root-caused to `GetFileAttributesEx("C:\")`
# returning ERROR_ACCESS_DENIED and .NET reporting that as "does not exist". Its fix (PR #27266,
# `SafeDoesPathExist`) shipped ONLY in 7.7.0-preview.1 and the 7.6.2 backport — NOT 7.5.x, and NOT
# the 7.4 LTS. So which pwsh a machine has decides which failure mode it gets, and a result read off
# this runner is a result about THIS version.
Fact 'pwsh-version' $(try { (& pwsh -NoLogo -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' 2>&1) } catch { 'ABSENT' })
Fact 'winps-version' $(try { (& powershell -NoLogo -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' 2>&1) } catch { 'ABSENT' })
# `\Device\Null`'s descriptor is the SECOND thing Microsoft ships a host-prep verb for
# (`prepare-null-device`): on builds predating `Feature_AgenticAppContainerBfsSupport` an
# AppContainer cannot open it, and it resets to that state at EVERY BOOT. Reported here because
# `>NUL` / `>/dev/null` is the modal redirection in a lifecycle script.
Fact 'null-device-sddl' $(try {
  $sd = (Get-Acl -LiteralPath '\\.\NUL' -ErrorAction Stop); $sd.Sddl
} catch { "unreadable: $($_.Exception.GetType().Name)" })

# `C:\` and `C:\Users` are reported and NEVER written by this probe. The whole point is whether the
# build jail can skip Microsoft's `prepare-system-drive`, so an ace here would beg the question.
foreach ($p in @('C:\', 'C:\Users')) {
  W "  fact:icacls[$p] ="
  (icacls $p 2>&1) | ForEach-Object { W "      $_" }
}

# ─────────────────────────────── fixture ───────────────────────────────

$root       = Join-Path $env:USERPROFILE "nubworks-$nonce"
$binDir     = Join-Path $root 'bin'        # ReadAndExecute: tool binaries not already on an AAP path
$workDir    = Join-Path $root 'work'       # Modify: the batteries, and anything they write
$deepDir    = Join-Path $workDir 'deep'    # Modify (inherited): a Set-Location / cd target
$tmpDir     = Join-Path $root 'tmp'        # Modify: TEMP/TMP, in EVERY arm
$ungranted  = Join-Path $root 'ungranted'  # never granted: the negative control
foreach ($d in @($root, $binDir, $workDir, $deepDir, $tmpDir, $ungranted)) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
}

function Get-AcAces([string]$path) {
  return @((Get-Acl -LiteralPath $path).Access | Where-Object { $_.IdentityReference.Value -like 'S-1-15-*' })
}
$acl = Get-Acl -LiteralPath $root
$acl.SetAccessRuleProtection($true, $true)
Set-Acl -LiteralPath $root -AclObject $acl
foreach ($t in @($root, $binDir, $workDir, $deepDir, $tmpDir, $ungranted)) {
  $a = Get-Acl -LiteralPath $t
  foreach ($r in (Get-AcAces $t)) { $null = $a.RemoveAccessRule($r) }
  Set-Acl -LiteralPath $t -AclObject $a
}
$residual = 0
foreach ($t in @($root, $binDir, $workDir, $deepDir, $tmpDir, $ungranted)) { $residual += (Get-AcAces $t).Count }
Prop 'no-preexisting-appcontainer-ace' ($residual -eq 0) `
  "$residual S-1-15-* aces remain on the fixture; a residual one would let an op pass for a reason this probe did not write"

$nodeSrc = (Get-Command node -ErrorAction Stop).Source
Copy-Item -LiteralPath $nodeSrc -Destination (Join-Path $binDir 'node.exe') -Force
$jailNode = Join-Path $binDir 'node.exe'

$bbSrcName = if ($env:PROCESSOR_ARCHITECTURE -match 'ARM') { 'busybox64a.exe' } else { 'busybox64.exe' }
$bbSrc = Join-Path $PSScriptRoot "..\..\vendor\busybox-w32\$bbSrcName"
$jailBusybox = ''
if (Test-Path -LiteralPath $bbSrc) {
  Copy-Item -LiteralPath $bbSrc -Destination (Join-Path $binDir 'busybox.exe') -Force
  $jailBusybox = Join-Path $binDir 'busybox.exe'
}
Fact 'busybox' $(if ($jailBusybox) { $jailBusybox } else { "MISSING $bbSrc" })

[IO.File]::WriteAllText((Join-Path $ungranted 'secret.txt'), "SECRET`n")

$sys32 = Join-Path $env:SystemRoot 'System32'

# ─────────────────────────────── the batteries ───────────────────────────────
# Every op prints ONE deterministic line. Paths arrive through `BT_*` so the script text is
# identical in every arm and the two logs are diffable with no normalisation.
#
# `exist-croot` is the load-bearing op of the whole probe: it asks the shell the exact question
# Microsoft says the shell cannot answer. A confined `cmd.exe` that prints `no` there has not
# crashed — it has LIED about the filesystem and gone on to exit 0.

#
# `%ERRORLEVEL%` IS NOT USABLE HERE and reading it cost this probe a wrong reading of run 1. A cmd
# BUILTIN that succeeds does not RESET errorlevel, so `echo x > file` followed by
# `echo %ERRORLEVEL%` reports whatever the previous command left — which made a working redirect
# look like a denial. Every op therefore uses the `(cmd) && ok || FAIL` form, which reads the
# command's OWN result.
$batCmd = @'
@echo off
echo op:echo=ok
echo op:cwd=%CD%
if exist "%BT_WORK%\marker.txt" (echo op:exist-granted-file=yes) else (echo op:exist-granted-file=no)
if exist "%BT_WORK%" (echo op:exist-granted-dir=yes) else (echo op:exist-granted-dir=no)
if exist "%BT_DEEP%" (echo op:exist-deep-dir-abs=yes) else (echo op:exist-deep-dir-abs=no)
if exist deep (echo op:exist-deep-dir-rel=yes) else (echo op:exist-deep-dir-rel=no)
if exist "%BT_UNGRANTED%\secret.txt" (echo op:exist-ungranted-file=yes) else (echo op:exist-ungranted-file=no)
if exist "%BT_SYS32%\cmd.exe" (echo op:exist-system32=yes) else (echo op:exist-system32=no)
if exist "C:\" (echo op:exist-croot=yes) else (echo op:exist-croot=no)
if exist "C:\Windows" (echo op:exist-windows=yes) else (echo op:exist-windows=no)
if exist "%ProgramFiles%" (echo op:exist-programfiles=yes) else (echo op:exist-programfiles=no)
if exist "%USERPROFILE%\.ssh\id_rsa" (echo op:exist-ssh-key=yes) else (echo op:exist-ssh-key=no)
cd /d "%BT_DEEP%" && (echo op:cd-deep-abs=ok) || (echo op:cd-deep-abs=FAIL)
echo op:cwd-after-cd=%CD%
cd /d "%BT_WORK%" && (echo op:cd-back-abs=ok) || (echo op:cd-back-abs=FAIL)
cd deep && (echo op:cd-deep-rel=ok) || (echo op:cd-deep-rel=FAIL)
cd .. && (echo op:cd-up-rel=ok) || (echo op:cd-up-rel=FAIL)
echo op:cwd-final=%CD%
for %%F in (*.js) do echo op:glob-js=%%F
for %%F in (*.txt) do echo op:glob-txt=%%F
(dir /b /a-d "%BT_WORK%" > "%BT_TMP%\d1.txt") && (echo op:dir-granted-abs=ok) || (echo op:dir-granted-abs=FAIL)
(dir /b /a-d . > "%BT_TMP%\d2.txt") && (echo op:dir-granted-rel=ok) || (echo op:dir-granted-rel=FAIL)
(dir /b /a-d "%BT_UNGRANTED%" > "%BT_TMP%\d3.txt") && (echo op:dir-ungranted=ok) || (echo op:dir-ungranted=FAIL)
(type "%BT_WORK%\marker.txt" > "%BT_TMP%\t1.txt") && (echo op:type-granted=ok) || (echo op:type-granted=FAIL)
(type "%BT_UNGRANTED%\secret.txt" > "%BT_TMP%\t2.txt") && (echo op:type-ungranted=ok) || (echo op:type-ungranted=FAIL)
(echo REDIRECT-PAYLOAD>"%BT_WORK%\out_cmd.txt") && (echo op:redirect-abs=ok) || (echo op:redirect-abs=FAIL)
type "%BT_WORK%\out_cmd.txt"
(echo NUL-PAYLOAD>NUL) && (echo op:nul-stdout-redirect=ok) || (echo op:nul-stdout-redirect=FAIL)
(echo NUL-PAYLOAD 2>NUL) && (echo op:nul-stderr-redirect=ok) || (echo op:nul-stderr-redirect=FAIL)
(mkdir "%BT_WORK%\sub_cmd") && (echo op:mkdir-abs=ok) || (echo op:mkdir-abs=FAIL)
(copy /y "%BT_WORK%\marker.txt" "%BT_WORK%\copy_cmd.txt" > "%BT_TMP%\c1.txt") && (echo op:copy-abs=ok) || (echo op:copy-abs=FAIL)
echo op:end=ok
'@

$batCmdSpawn = @'
@echo off
"%BT_SYS32%\whoami.exe"
echo op:whoami-rc=%ERRORLEVEL%
"%BT_SYS32%\where.exe" cmd.exe
echo op:where-rc=%ERRORLEVEL%
call "%BT_WORK%\fakebin.cmd"
echo op:call-batch-rc=%ERRORLEVEL%
"%BT_NODE%" --preserve-symlinks-main --preserve-symlinks "%BT_WORK%\hello.js"
echo op:node-rc=%ERRORLEVEL%
"%BT_NODE%" --preserve-symlinks-main --preserve-symlinks "%BT_WORK%\nosuch.js"
echo op:node-fail-rc-must-be-nonzero=%ERRORLEVEL%
"%BT_UNGRANTED%\nosuchtool.exe"
echo op:missing-exe-rc-must-be-nonzero=%ERRORLEVEL%
echo PIPE-PAYLOAD | "%BT_SYS32%\findstr.exe" PIPE
echo op:pipe-rc=%ERRORLEVEL%
echo op:end=ok
'@

# `[IO.DirectoryInfo]::GetAccessControl('C:\')` is one of the THREE APIs Microsoft names by hand.
# PowerShell is the only shell here that can call it directly, so this arm tests their claim at the
# API level and at the shell level in the same child.
$batPs = @'
$ErrorActionPreference = 'Continue'
function Op($k, $v) { Write-Output "op:$k=$v" }
# `Stop` INSIDE the wrapper only: a denial from most cmdlets is a NON-terminating error, which
# `try/catch` never sees under `Continue` — the op would print empty and read as a wrong answer
# rather than a refusal. Raising it only here keeps the un-wrapped ops (Test-Path, which reports
# `False` on a denial rather than throwing) behaving as a shell user would see them.
function Try1($k, $sb) { try { $ErrorActionPreference = 'Stop'; Op $k (& $sb) } catch { Op $k ("ERR " + $_.Exception.GetType().Name) } }
Op 'echo' 'ok'
Op 'cwd' (Get-Location).Path
Op 'test-path-granted-file' (Test-Path -LiteralPath "$env:BT_WORK\marker.txt")
Op 'test-path-ungranted-file' (Test-Path -LiteralPath "$env:BT_UNGRANTED\secret.txt")
Op 'test-path-system32' (Test-Path -LiteralPath "$env:BT_SYS32\cmd.exe")
Op 'test-path-croot' (Test-Path -LiteralPath 'C:\')
Op 'test-path-windows' (Test-Path -LiteralPath 'C:\Windows')
Op 'test-path-ssh-key' (Test-Path -LiteralPath "$env:USERPROFILE\.ssh\id_rsa")
Try1 'resolve-path-granted' { (Resolve-Path -LiteralPath "$env:BT_WORK\marker.txt").Path }
Try1 'resolve-path-croot' { (Resolve-Path -LiteralPath 'C:\').Path }
Try1 'set-location-deep' { Set-Location -LiteralPath $env:BT_DEEP; (Get-Location).Path }
Try1 'gci-after-set-location' { ((Get-ChildItem -Name) | Sort-Object) -join ',' }
Try1 'set-location-back' { Set-Location -LiteralPath $env:BT_WORK; (Get-Location).Path }
Try1 'gci-granted' { ((Get-ChildItem -LiteralPath $env:BT_WORK -Name -File) | Sort-Object) -join ',' }
Try1 'gci-ungranted' { ((Get-ChildItem -LiteralPath $env:BT_UNGRANTED -Name -File) | Sort-Object) -join ',' }
Try1 'gci-glob' { ((Get-ChildItem -Path "$env:BT_WORK\*.js" -Name) | Sort-Object) -join ',' }
Try1 'get-content-granted' { (Get-Content -LiteralPath "$env:BT_WORK\marker.txt" -Raw).Trim() }
Try1 'get-content-ungranted' { (Get-Content -LiteralPath "$env:BT_UNGRANTED\secret.txt" -Raw).Trim() }
Try1 'dotnet-readalltext' { [IO.File]::ReadAllText("$env:BT_WORK\marker.txt").Trim() }
Try1 'dotnet-getfileattributes-croot' { [IO.File]::GetAttributes('C:\').ToString() }
Try1 'dotnet-getaccesscontrol-croot' { ([IO.DirectoryInfo]'C:\').GetAccessControl().Owner }
Try1 'dotnet-getaccesscontrol-granted' { ([IO.DirectoryInfo]$env:BT_WORK).GetAccessControl().Owner }
Try1 'set-content-roundtrip' { Set-Content -LiteralPath "$env:BT_WORK\out_ps.txt" -Value 'PS-PAYLOAD'; (Get-Content -LiteralPath "$env:BT_WORK\out_ps.txt" -Raw).Trim() }
Try1 'new-item-dir' { (New-Item -ItemType Directory -Force -Path "$env:BT_WORK\sub_ps").Name }
Try1 'out-null-redirect' { 'x' > $null; 'ok' }
Try1 'get-command-node' { (Get-Command node -ErrorAction Stop).Source }
Try1 'temp-writable' { $f = Join-Path $env:TEMP "ps-$PID.txt"; Set-Content -LiteralPath $f -Value 'T'; (Get-Content -LiteralPath $f -Raw).Trim() }
Op 'end' 'ok'
'@

$batPsSpawn = @'
$ErrorActionPreference = 'Continue'
function Op($k, $v) { Write-Output "op:$k=$v" }
& "$env:BT_SYS32\whoami.exe"
Op 'whoami-rc' $LASTEXITCODE
& "$env:BT_NODE" --preserve-symlinks-main --preserve-symlinks "$env:BT_WORK\hello.js"
Op 'node-rc' $LASTEXITCODE
& "$env:BT_NODE" --preserve-symlinks-main --preserve-symlinks "$env:BT_WORK\nosuch.js" 2>&1 | Out-Null
Op 'node-fail-rc-must-be-nonzero' $LASTEXITCODE
& "$env:BT_SYS32\cmd.exe" /d /s /c "echo CMD-FROM-PS"
Op 'spawn-cmd-rc' $LASTEXITCODE
Write-Output 'PIPE-PAYLOAD' | & "$env:BT_SYS32\findstr.exe" PIPE
Op 'pipe-rc' $LASTEXITCODE
Op 'end' 'ok'
'@

# busybox-w32 builtins only. `/dev/null` is busybox's mapping onto the `NUL` device, which
# Microsoft ships a SECOND host-prep verb for (`prepare-null-device`: on builds predating
# `Feature_AgenticAppContainerBfsSupport` an AppContainer cannot open `\Device\Null`, and the
# descriptor resets at every boot). A lifecycle script redirecting to `/dev/null` is the modal
# shape, so this cell is the one that would make that host-prep bind on nub too.
$batSh = @'
echo op:echo=ok
echo op:pwd=$PWD
if [ -f "$BT_WORK/marker.txt" ]; then echo op:exist-granted-file=yes; else echo op:exist-granted-file=no; fi
if [ -f "$BT_UNGRANTED/secret.txt" ]; then echo op:exist-ungranted-file=yes; else echo op:exist-ungranted-file=no; fi
if [ -d "$BT_SYS32" ]; then echo op:exist-system32=yes; else echo op:exist-system32=no; fi
if [ -d "C:/" ]; then echo op:exist-croot=yes; else echo op:exist-croot=no; fi
if [ -f "$USERPROFILE/.ssh/id_rsa" ]; then echo op:exist-ssh-key=yes; else echo op:exist-ssh-key=no; fi
cd "$BT_DEEP"; echo op:cd-deep-rc=$?
echo op:pwd-after-cd=$PWD
cd "$BT_WORK"; echo op:cd-back-rc=$?
for f in *.js; do echo op:glob-js=$f; done
for f in *.txt; do echo op:glob-txt=$f; done
echo REDIRECT-PAYLOAD > "$BT_WORK/out_sh.txt"; echo op:redirect-rc=$?
echo NUL-PAYLOAD > /dev/null; echo op:devnull-redirect-rc=$?
read line < "$BT_WORK/marker.txt"; echo op:read-granted=$line
echo op:temp-is-granted=$TEMP
echo op:end=ok
# LAST, deliberately: a failed input redirection is fatal in some ash builds, and losing the table
# after it would read as a hang rather than as a denial.
read line2 < "$BT_UNGRANTED/secret.txt"; echo op:read-ungranted-rc=$?
'@

$batShSpawn = @'
"$BT_SYS32/whoami.exe"; echo op:whoami-rc=$?
"$BT_NODE" --preserve-symlinks-main --preserve-symlinks "$BT_WORK/hello.js"; echo op:node-rc=$?
"$BT_NODE" --preserve-symlinks-main --preserve-symlinks "$BT_WORK/nosuch.js"; echo op:node-fail-rc-must-be-nonzero=$?
"$BT_UNGRANTED/nosuchtool.exe"; echo op:missing-exe-rc-must-be-nonzero=$?
sub=$(echo SUBSHELL-PAYLOAD); echo op:cmdsub=$sub
echo PIPE-PAYLOAD | "$BT_SYS32/findstr.exe" PIPE; echo op:pipe-rc=$?
sh "$BT_WORK/fakebin"; echo op:sh-shim-rc=$?
echo op:end=ok
'@

# python's `DLLs\` subdirectory is what `import ssl` / `import sqlite3` / `import ctypes` reach, so
# these imports are the cell that distinguishes "the exe loaded" from "python is usable". `gyp`
# itself needs `subprocess`, `re`, `shlex` and `os` only, but node-gyp's generated build reaches
# further, and a partially-importable python is exactly the silent failure this probe hunts.
$batPy = @'
import os, sys
def op(k, v): print('op:%s=%s' % (k, v))
op('version', sys.version.split()[0])
op('executable', sys.executable)
op('cwd', os.getcwd())
for m in ('os', 're', 'shlex', 'json', 'subprocess', 'ctypes', 'zlib', 'hashlib', 'sqlite3', 'ssl'):
    try:
        __import__(m); op('import-' + m, 'ok')
    except Exception as e:
        op('import-' + m, 'ERR ' + type(e).__name__)
for label, path in (('granted', os.path.join(os.environ['BT_WORK'], 'marker.txt')),
                    ('ungranted', os.path.join(os.environ['BT_UNGRANTED'], 'secret.txt'))):
    try:
        with open(path) as f: op('read-' + label, f.read().strip())
    except Exception as e:
        op('read-' + label, 'ERR ' + type(e).__name__)
for label, path in (('croot', 'C:\\'), ('system32', os.environ['BT_SYS32']), ('granted', os.environ['BT_WORK'])):
    try:
        os.stat(path); op('stat-' + label, 'ok')
    except Exception as e:
        op('stat-' + label, 'ERR ' + type(e).__name__)
try:
    with open(os.path.join(os.environ['BT_WORK'], 'out_py.txt'), 'w') as f: f.write('PY-PAYLOAD')
    op('write-granted', 'ok')
except Exception as e:
    op('write-granted', 'ERR ' + type(e).__name__)
try:
    os.chdir(os.environ['BT_DEEP']); op('chdir-deep', os.getcwd())
except Exception as e:
    op('chdir-deep', 'ERR ' + type(e).__name__)
try:
    op('listdir-granted', ','.join(sorted(os.listdir(os.environ['BT_WORK']))))
except Exception as e:
    op('listdir-granted', 'ERR ' + type(e).__name__)
import subprocess
try:
    r = subprocess.run([sys.executable, '-c', 'print("SUBPROC-PAYLOAD")'],
                       capture_output=True, text=True, timeout=20)
    op('subprocess-rc', r.returncode)
    op('subprocess-stdout', r.stdout.strip())
except Exception as e:
    op('subprocess-rc', 'ERR ' + type(e).__name__)
op('end', 'ok')
'@

# RESET BEFORE EVERY ARM, not once. The batteries WRITE into `$workDir` (`out_cmd.txt`, `sub_cmd`,
# a copy), so an arm that runs second sees the first arm's leftovers and every directory-listing and
# glob op diffs for a reason that has nothing to do with the token. Run 1 of this probe reported ~20
# such lines as findings before this existed — a diff is only evidence if the two arms started from
# byte-identical trees.
function Reset-Work {
  Get-ChildItem -LiteralPath $workDir -Force -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
  Get-ChildItem -LiteralPath $tmpDir -Force -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path $deepDir | Out-Null
  [IO.File]::WriteAllText((Join-Path $workDir 'marker.txt'), "MARKER-CONTENT`n")
  [IO.File]::WriteAllText((Join-Path $workDir 'hello.js'), "console.log('HELLO-JS');`n")
  [IO.File]::WriteAllText((Join-Path $deepDir 'deep.txt'), "DEEP-CONTENT`n")
  [IO.File]::WriteAllText((Join-Path $workDir 'fakebin.cmd'), "@echo off`r`necho FAKEBIN-CMD`r`n")
  [IO.File]::WriteAllText((Join-Path $workDir 'fakebin'), "echo FAKEBIN-SH`n")
  [IO.File]::WriteAllText((Join-Path $workDir 'bat.cmd'), ($batCmd -replace "`r?`n", "`r`n"))
  [IO.File]::WriteAllText((Join-Path $workDir 'batspawn.cmd'), ($batCmdSpawn -replace "`r?`n", "`r`n"))
  [IO.File]::WriteAllText((Join-Path $workDir 'bat.ps1'), $batPs)
  [IO.File]::WriteAllText((Join-Path $workDir 'batspawn.ps1'), $batPsSpawn)
  [IO.File]::WriteAllText((Join-Path $workDir 'bat.sh'), ($batSh -replace "`r", ''))
  [IO.File]::WriteAllText((Join-Path $workDir 'batspawn.sh'), ($batShSpawn -replace "`r", ''))
  [IO.File]::WriteAllText((Join-Path $workDir 'bat.py'), $batPy)
}
Reset-Work

# ─────────────────────────────── ace plumbing ───────────────────────────────

function Grant-Ace([string]$path, [string]$sid, [string]$rights, [bool]$inheritable) {
  $acl = Get-Acl -LiteralPath $path
  $inherit = if ($inheritable) {
    [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit
  } else { [Security.AccessControl.InheritanceFlags]::None }
  $rule = New-Object Security.AccessControl.FileSystemAccessRule(
    (New-Object Security.Principal.SecurityIdentifier($sid)),
    [Security.AccessControl.FileSystemRights]$rights, $inherit,
    [Security.AccessControl.PropagationFlags]::None,
    [Security.AccessControl.AccessControlType]::Allow)
  $acl.AddAccessRule($rule)
  Set-Acl -LiteralPath $path -AclObject $acl
}
function Revoke-Ace([string]$path, [string]$sid) {
  $acl = Get-Acl -LiteralPath $path
  $null = $acl.PurgeAccessRules((New-Object Security.Principal.SecurityIdentifier($sid)))
  Set-Acl -LiteralPath $path -AclObject $acl
}

$logDir = Join-Path (Get-Location) 'works-logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

$cells = @{}   # $cells[<battery>][<arm>] = @{ rc; lines; ms }

function Invoke-Battery {
  param(
    [string]$Battery,
    [string]$Arm,            # 'plain' | 'ac' | 'ac-ancestors' | 'ac-croot' | 'ac-pygrant'
    [string]$Exe,
    [string]$Cmdline,
    [bool]$AppContainer,
    [bool]$Lpac = $false,
    [string[]]$ExtraGrants = @(),
    # NON-INHERITABLE metadata+list aces on ancestor DIRECTORIES. Separate from `ExtraGrants`
    # (inheritable, for a tool's install tree) because an inheritable ace on `C:\` would propagate
    # across the whole volume. `$Privileged` marks the two that need WRITE_DAC on paths the user does
    # not own — the disqualifying setup step, measured so its necessity is a fact and not a guess.
    [string[]]$AncestorGrants = @(),
    [int]$TimeoutMs = 20000
  )
  if (-not $cells.ContainsKey($Battery)) { $cells[$Battery] = @{} }
  Reset-Work
  if (-not $Exe -or -not (Test-Path -LiteralPath $Exe)) {
    $cells[$Battery][$Arm] = @{ rc = 'TOOL-ABSENT'; lines = @(); ms = 0 }
    return
  }
  $sid = ''; $profileName = ''; $granted = @()
  try {
    if ($AppContainer) {
      $nm = "nubwk_${nonce}_$(($Battery + '_' + $Arm) -replace '[^A-Za-z0-9]','_')"
      if ($nm.Length -gt 60) { $nm = $nm.Substring(0, 60) }
      $profileName = $nm
      $sid = [Bt]::CreateProfile($profileName)
      if ($sid -like 'ERR*') {
        $cells[$Battery][$Arm] = @{ rc = "PROFILE-$sid"; lines = @(); ms = 0 }
        $profileName = ''
        return
      }
      Grant-Ace $binDir  $sid 'ReadAndExecute' $true; $granted += $binDir
      Grant-Ace $workDir $sid 'Modify'         $true; $granted += $workDir
      Grant-Ace $tmpDir  $sid 'Modify'         $true; $granted += $tmpDir
      foreach ($g in $ExtraGrants) {
        $sw2 = [Diagnostics.Stopwatch]::StartNew()
        Grant-Ace $g $sid 'ReadAndExecute' $true
        $sw2.Stop()
        $granted += $g
        Fact "grant-cost-ms[$Battery/$Arm/$g]" $sw2.ElapsedMilliseconds
      }
      foreach ($g in $AncestorGrants) {
        try {
          $sw3 = [Diagnostics.Stopwatch]::StartNew()
          Grant-Ace $g $sid 'ReadAndExecute' $false
          $sw3.Stop()
          $granted += $g
          Fact "ancestor-grant-cost-ms[$Battery/$Arm/$g]" $sw3.ElapsedMilliseconds
        } catch { W "    ancestor grant on $g FAILED (needs elevation?): $_" }
      }
    }
    # TEMP/TMP point at the granted dir in EVERY arm, plain included: making it a variable would
    # show up as a diff line that has nothing to do with the token.
    $prev = @{ TEMP = $env:TEMP; TMP = $env:TMP; PATH = $env:PATH }
    try {
      $env:TEMP = $tmpDir; $env:TMP = $tmpDir
      $env:PATH = "$binDir;$sys32;$($prev.PATH)"
      $log = Join-Path $logDir "$Battery.$Arm.log"
      $sw = [Diagnostics.Stopwatch]::StartNew()
      $status = [Bt]::LaunchEx($sid, $Exe, $Cmdline, $workDir, $log, $TimeoutMs, $Lpac)
      $sw.Stop()
      $lines = @()
      if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue) }
      $cells[$Battery][$Arm] = @{ rc = $status; lines = $lines; ms = $sw.ElapsedMilliseconds }
      W ("  {0,-14} {1,-11} {2,-34} lines={3} {4}ms" -f $Battery, $Arm, $status, $lines.Count, $sw.ElapsedMilliseconds)
    }
    finally { $env:TEMP = $prev.TEMP; $env:TMP = $prev.TMP; $env:PATH = $prev.PATH }
  }
  finally {
    foreach ($p in $granted) {
      $swr = [Diagnostics.Stopwatch]::StartNew()
      try { Revoke-Ace $p $sid } catch { W "    revoke failed on $p : $_" }
      $swr.Stop()
      if ($ExtraGrants -contains $p) { Fact "revoke-cost-ms[$Battery/$Arm/$p]" $swr.ElapsedMilliseconds }
    }
    if ($profileName) { $null = [Bt]::DeleteProfile($profileName) }
  }
}

# ─────────────────────────────── tool discovery ───────────────────────────────

$cmdExe  = Join-Path $sys32 'cmd.exe'
$psExe   = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$pwshExe = ''; $c = Get-Command pwsh -ErrorAction SilentlyContinue; if ($c) { $pwshExe = $c.Source }
$pyExe   = ''; $c = Get-Command python -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source }
if (-not $pyExe) { $c = Get-Command python3 -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source } }

$env:BT_WORK = $workDir; $env:BT_DEEP = $deepDir; $env:BT_UNGRANTED = $ungranted
$env:BT_SYS32 = $sys32;  $env:BT_NODE = $jailNode; $env:BT_TMP = $tmpDir

${q} = '"'
$batteries = @(
  # battery, exe, cmdline, timeoutMs
  @('cmd',      $cmdExe,      "${q}$cmdExe${q} /d /s /c ${q}${q}$workDir\bat.cmd${q}${q}", 20000),
  @('cmdspawn', $cmdExe,      "${q}$cmdExe${q} /d /s /c ${q}${q}$workDir\batspawn.cmd${q}${q}", 25000),
  @('ps',       $psExe,       "${q}$psExe${q} -NoLogo -NonInteractive -NoProfile -ExecutionPolicy Bypass -File ${q}$workDir\bat.ps1${q}", 40000),
  @('psspawn',  $psExe,       "${q}$psExe${q} -NoLogo -NonInteractive -NoProfile -ExecutionPolicy Bypass -File ${q}$workDir\batspawn.ps1${q}", 40000),
  # busybox gets RELATIVE script names: the launch cwd is `$workDir`, and a bare name avoids any
  # question about how busybox-w32's ash parses a drive-letter path as `$0`.
  @('sh',       $jailBusybox, "${q}$jailBusybox${q} sh bat.sh", 20000),
  @('shspawn',  $jailBusybox, "${q}$jailBusybox${q} sh batspawn.sh", 25000)
)

W ''
W '== tool discovery =='
foreach ($kv in @(@('cmd.exe', $cmdExe), @('powershell.exe', $psExe), @('pwsh.exe', $pwshExe),
                  @('busybox.exe', $jailBusybox), @('node.exe', $jailNode), @('python.exe', $pyExe))) {
  Fact "tool[$($kv[0])]" $(if ($kv[1] -and (Test-Path -LiteralPath $kv[1])) { $kv[1] } else { 'ABSENT' })
}

# ─────────────────────────────── PART 1: the operation batteries ───────────────────────────────

W ''
W '== PLAIN arms (control: identical child, identical script, no SECURITY_CAPABILITIES, no aces) =='
foreach ($b in $batteries) { Invoke-Battery -Battery $b[0] -Arm 'plain' -Exe $b[1] -Cmdline $b[2] -AppContainer $false -TimeoutMs $b[3] }

W ''
W '== APPCONTAINER arms (ordinary LowBox, zero capabilities; C:\ and C:\Users untouched) =='
foreach ($b in $batteries) { Invoke-Battery -Battery $b[0] -Arm 'ac' -Exe $b[1] -Cmdline $b[2] -AppContainer $true -TimeoutMs $b[3] }

# LPAC IS SETTLED AND NO LONGER MEASURED. Run 30518533077 killed it on both images: cmd.exe
# produced 0 bytes rc=1, powershell.exe died "Cannot open registry key SOFTWARE\Microsoft\PowerShell.
# Access is denied.", busybox died "WSAStartup failed, error 18". An LPAC stops honouring the
# ALL APPLICATION PACKAGES aces that blanket System32, so nothing starts. Recorded as a rejected
# mechanism; the arm is gone rather than kept red, and the launcher keeps its `LaunchEx` LPAC support
# so re-measuring is one argument away.

# ── THE ATTRIBUTION THAT DECIDES THE PRODUCT QUESTION.
# Run 1 measured a confined `cmd.exe` failing `cd` into a GRANTED directory, reporting a granted
# DIRECTORY as nonexistent, and answering `if exist C:\Windows` with `no` — all while exiting 0.
# Busybox's `cd` to the same path SUCCEEDED in the same run, so the cause is not a kernel traverse
# limit; something cmd does reaches an ancestor as a TARGET, which bypass-traverse does not cover.
# Which ancestors decides everything:
#   `ac-ancestors` grants the chain nub OWNS — the fixture root and `%USERPROFILE%`. Both are
#     ordinary owner DACL writes: NO elevation, so if this arm is clean the build jail needs no
#     setup step and Microsoft's `prepare-system-drive` does not bind on nub.
#   `ac-croot` additionally aces `C:\` and `C:\Users`, which DOES need WRITE_DAC on paths the user
#     does not own — i.e. Microsoft's one-time host-wide step. If cmd only behaves in THIS arm, the
#     AppContainer route requires a privileged setup and that is a disqualifying finding.
# Non-inheritable throughout, and revoked in `finally`; the primary arms above never touch either.
W ''
W '== ANCESTOR-GRANT arms (unprivileged: the chain nub owns) =='
foreach ($b in @('cmd')) {
  $m = $batteries | Where-Object { $_[0] -eq $b } | Select-Object -First 1
  if ($m) {
    Invoke-Battery -Battery $m[0] -Arm 'ac-ancestors' -Exe $m[1] -Cmdline $m[2] -AppContainer $true `
      -AncestorGrants @($root, $env:USERPROFILE) -TimeoutMs $m[3]
  }
}

W ''
W '== C-ROOT arms (PRIVILEGED: adds Microsoft`s prepare-system-drive shape on C:\ + C:\Users) =='
foreach ($b in @('cmd')) {
  $m = $batteries | Where-Object { $_[0] -eq $b } | Select-Object -First 1
  if ($m) {
    Invoke-Battery -Battery $m[0] -Arm 'ac-croot' -Exe $m[1] -Cmdline $m[2] -AppContainer $true `
      -AncestorGrants @($root, $env:USERPROFILE, 'C:\Users', 'C:\') -TimeoutMs $m[3]
  }
}

# ─────────────────────────────── PART 3: python ───────────────────────────────

W ''
W '== python: where it lives, and what its tree grants =='
$pyDir = ''
if ($pyExe) {
  $pyDir = Split-Path -Parent $pyExe
  Fact 'py/exe' $pyExe
  Fact 'py/dir' $pyDir
  Fact 'py/is-store-shim' ($pyExe -like '*\WindowsApps\*')
  $lnk = Get-Item -LiteralPath $pyExe -Force
  Fact 'py/exe-attributes' $lnk.Attributes
  Fact 'py/exe-is-reparse-point' (($lnk.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)
  Fact 'py/exe-size' $lnk.Length
  Fact 'py/dlls-beside-exe' ((Get-ChildItem -LiteralPath $pyDir -Filter '*.dll' -File -ErrorAction SilentlyContinue |
    ForEach-Object { $_.Name }) -join ',')
  Fact 'py/has-DLLs-subdir' (Test-Path -LiteralPath (Join-Path $pyDir 'DLLs'))
  # THE DECIDING FACT for the "ungranted install dir" theory: does anything on the path from the
  # volume root down to python's own directory already grant an AppContainer sid? If the tree
  # ALREADY carries `ALL APPLICATION PACKAGES` then an ungranted-directory theory is FALSE and the
  # loader failure has some other cause.
  $p = $pyDir
  while ($p) {
    $aces = @()
    try { $aces = @((Get-Acl -LiteralPath $p).Access | Where-Object { $_.IdentityReference.Value -like 'S-1-15-2-*' -or $_.IdentityReference.Value -like '*APPLICATION PACKAGES*' }) } catch { }
    Fact "py/ancestor-aap[$p]" $(if ($aces.Count) { ($aces | ForEach-Object { "$($_.IdentityReference.Value):$($_.FileSystemRights)" }) -join ' | ' } else { 'NONE' })
    $next = Split-Path -Parent $p
    if ($next -eq $p -or -not $next) { break }
    $p = $next
  }
  foreach ($t in @($pyExe, (Join-Path $pyDir 'DLLs'))) {
    if (Test-Path -LiteralPath $t) {
      W "  fact:icacls[$t] ="
      (icacls $t 2>&1) | ForEach-Object { W "      $_" }
    }
  }
  # The other python shapes a developer box actually has, reported so the grant mechanism can be
  # designed against real layouts rather than against the runner's `hostedtoolcache` one.
  $storeShim = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\python.exe'
  Fact 'py/store-shim-present' (Test-Path -LiteralPath $storeShim)
  if (Test-Path -LiteralPath $storeShim) {
    $si = Get-Item -LiteralPath $storeShim -Force
    Fact 'py/store-shim-attributes' $si.Attributes
    Fact 'py/store-shim-size' $si.Length
  }
  $pyLauncher = Join-Path $sys32 'py.exe'
  Fact 'py/py-launcher-present' (Test-Path -LiteralPath $pyLauncher)
  Fact 'py/PYTHON-env' $(if ($env:PYTHON) { $env:PYTHON } else { '(unset)' })
} else {
  Fact 'py/exe' 'ABSENT'
}

if ($pyExe) {
  $pyCmd = "${q}$pyExe${q} ${q}$workDir\bat.py${q}"
  W ''
  W '== python arms =='
  Invoke-Battery -Battery 'py' -Arm 'plain' -Exe $pyExe -Cmdline $pyCmd -AppContainer $false -TimeoutMs 60000
  Invoke-Battery -Battery 'py' -Arm 'ac' -Exe $pyExe -Cmdline $pyCmd -AppContainer $true -TimeoutMs 60000
  # THE GRANT DIFFERENTIAL: one inheritable ReadAndExecute ace on python's install root, nothing
  # else changed off the `ac` arm. Its cost is reported by `Invoke-Battery`.
  Invoke-Battery -Battery 'py' -Arm 'ac-pygrant' -Exe $pyExe -Cmdline $pyCmd -AppContainer $true `
    -ExtraGrants @($pyDir) -TimeoutMs 60000
}

# ─────────────────────────────── the diff ───────────────────────────────
# The measurement. Not "did it start" — "is the confined log the same log".

function ArmLines([string]$b, [string]$a) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey($a)) { return $null }
  return $cells[$b][$a].lines
}
function DiffCount([string]$b, [string]$baseArm, [string]$arm) {
  $x = ArmLines $b $baseArm; $y = ArmLines $b $arm
  if ($null -eq $x -or $null -eq $y) { return -1 }
  $n = [Math]::Max($x.Count, $y.Count)
  $d = 0
  for ($i = 0; $i -lt $n; $i++) {
    $lx = if ($i -lt $x.Count) { $x[$i] } else { '<absent>' }
    $ly = if ($i -lt $y.Count) { $y[$i] } else { '<absent>' }
    if ($lx -ne $ly) { $d++ }
  }
  return $d
}
function ShowDiff([string]$b, [string]$arm) {
  $x = ArmLines $b 'plain'; $y = ArmLines $b $arm
  if ($null -eq $x -or $null -eq $y) { W "    (arm missing: plain=$($null -ne $x) $arm=$($null -ne $y))"; return }
  $n = [Math]::Max($x.Count, $y.Count)
  for ($i = 0; $i -lt $n; $i++) {
    $lx = if ($i -lt $x.Count) { $x[$i] } else { '<absent>' }
    $ly = if ($i -lt $y.Count) { $y[$i] } else { '<absent>' }
    if ($lx -ne $ly) {
      W "    L$($i + 1) plain : $lx"
      W "    L$($i + 1) $arm : $ly"
    }
  }
}

W ''
W '== the diff table (0 = the confined log is byte-identical to the unconfined log) =='
$armsToDiff = @('ac', 'ac-ancestors', 'ac-croot', 'ac-pygrant')
W ("  {0,-12} {1,-8} {2}" -f 'battery', 'plain-n', (($armsToDiff | ForEach-Object { '{0,-16}' -f $_ }) -join ''))
foreach ($b in (@($batteries | ForEach-Object { $_[0] }) + @('py'))) {
  if (-not $cells.ContainsKey($b)) { continue }
  $pl = ArmLines $b 'plain'
  W ("  {0,-12} {1,-8} {2}" -f $b, $(if ($null -ne $pl) { $pl.Count } else { '-' }),
    (($armsToDiff | ForEach-Object { '{0,-16}' -f $(if ($cells[$b].ContainsKey($_)) { "diff=$(DiffCount $b 'plain' $_)" } else { '-' }) }) -join ''))
}

foreach ($b in (@($batteries | ForEach-Object { $_[0] }) + @('py'))) {
  if (-not $cells.ContainsKey($b)) { continue }
  foreach ($a in $armsToDiff) {
    if (-not $cells[$b].ContainsKey($a)) { continue }
    $d = DiffCount $b 'plain' $a
    if ($d -ne 0) {
      W ''
      W "  ── DIFF $b : plain vs $a ($d differing lines; rc plain=$($cells[$b]['plain'].rc) $a=$($cells[$b][$a].rc)) ──"
      ShowDiff $b $a
    }
  }
}

# ─────────────────────────────── verdict ───────────────────────────────

W ''
W '== verdict =='

# ── CONTROLS. A red one means the harness or the host is the story and no row below is attributable.
$plainDead = @()
foreach ($b in (@($batteries | ForEach-Object { $_[0] }) + @('py'))) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey('plain')) { continue }
  if ($cells[$b]['plain'].rc -eq 'TOOL-ABSENT') { continue }
  if ($cells[$b]['plain'].lines.Count -eq 0) { $plainDead += $b }
}
Prop 'baseline-every-plain-battery-produced-output' ($plainDead.Count -eq 0) `
  "unconfined arms must all emit their table; empty: $(if ($plainDead.Count) { $plainDead -join ' ' } else { '(none)' })"

# The gate. `cmd`'s `exist-croot` is asked in the CONFINED child, so if it still answers `yes` the
# metadata refusal Microsoft describes is not happening here and every 'identical' below is vacuous.
$acCmd = (ArmLines 'cmd' 'ac') -join "`n"
$plainCmd = (ArmLines 'cmd' 'plain') -join "`n"
Prop 'appcontainer-gate-is-live' `
  (($acCmd -match 'op:exist-ungranted-file=no') -and ($plainCmd -match 'op:exist-ungranted-file=yes')) `
  "the confined child must be refused the ungranted sibling while the unconfined one reads it — else the token is not confined"
Prop 'appcontainer-positive-control' ($acCmd -match 'op:exist-system32=yes') `
  "System32 carries an ALL APPLICATION PACKAGES ace, so a confined child must still see it: a column of denials would otherwise be about a dead child"

# ── THE QUESTION. One property per shell: confined behaviour must be INDISTINGUISHABLE from
# unconfined. A FAIL here is the answer, not a broken run — read the diff block above it.
foreach ($b in @('cmd', 'cmdspawn', 'ps', 'psspawn', 'sh', 'shspawn')) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey('ac')) { continue }
  $d = DiffCount $b 'plain' 'ac'
  Prop "confined-$b-behaves-identically" ($d -eq 0) `
    "byte-diff vs the unconfined arm: $d differing lines (rc plain=$($cells[$b]['plain'].rc) ac=$($cells[$b]['ac'].rc))"
}

# ── THE DISQUALIFYING QUESTION, stated as one property per grant tier. A green `ac-ancestors` means
# the confined-cmd defects are fixable with DACL writes nub can do UNPRIVILEGED, and Microsoft's
# one-time host-wide `prepare-system-drive` does not bind on nub. A red `ac-ancestors` with a green
# `ac-croot` is the opposite answer and would force a rethink of the whole Windows route.
foreach ($b in @('cmd', 'ps', 'sh')) {
  if (-not $cells.ContainsKey($b)) { continue }
  foreach ($tier in @('ac-ancestors', 'ac-croot')) {
    if (-not $cells[$b].ContainsKey($tier)) { continue }
    Prop "$tier-makes-$b-behave-identically" ((DiffCount $b 'plain' $tier) -eq 0) `
      "$tier byte-diff vs unconfined: $(DiffCount $b 'plain' $tier) (ac was $(DiffCount $b 'plain' 'ac'))"
  }
}

# ── THE NULL DEVICE, which Microsoft ships a SECOND host-prep verb for and which resets at every
# boot. `>NUL` and `>/dev/null` are the modal redirection in a lifecycle script, and under cmd a
# denied `>NUL` does not even set a non-zero errorlevel — the shell just skips the command.
$acSh = (ArmLines 'sh' 'ac') -join "`n"
Prop 'nul-redirection-works-confined' `
  (($acCmd -match 'op:nul-stdout-redirect=ok') -and ($acCmd -match 'op:nul-stderr-redirect=ok')) `
  "cmd `>NUL` / `2>NUL`: $(($acCmd -split "`n" | Select-String -Pattern 'op:nul-' -SimpleMatch) -join ' ')"
Prop 'devnull-redirection-works-confined' ($acSh -match 'op:devnull-redirect-rc=0') `
  "busybox `>/dev/null`: $(($acSh -split "`n" | Select-String -Pattern 'op:devnull' -SimpleMatch) -join ' ')"

# ── MICROSOFT'S NAMED APIS, measured directly rather than inferred from whether a shell started.
$acPs = (ArmLines 'ps' 'ac') -join "`n"
$plainPs = (ArmLines 'ps' 'plain') -join "`n"
Fact 'ms-claim/getfileattributes-croot' "plain=$(($plainPs -split "`n" | Select-String -Pattern 'dotnet-getfileattributes-croot' -SimpleMatch) -join '') ac=$(($acPs -split "`n" | Select-String -Pattern 'dotnet-getfileattributes-croot' -SimpleMatch) -join '')"
Fact 'ms-claim/getaccesscontrol-croot' "plain=$(($plainPs -split "`n" | Select-String -Pattern 'dotnet-getaccesscontrol-croot' -SimpleMatch) -join '') ac=$(($acPs -split "`n" | Select-String -Pattern 'dotnet-getaccesscontrol-croot' -SimpleMatch) -join '')"
Fact 'ms-claim/cmd-if-exist-croot' "plain=$(($plainCmd -split "`n" | Select-String -Pattern 'op:exist-croot' -SimpleMatch) -join '') ac=$(($acCmd -split "`n" | Select-String -Pattern 'op:exist-croot' -SimpleMatch) -join '')"
# The reason a metadata refusal on `C:\` can be survivable at all: an AppContainer that still
# answers ordinary path questions correctly does not need the drive root. Reported, not asserted.
Prop 'ms-claim-api-refusal-reproduces' `
  ((($acPs -match 'dotnet-getaccesscontrol-croot=ERR') -or ($acPs -match 'dotnet-getfileattributes-croot=ERR'))) `
  "Microsoft's named metadata APIs on C:\ must be REFUSED in the confined child, or this probe is not reproducing the condition their host-prep addresses"
Prop 'ms-claim-api-refusal-is-token-not-host' `
  ((-not ($plainPs -match 'dotnet-getaccesscontrol-croot=ERR')) -and (-not ($plainPs -match 'dotnet-getfileattributes-croot=ERR'))) `
  "and the unconfined child must succeed at both, or the refusal is about the host rather than the LowBox token"


# ── PYTHON.
if ($cells.ContainsKey('py')) {
  Fact 'py/rc-plain' $cells['py']['plain'].rc
  Fact 'py/rc-ac-nogrant' $(if ($cells['py'].ContainsKey('ac')) { $cells['py']['ac'].rc } else { '-' })
  Fact 'py/rc-ac-pygrant' $(if ($cells['py'].ContainsKey('ac-pygrant')) { $cells['py']['ac-pygrant'].rc } else { '-' })
  Prop 'python-fails-without-a-grant' `
    ($cells['py'].ContainsKey('ac') -and ($cells['py']['ac'].lines.Count -eq 0)) `
    "the control half: confined python with NO grant on its install tree must produce nothing (rc=$(if ($cells['py'].ContainsKey('ac')) { $cells['py']['ac'].rc } else { '-' }))"
  Prop 'python-works-with-one-install-dir-grant' `
    ($cells['py'].ContainsKey('ac-pygrant') -and ((DiffCount 'py' 'plain' 'ac-pygrant') -eq 0)) `
    "one inheritable RX ace on python's install root must make the confined battery byte-identical: diff=$(if ($cells['py'].ContainsKey('ac-pygrant')) { DiffCount 'py' 'plain' 'ac-pygrant' } else { 'n/a' })"
}

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED (a clean negative also lands here — read the diff blocks)" }

Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
W ''
W 'WORKS PROBE END'
