# The process contract of a compiled binary, on Windows.
#
# The corpus fixture `a-process.mjs` covers this on Unix, but the corpus is not
# wired into any workflow — it runs locally only — so without this the contract
# has no Windows gate at all. That matters more here than anywhere else: Windows
# has no argv ARRAY. A process receives one command-line STRING and each runtime
# re-splits it, so an argument containing a space survives only if the launcher
# quotes it correctly on the way to Node. Exit codes and the stdout/stderr split
# cross the same launcher hop.
#
# The probe runs as a CHILD of the artifact, spawned from the artifact itself, so
# it also covers a compiled binary re-entering itself — what any CLI that
# re-execs depends on.
param(
  [Parameter(Mandatory = $true)][string]$Nub,
  [Parameter(Mandatory = $true)][string]$Launcher
)

$ErrorActionPreference = 'Stop'
$work = Join-Path $env:RUNNER_TEMP 'nub-compile-process'
if (Test-Path $work) { Remove-Item -Recurse -Force $work }
New-Item -ItemType Directory -Force -Path $work | Out-Null
Push-Location $work

try {
  '26.5.0' | Set-Content -NoNewline .node-version
  npm init -y | Out-Null

  # `process.execPath` is the artifact once compiled, so the child needs no
  # script argument; under plain node it does. Same shape as the corpus fixture.
  @'
import { spawnSync } from "node:child_process";
const selfPrefix = /node(\.exe)?$/i.test(process.execPath) ? [process.argv[1]] : [];
const runSelf = (...args) =>
  spawnSync(process.execPath, [...selfPrefix, ...args], { encoding: "utf8" });

if (process.argv.includes("--probe")) {
  const tail = process.argv[process.argv.indexOf("--probe") + 1];
  process.stderr.write("E:" + tail + "\n");
  process.stdout.write("O:" + tail + "\n");
  process.exit(7);
}

const r = runSelf("--probe", "a b");
const checks = [
  ["exit", r.status === 7],
  ["stdout", r.stdout.trim() === "O:a b"],
  ["stderr", r.stderr.trim() === "E:a b"],
];
const failed = checks.filter(([, ok]) => !ok).map(([name]) => name);
console.log("ok:" + (failed.length ? failed.join(",") : "process"));
'@ | Set-Content app.mjs

  # The control. Without it a broken artifact and a broken fixture look alike.
  $control = (& node app.mjs | Select-Object -Last 1)
  if ($control -ne 'ok:process') { throw "control failed on plain node: '$control'" }

  $env:__NUB_LAUNCHER_TEMPLATE = $Launcher
  & $Nub compile (Join-Path $work 'app.mjs') --out (Join-Path $work 'app.exe')
  if ($LASTEXITCODE -ne 0) { throw "compile failed" }

  $env:XDG_CACHE_HOME = Join-Path $work 'cache'
  Push-Location $env:TEMP
  $out = (& (Join-Path $work 'app.exe') | Select-Object -Last 1)
  $rc = $LASTEXITCODE
  Pop-Location

  if ($rc -ne 0) { throw "artifact exited $rc" }
  # The verdict names which half broke — exit, stdout or stderr — rather than
  # only going red, so a Windows-only quoting fault is readable from the log.
  if ($out -ne $control) { throw "artifact printed '$out', control printed '$control'" }

  Write-Host "ok — $out (argv with a space, exit code and stream split all survived the launcher)"
}
finally {
  Pop-Location
}
