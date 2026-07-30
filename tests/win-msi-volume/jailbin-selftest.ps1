# Drive `Invoke-JailBinVerdict` with SYNTHETIC worlds and require the OPPOSITE answer from each, so a
# verdict block that has only ever been exercised against the world it expects cannot report PASS on a
# harness that measured nothing. Same discipline as `selftest.ps1` beside it. No Windows APIs — this
# runs on Linux, and a red selftest means every table the real jobs print is unreadable.
#
# THE FIVE WORLDS, and what each is designed to catch:
#
#   1 mechanism-works      Everything the design predicts. Must be all-PASS. If this world fails, the
#                          verdict is over-strict and would have condemned a working mechanism.
#   2 mechanism-fails      The grant does nothing (the decisive read is denied even when granted). The
#                          decisive properties must FAIL. Catches a verdict that passes unconditionally.
#   3 no-attribution       The mechanism "works" AND the ungranted arm ALSO works — i.e. something
#                          other than the ace is carrying the read. The attribution property must FAIL
#                          even though every decisive cell is green. This is the world a verdict
#                          without a negative control would wave through, and it is the exact failure
#                          shape §5k's run 1 shipped.
#   4 wrong-interpreter    The hybrid arm is green but ran jail-bin's OWN node.exe, so it says nothing
#                          about the ambient one. The interpreter read-back property must FAIL while
#                          the hybrid property itself passes — the two must be separable.
#   5 harness-dead         Nothing ran (every arm missing). Controls must FAIL rather than the whole
#                          block reporting an absence as a negative result.

. (Join-Path $PSScriptRoot 'jailbin-verdict.ps1')

$JB = 'C:\Users\u\.cache\nub\jail-bin'
$PF = 'C:\Program Files\nodejs'

function C([string]$v, [string]$d) { return @($v, $d) }

# The all-green world, spelled out once; the other worlds are one-variable mutations of it.
function New-WorkingCells {
  $ctl = @{
    'interp-execpath'     = C 'OK' "$JB\node.exe"
    'path-seen'           = C 'OK' 'jailbin=True progfiles=False'
    'stat-c-root'         = C 'ERR' 'EPERM'
    'read-system32-hosts' = C 'OK' '824'
    'read-ungranted'      = C 'ERR' 'EPERM'
    '__launch'            = C 'rc=0 (0x00000000) isAC=1' ''
    '__lines'             = C 6 ''
  }
  function Arm([hashtable]$extra, [hashtable]$over) {
    $h = @{}
    foreach ($k in $ctl.Keys) { $h[$k] = $ctl[$k] }
    foreach ($k in $extra.Keys) { $h[$k] = $extra[$k] }
    if ($over) { foreach ($k in $over.Keys) { $h[$k] = $over[$k] } }
    return $h
  }
  $cells = @{}
  # unconfined baselines: C:\ readable, canary readable, interpreter is whatever PATH gave
  $cells['plain-jailbin'] = Arm @{ 'npm-version' = C 'OK' '11.16.0' } `
    @{ 'stat-c-root' = C 'OK' 'ok'; 'read-ungranted' = C 'OK' 'CANARY-UNGRANTED';
       '__launch' = C 'rc=0 (0x00000000) token=?' '' }
  $cells['plain-ambient'] = Arm @{ 'npm-version' = C 'OK' '11.16.0' } `
    @{ 'interp-execpath' = C 'OK' "$PF\node.exe"; 'path-seen' = C 'OK' 'jailbin=False progfiles=True';
       'stat-c-root' = C 'OK' 'ok'; 'read-ungranted' = C 'OK' 'CANARY-UNGRANTED' }
  $cells['ac-cmd-floor'] = @{ 'cmd-runs' = C 'OK' ''; '__launch' = C 'rc=0 (0x00000000) isAC=1' ''; '__lines' = C 1 '' }
  $cells['ac-ambient'] = Arm @{ 'npm-version' = C 'ERR' 'MODULE_NOT_FOUND' } `
    @{ 'interp-execpath' = C 'OK' "$PF\node.exe"; 'path-seen' = C 'OK' 'jailbin=False progfiles=True' }
  $cells['ac-ambient-on-path'] = Arm @{ 'npm-version' = C 'ERR' '' } `
    @{ 'interp-execpath' = C 'OK' "$JB\node.exe"; 'path-seen' = C 'OK' 'jailbin=True progfiles=True' }
  $cells['ac-jailbin-ungranted'] = Arm @{ 'npm-direct' = C 'ERR' 'MODULE_NOT_FOUND' } $null
  $cells['ac-jailbin-node'] = Arm @{ 'node-version' = C 'OK' 'v24.18.1' } $null
  $cells['ac-jailbin-npm-direct'] = Arm @{ 'npm-direct' = C 'OK' '11.16.0' } $null
  $cells['ac-hybrid-npm-direct'] = Arm @{ 'npm-direct' = C 'OK' '11.16.0' } `
    @{ 'interp-execpath' = C 'OK' "$PF\node.exe"; 'path-seen' = C 'OK' 'jailbin=True progfiles=True' }
  $cells['ac-aap-only'] = Arm @{ 'npm-direct' = C 'OK' '11.16.0' } $null
  $cells['ac-jailbin-npm-stock'] = Arm @{ 'npm-version' = C 'OK' '11.16.0' } $null
  $cells['ac-jailbin-npm-nubshim'] = Arm @{ 'npm-version' = C 'OK' '11.16.0' } $null
  $cells['ac-absent-python'] = Arm @{ 'python-version' = C 'ERR' '' } $null
  $cells['ac-lifecycle'] = Arm @{ 'lc-start' = C 'OK' ''; 'lc-node-e' = C 'OK' 'v24.18.1'; 'lifecycle' = C 'OK' '' } $null
  $cells['ac-ungranted'] = @{ 'read-ungranted' = C 'ERR' 'EPERM'; '__launch' = C '(derived)' ''; '__lines' = C 0 '' }
  return $cells
}

function New-WorkingFacts {
  return @{
    'msi-installed'             = $true
    'inherited-ace-on-deep-npm-cli' = 'ReadAndExecute, Synchronize'
    'aap-only-perrun-ace'       = 'none'
    'aap-only-aap-ace'          = 'ReadAndExecute, Synchronize'
    'ungranted-arm-aap-ace'     = 'none'
    'ungranted-arm-deep-ace'    = 'none | perrun=none'
    'absent-tool-shape'         = 'not-recognized'
    'ambient-on-path-shape'     = 'node-not-resolvable'
    'hardlink-create'           = 'OK'
    'hardlink-leak'             = $true
    'hardlink-original-ace'     = 'ReadAndExecute, Synchronize'
    'hardlink-link-ace'         = 'ReadAndExecute, Synchronize'
    'deelev-jailbin-grant'      = 'OK'
    'deelev-jailbin-landed'     = 'ReadAndExecute, Synchronize'
    'deelev-admin'              = $false
    'deelev-privs'              = 'SeChangeNotifyPrivilege:on'
    'pf-residue'                = 'none'
  }
}

$worlds = @{}
$worlds['1-mechanism-works'] = @{ Cells = (New-WorkingCells); Facts = (New-WorkingFacts); MustFail = @() }

# 2. The grant does nothing: every decisive read is denied.
$c2 = New-WorkingCells
foreach ($a in @('ac-jailbin-npm-direct', 'ac-hybrid-npm-direct', 'ac-aap-only')) {
  $c2[$a]['npm-direct'] = C 'ERR' 'MODULE_NOT_FOUND'
}
$c2['ac-jailbin-node']['node-version'] = C 'ERR' ''
$c2['ac-jailbin-npm-stock']['npm-version'] = C 'ERR' ''
$c2['ac-jailbin-npm-nubshim']['npm-version'] = C 'ERR' ''
$c2['ac-lifecycle']['lifecycle'] = C 'ERR' 'npm-cli-exited-nonzero'
$f2 = New-WorkingFacts
$f2['inherited-ace-on-deep-npm-cli'] = 'none'
$worlds['2-mechanism-fails'] = @{ Cells = $c2; Facts = $f2; MustFail = @(
  'jb-inheritance-at-creation-covered-the-deep-file',
  'jb-node-resolves-and-runs-from-jailbin', 'jb-npm-tree-is-readable-from-jailbin',
  'jb-ambient-node-plus-owned-npm-tree-works', 'jb-stable-app-package-sid-grant-suffices',
  'jb-a-shim-without-the-prefix-probe-works', 'jb-real-lifecycle-script-runs') }

# 3. Everything green INCLUDING the ungranted arm — so the ace is not what carries the read.
$c3 = New-WorkingCells
$c3['ac-jailbin-ungranted']['npm-direct'] = C 'OK' '11.16.0'
$worlds['3-no-attribution'] = @{ Cells = $c3; Facts = (New-WorkingFacts); MustFail = @(
  'jb-attribution-ungranted-jailbin-is-denied') }

# 4. Hybrid green but it ran jail-bin's own node, so it says nothing about the ambient one.
$c4 = New-WorkingCells
$c4['ac-hybrid-npm-direct']['interp-execpath'] = C 'OK' "$JB\node.exe"
$worlds['4-wrong-interpreter'] = @{ Cells = $c4; Facts = (New-WorkingFacts); MustFail = @(
  'jb-control-hybrid-really-ran-the-ambient-interpreter') }

# 5. Nothing ran at all.
$worlds['5-harness-dead'] = @{ Cells = @{}; Facts = @{}; MustFail = @(
  'jb-control-plain-jailbin-npm-runs', 'jb-control-plain-ambient-npm-runs',
  'jb-control-cmd-runs-under-appcontainer', 'jb-control-appcontainer-gate-is-live',
  'jb-control-appcontainer-gate-is-passable', 'jb-control-child-saw-the-sanitized-path',
  'jb-control-ungranted-canary-exists', 'jb-control-grant-still-scopes',
  'jb-node-resolves-and-runs-from-jailbin', 'jb-npm-tree-is-readable-from-jailbin',
  'jb-ambient-node-plus-owned-npm-tree-works',
  'jb-inheritance-at-creation-covered-the-deep-file',
  'jb-ambient-install-dir-is-not-even-path-resolvable') }

$selftestFails = 0
foreach ($name in ($worlds.Keys | Sort-Object)) {
  $w = $worlds[$name]
  Write-Host ''
  Write-Host "########## SYNTHETIC WORLD: $name ##########"
  # `Prop` increments `$script:fails`, and `verdict.ps1` was dot-sourced into THIS script's scope, so
  # resetting it here really does reset the counter the verdict writes. The outcome is read off that
  # counter rather than off scraped `Write-Host` text, which is not reliably capturable.
  $script:fails = 0
  Invoke-JailBinVerdict -Cells $w.Cells -Facts $w.Facts
  $observedFails = $script:fails
  Write-Host ''
  Write-Host "  world $name : verdict reported $observedFails failing properties (expected at least $($w.MustFail.Count))"
  if ($name -eq '1-mechanism-works') {
    if ($observedFails -ne 0) {
      Write-Host "  SELFTEST FAIL $name : an all-green world must produce ZERO failing properties, got $observedFails"
      $selftestFails++
    } else { Write-Host "  SELFTEST OK   $name" }
  } else {
    if ($observedFails -lt $w.MustFail.Count) {
      Write-Host "  SELFTEST FAIL $name : expected at least $($w.MustFail.Count) failures, got $observedFails"
      Write-Host "                 the properties that had to fail: $($w.MustFail -join ', ')"
      $selftestFails++
    } else { Write-Host "  SELFTEST OK   $name" }
  }
}

Write-Host ''
if ($selftestFails -ne 0) {
  Write-Host "JAILBIN VERDICT SELFTEST FAILED ($selftestFails worlds) - the real tables would be unreadable"
  exit 1
}
Write-Host 'JAILBIN VERDICT SELFTEST PASSED (5 synthetic worlds, each discriminated correctly)'
exit 0
