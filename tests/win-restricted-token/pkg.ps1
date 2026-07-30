# Real-package arm. Mirrors what the PM actually does: lay the tree out with
# scripts SUPPRESSED (unjailed, as nub's linker would), then run each package's
# lifecycle script UNDER the jail from its own package dir. Running `npm install`
# with scripts enabled would confound the measurement — the install itself would
# be doing the writes.
#
#   powershell -File pkg.ps1 -Mode setup            # unjailed tree layout
#   powershell -File pkg.ps1 -Mode run -Il low      # jailed lifecycle scripts
param([string]$Mode = 'setup', [string]$Il = 'low', [string]$Only = '')

$ErrorActionPreference = 'Continue'
$PROBE = Join-Path $env:USERPROFILE 'p'
$J = Join-Path $PROBE 'Jail.exe'
$NODE = 'C:\node\node.exe'
$NPM = 'C:\node\npm.cmd'
$root = Join-Path $env:USERPROFILE 'pkgjail'

# name => the lifecycle script(s) to exercise, discovered from the installed
# package.json at run time so we never guess.
$PKGS = @(
  'esbuild', 'sqlite3', 'better-sqlite3', 'keytar', '@sentry/cli',
  'simple-git-hooks', 'yorkie', 'core-js', 'bufferutil', 'msgpackr-extract',
  '@parcel/watcher', 'cpu-features'
)
if ($Only) { $PKGS = @($Only) }

function Slug($n) { $n.Replace('@', '').Replace('/', '-') }

if ($Mode -eq 'setup') {
  New-Item -ItemType Directory -Force -Path $root | Out-Null
  foreach ($p in $PKGS) {
    $d = Join-Path $root (Slug $p)
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    Set-Content -Path (Join-Path $d 'package.json') -Value '{"name":"host","version":"1.0.0","private":true}'
    # A real .git so hook installers have a target OUTSIDE their package dir.
    New-Item -ItemType Directory -Force -Path (Join-Path $d '.git\hooks') | Out-Null
    Set-Content -Path (Join-Path $d '.git\HEAD') -Value 'ref: refs/heads/main'
    Push-Location $d
    & $NPM install --ignore-scripts --no-audit --no-fund --loglevel=error $p 2>&1 | Select-Object -Last 3
    Write-Output "setup $p rc=$LASTEXITCODE"
    Pop-Location
  }
  Write-Output "root=$root"
  exit 0
}

# ---- run: for each package, execute its real lifecycle script under the jail ----
# Apply the design's actual grants first. Measuring without them would be a
# strawman: %TEMP% is Medium-labeled, and node-gyp / npm / tarball extraction /
# prebuild-install all write there, so a jail that does not grant it fails
# everything for one uninteresting reason.
$lowTmp = Join-Path $env:LOCALAPPDATA 'Temp\Low'
New-Item -ItemType Directory -Force -Path $lowTmp | Out-Null
& $J label $lowTmp low
$env:TEMP = $lowTmp; $env:TMP = $lowTmp

foreach ($p in $PKGS) {
  $d = Join-Path $root (Slug $p)
  $pd = Join-Path $d "node_modules\$($p.Replace('/','\'))"
  if (-not (Test-Path (Join-Path $pd 'package.json'))) { Write-Output "SKIP $p (not installed)"; continue }
  $pj = Get-Content (Join-Path $pd 'package.json') -Raw | ConvertFrom-Json
  $scripts = @()
  foreach ($h in @('preinstall', 'install', 'postinstall')) {
    if ($pj.scripts -and $pj.scripts.$h) { $scripts += , @($h, $pj.scripts.$h) }
  }
  # npm SYNTHESIZES `node-gyp rebuild` when binding.gyp is present with no own
  # install script and no gypfile:false (@npmcli/node-gyp) — "no script" is not
  # "no work", so reproduce that here rather than reporting a false no-op.
  if ($scripts.Count -eq 0 -and (Test-Path (Join-Path $pd 'binding.gyp')) -and $pj.gypfile -ne $false) {
    $scripts += , @('install(synthesized)', 'node-gyp rebuild')
  }
  if ($scripts.Count -eq 0) { Write-Output "NOOP $p (no lifecycle script, no binding.gyp)"; continue }

  if ($Il -eq 'low') { & $J label $d low -r | Select-Object -Last 1 }
  foreach ($s in $scripts) {
    $hook = $s[0]; $cmd = $s[1]
    $log = Join-Path $PROBE "pkg-$(Slug $p)-$hook.out"
    Write-Output ""
    Write-Output "########## $p [$hook] il=$Il :: $cmd"
    $env:PATH = "$d\node_modules\.bin;C:\node;$env:PATH"
    $a = @('launch', '--il', $Il, '--api', 'asuser', '--out', $log, '--cwd', $pd, '--',
      'cmd.exe', '/d', '/s', '/c', "`"$cmd`"")
    & $J @a
  }
}
