# Round 3 for nubjs/nub#576. Rounds 1-2 showed a clean install -> add never
# fails and that post-hoc ACL/dangling/partial perturbations are all absorbed.
# These modes instead attack the ORIGIN of the "target directory is freshly
# created" invariant: an install that never finished, a tree another package
# manager wrote first, a workspace (the link.rs:1137 branch), and concurrency.

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$Mode,
  [Parameter(Mandatory = $true)][string]$Root
)

$ErrorActionPreference = 'Continue'
$Label = $Mode
function Section($t) { Write-Host ""; Write-Host "########## [$Label] $t ##########" }

function Run-Nub($fixture, $argList, $tag) {
  Section "RUN: nub $($argList -join ' ')   ($tag)"
  Push-Location $fixture
  & nub @argList 2>&1 | Out-String -Width 400 | Write-Host
  $code = $LASTEXITCODE
  Pop-Location
  Write-Host "[$Label] EXIT CODE ($tag) = $code"
  return $code
}

$pkg = @'
{
  "name": "nub576-fixture",
  "private": true,
  "version": "1.0.0",
  "dependencies": { "express": "^4.21.2" },
  "devDependencies": {
    "@types/express": "^5.0.0",
    "@types/node": "^24.0.0",
    "typescript": "^5.6.3"
  }
}
'@

$fixture = Join-Path $Root "r3-$Mode"
if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force -Path $fixture | Out-Null

$rcSetup = 0
$rcAdd = 0

switch ($Mode) {
  'workspace' {
    # Root + one member; the member carries the @types/express graph so the
    # failing entry lives under the workspace linker branch.
    @'
{ "name": "ws-root", "private": true, "version": "1.0.0", "workspaces": ["packages/*"] }
'@ | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')
    $m = Join-Path $fixture 'packages\app'
    New-Item -ItemType Directory -Force -Path $m | Out-Null
    $pkg | Set-Content -Encoding utf8 (Join-Path $m 'package.json')
    $rcSetup = Run-Nub $fixture @('install') 'workspace install'
    $rcAdd = Run-Nub $fixture @('add', '-D', 'oxlint') 'add at workspace root'
    if ($rcAdd -eq 0) { $rcAdd = Run-Nub $m @('add', '-D', 'prettier') 'add inside the member' }
  }
  'afterNpm' {
    $pkg | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')
    Push-Location $fixture; & npm install --silent 2>&1 | Out-String -Width 200 | Write-Host; Pop-Location
    Write-Host "npm node_modules present: $(Test-Path (Join-Path $fixture 'node_modules'))"
    $rcSetup = Run-Nub $fixture @('install') 'nub install OVER an npm tree'
    $rcAdd = Run-Nub $fixture @('add', '-D', 'oxlint') 'add'
  }
  'interrupted' {
    $pkg | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')
    # Kill install mid-link, repeatedly, then let a normal install + add run.
    foreach ($ms in @(900, 1400, 1900)) {
      $p = Start-Process -FilePath 'nub' -ArgumentList 'install' -WorkingDirectory $fixture -PassThru -NoNewWindow
      Start-Sleep -Milliseconds $ms
      if (-not $p.HasExited) { Write-Host "killing install after ${ms}ms (pid $($p.Id))"; Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
      else { Write-Host "install already exited before ${ms}ms" }
      Start-Sleep -Milliseconds 200
    }
    Write-Host ".store entries after the kills: $((Get-ChildItem -Force (Join-Path $fixture 'node_modules\.store') -ErrorAction SilentlyContinue | Measure-Object).Count)"
    $rcSetup = Run-Nub $fixture @('install') 'install after the interrupted runs'
    $rcAdd = Run-Nub $fixture @('add', '-D', 'oxlint') 'add'
  }
  'interruptedThenAdd' {
    $pkg | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')
    $p = Start-Process -FilePath 'nub' -ArgumentList 'install' -WorkingDirectory $fixture -PassThru -NoNewWindow
    Start-Sleep -Milliseconds 1200
    if (-not $p.HasExited) { Write-Host "killing install (pid $($p.Id))"; Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 300
    Write-Host ".store entries after the kill: $((Get-ChildItem -Force (Join-Path $fixture 'node_modules\.store') -ErrorAction SilentlyContinue | Measure-Object).Count)"
    # No repair install — go straight to add, exactly the invariant this tests.
    $rcAdd = Run-Nub $fixture @('add', '-D', 'oxlint') 'add straight after the killed install'
  }
  'concurrentAdd' {
    $pkg | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')
    $rcSetup = Run-Nub $fixture @('install') 'install'
    $a = Start-Process -FilePath 'nub' -ArgumentList 'add -D oxlint' -WorkingDirectory $fixture -PassThru -NoNewWindow
    $b = Start-Process -FilePath 'nub' -ArgumentList 'add -D prettier' -WorkingDirectory $fixture -PassThru -NoNewWindow
    $a.WaitForExit(); $b.WaitForExit()
    Write-Host "concurrent add exit codes: oxlint=$($a.ExitCode) prettier=$($b.ExitCode)"
    $rcAdd = [Math]::Max($a.ExitCode, $b.ExitCode)
  }
  'addChain' {
    $pkg | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')
    $rcSetup = Run-Nub $fixture @('install') 'install'
    foreach ($p in @('oxlint', 'prettier', '@types/lodash', 'vitest', '@types/react', 'esbuild', '@types/cors', 'cors')) {
      $rc = Run-Nub $fixture @('add', '-D', $p) "add $p"
      if ($rc -ne 0) { $rcAdd = $rc; break }
    }
  }
  default { Write-Host "unknown mode $Mode" }
}

Section "POST-STATE"
$store = Join-Path $fixture 'node_modules\.store'
if (-not (Test-Path $store)) { $store = Join-Path $fixture 'packages\app\node_modules\.store' }
Write-Host "store = $store (exists=$(Test-Path $store))"
if (Test-Path $store) {
  $bp = Join-Path $store '@types+body-parser@1.19.6'
  Write-Host "entry exists = $(Test-Path $bp)"
  $connect = Join-Path $bp 'node_modules\@types\connect'
  if (Test-Path -LiteralPath $connect) {
    $i = Get-Item -Force -LiteralPath $connect -ErrorAction SilentlyContinue
    Write-Host "connect LinkType=$($i.LinkType) Target=$($i.Target)"
  } else { Write-Host "connect absent" }
}

Write-Host ""
Write-Host "===== [$Label] SUMMARY: setup=$rcSetup add=$rcAdd ====="
if ($rcAdd -ne 0) { Write-Host "===== [$Label] VERDICT: FAILED =====" } else { Write-Host "===== [$Label] VERDICT: ok =====" }
exit 0
