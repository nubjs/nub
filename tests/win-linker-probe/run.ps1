# Windows linker probe for nubjs/nub#552 / #566 / #576.
# Runs each scenario in its own temp project, never fails the job on a scenario
# error — a reproduced failure IS the result, so every command's output is
# captured and reported rather than aborting the run.
$ErrorActionPreference = 'Continue'

$Root = Join-Path $env:RUNNER_TEMP 'winlinker'
if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
New-Item -ItemType Directory -Force -Path $Root | Out-Null

function Show-Header($text) {
  Write-Host ""
  Write-Host "=============================================================="
  Write-Host "== $text"
  Write-Host "=============================================================="
}

# Run a command, echo it, capture combined stdout+stderr and the exit code.
function Invoke-Probe($label, $exe, [string[]]$probeArgs, $workDir) {
  Write-Host "--- [$label] $exe $($probeArgs -join ' ')  (cwd=$workDir)"
  Push-Location $workDir
  $out = & $exe @probeArgs 2>&1 | Out-String
  $code = $LASTEXITCODE
  Pop-Location
  Write-Host $out
  Write-Host "--- [$label] exit=$code"
  if ($out -match 'os error (\d+)') {
    Write-Host "!!! [$label] REPRODUCED os error $($Matches[1])"
  }
  return $code
}

# The reported paths are `node_modules/.store/<entry>` — a REAL directory means
# the disk-materialize / GVS-off path is live, a junction means the shared
# global virtual store is. That distinction picks the failing call site.
function Show-StoreShape($projectDir, $limit = 12) {
  $store = Join-Path $projectDir 'node_modules\.store'
  if (-not (Test-Path $store)) { Write-Host "    (no node_modules\.store)"; return }
  $entries = Get-ChildItem -Force $store | Select-Object -First $limit
  $realDirs = 0; $links = 0
  foreach ($e in (Get-ChildItem -Force $store)) {
    if ($e.LinkType) { $links++ } else { $realDirs++ }
  }
  Write-Host "    .store: $realDirs real dir(s), $links reparse point(s)"
  foreach ($e in $entries) {
    $kind = if ($e.LinkType) { "$($e.LinkType) -> $($e.Target)" } else { 'REAL DIR' }
    Write-Host "      $($e.Name)  [$kind]"
  }
}

function New-Project($name, $manifest) {
  $dir = Join-Path $Root $name
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  Set-Content -Path (Join-Path $dir 'package.json') -Value $manifest -Encoding utf8
  return $dir
}

Show-Header 'environment'
Write-Host "nub:  $(nub --version)"
Write-Host "node: $(node --version)"
Write-Host "cwd root: $Root"
Get-MpComputerStatus 2>$null | Select-Object RealTimeProtectionEnabled, AntivirusEnabled | Format-List

# --------------------------------------------------------------------------
Show-Header 'Scenario A — #576: install, then `nub add -D oxlint` (expects os 183)'
$a = New-Project 'a-add-types' @'
{
  "name": "probe-a",
  "private": true,
  "version": "1.0.0",
  "dependencies": { "express": "4.21.2" },
  "devDependencies": { "@types/express": "4.17.21" }
}
'@
Invoke-Probe 'A/install' 'nub' @('install') $a
Show-StoreShape $a
Invoke-Probe 'A/add' 'nub' @('add', '-D', 'oxlint') $a
Show-StoreShape $a
Invoke-Probe 'A/add-again' 'nub' @('add', '-D', 'tinyexec') $a
Invoke-Probe 'A/remove' 'nub' @('remove', 'oxlint') $a

# --------------------------------------------------------------------------
Show-Header 'Scenario B — #552: vitepress + naive-ui, then `nub update` / `nub remove`'
# naive-ui pulls @css-render/vue3-ssr, whose published tarball carries the
# `coverage/lcov-report/index.html` tree named in the report.
$b = New-Project 'b-vitepress-naive' @'
{
  "name": "probe-b",
  "private": true,
  "version": "1.0.0",
  "dependencies": { "naive-ui": "2.40.1", "vue": "3.5.13" },
  "devDependencies": { "vitepress": "1.5.0", "typescript": "5.7.2" }
}
'@
Invoke-Probe 'B/install' 'nub' @('install') $b
Show-StoreShape $b
Write-Host "    @css-render entry:"
Get-ChildItem -Force (Join-Path $b 'node_modules\.store') -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -like '*css-render*' } |
  ForEach-Object { Write-Host "      $($_.Name) [$(if ($_.LinkType) { $_.LinkType } else { 'REAL DIR' })]" }
Invoke-Probe 'B/update' 'nub' @('update') $b
Invoke-Probe 'B/remove' 'nub' @('remove', 'typescript') $b
Invoke-Probe 'B/add' 'nub' @('add', '-D', 'oxlint') $b

# --------------------------------------------------------------------------
Show-Header 'Scenario C — interrupt mid-link, then re-run (half-materialized entry)'
$c = New-Project 'c-interrupt' @'
{
  "name": "probe-c",
  "private": true,
  "version": "1.0.0",
  "dependencies": { "naive-ui": "2.40.1", "vue": "3.5.13", "express": "4.21.2" }
}
'@
Write-Host '--- [C/kill] starting install, killing it 6s in'
Push-Location $c
# `nub` on PATH is an npm shim, not a Win32 image — `Start-Process -FilePath 'nub'`
# fails with "%1 is not a valid Win32 application". Resolve the real executable
# (or drive the shim through cmd) so the interrupt actually interrupts something.
$nubCmd = Get-Command nub -ErrorAction SilentlyContinue
$nubPath = if ($nubCmd) { $nubCmd.Source } else { 'nub' }
Write-Host "--- [C/kill] resolved nub -> $nubPath"
$proc = if ($nubPath -match '\.(cmd|bat)$') {
  Start-Process -FilePath $env:ComSpec -ArgumentList '/c', $nubPath, 'install' -PassThru -NoNewWindow
} elseif ($nubPath -match '\.ps1$') {
  Start-Process -FilePath 'pwsh' -ArgumentList '-File', $nubPath, 'install' -PassThru -NoNewWindow
} else {
  Start-Process -FilePath $nubPath -ArgumentList 'install' -PassThru -NoNewWindow
}
Start-Sleep -Seconds 6
if ($proc -and -not $proc.HasExited) {
  # Kill the whole tree: the shim spawns nub.exe as a child, and killing only the
  # shim would leave the real linker running and the interrupt untested.
  & taskkill.exe /PID $proc.Id /T /F 2>&1 | Write-Host
  Write-Host '--- [C/kill] killed process tree'
} elseif ($proc) {
  Write-Host "--- [C/kill] already exited ($($proc.ExitCode)) — install was too fast to interrupt"
} else {
  Write-Host '!!! [C/kill] FAILED TO LAUNCH — scenario C did not run'
}
Pop-Location
Show-StoreShape $c
Invoke-Probe 'C/resume' 'nub' @('install') $c
Show-StoreShape $c
Invoke-Probe 'C/add-after-resume' 'nub' @('add', '-D', 'oxlint') $c

# --------------------------------------------------------------------------
# The half-materialized-entry hypothesis, tested deterministically instead of by
# racing an interrupt. Reproduced on macOS already; this confirms it on Windows
# and shows whether the incomplete entry ALSO produces os 183 on a later `add`.
Show-Header 'Scenario D — hand-truncated .store entry, then re-run (deterministic)'
$d = New-Project 'd-partial' @'
{
  "name": "probe-d",
  "private": true,
  "version": "1.0.0",
  "dependencies": { "express": "4.21.2" }
}
'@
Invoke-Probe 'D/install' 'nub' @('install') $d
$victim = Join-Path $d 'node_modules\.store\accepts@1.3.8\node_modules\accepts'
if (Test-Path $victim) {
  # `materialize_into` pre-creates the package dir before the file loop, so a
  # mid-link failure leaves exactly this shape: entry present, files missing.
  Remove-Item -Force (Join-Path $victim 'index.js'), (Join-Path $victim 'package.json') -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force (Join-Path $d 'node_modules\.store\.nub-state') -ErrorAction SilentlyContinue
  Write-Host "--- [D] truncated $victim; remaining:"
  Get-ChildItem $victim | ForEach-Object { Write-Host "      $($_.Name)" }
  Invoke-Probe 'D/reinstall' 'nub' @('install') $d
  Write-Host '--- [D] entry after reinstall (repaired, or still truncated?):'
  Get-ChildItem $victim | ForEach-Object { Write-Host "      $($_.Name)" }
  Invoke-Probe 'D/add-after-partial' 'nub' @('add', '-D', 'oxlint') $d
  Write-Host '--- [D] does express resolve?'
  Push-Location $d; node -e "require('express'); console.log('express loads OK')" 2>&1 | Write-Host; Pop-Location
} else {
  Write-Host "!!! [D] victim path not found — layout differs, scenario D did not run: $victim"
}

Show-Header 'done — probe never fails the job; read the log for reproduced os errors'
exit 0
