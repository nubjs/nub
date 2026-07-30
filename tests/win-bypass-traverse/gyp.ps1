# DOES node-gyp's VISUAL-STUDIO DETECTION SURVIVE THE JAIL — AND WHAT DOES PYTHON'S GRANT COST?
#
# WHY THIS IS THE REMAINING BLOCKER. `works.ps1` established that a confined Windows PowerShell
# cannot spawn an external program at all: every `& <exe>` dies `DriveNotFoundException: a drive with
# the name 'Microsoft.PowerShell.Core\FileSystem' does not exist`, and `$LASTEXITCODE` comes back
# EMPTY so a caller's own error check reads as success. node-gyp reaches PowerShell for exactly that
# job — `lib/find-visualstudio.js` shells `System32\WindowsPowerShell\v1.0\powershell.exe` three
# different ways — and node-gyp is on the path for 121 of the 344 corpus packages. If VS detection
# fails, every from-source native build on Windows fails with it. That sits in real tension with an
# earlier measurement showing a from-source MSBuild compile byte-identical under the jail, so the
# chain has to be run rather than reasoned about.
#
# THE THREE COMMANDS ARE COPIED FROM node-gyp 12.4.0, not invented (`lib/find-visualstudio.js`):
#   findNewVS            -> powershell.exe -ExecutionPolicy Unrestricted -NoProfile -Command
#                             "&{Add-Type -IgnoreWarnings -Path '<Find-VisualStudio.cs>';
#                                [VisualStudioConfiguration.Main]::PrintJson()}"
#   findNewVSUsingSetupModule -> ... -Command "&{@(Get-Module -ListAvailable -Name VSSetup).Version.ToString()}"
#                             then ... -Command "&{Get-VSSetupInstance | ConvertTo-Json -Depth 3}"
# `Add-Type -Path` is the load-bearing one, and it is NOT obviously in-process: Windows PowerShell
# 5.1 compiles through CodeDom, which SPAWNS `csc.exe`. So the detection may fail for the same reason
# every other PowerShell spawn fails. `Get-Module -ListAvailable` is the other suspect — it
# enumerates `$env:PSModulePath`, which is precisely what the missing FileSystem PSDrive breaks
# (PowerShell/PowerShell#27253).
#
# EACH SHAPE IS MEASURED TWICE, and the pair is the point:
#   * as the DIRECT child of the AppContainer launch — does the command itself work confined?
#   * spawned BY NODE through `child_process.execFile` with piped stdio, which is node-gyp's ACTUAL
#     shape. That second arm carries its own independent hazard: a piped spawn from node under an
#     AppContainer has been recorded hanging indefinitely (MECHANISM-FACTS §5h). Python's
#     `subprocess.run(capture_output=True)` was measured working confined, so the hang is not
#     universal and node's own case has to be measured, not inherited.
# Then `node-gyp configure` end-to-end, which is the claim that actually matters: it is the phase
# that runs detection and writes the vcxproj, and it needs no compiler, so it costs seconds rather
# than the minutes a full `rebuild` would.
#
# PART 2 — WHAT PYTHON'S GRANT COSTS, AND WHETHER IT CAN BE MADE CHEAP. The grant itself is settled:
# one inheritable ReadAndExecute ace on python's install root turns a `0xc0000135
# STATUS_DLL_NOT_FOUND` into a fully working interpreter. The problem is 3.7 s to write plus 0.7 s to
# revoke, PER LAUNCH, because `SetNamedSecurityInfoW` with CONTAINER|OBJECT inherit propagates into
# every existing descendant — and that is the same Win32 call the production backend makes
# (`crates/nub-sandbox/src/backend/windows.rs`, `grfInheritance = CONTAINER_INHERIT_ACE |
# OBJECT_INHERIT_ACE`), so the number transfers. Four hypotheses are timed here rather than argued:
#   (a) the whole install root, inheritable — the shipping shape, and the baseline.
#   (b) a RE-grant when the identical ace is already present — is the cost the tree walk, or the
#       write? If a re-grant is also slow, caching the grant across spawns is the only lever.
#   (c) exe + `DLLs\` + `Lib\` granted separately instead of the root — narrower, but three walks.
#   (d) an EMPTY directory, as the O(1) control: a sibling lane measured 24 ms empty vs 426 ms
#       populated, which is the evidence that the cost is propagation and not the DACL write.
# And the cheapest lever of all is measured first: whether python's tree ALREADY grants an
# AppContainer-readable right, in which case the correct action is to skip the grant entirely. That
# is the common case for an all-users install under `Program Files`, which inherits
# `ALL APPLICATION PACKAGES` Read & Execute.

Set-StrictMode -Off
$ErrorActionPreference = 'Continue'

. (Join-Path $PSScriptRoot 'verdict.ps1')
. (Join-Path $PSScriptRoot 'launcher.ps1')

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)

W '== host =='
Fact 'os' ((Get-CimInstance Win32_OperatingSystem).Caption)
Fact 'os-build' ((Get-CimInstance Win32_OperatingSystem).BuildNumber)
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'winps-version' (& powershell -NoLogo -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' 2>&1)
Fact 'node-version' (& node --version 2>&1)

$root      = Join-Path $env:USERPROFILE "nubgyp-$nonce"
$workDir   = Join-Path $root 'work'
$tmpDir    = Join-Path $root 'tmp'
$emptyDir  = Join-Path $root 'empty'
foreach ($d in @($root, $workDir, $tmpDir, $emptyDir)) { New-Item -ItemType Directory -Force -Path $d | Out-Null }

$acl = Get-Acl -LiteralPath $root
$acl.SetAccessRuleProtection($true, $true)
Set-Acl -LiteralPath $root -AclObject $acl

function Grant-Ace([string]$path, [string]$sid, [string]$rights, [bool]$inheritable) {
  $a = Get-Acl -LiteralPath $path
  $inherit = if ($inheritable) {
    [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit
  } else { [Security.AccessControl.InheritanceFlags]::None }
  $a.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
    (New-Object Security.Principal.SecurityIdentifier($sid)),
    [Security.AccessControl.FileSystemRights]$rights, $inherit,
    [Security.AccessControl.PropagationFlags]::None,
    [Security.AccessControl.AccessControlType]::Allow)))
  Set-Acl -LiteralPath $path -AclObject $a
}
function Revoke-Ace([string]$path, [string]$sid) {
  $a = Get-Acl -LiteralPath $path
  $null = $a.PurgeAccessRules((New-Object Security.Principal.SecurityIdentifier($sid)))
  Set-Acl -LiteralPath $path -AclObject $a
}
# Whether a path ALREADY grants an AppContainer-readable right. When it does, nub's grant is pure
# cost and the right action is to skip it — the `Program Files` case.
function Get-AapRights([string]$path) {
  try {
    $aces = @((Get-Acl -LiteralPath $path).Access | Where-Object {
      $_.IdentityReference.Value -like 'S-1-15-2-*' -or $_.IdentityReference.Value -like '*APPLICATION PACKAGES*'
    })
    if (-not $aces.Count) { return 'NONE' }
    return (($aces | ForEach-Object { "$($_.IdentityReference.Value):$($_.FileSystemRights):inherit=$($_.InheritanceFlags)" }) -join ' | ')
  } catch { return "unreadable: $($_.Exception.GetType().Name)" }
}

# ─────────────────────────────── toolchain discovery ───────────────────────────────

$sys32 = Join-Path $env:SystemRoot 'System32'
$winPs = Join-Path $sys32 'WindowsPowerShell\v1.0\powershell.exe'
$nodeExe = (Get-Command node -ErrorAction Stop).Source
$nodeDir = Split-Path -Parent $nodeExe
$pyExe = ''; $c = Get-Command python -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source }
if (-not $pyExe) { $c = Get-Command python3 -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source } }
$pyDir = if ($pyExe) { Split-Path -Parent $pyExe } else { '' }

# node-gyp as npm ships it, which is the copy a lifecycle script reaches.
$gypJs = Join-Path $nodeDir 'node_modules\npm\node_modules\node-gyp\bin\node-gyp.js'
$gypLib = Join-Path $nodeDir 'node_modules\npm\node_modules\node-gyp\lib'
$csFile = Join-Path $gypLib 'Find-VisualStudio.cs'
$devDir = Join-Path $env:LOCALAPPDATA 'node-gyp\Cache'

W ''
W '== toolchain =='
Fact 'node-exe' $nodeExe
Fact 'node-dir' $nodeDir
# THE FREE-GRANT QUESTION. A tool under `Program Files` inherits ALL APPLICATION PACKAGES Read &
# Execute from its parent, so a confined child can already load it and nub should write NO ace at
# all. Reported for every tool so the grant policy can key on measurement instead of a path guess.
Fact 'node-dir-aap' (Get-AapRights $nodeDir)
Fact 'gyp-js' $(if (Test-Path -LiteralPath $gypJs) { $gypJs } else { 'ABSENT' })
Fact 'gyp-cs' $(if (Test-Path -LiteralPath $csFile) { $csFile } else { 'ABSENT' })
Fact 'gyp-lib-aap' $(if (Test-Path -LiteralPath $gypLib) { Get-AapRights $gypLib } else { 'ABSENT' })
Fact 'python-exe' $(if ($pyExe) { $pyExe } else { 'ABSENT' })
Fact 'python-dir-aap' $(if ($pyDir) { Get-AapRights $pyDir } else { 'ABSENT' })
Fact 'vs-installer-present' (Test-Path -LiteralPath (Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'))
Fact 'programfiles-aap' (Get-AapRights $env:ProgramFiles)

# ─────────────────────────────── fixture ───────────────────────────────
# A real from-source addon: `node-gyp configure` on this runs the full VS + python + SDK detection
# and writes a vcxproj, which is precisely the phase under test.

[IO.File]::WriteAllText((Join-Path $workDir 'binding.gyp'), @'
{ "targets": [ { "target_name": "probe", "sources": [ "probe.cc" ] } ] }
'@)
[IO.File]::WriteAllText((Join-Path $workDir 'probe.cc'), @'
#include <node_api.h>
napi_value Init(napi_env env, napi_value exports) { return exports; }
NAPI_MODULE(NODE_GYP_MODULE_NAME, Init)
'@)
[IO.File]::WriteAllText((Join-Path $workDir 'package.json'), @'
{ "name": "probe", "version": "1.0.0" }
'@)

# node-gyp's spawn shape, reproduced: `execFile` with PIPED stdio, which is both what node-gyp does
# and the shape recorded hanging under an AppContainer. Prints the child's rc and a stdout prefix so
# a hang, a refusal, and a wrong-but-successful answer are three distinguishable outcomes.
[IO.File]::WriteAllText((Join-Path $workDir 'spawn-ps.js'), @'
const { execFile } = require('child_process');
const ps = process.env.BT_WINPS;
const cs = process.env.BT_CSFILE;
const shapes = [
  ['addtype', ['-ExecutionPolicy', 'Unrestricted', '-NoProfile', '-Command',
    "&{Add-Type -IgnoreWarnings -Path '" + cs + "';[VisualStudioConfiguration.Main]::PrintJson()}"]],
  ['vssetup-probe', ['-NoProfile', '-Command', '&{@(Get-Module -ListAvailable -Name VSSetup).Version.ToString()}']],
  ['echo', ['-NoProfile', '-Command', 'Write-Output PS-FROM-NODE']],
];
(async () => {
  for (const [name, args] of shapes) {
    await new Promise((resolve) => {
      const t = setTimeout(() => { console.log('op:' + name + '=TIMEOUT'); resolve(); }, 25000);
      execFile(ps, args, { encoding: 'utf8', maxBuffer: 8 << 20 }, (err, stdout, stderr) => {
        clearTimeout(t);
        const head = (stdout || '').trim().split(/\r?\n/)[0] || '';
        const eh = (stderr || '').trim().split(/\r?\n/)[0] || '';
        console.log('op:' + name + '=rc:' + (err ? (err.code === undefined ? 'ERR' : err.code) : 0) +
          ' stdout-len:' + (stdout || '').length + ' stdout-head:' + head.slice(0, 90) +
          ' stderr-head:' + eh.slice(0, 90));
        resolve();
      });
    });
  }
  console.log('op:end=ok');
})();
'@)

$logDir = Join-Path (Get-Location) 'gyp-logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

$cells = @{}

function Invoke-Arm {
  param(
    [string]$Battery, [string]$Arm, [string]$Exe, [string]$Cmdline,
    [bool]$AppContainer, [string[]]$Reads = @(), [string[]]$Writes = @(),
    [int]$TimeoutMs = 90000
  )
  if (-not $cells.ContainsKey($Battery)) { $cells[$Battery] = @{} }
  if (-not $Exe -or -not (Test-Path -LiteralPath $Exe)) {
    $cells[$Battery][$Arm] = @{ rc = 'TOOL-ABSENT'; lines = @() }
    return
  }
  $sid = ''; $profileName = ''; $granted = @()
  try {
    if ($AppContainer) {
      $nm = "nubgyp_${nonce}_$(($Battery + '_' + $Arm) -replace '[^A-Za-z0-9]','_')"
      if ($nm.Length -gt 60) { $nm = $nm.Substring(0, 60) }
      $profileName = $nm
      $sid = [Bt]::CreateProfile($profileName)
      if ($sid -like 'ERR*') { $cells[$Battery][$Arm] = @{ rc = "PROFILE-$sid"; lines = @() }; $profileName = ''; return }
      foreach ($p in $Writes) { if (Test-Path -LiteralPath $p) { Grant-Ace $p $sid 'Modify' $true; $granted += $p } }
      foreach ($p in $Reads) {
        if (-not (Test-Path -LiteralPath $p)) { continue }
        # SKIP what already grants an AppContainer read — the same decision the production grant
        # should make, and reported so the saving is visible rather than assumed.
        if ((Get-AapRights $p) -ne 'NONE') { Fact "grant-skipped-already-aap[$Battery/$Arm]" $p; continue }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        Grant-Ace $p $sid 'ReadAndExecute' $true
        $sw.Stop()
        Fact "grant-ms[$Battery/$Arm]" "$($sw.ElapsedMilliseconds) $p"
        $granted += $p
      }
    }
    $prev = @{ TEMP = $env:TEMP; TMP = $env:TMP }
    try {
      $env:TEMP = $tmpDir; $env:TMP = $tmpDir
      $log = Join-Path $logDir "$Battery.$Arm.log"
      $sw = [Diagnostics.Stopwatch]::StartNew()
      $status = [Bt]::Launch($sid, $Exe, $Cmdline, $workDir, $log, $TimeoutMs)
      $sw.Stop()
      $lines = @()
      if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue) }
      $cells[$Battery][$Arm] = @{ rc = $status; lines = $lines }
      W ("  {0,-16} {1,-7} {2,-34} lines={3} {4}ms" -f $Battery, $Arm, $status, $lines.Count, $sw.ElapsedMilliseconds)
    }
    finally { $env:TEMP = $prev.TEMP; $env:TMP = $prev.TMP }
  }
  finally {
    foreach ($p in $granted) { try { Revoke-Ace $p $sid } catch { W "    revoke failed on $p : $_" } }
    if ($profileName) { $null = [Bt]::DeleteProfile($profileName) }
  }
}

# ─────────────────────────────── PART 1: VS detection ───────────────────────────────

$env:BT_WINPS = $winPs; $env:BT_CSFILE = $csFile

${q} = '"'
$addTypeCmd = "${q}$winPs${q} -ExecutionPolicy Unrestricted -NoProfile -Command ${q}&{Add-Type -IgnoreWarnings -Path '$csFile';[VisualStudioConfiguration.Main]::PrintJson()}${q}"
$vsSetupCmd = "${q}$winPs${q} -NoProfile -Command ${q}&{@(Get-Module -ListAvailable -Name VSSetup).Version.ToString()}${q}"

# Read closure for a node-gyp run: node's own tree (skipped when Program Files already grants AAP),
# npm's `lib/node_modules` for node-gyp itself, the pre-fetched headers, and python.
$gypReads = @($nodeDir, $devDir) + @($pyDir | Where-Object { $_ })

W ''
W '== node-gyp VS detection, as the DIRECT child (plain vs confined) =='
foreach ($arm in @('plain', 'ac')) {
  $ac = ($arm -eq 'ac')
  Invoke-Arm -Battery 'ps-addtype' -Arm $arm -Exe $winPs -Cmdline $addTypeCmd -AppContainer $ac -Reads $gypReads -Writes @($workDir, $tmpDir) -TimeoutMs 90000
  Invoke-Arm -Battery 'ps-vssetup' -Arm $arm -Exe $winPs -Cmdline $vsSetupCmd -AppContainer $ac -Reads $gypReads -Writes @($workDir, $tmpDir) -TimeoutMs 90000
}

W ''
W '== the same shapes SPAWNED BY NODE through execFile with piped stdio (node-gyp`s real shape) =='
foreach ($arm in @('plain', 'ac')) {
  Invoke-Arm -Battery 'node-spawns-ps' -Arm $arm -Exe $nodeExe `
    -Cmdline "${q}$nodeExe${q} --preserve-symlinks-main --preserve-symlinks ${q}$workDir\spawn-ps.js${q}" `
    -AppContainer ($arm -eq 'ac') -Reads $gypReads -Writes @($workDir, $tmpDir) -TimeoutMs 120000
}

# ── END TO END. Headers are pre-fetched UNCONFINED because the jail denies egress by design; a
# network failure here would be a result about `net`, which is already settled, rather than about the
# toolchain.
W ''
W '== node-gyp configure end-to-end =='
Fact 'header-prefetch' (& node $gypJs install --devdir $devDir 2>&1 | Select-Object -Last 1)
Fact 'devdir-exists' (Test-Path -LiteralPath $devDir)
foreach ($arm in @('plain', 'ac')) {
  Invoke-Arm -Battery 'gyp-configure' -Arm $arm -Exe $nodeExe `
    -Cmdline "${q}$nodeExe${q} --preserve-symlinks-main --preserve-symlinks ${q}$gypJs${q} configure --devdir ${q}$devDir${q} --loglevel silly" `
    -AppContainer ($arm -eq 'ac') -Reads $gypReads -Writes @($workDir, $tmpDir) -TimeoutMs 180000
}

# ─────────────────────────────── PART 2: what the python grant costs ───────────────────────────────

W ''
W '== python grant cost: four shapes, timed through the same Win32 call the backend uses =='
if ($pyDir) {
  $probeName = "nubcost_$nonce"
  $costSid = [Bt]::CreateProfile($probeName)
  if ($costSid -notlike 'ERR*') {
    try {
      $tree = @(Get-ChildItem -LiteralPath $pyDir -Recurse -Force -ErrorAction SilentlyContinue)
      Fact 'cost/python-tree-entries' $tree.Count
      Fact 'cost/python-tree-bytes' (($tree | Where-Object { -not $_.PSIsContainer } | Measure-Object -Property Length -Sum).Sum)

      # (d) the O(1) control FIRST: an empty directory isolates the DACL write from the propagation.
      $sw = [Diagnostics.Stopwatch]::StartNew(); Grant-Ace $emptyDir $costSid 'ReadAndExecute' $true; $sw.Stop()
      Fact 'cost/empty-dir-grant-ms' $sw.ElapsedMilliseconds
      $sw = [Diagnostics.Stopwatch]::StartNew(); Revoke-Ace $emptyDir $costSid; $sw.Stop()
      Fact 'cost/empty-dir-revoke-ms' $sw.ElapsedMilliseconds

      # (a) the shipping shape.
      $sw = [Diagnostics.Stopwatch]::StartNew(); Grant-Ace $pyDir $costSid 'ReadAndExecute' $true; $sw.Stop()
      Fact 'cost/python-root-grant-ms' $sw.ElapsedMilliseconds
      # (b) the identical ace AGAIN, ace already present. If this is still slow the cost is the tree
      # walk and only CACHING the grant across spawns helps; if it is fast, a persistent grant is not
      # even needed and re-asserting per spawn is cheap.
      $sw = [Diagnostics.Stopwatch]::StartNew(); Grant-Ace $pyDir $costSid 'ReadAndExecute' $true; $sw.Stop()
      Fact 'cost/python-root-REgrant-ms' $sw.ElapsedMilliseconds
      $sw = [Diagnostics.Stopwatch]::StartNew(); Revoke-Ace $pyDir $costSid; $sw.Stop()
      Fact 'cost/python-root-revoke-ms' $sw.ElapsedMilliseconds

      # (c) the narrow shape: the exe non-inheritably plus only the two dirs the loader and stdlib
      # need. Whether this is cheaper depends on how much of the tree is `Lib\`, so it is timed.
      $narrow = @($pyExe) + @('DLLs', 'Lib') | ForEach-Object {
        if ($_ -eq $pyExe) { $_ } else { Join-Path $pyDir $_ }
      }
      $swAll = [Diagnostics.Stopwatch]::StartNew()
      foreach ($n in $narrow) {
        if (-not (Test-Path -LiteralPath $n)) { continue }
        $inh = -not (Test-Path -LiteralPath $n -PathType Leaf)
        $sw = [Diagnostics.Stopwatch]::StartNew(); Grant-Ace $n $costSid 'ReadAndExecute' $inh; $sw.Stop()
        Fact 'cost/narrow-grant-ms' "$($sw.ElapsedMilliseconds) inherit=$inh $n"
      }
      $swAll.Stop()
      Fact 'cost/narrow-total-grant-ms' $swAll.ElapsedMilliseconds
      # Does the NARROW grant actually run python? A cheaper grant that does not work is not a fix.
      Invoke-Arm -Battery 'py-narrow' -Arm 'ac' -Exe $pyExe `
        -Cmdline "${q}$pyExe${q} -c ${q}import sys,ssl,sqlite3,ctypes;print('op:narrow-ok='+sys.version.split()[0])${q}" `
        -AppContainer $false -TimeoutMs 60000
      foreach ($n in $narrow) { if (Test-Path -LiteralPath $n) { try { Revoke-Ace $n $costSid } catch { } } }
    }
    finally { $null = [Bt]::DeleteProfile($probeName) }
  } else { Fact 'cost/profile' $costSid }
}

# The narrow-grant EXECUTION check needs the grants live, so it runs as its own arm with the narrow
# set passed as reads — the loop above only timed the writes.
if ($pyDir) {
  W ''
  W '== does the NARROW grant actually run python? =='
  $narrowReads = @((Join-Path $pyDir 'DLLs'), (Join-Path $pyDir 'Lib'))
  Invoke-Arm -Battery 'py-narrow-exec' -Arm 'ac' -Exe $pyExe `
    -Cmdline "${q}$pyExe${q} -c ${q}import sys,ssl,sqlite3,ctypes;print('op:narrow-ok='+sys.version.split()[0])${q}" `
    -AppContainer $true -Reads $narrowReads -TimeoutMs 60000
  Invoke-Arm -Battery 'py-root-exec' -Arm 'ac' -Exe $pyExe `
    -Cmdline "${q}$pyExe${q} -c ${q}import sys,ssl,sqlite3,ctypes;print('op:root-ok='+sys.version.split()[0])${q}" `
    -AppContainer $true -Reads @($pyDir) -TimeoutMs 60000
}

# ─────────────────────────────── the table ───────────────────────────────

function Lines2([string]$b, [string]$a) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey($a)) { return $null }
  return $cells[$b][$a].lines
}
function Rc2([string]$b, [string]$a) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey($a)) { return '-' }
  return $cells[$b][$a].rc
}

W ''
W '== results =='
foreach ($b in @('ps-addtype', 'ps-vssetup', 'node-spawns-ps', 'gyp-configure', 'py-narrow-exec', 'py-root-exec')) {
  foreach ($a in @('plain', 'ac')) {
    $l = Lines2 $b $a
    if ($null -eq $l) { continue }
    W ("  {0,-16} {1,-7} rc={2,-28} lines={3}" -f $b, $a, (Rc2 $b $a), $l.Count)
  }
}

W ''
W '== child output verbatim =='
foreach ($b in @('ps-addtype', 'ps-vssetup', 'node-spawns-ps', 'gyp-configure', 'py-narrow-exec', 'py-root-exec')) {
  foreach ($a in @('plain', 'ac')) {
    $l = Lines2 $b $a
    if ($null -eq $l) { continue }
    W "  ── $b / $a (rc=$(Rc2 $b $a)) ──"
    # `gyp-configure --loglevel silly` is verbose; the head and tail carry the verdict and the
    # failure, and the artifact holds the whole thing.
    $show = if ($l.Count -gt 60) { @($l[0..29]) + @('  … (trimmed; full log in the artifact) …') + @($l[($l.Count - 30)..($l.Count - 1)]) } else { $l }
    foreach ($x in $show) { W "      $x" }
  }
}

# ─────────────────────────────── verdict ───────────────────────────────

W ''
W '== verdict =='

function HasLine([string]$b, [string]$a, [string]$pat) {
  $l = Lines2 $b $a
  if ($null -eq $l) { return $false }
  return (($l -join "`n") -match $pat)
}

# CONTROLS: every shape must work UNCONFINED, or its confined row says nothing.
Prop 'baseline-addtype-works-unconfined' (HasLine 'ps-addtype' 'plain' '"path"|InstallationPath|\[') `
  "node-gyp's Add-Type detection must return JSON unconfined: rc=$(Rc2 'ps-addtype' 'plain')"
Prop 'baseline-node-spawns-ps-unconfined' (HasLine 'node-spawns-ps' 'plain' 'op:echo=rc:0') `
  "node must be able to execFile powershell unconfined: rc=$(Rc2 'node-spawns-ps' 'plain')"
Prop 'baseline-gyp-configure-unconfined' (HasLine 'gyp-configure' 'plain' 'gyp info ok|ok$') `
  "node-gyp configure must succeed unconfined, or the confined row is about the fixture: rc=$(Rc2 'gyp-configure' 'plain')"

# THE DECISIVE CELLS. PASS = the confined chain works.
Prop 'vs-detection-addtype-works-confined' (HasLine 'ps-addtype' 'ac' '"path"|InstallationPath|\[') `
  "the command node-gyp actually uses, confined: rc=$(Rc2 'ps-addtype' 'ac')"
Prop 'node-can-spawn-powershell-confined' (HasLine 'node-spawns-ps' 'ac' 'op:echo=rc:0') `
  "node-gyp's spawn shape (execFile, piped stdio) confined — a TIMEOUT here is the recorded piped-spawn hang: rc=$(Rc2 'node-spawns-ps' 'ac')"
Prop 'node-piped-spawn-does-not-hang-confined' (-not (HasLine 'node-spawns-ps' 'ac' 'TIMEOUT')) `
  "no shape may time out: $((Lines2 'node-spawns-ps' 'ac') -join ' | ')"
Prop 'gyp-configure-works-confined' (HasLine 'gyp-configure' 'ac' 'gyp info ok|ok$') `
  "THE claim that matters — node-gyp configure under the jail: rc=$(Rc2 'gyp-configure' 'ac')"

Prop 'python-narrow-grant-runs-python' (HasLine 'py-narrow-exec' 'ac' 'op:narrow-ok=') `
  "exe + DLLs + Lib instead of the whole install root: rc=$(Rc2 'py-narrow-exec' 'ac')"
Prop 'python-root-grant-runs-python' (HasLine 'py-root-exec' 'ac' 'op:root-ok=') `
  "the control for the row above — the shipping whole-root grant: rc=$(Rc2 'py-root-exec' 'ac')"

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED (a clean negative also lands here — read the verbatim output)" }

Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
W ''
W 'GYP PROBE END'
