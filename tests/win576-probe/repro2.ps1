# Round 2 for nubjs/nub#576: the clean install -> add sequence does NOT
# reproduce, so each mode here perturbs the post-install .store in a way that
# could break the "callers guarantee the target directory is freshly created"
# invariant (materialize.rs:664-671) and drive `nub add` into an occupied path.

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
  $env:RUST_LOG = 'info'
  & nub @argList 2>&1 | Out-String -Width 400 | Write-Host
  $code = $LASTEXITCODE
  Pop-Location
  Write-Host "[$Label] EXIT CODE ($tag) = $code"
  return $code
}

$fixture = Join-Path $Root "r2-$Mode"
if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force -Path $fixture | Out-Null
@'
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
'@ | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')

$rc1 = Run-Nub $fixture @('install') 'install'

$store   = Join-Path $fixture 'node_modules\.store'
$bp      = Join-Path $store '@types+body-parser@1.19.6'
$connect = Join-Path $bp 'node_modules\@types\connect'
$user    = $env:USERNAME

Section "PERTURB: $Mode"
switch ($Mode) {
  'denyRA' {
    icacls $bp /deny "${user}:(RA)" | Write-Host
    Write-Host "Test-Path after deny RA = $(Test-Path $bp)"
  }
  'denyFull' {
    icacls $bp /deny "${user}:(F)" | Write-Host
    Write-Host "Test-Path after deny F = $(Test-Path $bp)"
  }
  'danglingConnect' {
    $tgt = Join-Path $store '@types+connect@3.4.38'
    Write-Host "removing junction TARGET $tgt"
    cmd /c "rmdir /s /q `"$tgt`"" 2>&1 | Write-Host
    Write-Host "connect junction still present? $(Test-Path -LiteralPath $connect)  (Get-Item ok? $((Get-Item -Force -LiteralPath $connect -ErrorAction SilentlyContinue) -ne $null))"
  }
  'dropState' {
    foreach ($p in @((Join-Path $fixture 'node_modules\.nub-state'), (Join-Path $fixture 'node_modules\.aube-state'), (Join-Path $store '.nub-state'))) {
      if (Test-Path $p) { Write-Host "removing $p"; Remove-Item -Recurse -Force $p }
    }
  }
  'partialEntry' {
    $victim = Join-Path $bp 'node_modules\@types\body-parser'
    Write-Host "removing $victim (leaving the connect junction in place)"
    cmd /c "rmdir /s /q `"$victim`"" 2>&1 | Write-Host
    Write-Host "connect junction still present? $(Test-Path -LiteralPath $connect)"
  }
  'readonlyEntry' {
    attrib +R $bp | Write-Host
    Write-Host "Test-Path after +R = $(Test-Path $bp)"
  }
  default { Write-Host "no perturbation" }
}

$rc2 = Run-Nub $fixture @('add', '-D', 'oxlint') 'add -D oxlint AFTER perturbation'

Section "POST-STATE"
Write-Host "bp exists      = $(Test-Path $bp)"
Write-Host "connect exists = $(Test-Path -LiteralPath $connect)"
if (Test-Path -LiteralPath $connect) {
  $i = Get-Item -Force -LiteralPath $connect -ErrorAction SilentlyContinue
  Write-Host "connect LinkType=$($i.LinkType) Target=$($i.Target)"
}

# Restore ACLs so the runner can clean up.
icacls $bp /remove:d $user 2>&1 | Out-Null
attrib -R $bp 2>&1 | Out-Null

Write-Host ""
Write-Host "===== [$Label] SUMMARY: install=$rc1 add=$rc2 ====="
if ($rc2 -ne 0) { Write-Host "===== [$Label] VERDICT: add FAILED =====" }
else { Write-Host "===== [$Label] VERDICT: add succeeded =====" }
exit 0
