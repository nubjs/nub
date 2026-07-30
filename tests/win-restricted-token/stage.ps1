# Driver for the restricted-token + low-IL build-jail probe.
#   powershell -File stage.ps1 -Stage <launch|write|edge|ops|packages>
# Every stage prints a BASELINE arm first. A prior lane's two treatment arms both
# failed ACCESS_DENIED and read exactly like "mechanism unavailable" when the real
# cause was a harness bug; only the baseline localized it.
param([string]$Stage = 'launch', [string]$Pkg = '')

$ErrorActionPreference = 'Continue'
$PROBE = Join-Path $env:USERPROFILE 'p'
$J = Join-Path $PROBE 'Jail.exe'
$NODE = 'C:\node\node.exe'
$proj = Join-Path $env:USERPROFILE 'jail\project'
$store = Join-Path $env:LOCALAPPDATA 'nub\store'
$cache = Join-Path $env:LOCALAPPDATA 'nub\cache'
$outf = (Join-Path $PROBE 'child.out')

function Hdr($s) { Write-Output ""; Write-Output "########## $s" }

function Launch($il, $api, $extra, $cmd) {
  $a = @('launch', '--il', $il, '--api', $api, '--out', $outf, '--cwd', $proj)
  if ($extra) { $a += $extra }
  $a += '--'
  $a += $cmd
  & $J @a
}

if ($Stage -eq 'launch') {
  Hdr "who am I (is this arm de-elevated?)"
  & $J report
  Hdr "Q_LAUNCH: CreateProcessAsUserW vs CreateProcessWithTokenW x integrity level"
  foreach ($api in @('asuser', 'withtoken')) {
    foreach ($il in @('none', 'medium', 'low')) {
      Write-Output "----- api=$api il=$il"
      Launch $il $api $null @('cmd.exe', '/c', 'echo LAUNCH_OK && whoami /groups | findstr /i "Mandatory"')
    }
  }
  Hdr "Q_LAUNCH variants that address a station/desktop refusal"
  Write-Output "----- low + keep-logon-sid"
  Launch 'low' 'asuser' @('--keep-logon-sid') @('cmd.exe', '/c', 'echo LAUNCH_OK')
  Write-Output "----- low + new labeled desktop"
  Launch 'low' 'asuser' @('--new-desktop') @('cmd.exe', '/c', 'echo LAUNCH_OK')
  Write-Output "----- untrusted IL (for the record)"
  Launch 'untrusted' 'asuser' $null @('cmd.exe', '/c', 'echo LAUNCH_OK')
}

if ($Stage -eq 'write') {
  Hdr "Q_WRITE: can an unprivileged OWNER lower an object's integrity label?"
  foreach ($p in @($proj, $store, $cache)) {
    Write-Output "----- before: $p"
    & $J showlabel $p
    Write-Output "----- set low:"
    & $J label $p low
    Write-Output "----- after:"
    & $J showlabel $p
  }
  Hdr "recursive label of the project tree (cost + coverage)"
  $sw = [Diagnostics.Stopwatch]::StartNew()
  & $J label $proj low -r
  $sw.Stop()
  Write-Output "recurse_ms=$($sw.ElapsedMilliseconds)"
  Hdr "NEGATIVE CONTROL: relabel something we do NOT own"
  & $J label 'C:\' low
  & $J label 'C:\Windows' low
  Hdr "low-IL child writes into the relabeled dir vs elsewhere"
  Launch 'low' 'asuser' $null @('cmd.exe', '/c', "echo x > `"$proj\native-write.txt`" && echo WROTE_PROJ || echo DENIED_PROJ")
  Launch 'low' 'asuser' $null @('cmd.exe', '/c', "echo x > `"$env:USERPROFILE\native-escape.txt`" && echo WROTE_PROFILE || echo DENIED_PROFILE")
  Launch 'low' 'asuser' $null @('cmd.exe', '/c', "echo x > C:\native-escape.txt && echo WROTE_C || echo DENIED_C")
}

if ($Stage -eq 'edge') {
  Hdr "Low-IL hazard probe (srt runs Medium on purpose to dodge these)"
  foreach ($il in @('none', 'medium', 'low')) {
    Write-Output "===== integrity=$il"
    Launch $il 'asuser' $null @($NODE, (Join-Path $PROBE 'edge.js'))
  }
}

if ($Stage -eq 'ops') {
  Hdr "Operations table (the ones that killed the AppContainer route)"
  foreach ($il in @('none', 'medium', 'low')) {
    Write-Output "===== integrity=$il"
    Launch $il 'asuser' $null @($NODE, (Join-Path $PROBE 'ops.js'), $proj)
  }
}

if ($Stage -eq 'packages') {
  Hdr "Real package lifecycle scripts under the jail: $Pkg"
  Launch 'low' 'asuser' $null @((Join-Path $PROBE 'runpkg.cmd'), $Pkg)
}
