# FROM-SOURCE native-compile arm for the restricted-token + low-IL build jail.
#
# The prior lane's box had no MSVC, so every native package took the
# prebuilt-DOWNLOAD path and the heaviest write pattern in the ecosystem —
# MSBuild creating build\ with hundreds of intermediates — was never exercised.
# This script exercises it.
#
# Shape mirrors pkg.ps1 and nub's actual design: lay the tree out UNJAILED with
# scripts suppressed (as the linker would), then run the package's real lifecycle
# script UNDER the jail from its own package dir. npm_config_build_from_source
# forces the compile so a prebuild download cannot silently stand in for it.
#
#   powershell -File gyp.ps1 -Mode arm -Arm A_none    -Il none -Grants ''
#   powershell -File gyp.ps1 -Mode arm -Arm B_low_all -Il low  -Grants pkgdir,temp,cache,nodegyp
#   powershell -File gyp.ps1 -Mode writes -Il low -Grants pkgdir,temp,cache,nodegyp
#   powershell -File gyp.ps1 -Mode labelcost -Arm B_low_all
param(
  [string]$Mode = 'arm',
  [string]$Arm = 'A_none',
  [string]$Il = 'none',
  [string]$Grants = '',
  [switch]$ColdHeaders,
  [string]$Only = ''
)

$ErrorActionPreference = 'Continue'
$PROBE = Join-Path $env:USERPROFILE 'p'
$J = Join-Path $PROBE 'Jail.exe'
$NPM = 'C:\node\npm.cmd'
$ROOT = Join-Path $env:USERPROFILE 'gyp'
$LOG = Join-Path $PROBE 'gyp-findings.txt'

# Grant targets. Every one of these is Medium-labeled by default, so a low-IL
# child cannot write it until we relabel — which is what makes each a GRANT.
$LOWTMP = Join-Path $env:LOCALAPPDATA 'Temp\Low'
$MEDTMP = Join-Path $env:LOCALAPPDATA 'Temp'
$CACHE = Join-Path $env:LOCALAPPDATA 'nub\cache'
# node-gyp's header/devdir cache moved across versions: <=8 used ~/.node-gyp,
# 9+ uses env-paths -> %LOCALAPPDATA%\node-gyp\Cache. Cover both; the arms report
# which one the installed node-gyp actually touched.
$GYPDIRS = @((Join-Path $env:USERPROFILE '.node-gyp'), (Join-Path $env:LOCALAPPDATA 'node-gyp'))

# name => package spec. Every entry must COMPILE, not download.
# The last two fail at BASELINE for toolchain reasons that have nothing to do with
# the jail (see gyp-findings). They stay in the corpus deliberately: an identical
# failure in every arm is what proves a failure is not jail-attributable.
$CORPUS = @(
  @{ n = 'better-sqlite3'; spec = 'better-sqlite3@11.10.0' }   # big C++, sqlite3 amalgamation
  @{ n = 'cpu-features'; spec = 'cpu-features@0.0.10' }        # zero prebuilds, always compiles
  @{ n = 'keytar'; spec = 'keytar@7.9.0' }                     # node-addon-api boilerplate
  @{ n = 'gc-stats'; spec = 'gc-stats@1.4.1' }                 # zero prebuilds, zero optionalDeps
  @{ n = 'bcrypt'; spec = 'bcrypt@5.1.1' }                     # node-pre-gyp: spawn EINVAL on Node 24
  @{ n = 'sqlite3'; spec = 'sqlite3@5.1.7' }                   # bundles old node-gyp: no SDK 10.0.26100
)
if ($Only) { $sel = $Only -split ','; $CORPUS = @($CORPUS | Where-Object { $sel -contains $_.n }) }

function Say($s) { Write-Output $s; Add-Content -Path $LOG -Value $s }
function Hdr($s) { Say ''; Say "########## $s" }
function Has($g) { return ($Grants -split ',') -contains $g }

# ---------------- grant application ----------------
# Grants are per-launch and expressed as a LOW mandatory label on objects the
# probe account OWNS. Withholding one is how each arm attributes a failure.
function ApplyGrants($pkgRoot) {
  New-Item -ItemType Directory -Force -Path $LOWTMP, $CACHE, $pkgRoot | Out-Null
  foreach ($d in $GYPDIRS) { New-Item -ItemType Directory -Force -Path $d | Out-Null }

  if (Has 'temp') { & $J label $LOWTMP low | Out-Null; $env:TEMP = $LOWTMP; $env:TMP = $LOWTMP }
  else { $env:TEMP = $MEDTMP; $env:TMP = $MEDTMP }

  if (Has 'cache') { & $J label $CACHE low | Out-Null }
  else { & $J label $CACHE remove | Out-Null }
  $env:npm_config_cache = $CACHE

  foreach ($d in $GYPDIRS) {
    # `remove` must be RECURSIVE: an earlier arm labeled this tree with -r, and
    # clearing only the top level would leave Low labels on the children, so a
    # "withheld" arm would silently still be granted.
    if (Has 'nodegyp') { & $J label $d low -r | Out-Null } else { & $J label $d remove -r | Out-Null }
  }

  if (Has 'pkgdir') { & $J label $pkgRoot low -r | Select-Object -Last 1 | ForEach-Object { Say "  grant pkgdir: $_" } }
}

# ---------------- fixture ----------------
# Fresh tree per arm at its own path. A re-used tree carries the previous arm's
# build\ output and would read as a pass.
# Returns NOTHING on purpose. A PowerShell function returns its whole pipeline,
# so emitting npm's output or a Say line here would come back as part of the
# "return value" and be used as a path — which it was, silently, until a smoke
# arm caught it. The caller computes the directory itself.
function Layout($d, $armName, $pkg) {
  if (Test-Path $d) { Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue }
  New-Item -ItemType Directory -Force -Path $d | Out-Null
  # Bump the host fixture's name+version per arm: a stale identity is how an
  # edited fixture silently re-materializes a cached older copy.
  Set-Content -Path (Join-Path $d 'package.json') `
    -Value "{`"name`":`"host-$($armName.ToLower())`",`"version`":`"1.0.$([Math]::Abs($armName.GetHashCode()) % 997)`",`"private`":true}"
  Push-Location $d
  $env:npm_config_build_from_source = ''
  $nout = Join-Path $PROBE "layout-$armName-$($pkg.n).out"
  & $NPM install --ignore-scripts --no-audit --no-fund --loglevel=error $pkg.spec > $nout 2>&1
  $rc = $LASTEXITCODE
  Pop-Location
  Say "  layout $($pkg.n) rc=$rc"
}

# ---------------- artifact accounting ----------------
# Exit code is not evidence: packages in this corpus exit 0 with the work
# skipped. Bytes and object-file counts are.
function Artifacts($pkgDir, $name) {
  $b = Join-Path $pkgDir 'build'
  if (-not (Test-Path $b)) { return "$name build=ABSENT" }
  $all = @(Get-ChildItem -Recurse -File $b -ErrorAction SilentlyContinue)
  $objs = @($all | Where-Object { $_.Extension -eq '.obj' })
  $nodes = @($all | Where-Object { $_.Extension -eq '.node' })
  $nb = ($nodes | ForEach-Object { "$($_.Name)=$($_.Length)B" }) -join ' '
  $tot = ($all | Measure-Object -Property Length -Sum).Sum
  return ("$name build_files=$($all.Count) build_bytes=$tot obj=$($objs.Count) node[$nb]")
}

# ---------------- modes ----------------

if ($Mode -eq 'arm') {
  Hdr "ARM $Arm  il=$Il  grants=[$Grants]  coldheaders=$ColdHeaders"
  & $J report | ForEach-Object { Say "  $_" }

  if ($ColdHeaders) {
    foreach ($d in $GYPDIRS) { Remove-Item -Recurse -Force $d -ErrorAction SilentlyContinue }
    Say '  headers: devdir wiped (COLD)'
  }

  foreach ($pkg in $CORPUS) {
    $d = Join-Path (Join-Path $ROOT $Arm) $pkg.n
    Layout $d $Arm $pkg
    $pd = Join-Path $d "node_modules\$($pkg.n)"
    if (-not (Test-Path (Join-Path $pd 'package.json'))) { Say "SKIP $($pkg.n) (not laid out)"; continue }
    $pj = Get-Content (Join-Path $pd 'package.json') -Raw | ConvertFrom-Json
    $scripts = @()
    foreach ($h in @('preinstall', 'install', 'postinstall')) {
      if ($pj.scripts -and $pj.scripts.$h) { $scripts += , @($h, $pj.scripts.$h) }
    }
    # npm synthesizes `node-gyp rebuild` when binding.gyp is present and there is
    # no own install script — "no script" is not "no work".
    if ($scripts.Count -eq 0 -and (Test-Path (Join-Path $pd 'binding.gyp')) -and $pj.gypfile -ne $false) {
      $scripts += , @('install(synthesized)', 'node-gyp rebuild')
    }
    if ($scripts.Count -eq 0) { Say "NOOP $($pkg.n)"; continue }

    ApplyGrants $d
    # THE forcing function: without this, prebuild-install / node-pre-gyp fetch a
    # binary and the MSBuild path this whole arm exists to test never runs.
    $env:npm_config_build_from_source = 'true'
    # npm's own node-gyp-bin shim dir is on PATH during a real lifecycle script —
    # that is how a bare `node-gyp rebuild` resolves at all. Omitting it makes
    # every from-source package fail "'node-gyp' is not recognized", which reads
    # like a jail failure and is not one.
    $env:PATH = "$d\node_modules\.bin;C:\node\node_modules\npm\bin\node-gyp-bin;C:\node;C:\Python312;C:\Python312\Scripts;$env:PATH"

    foreach ($s in $scripts) {
      $hook = $s[0]; $cmd = $s[1]
      $out = Join-Path $PROBE "gyp-$Arm-$($pkg.n)-$hook.out"
      Say ''
      Say "----- $($pkg.n) [$hook] il=$Il :: $cmd"
      $sw = [Diagnostics.Stopwatch]::StartNew()
      $a = @('launch', '--il', $Il, '--api', 'asuser', '--out', $out, '--cwd', $pd, '--',
        'cmd.exe', '/d', '/s', '/c', "`"$cmd`"")
      $res = & $J @a
      $jrc = $LASTEXITCODE   # Jail.exe exits WITH the child's code
      $sw.Stop()
      $launch = ($res | Select-String 'LAUNCHED|FAILED' | Select-Object -First 1)
      Say "  rc=$jrc  wall_ms=$($sw.ElapsedMilliseconds)  [$launch]"
      # Surface the first real diagnostic: a denied write names its own path,
      # which is how a missing grant gets identified rather than guessed.
      $tail = @(Get-Content $out -ErrorAction SilentlyContinue |
        Select-String -Pattern 'error|Error|denied|Denied|EACCES|EPERM|EBADF|ENOENT|fatal|MSB|LNK|C1|C2|gyp ERR' |
        Select-Object -First 12)
      foreach ($t in $tail) { Say "    | $t" }
    }
    Say "  ARTIFACT $(Artifacts $pd $pkg.n)"
  }
  exit 0
}

if ($Mode -eq 'writes') {
  # A direct label-state map: which candidate location can a low-IL child write?
  # Cheaper and more legible than inferring the same thing from build failures.
  Hdr "WRITE MAP il=$Il grants=[$Grants]"
  ApplyGrants (Join-Path $ROOT 'writemap')
  $targets = @(
    @{ n = 'TEMP(effective)'; p = $env:TEMP }
    @{ n = 'LOCALAPPDATA\Temp(medium)'; p = $MEDTMP }
    @{ n = 'nub cache'; p = $CACHE }
    @{ n = 'USERPROFILE\.node-gyp'; p = $GYPDIRS[0] }
    @{ n = 'LOCALAPPDATA\node-gyp'; p = $GYPDIRS[1] }
    @{ n = 'APPDATA\npm-cache'; p = (Join-Path $env:APPDATA 'npm-cache') }
    @{ n = 'LOCALAPPDATA\Microsoft\VisualStudio'; p = (Join-Path $env:LOCALAPPDATA 'Microsoft\VisualStudio') }
    @{ n = 'LOCALAPPDATA\Microsoft\Windows\PowerShell'; p = (Join-Path $env:LOCALAPPDATA 'Microsoft\Windows\PowerShell') }
    @{ n = 'USERPROFILE\.vs'; p = (Join-Path $env:USERPROFILE '.vs') }
    @{ n = 'USERPROFILE(negative control)'; p = $env:USERPROFILE }
    @{ n = 'VS install tree'; p = 'C:\BuildTools' }
    @{ n = 'C:\ (negative control)'; p = 'C:\' }
  )
  $lines = @()
  foreach ($t in $targets) {
    # Create each target UNJAILED first: otherwise a missing directory yields the
    # same DENY as a labeled one and the map means nothing.
    New-Item -ItemType Directory -Force -Path $t.p -ErrorAction SilentlyContinue | Out-Null
    $f = Join-Path $t.p ("wm-" + [Guid]::NewGuid().ToString('N').Substring(0, 8) + ".tmp")
    $lines += "echo x > `"$f`" && echo ALLOW $($t.n) || echo DENY $($t.n)"
  }
  $lines += 'reg add HKCU\Software\nubjail /v x /t REG_SZ /d y /f >nul 2>&1 && echo ALLOW HKCU-write || echo DENY HKCU-write'
  $bat = Join-Path $PROBE 'writemap.cmd'
  Set-Content -Path $bat -Value (@('@echo off') + $lines) -Encoding ascii
  $out = Join-Path $PROBE "writemap-$Il.out"
  & $J launch --il $Il --api asuser --out $out --cwd $PROBE -- cmd.exe /d /s /c "`"$bat`"" | Out-Null
  Get-Content $out -ErrorAction SilentlyContinue | ForEach-Object { if ($_.Trim()) { Say "  $_" } }
  exit 0
}

if ($Mode -eq 'labelcost') {
  # Cost matters because grants are per-launch: an MSBuild build\ tree is orders
  # of magnitude bigger than the 11-entry fixture the prior lane timed.
  Hdr "LABEL COST on a real post-build tree ($Arm)"
  foreach ($pkg in $CORPUS) {
    $d = Join-Path (Join-Path $ROOT $Arm) $pkg.n
    if (-not (Test-Path $d)) { continue }
    $n = @(Get-ChildItem -Recurse -Force $d -ErrorAction SilentlyContinue).Count
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $r = & $J label $d low -r | Select-Object -Last 1
    $sw.Stop()
    Say "  $($pkg.n): entries=$n  ms=$($sw.ElapsedMilliseconds)  $r"
  }
  exit 0
}

if ($Mode -eq 'verify') {
  Hdr "ARTIFACTS $Arm"
  foreach ($pkg in $CORPUS) {
    $pd = Join-Path (Join-Path (Join-Path $ROOT $Arm) $pkg.n) "node_modules\$($pkg.n)"
    if (Test-Path $pd) { Say "  $(Artifacts $pd $pkg.n)" } else { Say "  $($pkg.n) MISSING" }
  }
  exit 0
}

Write-Output "unknown mode $Mode"
