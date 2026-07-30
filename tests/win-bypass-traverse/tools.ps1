# DO THE OTHER TOOLS npm LIFECYCLE SCRIPTS INVOKE ALSO DIE INSIDE AN APPCONTAINER?
#
# THE QUESTION, and why fixing Node is not obviously enough. `probe.ps1` (this directory) settled
# that bypass-traverse carries a DEEP read through an un-ACE'd `C:\`, and that the one thing that
# still kills a confined `node` is its JS realpath walk opening the volume ROOT as a target
# (MECHANISM-FACTS §5h). A parallel lane is fixing that. But Microsoft's own `mxc` host-prep doc
# (`.repos/mxc/docs/host-prep.md:62`) names FOUR binaries with the same habit, not one:
#
#   "Many common tools — cmd.exe, powershell.exe, pwsh.exe, node.exe — call APIs like
#    GetFileAttributesW("C:\\"), _stat("C:\\") ... during startup and fail with
#    ERROR_ACCESS_DENIED inside an AppContainer because the well-known AppContainer SIDs are not
#    granted any rights on the system-drive root by default."
#
# `cmd.exe` is on that list, and `vendor/aube/crates/aube-scripts/src/lib.rs:343` runs every
# dependency lifecycle script through `cmd.exe /d /s /c`. If cmd.exe cannot START, fixing Node buys
# nothing — so this probe tests the SHELL and the TOOLS, not the runtime. Every prior AppContainer
# arm launched `node <file>` directly and therefore never asked.
#
# WHY EACH TOOL IS THE DIRECT CHILD OF THE LAUNCH. `spawnSync` with piped stdio HANGS INDEFINITELY
# under an AppContainer (§5h, independently reproduced twice). Spawning these tools FROM Node would
# measure that hang instead of the tool, so every tool here is launched by `CreateProcessW` as the
# AppContainer's own first process. Two known confounds are removed BY CONSTRUCTION rather than
# argued away:
#   - stdio: the log handle and the `NUL` stdin handle are opened by the UNCONFINED parent and
#     inherited already-open (`launcher.ps1`), so `\Device\Null`'s descriptor excluding
#     AppContainer sids cannot be what fails. (A parallel lane owns that question; this one must
#     not be attributable to it.)
#   - `%TEMP%`: every AppContainer arm gets TEMP/TMP pointed at a GRANTED directory, because the
#     host `%LOCALAPPDATA%\Temp` is ungranted and produces broken-toolchain-shaped errors (§5e).
#     One arm deliberately keeps the host TEMP so the size of that confound is measured, not
#     assumed.
#
# THE ATTRIBUTION ARM IS THE POINT OF PART 3. A tool that fails tells you nothing about whether
# ONE fix covers the class. So every failing tool is re-run with a NON-INHERITABLE ReadAndExecute
# ace on `C:\` + `C:\Users` and nothing else changed. If it then starts, the failure IS the
# root-path class Microsoft describes — and since nub cannot write an ace on `C:\` unprivileged and
# cannot patch a Microsoft binary, that is a STRUCTURAL blocker, not a bug to fix. If it still
# fails, it is a different problem with a different remedy.
#   ⚠️ Those `diag-*` arms are the ONLY thing in this file that touches `C:\`. They are fenced,
#   NON-INHERITABLE (an inheritable ace on the volume root would propagate across the whole disk),
#   revoked in `finally`, and excluded from every primary property. The primary arms leave `C:\`
#   and `C:\Users` exactly as the image ships them, and both are read with `icacls` and reported.
#
# THE BUSYBOX CELL, which can make the cmd.exe answer moot. nub already VENDORS busybox-w32 and
# already uses it as the Windows script shell for `nub run` (`crates/nub-cli/src/cli.rs`,
# `resolve_bundled_busybox`); only DEPENDENCY lifecycle scripts still go through cmd.exe, and
# `script_shell` is a settings field, so substituting it is configuration rather than a fork. A
# single static Win32 binary has no reason to stat the drive root — but that is a hypothesis, so it
# is measured alongside cmd.exe rather than assumed.

Set-StrictMode -Off
$ErrorActionPreference = 'Continue'

. (Join-Path $PSScriptRoot 'verdict.ps1')   # W / Fact / Prop / $script:fails
. (Join-Path $PSScriptRoot 'launcher.ps1')  # [Bt]::CreateProfile / DeleteProfile / DeriveSid / Launch

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)

W '== host =='
Fact 'os' ((Get-CimInstance Win32_OperatingSystem).Caption)
Fact 'os-version' ([Environment]::OSVersion.VersionString)
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'whoami' (whoami)
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
# REPORTED BECAUSE TWO LANES DIFFERED ON IT and it invalidated a zero-setup claim. The primary
# arms cannot be elevation-dependent (no write above %USERPROFILE%), but the diag arms ACE `C:\`
# and that plainly does need it — so the reader must be able to see which context produced which.
Fact 'elevated' ($pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))
Fact 'session-id' (Get-Process -Id $PID).SessionId
Fact 'node-version' (& node --version 2>&1)

W ''
W '== the two descriptors this probe must not write (primary arms) =='
foreach ($p in @('C:\', 'C:\Users')) {
  W "  fact:icacls[$p] ="
  (icacls $p 2>&1) | ForEach-Object { W "      $_" }
}

# ─────────────────────────────── fixture ───────────────────────────────
# Mirrors probe.ps1: everything inside the invoking user's OWN profile, so every primary-arm ace is
# an ordinary owner operation needing no privilege.

$root    = Join-Path $env:USERPROFILE "nubtools-$nonce"
$binDir  = Join-Path $root 'bin'    # RX-granted: the tool binaries that are not already on an AAP path
$workDir = Join-Path $root 'work'   # Modify-granted: scripts, and anything a tool writes
$tmpDir  = Join-Path $root 'tmp'    # Modify-granted: TEMP/TMP for the confined child
foreach ($d in @($root, $binDir, $workDir, $tmpDir)) { New-Item -ItemType Directory -Force -Path $d | Out-Null }

# Strip any inherited AppContainer ace, or a "denied" cell could be a grant this probe did not
# write and a "started" cell could be one it did not intend. Same reasoning as probe.ps1.
function Get-AcAces([string]$path) {
  return @((Get-Acl -LiteralPath $path).Access | Where-Object { $_.IdentityReference.Value -like 'S-1-15-*' })
}
$acl = Get-Acl -LiteralPath $root
$acl.SetAccessRuleProtection($true, $true)
Set-Acl -LiteralPath $root -AclObject $acl
foreach ($t in @($root, $binDir, $workDir, $tmpDir)) {
  $a = Get-Acl -LiteralPath $t
  foreach ($r in (Get-AcAces $t)) { $null = $a.RemoveAccessRule($r) }
  Set-Acl -LiteralPath $t -AclObject $a
}
$residual = 0
foreach ($t in @($root, $binDir, $workDir, $tmpDir)) { $residual += (Get-AcAces $t).Count }
Fact 'preexisting-appcontainer-aces-on-fixture' $residual
Prop 'no-preexisting-appcontainer-ace' ($residual -eq 0) `
  "$residual S-1-15-* aces remain on the fixture; a residual one would let a tool start for a reason this probe did not write"

# node.exe is COPIED into the granted bin dir rather than granted where it lives — granting
# `C:\Program Files\nodejs` needs WRITE_DAC on a path the user does not own, i.e. exactly the
# elevation this design must not require. Same choice probe.ps1 makes.
$nodeSrc = (Get-Command node -ErrorAction Stop).Source
Copy-Item -LiteralPath $nodeSrc -Destination (Join-Path $binDir 'node.exe') -Force
$jailNode = Join-Path $binDir 'node.exe'

# busybox-w32, arch-matched exactly as `.github/workflows/release.yml` assembles the win32 packages:
# busybox64.exe is machine 0x8664 (x64), busybox64a.exe is 0xAA64 (arm64).
$bbSrcName = if ($env:PROCESSOR_ARCHITECTURE -match 'ARM') { 'busybox64a.exe' } else { 'busybox64.exe' }
$bbSrc = Join-Path $PSScriptRoot "..\..\vendor\busybox-w32\$bbSrcName"
$jailBusybox = ''
if (Test-Path -LiteralPath $bbSrc) {
  Copy-Item -LiteralPath $bbSrc -Destination (Join-Path $binDir 'busybox.exe') -Force
  $jailBusybox = Join-Path $binDir 'busybox.exe'
  Fact 'busybox-source' "$bbSrcName -> $jailBusybox"
} else {
  Fact 'busybox-source' "MISSING $bbSrc"
}

# ── the fixtures each tool is asked to run ──
# `hello.js` is reached as a FILE entry point, which is what makes it comparable to the known Node
# defect: `resolveMainPath` realpaths the entry before user code runs.
[IO.File]::WriteAllText((Join-Path $workDir 'hello.js'), @'
console.log('MARK-NODE-OK');
'@)
# A real corpus-shaped lifecycle body. 206 of the 344 corpus packages head a script segment with
# `node`, and 6 chain with `&&`, so "node a file, then chain" is the modal shape.
[IO.File]::WriteAllText((Join-Path $workDir 'lifecycle.sh'), @'
node hello.js && echo MARK-LIFECYCLE-OK
'@)
# THE TRANSITIVE cmd.exe DEPENDENCY, which is easy to miss: `aube-linker`'s `create_bin_shim`
# writes THREE wrappers per Windows bin — `<name>.cmd`, `<name>.ps1`, and an extensionless POSIX
# `sh` script. So `node-gyp rebuild` under cmd.exe runs `node-gyp.cmd`, but under busybox `sh` it
# runs the extensionless shim instead. Whether the busybox route ESCAPES cmd.exe depends on which
# of these two a tool actually reaches for, so both are measured.
[IO.File]::WriteAllText((Join-Path $workDir 'fakebin.cmd'), "@echo off`r`necho MARK-CMDSHIM-OK`r`n")
[IO.File]::WriteAllText((Join-Path $workDir 'fakebin'), "echo MARK-SHSHIM-OK`n")

$sys32 = Join-Path $env:SystemRoot 'System32'

# ─────────────────────────────── ace plumbing ───────────────────────────────

function Grant-Ace([string]$path, [string]$sid, [string]$rights, [bool]$inheritable) {
  $acl = Get-Acl -LiteralPath $path
  $trustee = New-Object Security.Principal.SecurityIdentifier($sid)
  $inherit = if ($inheritable) {
    [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit
  } else {
    [Security.AccessControl.InheritanceFlags]::None
  }
  $rule = New-Object Security.AccessControl.FileSystemAccessRule(
    $trustee, [Security.AccessControl.FileSystemRights]$rights, $inherit,
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

# ─────────────────────────────── the arm runner ───────────────────────────────

$cells = @{}   # $cells[<tool>][<arm>] = @{ rc; status; marker; lines; log }

# NTSTATUS shapes that a startup failure actually produces, decoded so a reader is not left
# guessing whether 0xc0000142 is an access problem (it is not — it is loader/session).
function Decode-Rc([string]$status) {
  if ($status -match '0x([0-9a-f]{8})') {
    switch ($Matches[1]) {
      'c0000022' { return 'STATUS_ACCESS_DENIED' }
      'c0000135' { return 'STATUS_DLL_NOT_FOUND' }
      'c0000142' { return 'STATUS_DLL_INIT_FAILED' }
      'c0000034' { return 'STATUS_OBJECT_NAME_NOT_FOUND' }
      'c0000005' { return 'STATUS_ACCESS_VIOLATION' }
      'ffffffff' { return 'no-exit-code (timed out / terminated)' }
      default    { return '' }
    }
  }
  return ''
}

function Invoke-Tool {
  param(
    [string]$Tool,          # cell row
    [string]$Arm,           # 'plain' | 'ac' | 'ac-hosttemp' | 'diag-croot'
    [string]$Exe,
    [string]$Cmdline,
    [string]$Marker,
    [bool]$AppContainer,
    [bool]$GrantCRoot = $false,
    [bool]$HostTemp = $false,
    [int]$TimeoutMs = 20000
  )
  if (-not $Exe -or -not (Test-Path -LiteralPath $Exe)) {
    if (-not $cells.ContainsKey($Tool)) { $cells[$Tool] = @{} }
    $cells[$Tool][$Arm] = @{ rc = 'TOOL-ABSENT'; marker = $false; lines = 0; log = @(); decode = '' }
    return
  }
  $sid = ''
  $profileName = ''
  $granted = @()
  $grantedRoots = @()
  try {
    if ($AppContainer) {
      $nm = "nubtl_${nonce}_$(($Tool + '_' + $Arm) -replace '[^A-Za-z0-9]','_')"
      if ($nm.Length -gt 60) { $nm = $nm.Substring(0, 60) }
      $profileName = $nm
      $sid = [Bt]::CreateProfile($profileName)
      if ($sid -like 'ERR*') {
        if (-not $cells.ContainsKey($Tool)) { $cells[$Tool] = @{} }
        $cells[$Tool][$Arm] = @{ rc = "PROFILE-$sid"; marker = $false; lines = 0; log = @(); decode = '' }
        $profileName = ''
        return
      }
      Grant-Ace $binDir  $sid 'ReadAndExecute' $true; $granted += $binDir
      Grant-Ace $workDir $sid 'Modify'         $true; $granted += $workDir
      Grant-Ace $tmpDir  $sid 'Modify'         $true; $granted += $tmpDir
      if ($GrantCRoot) {
        # NON-INHERITABLE, on the container object only. An inheritable ace here would propagate
        # across the entire volume — minutes of I/O and a real mess to unwind.
        foreach ($r in @('C:\', 'C:\Users')) {
          try { Grant-Ace $r $sid 'ReadAndExecute' $false; $grantedRoots += $r }
          catch { W "    diag: grant on $r FAILED (needs elevation): $_" }
        }
      }
    }
    $prevTemp = $env:TEMP; $prevTmp = $env:TMP; $prevPath = $env:PATH
    try {
      if ($AppContainer -and -not $HostTemp) { $env:TEMP = $tmpDir; $env:TMP = $tmpDir }
      $env:PATH = "$binDir;$sys32;$prevPath"
      $log = Join-Path $logDir "$Tool.$Arm.log"
      $sw = [Diagnostics.Stopwatch]::StartNew()
      $status = [Bt]::Launch($sid, $Exe, $Cmdline, $workDir, $log, $TimeoutMs)
      $sw.Stop()
      $lines = @()
      if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue) }
      $raw = ($lines -join "`n")
      if (-not $cells.ContainsKey($Tool)) { $cells[$Tool] = @{} }
      $cells[$Tool][$Arm] = @{
        rc      = $status
        marker  = ($Marker -and ($raw -match [regex]::Escape($Marker)))
        lines   = $lines.Count
        log     = $lines
        decode  = (Decode-Rc $status)
        ms      = $sw.ElapsedMilliseconds
        # Does the failure text NAME a filesystem-root path? A secondary attribution signal that
        # does not need the diag arm — usable only when a tool is chatty enough to say so, which a
        # native binary dying in its CRT startup is not.
        namesRoot = ($raw -match "C:\\\\'") -or ($raw -match "C:\\\\`"") -or ($raw -match 'lstat')
        rootsGranted = ($grantedRoots -join ',')
      }
      W ("  {0,-22} {1,-13} {2,-34} marker={3} lines={4} {5}ms {6}" -f `
        $Tool, $Arm, $status, $cells[$Tool][$Arm].marker, $lines.Count, $sw.ElapsedMilliseconds, $cells[$Tool][$Arm].decode)
      foreach ($l in $lines) { W "      | $l" }
    }
    finally { $env:TEMP = $prevTemp; $env:TMP = $prevTmp; $env:PATH = $prevPath }
  }
  finally {
    foreach ($p in $granted)      { try { Revoke-Ace $p $sid } catch { W "    revoke failed on $p : $_" } }
    foreach ($p in $grantedRoots) { try { Revoke-Ace $p $sid; W "    diag: revoked ace on $p" } catch { W "    diag: REVOKE FAILED on $p : $_" } }
    if ($profileName) { $null = [Bt]::DeleteProfile($profileName) }
  }
}

$logDir = Join-Path (Get-Location) 'tools-logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

# ─────────────────────────────── the tool matrix ───────────────────────────────

$cmdExe   = Join-Path $sys32 'cmd.exe'
$psExe    = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$pwshExe  = ''
$c = Get-Command pwsh -ErrorAction SilentlyContinue; if ($c) { $pwshExe = $c.Source }
$gitExe = ''; $c = Get-Command git -ErrorAction SilentlyContinue; if ($c) { $gitExe = $c.Source }
$pyExe  = ''; $c = Get-Command python -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source }
$msbuildExe = ''
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path -LiteralPath $vswhere) {
  $msbuildExe = (& $vswhere -latest -products '*' -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\MSBuild.exe' 2>$null | Select-Object -First 1)
}
$tarExe  = Join-Path $sys32 'tar.exe'
$curlExe = Join-Path $sys32 'curl.exe'
$whoExe  = Join-Path $sys32 'whoami.exe'

W ''
W '== tool discovery =='
foreach ($kv in @(
  @('cmd.exe', $cmdExe), @('powershell.exe', $psExe), @('pwsh.exe', $pwshExe),
  @('busybox.exe', $jailBusybox), @('node.exe', $jailNode), @('git.exe', $gitExe),
  @('python.exe', $pyExe), @('MSBuild.exe', $msbuildExe), @('tar.exe', $tarExe),
  @('curl.exe', $curlExe), @('whoami.exe', $whoExe))) {
  Fact "tool[$($kv[0])]" $(if ($kv[1] -and (Test-Path -LiteralPath $kv[1])) { $kv[1] } else { 'ABSENT' })
}

${q} = '"'
# `node -e` is deliberately the gate cell: with no file entry point there is no `resolveMainPath`,
# so it is the one Node invocation that is NOT expected to die in the realpath walk — which makes
# it usable to ask the child whether `C:\` is readable at all.
$gateJs = "console.log('MARK-DASH-E');const fs=require('fs');try{fs.statSync('C:\\');console.log('CROOT-READABLE')}catch(e){console.log('CROOT-DENIED '+e.code)}"

$matrix = @(
  # tool, exe, cmdline, marker
  # A LowBox token retains the invoking user's sid, so the confined `whoami` still prints that
  # user — which is exactly what makes the username a reliable marker here.
  @('whoami',            $whoExe,      "${q}$whoExe${q}",                                                          $env:USERNAME),
  @('node-dash-e',       $jailNode,    "${q}$jailNode${q} -e ${q}$gateJs${q}",                                          'MARK-DASH-E'),
  @('node-file-noflags', $jailNode,    "${q}$jailNode${q} ${q}$workDir\hello.js${q}",                                   'MARK-NODE-OK'),
  @('node-file-flags',   $jailNode,    "${q}$jailNode${q} --preserve-symlinks-main --preserve-symlinks ${q}$workDir\hello.js${q}", 'MARK-NODE-OK'),
  @('cmd-echo',          $cmdExe,      "${q}$cmdExe${q} /d /s /c ${q}echo MARK-CMD-OK${q}",                              'MARK-CMD-OK'),
  @('cmd-node',          $cmdExe,      "${q}$cmdExe${q} /d /s /c ${q}${q}$jailNode${q} hello.js${q}",                        'MARK-NODE-OK'),
  @('cmd-lifecycle',     $cmdExe,      "${q}$cmdExe${q} /d /s /c ${q}${q}$jailNode${q} hello.js && echo MARK-LIFECYCLE-OK${q}", 'MARK-LIFECYCLE-OK'),
  @('cmd-cmdshim',       $cmdExe,      "${q}$cmdExe${q} /d /s /c ${q}fakebin.cmd${q}",                                   'MARK-CMDSHIM-OK'),
  @('powershell-echo',   $psExe,       "${q}$psExe${q} -NoLogo -NonInteractive -NoProfile -Command ${q}Write-Output MARK-PS-OK${q}", 'MARK-PS-OK'),
  @('powershell-node',   $psExe,       "${q}$psExe${q} -NoLogo -NonInteractive -NoProfile -Command ${q}& '$jailNode' hello.js${q}", 'MARK-NODE-OK'),
  @('pwsh-echo',         $pwshExe,     "${q}$pwshExe${q} -NoLogo -NonInteractive -NoProfile -Command ${q}Write-Output MARK-PWSH-OK${q}", 'MARK-PWSH-OK'),
  @('busybox-echo',      $jailBusybox, "${q}$jailBusybox${q} sh -c ${q}echo MARK-BB-OK${q}",                             'MARK-BB-OK'),
  @('busybox-node',      $jailBusybox, "${q}$jailBusybox${q} sh -c ${q}node hello.js${q}",                               'MARK-NODE-OK'),
  @('busybox-lifecycle', $jailBusybox, "${q}$jailBusybox${q} sh -c ${q}. ./lifecycle.sh${q}",                            'MARK-LIFECYCLE-OK'),
  @('busybox-shshim',    $jailBusybox, "${q}$jailBusybox${q} sh -c ${q}sh ./fakebin${q}",                                'MARK-SHSHIM-OK'),
  @('busybox-cmdshim',   $jailBusybox, "${q}$jailBusybox${q} sh -c ${q}./fakebin.cmd${q}",                               'MARK-CMDSHIM-OK'),
  @('git',               $gitExe,      "${q}$gitExe${q} --version",                                                  'git version'),
  @('python',            $pyExe,       "${q}$pyExe${q} -c ${q}print('MARK-PY-OK')${q}",                                  'MARK-PY-OK'),
  @('msbuild',           $msbuildExe,  "${q}$msbuildExe${q} -version -nologo",                                       '.'),
  @('tar',               $tarExe,      "${q}$tarExe${q} --version",                                                  'bsdtar'),
  @('curl',              $curlExe,     "${q}$curlExe${q} --version",                                                 'curl')
)

W ''
W '== PLAIN arms (control: identical child, identical cmdline, no SECURITY_CAPABILITIES, no aces) =='
foreach ($m in $matrix) { Invoke-Tool -Tool $m[0] -Arm 'plain' -Exe $m[1] -Cmdline $m[2] -Marker $m[3] -AppContainer $false }

W ''
W '== APPCONTAINER arms (zero capabilities; C:\ and C:\Users untouched; TEMP granted) =='
foreach ($m in $matrix) { Invoke-Tool -Tool $m[0] -Arm 'ac' -Exe $m[1] -Cmdline $m[2] -Marker $m[3] -AppContainer $true }

W ''
W '== %TEMP% CONFOUND arm (AppContainer, host TEMP left ungranted — one variable off the `ac` arm) =='
foreach ($t in @('cmd-echo', 'powershell-echo', 'busybox-echo')) {
  $m = $matrix | Where-Object { $_[0] -eq $t } | Select-Object -First 1
  if ($m) { Invoke-Tool -Tool $m[0] -Arm 'ac-hosttemp' -Exe $m[1] -Cmdline $m[2] -Marker $m[3] -AppContainer $true -HostTemp $true }
}

# ── THE ATTRIBUTION ARM. Only for tools that actually failed, and the only place `C:\` is touched.
W ''
W '== DIAGNOSTIC arms: non-inheritable RX ace on C:\ + C:\Users, one variable off `ac` =='
W '   (the ONLY arms that touch the volume root; revoked immediately; excluded from every primary property)'
$failedTools = @()
foreach ($m in $matrix) {
  $t = $m[0]
  if ($cells.ContainsKey($t) -and $cells[$t].ContainsKey('ac') -and $cells[$t].ContainsKey('plain')) {
    $acOk = $cells[$t]['ac'].marker
    $plainOk = $cells[$t]['plain'].marker
    if ($plainOk -and -not $acOk) { $failedTools += $t }
  }
}
Fact 'tools-failing-under-appcontainer' $(if ($failedTools.Count) { ($failedTools -join ' ') } else { '(none)' })
foreach ($t in $failedTools) {
  $m = $matrix | Where-Object { $_[0] -eq $t } | Select-Object -First 1
  if ($m) { Invoke-Tool -Tool $m[0] -Arm 'diag-croot' -Exe $m[1] -Cmdline $m[2] -Marker $m[3] -AppContainer $true -GrantCRoot $true }
}

# ─────────────────────────────── the table ───────────────────────────────

$arms = @('plain', 'ac', 'ac-hosttemp', 'diag-croot')
function TCell([string]$tool, [string]$arm) {
  if (-not $cells.ContainsKey($tool)) { return '-' }
  if (-not $cells[$tool].ContainsKey($arm)) { return '-' }
  $c = $cells[$tool][$arm]
  if ($c.rc -eq 'TOOL-ABSENT') { return 'ABSENT' }
  if ($c.marker) { return 'STARTED' }
  if ($c.rc -match 'TIMED-OUT') { return 'HUNG' }
  return 'FAILED'
}
function TRc([string]$tool, [string]$arm) {
  if (-not $cells.ContainsKey($tool) -or -not $cells[$tool].ContainsKey($arm)) { return '' }
  return "$($cells[$tool][$arm].rc) $($cells[$tool][$arm].decode)"
}

W ''
W '== tool startup table (STARTED = launched AND emitted its marker) =='
W ("  {0,-20} {1}" -f 'tool', (($arms | ForEach-Object { '{0,-14}' -f $_ }) -join ''))
foreach ($m in $matrix) {
  W ("  {0,-20} {1}" -f $m[0], (($arms | ForEach-Object { '{0,-14}' -f (TCell $m[0] $_) }) -join ''))
}
W ''
W '== exit status detail =='
foreach ($m in $matrix) {
  foreach ($a in $arms) {
    if ($cells.ContainsKey($m[0]) -and $cells[$m[0]].ContainsKey($a)) {
      W ("  {0,-20} {1,-13} {2}" -f $m[0], $a, (TRc $m[0] $a))
    }
  }
}

# ─────────────────────────────── verdict ───────────────────────────────

W ''
W '== verdict =='

function Started([string]$tool, [string]$arm) { return ((TCell $tool $arm) -eq 'STARTED') }
function Present([string]$tool) { return ((TCell $tool 'plain') -ne 'ABSENT') }

# ── CONTROLS. A red one here means the harness or the host is the story and no row below is
# attributable to the token. This is the discipline that caught a CharSet marshalling bug that had
# every arm failing identically in a way that read exactly like "mechanism unavailable" (§5e).
$plainFails = @($matrix | Where-Object { (Present $_[0]) -and -not (Started $_[0] 'plain') } | ForEach-Object { $_[0] })
Prop 'tools-baseline-plain-all-started' ($plainFails.Count -eq 0) `
  "every present tool must start unconfined; not started: $(if($plainFails.Count){($plainFails -join ' ')}else{'(none)'})"

# The gate: an AppContainer arm must be DENIED reading `C:\`. GRANTED would mean the child is not
# confined and every 'STARTED' below is vacuous.
$gateLog = ''
if ($cells.ContainsKey('node-dash-e') -and $cells['node-dash-e'].ContainsKey('ac')) {
  $gateLog = ($cells['node-dash-e']['ac'].log -join ' ')
}
Prop 'appcontainer-gate-is-live' ($gateLog -match 'CROOT-DENIED') `
  "the confined child must be refused a stat of C:\ — node-dash-e/ac said: '$gateLog'"

# The positive control: an AppContainer arm must run a System32 binary (System32 carries an
# ALL APPLICATION PACKAGES ace). Without this, a column of failures cannot be told from a child
# that fails at everything.
# Two candidates rather than one, because a single System32 binary that turns out to have its own
# quirk would take the whole run's readability with it.
Prop 'appcontainer-positive-control' ((Started 'whoami' 'ac') -or (Started 'tar' 'ac') -or (Started 'curl' 'ac')) `
  "a System32 binary must run confined, or a table of failures is about a dead child and not about DACLs: whoami/ac=$(TCell 'whoami' 'ac') tar/ac=$(TCell 'tar' 'ac') curl/ac=$(TCell 'curl' 'ac')"

# The reference defect, reproduced IN THIS RUN rather than recalled: an unflagged confined `node`
# with a FILE entry point dies in its realpath walk (§5h). This is what makes "same class as Node"
# a measured comparison instead of an assertion.
Prop 'node-realpath-defect-reproduces' (-not (Started 'node-file-noflags' 'ac')) `
  "the known Node defect must reproduce here: node-file-noflags/ac=$(TCell 'node-file-noflags' 'ac') $(TRc 'node-file-noflags' 'ac')"
Prop 'node-realpath-defect-is-the-flags' (Started 'node-file-flags' 'ac') `
  "and the flag pair must lift it, or the arm above failed for some other reason: node-file-flags/ac=$(TCell 'node-file-flags' 'ac')"

# ── THE DECISIVE CELLS. Each is stated so PASS means "this tool works confined". A FAIL here is a
# real answer to the maintainer's question, not a broken run — read the table, not the badge.
Prop 'cmd-starts-under-appcontainer'      (Started 'cmd-echo' 'ac')          "cmd.exe /c echo: $(TRc 'cmd-echo' 'ac')"
Prop 'cmd-runs-node-under-appcontainer'   (Started 'cmd-node' 'ac')          "cmd.exe /c node file: $(TRc 'cmd-node' 'ac')"
Prop 'cmd-runs-lifecycle-shape'           (Started 'cmd-lifecycle' 'ac')     "cmd.exe running the modal corpus script shape: $(TRc 'cmd-lifecycle' 'ac')"
Prop 'powershell-starts-under-appcontainer' (Started 'powershell-echo' 'ac') "powershell.exe: $(TRc 'powershell-echo' 'ac')"
Prop 'busybox-starts-under-appcontainer'  (Started 'busybox-echo' 'ac')      "vendored busybox-w32 sh -c echo: $(TRc 'busybox-echo' 'ac')"
Prop 'busybox-runs-node-under-appcontainer' (Started 'busybox-node' 'ac')    "busybox sh -c 'node hello.js': $(TRc 'busybox-node' 'ac')"
Prop 'busybox-runs-lifecycle-shape'       (Started 'busybox-lifecycle' 'ac') "busybox running the modal corpus script shape: $(TRc 'busybox-lifecycle' 'ac')"

W ''
W '  ── attribution (Part 3): is a failure the ROOT-PATH class, and would one fix cover it? ──'
foreach ($t in $failedTools) {
  $acRc   = TRc $t 'ac'
  $diag   = TCell $t 'diag-croot'
  $roots  = if ($cells[$t].ContainsKey('diag-croot')) { $cells[$t]['diag-croot'].rootsGranted } else { '' }
  $names  = if ($cells[$t].ContainsKey('ac')) { $cells[$t]['ac'].namesRoot } else { $false }
  $verdict = if ($diag -eq 'STARTED') { 'ROOT-PATH CLASS (a C:\ ace lifts it — same bug as Node, and nub cannot write that ace unprivileged)' }
             elseif ($diag -eq 'ABSENT' -or $diag -eq '-') { 'UNATTRIBUTED (no diag arm ran)' }
             else { 'NOT the root-path class (a C:\ + C:\Users ace does NOT lift it — different cause, different remedy)' }
  Fact "attribution[$t]" "ac=$acRc | diag-croot=$diag (roots granted: $roots) | failure-text-names-a-root-path=$names => $verdict"
}
if (-not $failedTools.Count) { Fact 'attribution' 'no tool that worked unconfined failed confined' }

W ''
W '  ── the %TEMP% confound, sized rather than assumed ──'
foreach ($t in @('cmd-echo', 'powershell-echo', 'busybox-echo')) {
  Fact "temp-confound[$t]" "ac(granted TEMP)=$(TCell $t 'ac')  ac-hosttemp(ungranted TEMP)=$(TCell $t 'ac-hosttemp')"
}

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED (a clean negative also lands here — read the table)" }

Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
W ''
W 'TOOLS PROBE END'
