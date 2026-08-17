# Compile a package that ships DATA beside its source on Windows, and run the
# artifact with node_modules deleted.
#
# The sibling gate (compile-native-package.ps1) covers a dependency that resolves
# an ADDON by building a path at run time. This covers the other reason a package
# gets ejected: it reads an ordinary FILE — a font, a lookup table, a WebAssembly
# module — from a path it builds off __dirname. Same payload-name machinery, but
# the failure surface is different, and on Windows __dirname is a backslash path
# while payload names are not. Separator handling is where this repo keeps finding
# Windows faults, so the class that ships data needs its own gate rather than
# inheriting confidence from the addon one.
#
# pdfkit is the fixture because it fails LOUDLY when it goes wrong — it reads its
# built-in fonts on the first text() call — and because it is on the curated
# unbundlable list, so this also pins that the list entry still takes effect here.
param(
  [Parameter(Mandatory = $true)][string]$Nub,
  [Parameter(Mandatory = $true)][string]$Launcher
)

$ErrorActionPreference = 'Stop'
$work = Join-Path $env:RUNNER_TEMP 'nub-compile-data'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force -Path $work | Out-Null
Push-Location $work

try {
  '26.5.0' | Set-Content -NoNewline .node-version
  npm init -y | Out-Null
  npm i pdfkit --no-audit --no-fund --silent
  if ($LASTEXITCODE -ne 0) { throw "npm install failed" }

  @'
import PDFDocument from "pdfkit";
const doc = new PDFDocument();
doc.text("hi");
doc.end();
console.log("ok:pdf");
'@ | Set-Content app.mjs

  # The control. An artifact that prints nothing looks identical to one that works
  # unless there is a known-good answer to compare against.
  $control = (& node app.mjs | Select-Object -Last 1)
  if ($control -ne 'ok:pdf') { throw "control failed on plain node: '$control'" }

  $env:__NUB_LAUNCHER_TEMPLATE = $Launcher
  & $Nub compile (Join-Path $work 'app.mjs') --out (Join-Path $work 'app.exe')
  if ($LASTEXITCODE -ne 0) { throw "compile failed" }

  # Deleted, not renamed: a rename leaves the tree reachable through a parent
  # walk, which would let the artifact pass by finding the files it was supposed
  # to have carried.
  Remove-Item -Recurse -Force node_modules

  $env:XDG_CACHE_HOME = Join-Path $work 'cache'
  Push-Location $env:TEMP
  $out = (& (Join-Path $work 'app.exe') | Select-Object -Last 1)
  $rc = $LASTEXITCODE
  Pop-Location

  if ($rc -ne 0) { throw "artifact exited $rc" }
  if ($out -ne $control) { throw "artifact printed '$out', control printed '$control'" }

  # pdfkit must have travelled as real files. Its fonts are the point: they are
  # what __dirname reaches, and shipping the JavaScript without them compiles
  # clean and dies on the first text() call.
  $app = Get-ChildItem -Directory (Join-Path $env:XDG_CACHE_HOME 'nub\compile-app') |
    Select-Object -First 1
  $pdfkit = Join-Path $app.FullName 'node_modules\pdfkit'
  if (-not (Test-Path $pdfkit)) { throw "pdfkit did not ship unbundled — nothing at $pdfkit" }
  $data = Get-ChildItem -Recurse -File $pdfkit -ErrorAction SilentlyContinue |
    Where-Object { $_.Extension -in '.afm', '.ttf', '.otf' }
  if (-not $data) { throw "pdfkit shipped without the font files __dirname reaches" }

  Write-Host "ok — $out, pdfkit shipped with $($data.Count) font file(s)"
}
finally {
  Pop-Location
}
