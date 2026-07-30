# REPRODUCE NUB'S ACTUAL LAUNCH, then re-measure everything the earlier probes claimed.
#
# WHY THIS EXISTS: every arm in `works.ps1` and `gyp.ps1` launched a BARE AppContainer — zero
# capabilities, no ancestor repair, no preload — and I reported its results as facts about nub. They
# are facts about raw AppContainer physics. nub's production launch
# (`crates/nub-sandbox/src/backend/windows.rs`, `crates/nub-sandbox/src/compiler/defaults.rs`,
# `crates/nub-cli/src/pm_engine/build_jail.rs`) carries THREE repairs none of those arms had, and
# each one targets a defect I attributed to the mechanism:
#
#   1. THE ANCESTOR REPAIR (`windows.rs:1555-1595`). Two mechanisms, because one cannot cover the
#      chain unprivileged. Where nub holds `WRITE_DAC` it writes a NON-INHERITED ace carrying exactly
#      traverse + read-attributes (`TRAVERSE_MASK = 0x001000a1`). Where it does not — `C:\`,
#      `C:\Users` — it REQUESTS the capability Windows ALREADY granted on that exact path: `C:\`
#      carries `(A;;0x1000a1;;;S-1-15-3-65536-…)`, the same mask, and requesting a raw capability sid
#      is UNPRIVILEGED. That is the unprivileged equivalent of the privileged `C:\` ace my
#      `ac-croot` arm needed, and my `ac-ancestors` arm tested neither half of it: it wrote
#      `ReadAndExecute` on two paths nub already owns and requested no capability at all.
#   2. THE STDIO SHIM (`defaults.rs:648-692`). `windows_stdio_shim.js`, delivered as
#      `--import data:text/javascript;base64,…` on a STAMPED `NODE_OPTIONS`. Its header documents the
#      exact spin I measured — libuv retrying a refused global-NPFS pipe name forever inside
#      `uv_spawn` — and calls `execFile` "exact" under the repair. `data:` is load-bearing:
#      `defaultResolve` short-circuits it before any filesystem access, so the preload needs no grant.
#      Base64 rather than percent-encoding because `NODE_OPTIONS` is whitespace-split.
#   3. THE REALPATH SHIM (`windows_realpath_shim.js` + `--preserve-symlinks-main`), which is the
#      SHIPPABLE fix for the defect my arms worked around with the unshippable
#      `--preserve-symlinks --preserve-symlinks-main` pair.
#
# THE CONTROL IS THE POINT. `ac-bare` reproduces the defect in the same run, so a green `ac-prod` is
# a measurement rather than an assertion — the discipline `windows.rs:1580-1583` states for its own
# `NUB_SANDBOX_WIN_NO_ANCESTOR_REPAIR` seam: "without an arm where the defect still reproduces, a
# green repaired arm is measuring nothing."
#
# AND NO ARM COUNTS UNLESS THE SHIM ACTUALLY LOADED. `globalThis.__nubJailStdioShim` is the only
# in-process evidence the module installed (the sentinel `windows_stdio_shim.js:42` sets), so the
# node arms assert it and `shim-was-active` gates reading any of them. A preload that silently failed
# to resolve would otherwise look exactly like a repair that did not work.

Set-StrictMode -Off
$ErrorActionPreference = 'Continue'

. (Join-Path $PSScriptRoot 'verdict.ps1')
. (Join-Path $PSScriptRoot 'launcher.ps1')

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)

W '== host =='
Fact 'os' ((Get-CimInstance Win32_OperatingSystem).Caption)
Fact 'os-build' ((Get-CimInstance Win32_OperatingSystem).BuildNumber)
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'node-version' (& node --version 2>&1)
Fact 'winps-version' (& powershell -NoLogo -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' 2>&1)

# ─────────────────────────────── fixture ───────────────────────────────

$root      = Join-Path $env:USERPROFILE "nubprod-$nonce"
$binDir    = Join-Path $root 'bin'
$workDir   = Join-Path $root 'work'
$deepDir   = Join-Path $workDir 'deep'
$tmpDir    = Join-Path $root 'tmp'
$ungranted = Join-Path $root 'ungranted'
foreach ($d in @($root, $binDir, $workDir, $deepDir, $tmpDir, $ungranted)) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
}
$acl = Get-Acl -LiteralPath $root
$acl.SetAccessRuleProtection($true, $true)
Set-Acl -LiteralPath $root -AclObject $acl

$sys32   = Join-Path $env:SystemRoot 'System32'
$cmdExe  = Join-Path $sys32 'cmd.exe'
$winPs   = Join-Path $sys32 'WindowsPowerShell\v1.0\powershell.exe'
$nodeExe = (Get-Command node -ErrorAction Stop).Source
$nodeDir = Split-Path -Parent $nodeExe
$pyExe   = ''; $c = Get-Command python -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source }
if (-not $pyExe) { $c = Get-Command python3 -ErrorAction SilentlyContinue; if ($c) { $pyExe = $c.Source } }
$pyDir   = if ($pyExe) { Split-Path -Parent $pyExe } else { '' }
$gypJs   = Join-Path $nodeDir 'node_modules\npm\node_modules\node-gyp\bin\node-gyp.js'
$csFile  = Join-Path $nodeDir 'node_modules\npm\node_modules\node-gyp\lib\Find-VisualStudio.cs'
$devDir  = Join-Path $env:LOCALAPPDATA 'node-gyp\Cache'

# THE PRELOAD, built exactly as `build_jail.rs:165-173` builds it: base64 of the shim bytes in an
# `--import data:` term. Only the stdio term — the net gate needs a policy JSON this probe has no
# business synthesising, and the realpath term is measured by a sibling lane. Reported so a reader
# can confirm the value is the production shape and not a paraphrase of it.
$shimPath = Join-Path $PSScriptRoot 'windows_stdio_shim.js'
$shimB64 = if (Test-Path -LiteralPath $shimPath) {
  [Convert]::ToBase64String([IO.File]::ReadAllBytes($shimPath))
} else { '' }
$shimNodeOptions = if ($shimB64) { "--import data:text/javascript;base64,$shimB64" } else { '' }
Fact 'shim/file' $(if ($shimB64) { $shimPath } else { "MISSING $shimPath" })
Fact 'shim/base64-len' $shimB64.Length
Fact 'shim/node-options-len' $shimNodeOptions.Length
Fact 'shim/node-options-head' $(if ($shimNodeOptions.Length -gt 90) { $shimNodeOptions.Substring(0, 90) } else { $shimNodeOptions })

# ─────────────────────────────── ace + capability plumbing ───────────────────────────────

# SYNCHRONIZE | FILE_READ_ATTRIBUTES | FILE_TRAVERSE | FILE_LIST_DIRECTORY — the mask
# `windows.rs:924` uses, commented there as byte-identical to the one Windows itself puts on `C:\`
# for that path's capability sid.
$TRAVERSE_MASK = 0x001000a1

function Grant-Raw([string]$path, [string]$sid, [int]$mask, [bool]$inheritable) {
  $a = Get-Acl -LiteralPath $path
  $inherit = if ($inheritable) {
    [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [Security.AccessControl.InheritanceFlags]::ObjectInherit
  } else { [Security.AccessControl.InheritanceFlags]::None }
  $a.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule(
    (New-Object Security.Principal.SecurityIdentifier($sid)),
    [Security.AccessControl.FileSystemRights]$mask, $inherit,
    [Security.AccessControl.PropagationFlags]::None,
    [Security.AccessControl.AccessControlType]::Allow)))
  Set-Acl -LiteralPath $path -AclObject $a
}
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

# `ancestor_chain` (`windows.rs:1046`): every granted leaf's STRICT ancestors, deduped,
# shallowest-first.
function Get-AncestorChain([string[]]$leaves) {
  $seen = @{}; $out = @()
  foreach ($leaf in $leaves) {
    if (-not $leaf) { continue }
    $chain = @()
    $p = Split-Path -Parent $leaf
    while ($p) {
      $chain += $p
      $next = Split-Path -Parent $p
      if ($next -eq $p -or -not $next) { break }
      $p = $next
    }
    [array]::Reverse($chain)
    foreach ($d in $chain) { if (-not $seen.ContainsKey($d)) { $seen[$d] = $true; $out += $d } }
  }
  return $out
}

# `harvest_capability_sids` (`windows.rs:1080`): the capability sids (`S-1-15-3-…`) named by an ALLOW
# ace on any ancestor. Read off the SDDL because a capability sid does not resolve through
# `LookupAccountSid` (`ERROR_NONE_MAPPED`), so there is no name to match on.
function Get-CapabilitySids([string[]]$paths) {
  $seen = @{}
  foreach ($p in $paths) {
    $sddl = $null
    try { $sddl = (Get-Acl -LiteralPath $p).GetSecurityDescriptorSddlForm('Access') } catch { continue }
    if (-not $sddl) { continue }
    foreach ($m in [regex]::Matches($sddl, '\(A;[^)]*;(S-1-15-3-[0-9\-]+)\)')) {
      $seen[$m.Groups[1].Value] = $true
    }
  }
  return @($seen.Keys)
}

# ─────────────────────────────── the batteries ───────────────────────────────

$batCmd = @'
@echo off
echo op:cwd=%CD%
if exist "%BT_WORK%" (echo op:exist-granted-dir=yes) else (echo op:exist-granted-dir=no)
if exist "C:\" (echo op:exist-croot=yes) else (echo op:exist-croot=no)
if exist "C:\Windows" (echo op:exist-windows=yes) else (echo op:exist-windows=no)
if exist "%ProgramFiles%" (echo op:exist-programfiles=yes) else (echo op:exist-programfiles=no)
if exist "%BT_UNGRANTED%\secret.txt" (echo op:exist-ungranted=yes) else (echo op:exist-ungranted=no)
cd /d "%BT_DEEP%" && (echo op:cd-deep-abs=ok) || (echo op:cd-deep-abs=FAIL)
cd /d "%BT_WORK%" && (echo op:cd-back-abs=ok) || (echo op:cd-back-abs=FAIL)
(dir /b /a-d "%BT_WORK%" > "%BT_TMP%\d.txt") && (echo op:dir-granted-abs=ok) || (echo op:dir-granted-abs=FAIL)
(type "%BT_WORK%\marker.txt" > "%BT_TMP%\t.txt") && (echo op:type-granted=ok) || (echo op:type-granted=FAIL)
(type "%BT_UNGRANTED%\secret.txt" > "%BT_TMP%\u.txt") && (echo op:type-ungranted=ok) || (echo op:type-ungranted=FAIL)
echo op:end=ok
'@

$batPs = @'
$ErrorActionPreference = 'Continue'
function Op($k, $v) { Write-Output "op:$k=$v" }
function Try1($k, $sb) { try { $ErrorActionPreference = 'Stop'; Op $k (& $sb) } catch { Op $k ("ERR " + $_.Exception.GetType().Name) } }
Op 'cwd' (Get-Location).Path
Op 'test-path-croot' (Test-Path -LiteralPath 'C:\')
Op 'test-path-ungranted' (Test-Path -LiteralPath "$env:BT_UNGRANTED\secret.txt")
Try1 'set-location-deep' { Set-Location -LiteralPath $env:BT_DEEP; (Get-Location).Path }
Try1 'set-location-back' { Set-Location -LiteralPath $env:BT_WORK; (Get-Location).Path }
Try1 'getattributes-croot' { [IO.File]::GetAttributes('C:\').ToString() }
Try1 'spawn-whoami' { $o = & "$env:BT_SYS32\whoami.exe"; "rc=$LASTEXITCODE out=$o" }
Op 'end' 'ok'
'@

# The sentinel is asserted FIRST, before anything that depends on the repair, so an arm whose
# preload never resolved is distinguishable from one whose repair did not work.
$spawnJs = @'
const { execFile } = require('child_process');
console.log('op:shim-sentinel=' + (globalThis.__nubJailStdioShim === true));
console.log('op:node-options-present=' + (typeof process.env.NODE_OPTIONS === 'string' && process.env.NODE_OPTIONS.includes('--import')));
const ps = process.env.BT_WINPS;
(async () => {
  await new Promise((resolve) => {
    const t = setTimeout(() => { console.log('op:execfile-piped=TIMEOUT'); resolve(); }, 30000);
    execFile(ps, ['-NoProfile', '-Command', 'Write-Output PS-FROM-NODE'],
      { encoding: 'utf8', maxBuffer: 8 << 20 }, (err, stdout) => {
        clearTimeout(t);
        console.log('op:execfile-piped=rc:' + (err ? (err.code === undefined ? 'ERR' : err.code) : 0) +
          ' out:' + (stdout || '').trim().split(/\r?\n/)[0]);
        resolve();
      });
  });
  console.log('op:end=ok');
})();
'@

function Reset-Work {
  Get-ChildItem -LiteralPath $workDir -Force -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
  Get-ChildItem -LiteralPath $tmpDir -Force -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path $deepDir | Out-Null
  [IO.File]::WriteAllText((Join-Path $workDir 'marker.txt'), "MARKER-CONTENT`n")
  [IO.File]::WriteAllText((Join-Path $deepDir 'deep.txt'), "DEEP`n")
  [IO.File]::WriteAllText((Join-Path $workDir 'bat.cmd'), ($batCmd -replace "`r?`n", "`r`n"))
  [IO.File]::WriteAllText((Join-Path $workDir 'bat.ps1'), $batPs)
  [IO.File]::WriteAllText((Join-Path $workDir 'spawn.js'), $spawnJs)
  [IO.File]::WriteAllText((Join-Path $workDir 'binding.gyp'),
    '{ "targets": [ { "target_name": "probe", "sources": [ "probe.cc" ] } ] }')
  [IO.File]::WriteAllText((Join-Path $workDir 'probe.cc'),
    "#include <node_api.h>`nnapi_value Init(napi_env env, napi_value exports) { return exports; }`nNAPI_MODULE(NODE_GYP_MODULE_NAME, Init)`n")
  [IO.File]::WriteAllText((Join-Path $workDir 'package.json'), '{ "name": "probe", "version": "1.0.0" }')
}
[IO.File]::WriteAllText((Join-Path $ungranted 'secret.txt'), "SECRET`n")
Reset-Work

$logDir = Join-Path (Get-Location) 'prod-logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$cells = @{}

# ─────────────────────────────── the arm runner ───────────────────────────────

function Invoke-Arm {
  param(
    [string]$Battery,
    [string]$Arm,        # 'plain' | 'ac-bare' | 'ac-prod'
    [string]$Exe, [string]$Cmdline,
    [string[]]$Reads = @(), [int]$TimeoutMs = 60000,
    [switch]$Shim
  )
  if (-not $cells.ContainsKey($Battery)) { $cells[$Battery] = @{} }
  if (-not $Exe -or -not (Test-Path -LiteralPath $Exe)) {
    $cells[$Battery][$Arm] = @{ rc = 'TOOL-ABSENT'; lines = @(); caps = ''; aces = '' }
    return
  }
  Reset-Work
  $confined = ($Arm -ne 'plain')
  $prod = ($Arm -eq 'ac-prod')
  $sid = ''; $profileName = ''; $granted = @(); $traversed = @(); $capSids = @()
  try {
    if ($confined) {
      $nm = "nubpd_${nonce}_$(($Battery + '_' + $Arm) -replace '[^A-Za-z0-9]','_')"
      if ($nm.Length -gt 60) { $nm = $nm.Substring(0, 60) }
      $profileName = $nm
      $sid = [Bt]::CreateProfile($profileName)
      if ($sid -like 'ERR*') {
        $cells[$Battery][$Arm] = @{ rc = "PROFILE-$sid"; lines = @(); caps = ''; aces = '' }
        $profileName = ''; return
      }
      Grant-Ace $binDir  $sid 'ReadAndExecute' $true; $granted += $binDir
      Grant-Ace $workDir $sid 'Modify'         $true; $granted += $workDir
      Grant-Ace $tmpDir  $sid 'Modify'         $true; $granted += $tmpDir
      foreach ($r in $Reads) {
        if (-not (Test-Path -LiteralPath $r)) { continue }
        Grant-Ace $r $sid 'ReadAndExecute' $true; $granted += $r
      }
      if ($prod) {
        # THE ANCESTOR REPAIR, both halves. The ace write is best-effort by design — a refused one
        # is skipped, exactly as `windows.rs` does — and the capabilities are harvested off the
        # SAME chain, which is what covers the paths the write could not reach.
        $leaves = @($workDir, $binDir, $tmpDir) + @($Reads) + @($Exe)
        $chain = Get-AncestorChain $leaves
        foreach ($dir in $chain) {
          try { Grant-Raw $dir $sid $TRAVERSE_MASK $false; $traversed += $dir } catch { }
        }
        $capSids = Get-CapabilitySids $chain
      }
    }
    $prev = @{ TEMP = $env:TEMP; TMP = $env:TMP; PATH = $env:PATH; NODE_OPTIONS = $env:NODE_OPTIONS }
    try {
      $env:TEMP = $tmpDir; $env:TMP = $tmpDir
      $env:PATH = "$binDir;$sys32;$($prev.PATH)"
      # STAMPED, never merged — `defaults.rs:534` admits `NODE_OPTIONS` to the lifecycle env
      # allowlist only because the jail overwrites whatever the user's environment held.
      if ($Shim) { $env:NODE_OPTIONS = $shimNodeOptions } else { $env:NODE_OPTIONS = $null }
      $log = Join-Path $logDir "$Battery.$Arm.log"
      $sw = [Diagnostics.Stopwatch]::StartNew()
      $status = [Bt]::LaunchCaps($sid, $Exe, $Cmdline, $workDir, $log, $TimeoutMs, $false, $capSids)
      $sw.Stop()
      $lines = @()
      if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -ErrorAction SilentlyContinue) }
      $cells[$Battery][$Arm] = @{
        rc = $status; lines = $lines
        caps = ($capSids -join ',')
        aces = "$($traversed.Count) traverse aces"
      }
      W ("  {0,-14} {1,-9} {2,-32} lines={3} {4}ms caps={5} {6}" -f `
        $Battery, $Arm, $status, $lines.Count, $sw.ElapsedMilliseconds, $capSids.Count, $traversed.Count)
    }
    finally {
      $env:TEMP = $prev.TEMP; $env:TMP = $prev.TMP; $env:PATH = $prev.PATH
      $env:NODE_OPTIONS = $prev.NODE_OPTIONS
    }
  }
  finally {
    foreach ($p in $granted)   { try { Revoke-Ace $p $sid } catch { } }
    foreach ($p in $traversed) { try { Revoke-Ace $p $sid } catch { W "    traverse revoke FAILED on $p" } }
    if ($profileName) { $null = [Bt]::DeleteProfile($profileName) }
  }
}

# ─────────────────────────────── the arms ───────────────────────────────

$env:BT_WORK = $workDir; $env:BT_DEEP = $deepDir; $env:BT_UNGRANTED = $ungranted
$env:BT_SYS32 = $sys32;  $env:BT_TMP = $tmpDir;   $env:BT_WINPS = $winPs

${q} = '"'
$gypReads = @($nodeDir, $devDir) + @($pyDir | Where-Object { $_ })

W ''
W '== the capability sids the ancestor chain actually offers =='
$demoChain = Get-AncestorChain @($workDir, $nodeExe)
foreach ($d in $demoChain) {
  Fact "chain[$d]" ((Get-CapabilitySids @($d)) -join ',')
}

W ''
W '== cmd: plain vs BARE AppContainer vs PRODUCTION launch =='
foreach ($arm in @('plain', 'ac-bare', 'ac-prod')) {
  Invoke-Arm -Battery 'cmd' -Arm $arm -Exe $cmdExe `
    -Cmdline "${q}$cmdExe${q} /d /s /c ${q}${q}$workDir\bat.cmd${q}${q}" -TimeoutMs 30000
}

W ''
W '== Windows PowerShell 5.1: same three arms =='
foreach ($arm in @('plain', 'ac-bare', 'ac-prod')) {
  Invoke-Arm -Battery 'ps' -Arm $arm -Exe $winPs `
    -Cmdline "${q}$winPs${q} -NoLogo -NonInteractive -NoProfile -ExecutionPolicy Bypass -File ${q}$workDir\bat.ps1${q}" `
    -TimeoutMs 60000
}

# The node arms carry `--preserve-symlinks-main --preserve-symlinks` on ac-bare only, because the
# SHIPPABLE realpath fix is the sibling lane's `windows_realpath_shim.js` and this probe does not
# stamp it; without either, an unflagged confined node dies before user code and the arm would
# measure the realpath defect instead of the stdio one.
$flags = '--preserve-symlinks-main --preserve-symlinks'
W ''
W '== node execFile with PIPED stdio — the shape node-gyp uses =='
Invoke-Arm -Battery 'node-spawn' -Arm 'plain'   -Exe $nodeExe -Cmdline "${q}$nodeExe${q} $flags ${q}$workDir\spawn.js${q}" -Reads $gypReads -TimeoutMs 90000
Invoke-Arm -Battery 'node-spawn' -Arm 'ac-bare' -Exe $nodeExe -Cmdline "${q}$nodeExe${q} $flags ${q}$workDir\spawn.js${q}" -Reads $gypReads -TimeoutMs 90000
Invoke-Arm -Battery 'node-spawn' -Arm 'ac-prod' -Exe $nodeExe -Cmdline "${q}$nodeExe${q} $flags ${q}$workDir\spawn.js${q}" -Reads $gypReads -TimeoutMs 90000 -Shim

W ''
W '== node-gyp configure end-to-end =='
Fact 'header-prefetch' (& node $gypJs install --devdir $devDir 2>&1 | Select-Object -Last 1)
foreach ($a in @(@('plain', $false), @('ac-bare', $false), @('ac-prod', $true))) {
  if ($a[1]) {
    Invoke-Arm -Battery 'gyp' -Arm $a[0] -Exe $nodeExe -Reads $gypReads -TimeoutMs 240000 -Shim `
      -Cmdline "${q}$nodeExe${q} $flags ${q}$gypJs${q} configure --devdir ${q}$devDir${q}"
  } else {
    Invoke-Arm -Battery 'gyp' -Arm $a[0] -Exe $nodeExe -Reads $gypReads -TimeoutMs 240000 `
      -Cmdline "${q}$nodeExe${q} $flags ${q}$gypJs${q} configure --devdir ${q}$devDir${q}"
  }
}

# ─────────────────────────────── the table ───────────────────────────────

function L([string]$b, [string]$a) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey($a)) { return $null }
  return $cells[$b][$a].lines
}
function R([string]$b, [string]$a) {
  if (-not $cells.ContainsKey($b) -or -not $cells[$b].ContainsKey($a)) { return '-' }
  return $cells[$b][$a].rc
}
function Has([string]$b, [string]$a, [string]$pat) {
  $l = L $b $a; if ($null -eq $l) { return $false }
  return (($l -join "`n") -match $pat)
}
function DiffN([string]$b, [string]$a) {
  $x = L $b 'plain'; $y = L $b $a
  if ($null -eq $x -or $null -eq $y) { return -1 }
  $n = [Math]::Max($x.Count, $y.Count); $d = 0
  for ($i = 0; $i -lt $n; $i++) {
    $lx = if ($i -lt $x.Count) { $x[$i] } else { '<absent>' }
    $ly = if ($i -lt $y.Count) { $y[$i] } else { '<absent>' }
    if ($lx -ne $ly) { $d++ }
  }
  return $d
}

W ''
W '== per-op comparison =='
foreach ($b in @('cmd', 'ps', 'node-spawn', 'gyp')) {
  if (-not $cells.ContainsKey($b)) { continue }
  W "  ── $b ──"
  foreach ($a in @('plain', 'ac-bare', 'ac-prod')) {
    if (-not $cells[$b].ContainsKey($a)) { continue }
    W ("    {0,-9} rc={1,-30} diff-vs-plain={2} caps={3}" -f $a, (R $b $a), (DiffN $b $a), $cells[$b][$a].caps)
  }
  foreach ($a in @('plain', 'ac-bare', 'ac-prod')) {
    $l = L $b $a; if ($null -eq $l) { continue }
    W "    [$a]"
    $show = if ($l.Count -gt 40) { @($l[0..19]) + @('      … trimmed …') + @($l[($l.Count - 20)..($l.Count - 1)]) } else { $l }
    foreach ($x in $show) { W "      $x" }
  }
}

# ─────────────────────────────── verdict ───────────────────────────────

W ''
W '== verdict =='

# ── THE PRELOAD GATE. Nothing below is readable unless the shim actually installed.
Prop 'shim-was-active-in-the-prod-arm' (Has 'node-spawn' 'ac-prod' 'op:shim-sentinel=true') `
  "globalThis.__nubJailStdioShim must be true inside the confined child: $((L 'node-spawn' 'ac-prod') -join ' | ')"
Prop 'shim-was-absent-in-the-bare-arm' (-not (Has 'node-spawn' 'ac-bare' 'op:shim-sentinel=true')) `
  "and FALSE in the control, or the two arms are the same arm"

# ── CONTROLS.
Prop 'baseline-cmd-plain' (Has 'cmd' 'plain' 'op:end=ok') "unconfined cmd must complete: $(R 'cmd' 'plain')"
Prop 'baseline-ps-plain' (Has 'ps' 'plain' 'op:end=ok') "unconfined powershell must complete: $(R 'ps' 'plain')"
Prop 'baseline-node-spawn-plain' (Has 'node-spawn' 'plain' 'op:execfile-piped=rc:0') `
  "unconfined node must execFile powershell: $(R 'node-spawn' 'plain')"

# ── THE DEFECTS MUST REPRODUCE IN THE BARE ARM, or a green prod arm proves nothing.
Prop 'bare-arm-reproduces-cmd-defect' ((Has 'cmd' 'ac-bare' 'op:cd-back-abs=FAIL') -or (Has 'cmd' 'ac-bare' 'op:exist-croot=no')) `
  "the bare AppContainer must still show the cmd defects: $((L 'cmd' 'ac-bare') -join ' | ')"
Prop 'bare-arm-reproduces-piped-spawn-hang' `
  ((R 'node-spawn' 'ac-bare') -match 'TIMED-OUT' -or (Has 'node-spawn' 'ac-bare' 'op:execfile-piped=TIMEOUT')) `
  "the bare AppContainer must still hang on a piped spawn: $(R 'node-spawn' 'ac-bare')"

# ── THE DECISIVE CELLS: does nub's ACTUAL launch repair them, unprivileged?
Prop 'prod-launch-repairs-cmd' ((DiffN 'cmd' 'ac-prod') -eq 1) `
  "cmd under the production launch should differ from unconfined ONLY by the intended ungranted denial: diff=$(DiffN 'cmd' 'ac-prod') (bare was $(DiffN 'cmd' 'ac-bare'))"
Prop 'prod-launch-repairs-cmd-croot' (Has 'cmd' 'ac-prod' 'op:exist-croot=yes') `
  "the capability request should make `if exist C:\` answer correctly with NO privileged ace"
Prop 'prod-launch-repairs-cmd-cd' (Has 'cmd' 'ac-prod' 'op:cd-back-abs=ok') `
  "and `cd` into a granted dir should work"
Prop 'prod-launch-repairs-powershell-spawn' (Has 'ps' 'ac-prod' 'op:spawn-whoami=rc=0') `
  "PowerShell should be able to spawn an external program: $((L 'ps' 'ac-prod') -join ' | ')"
Prop 'prod-launch-repairs-piped-spawn' (Has 'node-spawn' 'ac-prod' 'op:execfile-piped=rc:0') `
  "the stdio shim should make node's piped execFile work: $((L 'node-spawn' 'ac-prod') -join ' | ')"
Prop 'prod-launch-runs-gyp-configure' (Has 'gyp' 'ac-prod' 'gyp info ok') `
  "and node-gyp configure should complete: $(R 'gyp' 'ac-prod')"

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED (a clean negative also lands here — read the per-op comparison)" }

Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
W ''
W 'PROD PROBE END'
