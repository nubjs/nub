# Compile a real third-party native package on Windows and run the artifact with
# node_modules deleted.
#
# The existing Windows round-trip covers nub's OWN addon, staged into
# runtime/addons. It never exercises the path a dependency takes: a package that
# resolves its addon by building a path at run time is ejected from the bundle and
# materialised under node_modules/ beside it. That path writes payload names by
# turning a filesystem path into a payload one, and separator handling is where
# this repo has repeatedly found Windows faults — so it needs to run here rather
# than be argued from macOS.
#
# better-sqlite3 is the fixture because its tree ships prebuilds for eight
# platforms, so it exercises selection rather than luck: the win32 build must
# travel and the other seven must not.
param(
  [Parameter(Mandatory = $true)][string]$Nub,
  [Parameter(Mandatory = $true)][string]$Launcher
)

$ErrorActionPreference = 'Stop'
$work = Join-Path $env:RUNNER_TEMP 'nub-compile-native'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force -Path $work | Out-Null
Push-Location $work

try {
  '26.5.0' | Set-Content -NoNewline .node-version
  npm init -y | Out-Null
  npm i better-sqlite3 --no-audit --no-fund --silent
  if ($LASTEXITCODE -ne 0) { throw "npm install failed" }

  @'
import Database from "better-sqlite3";
const db = new Database(":memory:");
db.exec("create table t(x)");
console.log("ok:" + db.prepare("select count(*) c from t").get().c);
'@ | Set-Content app.mjs

  # The control. A compiled artifact that prints nothing looks identical to one
  # that works unless there is a known-good answer to compare against.
  $control = (& node app.mjs | Select-Object -Last 1)
  if ($control -ne 'ok:0') { throw "control failed on plain node: '$control'" }

  $env:__NUB_LAUNCHER_TEMPLATE = $Launcher
  & $Nub compile (Join-Path $work 'app.mjs') --out (Join-Path $work 'app.exe')
  if ($LASTEXITCODE -ne 0) { throw "compile failed" }

  # Deleted, not renamed: a rename leaves the tree reachable through a parent
  # walk, which would let the artifact pass by finding the packages it was
  # supposed to have carried.
  Remove-Item -Recurse -Force node_modules

  $env:XDG_CACHE_HOME = Join-Path $work 'cache'
  Push-Location $env:TEMP
  $out = (& (Join-Path $work 'app.exe') | Select-Object -Last 1)
  $rc = $LASTEXITCODE
  Pop-Location

  if ($rc -ne 0) { throw "artifact exited $rc" }
  if ($out -ne $control) { throw "artifact printed '$out', control printed '$control'" }

  # The addon must have travelled as a real file at its installed path, and the
  # seven foreign prebuilds must not have.
  $app = Get-ChildItem -Directory (Join-Path $env:XDG_CACHE_HOME 'nub\compile-app') |
    Select-Object -First 1
  $addons = Get-ChildItem -Recurse -Filter '*.node' $app.FullName -ErrorAction SilentlyContinue
  if (-not $addons) { throw "no addon in the payload — the package did not ship" }
  foreach ($a in $addons) {
    if ($a.Name -notlike '*win32*') {
      throw "a foreign prebuild travelled into a win32 artifact: $($a.Name)"
    }
  }

  Write-Host "ok — $out, addons: $(($addons | ForEach-Object Name) -join ', ')"
}
finally {
  Pop-Location
}
