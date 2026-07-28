# Repro harness for nubjs/nub#576 — `nub add` fails with os error 183 at
# <project>/node_modules/.store/@types+body-parser@1.19.6/node_modules/@types/connect
#
# One invocation = one scenario. The workflow calls it once per
# (nub version x drive x GVS) cell so the cells are independent and a crash in
# one still leaves the others' evidence.

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$Label,
  [Parameter(Mandatory = $true)][string]$Root,
  [switch]$GvsOff
)

$ErrorActionPreference = 'Continue'

function Section($t) {
  Write-Host ""
  Write-Host "########## [$Label] $t ##########"
}

function Dump-State($fixture, $tag) {
  Section "ON-DISK STATE ($tag)"
  $store = Join-Path $fixture 'node_modules\.store'
  Write-Host "-- project virtual store: $store  (exists=$(Test-Path $store))"
  if (Test-Path $store) {
    Write-Host "-- cmd dir /AL (reparse points) in .store:"
    cmd /c "dir /AL `"$store`"" 2>&1 | Out-String | Write-Host
    Write-Host "-- top-level .store entries (first 60):"
    Get-ChildItem -Force $store -ErrorAction SilentlyContinue |
      Select-Object -First 60 Name, Mode, LinkType, @{n='Target';e={$_.Target}} |
      Format-Table -AutoSize | Out-String -Width 400 | Write-Host

    $bp = Join-Path $store '@types+body-parser@1.19.6'
    Write-Host "-- entry @types+body-parser@1.19.6 exists=$(Test-Path $bp)"
    if (Test-Path $bp) {
      Write-Host "-- fsutil reparsepoint query on the ENTRY dir:"
      cmd /c "fsutil reparsepoint query `"$bp`"" 2>&1 | Out-String | Write-Host
      Write-Host "-- recursive listing of the entry:"
      Get-ChildItem -Force -Recurse $bp -ErrorAction SilentlyContinue |
        Select-Object FullName, Mode, LinkType, @{n='Target';e={$_.Target}} |
        Format-Table -AutoSize | Out-String -Width 400 | Write-Host

      $connect = Join-Path $bp 'node_modules\@types\connect'
      Write-Host "-- THE FAILING PATH: $connect"
      Write-Host "   Test-Path            = $(Test-Path $connect)"
      Write-Host "   Test-Path -Type Leaf = $(Test-Path $connect -PathType Leaf)"
      if (Test-Path $connect) {
        $i = Get-Item -Force $connect
        Write-Host "   Attributes = $($i.Attributes)"
        Write-Host "   LinkType   = $($i.LinkType)"
        Write-Host "   Target     = $($i.Target)"
        Write-Host "   fsutil reparsepoint query:"
        cmd /c "fsutil reparsepoint query `"$connect`"" 2>&1 | Out-String | Write-Host
      }
    }
  }

  Section "GLOBAL VIRTUAL STORE ($tag)"
  foreach ($c in @("$env:LOCALAPPDATA\nub", "$env:LOCALAPPDATA\nub\cache", "$env:APPDATA\nub")) {
    if (Test-Path $c) {
      Write-Host "-- $c :"
      Get-ChildItem -Force $c -ErrorAction SilentlyContinue |
        Select-Object FullName, Mode | Format-Table -AutoSize | Out-String -Width 300 | Write-Host
    }
  }
  $gvs = "$env:LOCALAPPDATA\nub\cache\store"
  Write-Host "-- expected GVS dir $gvs exists=$(Test-Path $gvs)"
  if (Test-Path $gvs) {
    Write-Host "   entry count: $((Get-ChildItem -Force $gvs -ErrorAction SilentlyContinue | Measure-Object).Count)"
    $gbp = Get-ChildItem -Force $gvs -Filter '@types+body-parser*' -ErrorAction SilentlyContinue
    foreach ($e in $gbp) {
      Write-Host "   GVS entry: $($e.FullName)"
      Get-ChildItem -Force -Recurse $e.FullName -ErrorAction SilentlyContinue |
        Select-Object FullName, LinkType, @{n='Target';e={$_.Target}} |
        Format-Table -AutoSize | Out-String -Width 400 | Write-Host
    }
  }
}

function Run-Nub($fixture, $argList, $tag) {
  Section "RUN: nub $($argList -join ' ')   ($tag)"
  Push-Location $fixture
  $env:RUST_LOG = 'debug'
  & nub @argList 2>&1 | Out-String -Width 400 | Write-Host
  $code = $LASTEXITCODE
  Pop-Location
  Write-Host "[$Label] EXIT CODE ($tag) = $code"
  return $code
}

# ---------------------------------------------------------------- scenario ---
$fixture = Join-Path $Root "fixture-$Label"
if (Test-Path $fixture) { Remove-Item -Recurse -Force $fixture }
New-Item -ItemType Directory -Force -Path $fixture | Out-Null

@'
{
  "name": "nub576-fixture",
  "private": true,
  "version": "1.0.0",
  "dependencies": {
    "express": "^4.21.2"
  },
  "devDependencies": {
    "@types/express": "^5.0.0",
    "@types/node": "^24.0.0",
    "typescript": "^5.6.3"
  }
}
'@ | Set-Content -Encoding utf8 (Join-Path $fixture 'package.json')

if ($GvsOff) {
  $env:npm_config_enable_global_virtual_store = 'false'
  Write-Host "[$Label] GVS DISABLED via npm_config_enable_global_virtual_store=false"
} else {
  Remove-Item Env:npm_config_enable_global_virtual_store -ErrorAction SilentlyContinue
  Write-Host "[$Label] GVS at default"
}

Section "ENV"
Write-Host "fixture      = $fixture"
Write-Host "nub --version:"; & nub --version 2>&1 | Write-Host
Write-Host "node --version:"; & node --version 2>&1 | Write-Host
Write-Host "where nub:"; & where.exe nub 2>&1 | Write-Host

$rc1 = Run-Nub $fixture @('install') 'install #1'
$rc2 = Run-Nub $fixture @('install') 'install #2 (expect already up to date)'

Dump-State $fixture 'BEFORE the failing add'

$rc3 = Run-Nub $fixture @('add', '-D', 'oxlint') 'add -D oxlint'

Dump-State $fixture 'AFTER add -D oxlint'

$rc4 = 0
if ($rc3 -eq 0) {
  Write-Host "[$Label] add #1 SUCCEEDED — trying a second add (reporter's failure may need two)"
  $rc4 = Run-Nub $fixture @('add', '-D', 'prettier') 'add -D prettier'
  if ($rc4 -eq 0) {
    $rc4 = Run-Nub $fixture @('add', '-D', '@types/lodash') 'add -D @types/lodash (shares the @types scope)'
    Dump-State $fixture 'AFTER add @types/lodash'
  }
}

Write-Host ""
Write-Host "===== [$Label] SUMMARY: install#1=$rc1 install#2=$rc2 add-oxlint=$rc3 followup=$rc4 ====="
if ($rc3 -ne 0 -or $rc4 -ne 0) {
  Write-Host "===== [$Label] VERDICT: REPRODUCED (non-zero add) ====="
} else {
  Write-Host "===== [$Label] VERDICT: no failure observed ====="
}
# Never fail the job — every cell must run and every dump must land.
exit 0
