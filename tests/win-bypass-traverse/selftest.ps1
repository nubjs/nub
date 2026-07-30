# Proves the verdict block DISCRIMINATES — runs anywhere, no Windows APIs, no launch.
#
# WHY THIS EXISTS. A verdict block only ever exercised against the world it expects will happily
# report PASS on a harness that measured nothing; that is precisely how this effort previously
# produced a negative it could not falsify and a "confirmation" from a broken control. So the
# verdict is driven against FIVE synthetic worlds and required to give a DIFFERENT answer in each:
#
#   works        bypass-traverse works        => every property PASSes
#   denied       bypass-traverse fails        => the bypass-traverse properties FAIL, and every
#                                               control still PASSes (this is what a CLEAN
#                                               NEGATIVE must look like)
#   harness-dead every arm fails, plain too   => the BASELINE properties FAIL, so a broken harness
#                                               can never be reported as a clean negative
#   ace-inert    everything passes everywhere => the CONTROL properties FAIL, so a grant that
#                                               scopes nothing can never be reported as a pass
#   grant-never  the ace never propagated into => the grant-reached control FAILS. Read-cell for
#   -landed      the deep file, so reads fail     read-cell IDENTICAL to 'denied', and that is the
#                                                point: without this control a harness slip is
#                                                indistinguishable from a real kernel denial.
#
# Run: pwsh -File selftest.ps1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Off
. (Join-Path $PSScriptRoot 'verdict.ps1')

function Mk([hashtable]$ops, [int]$lines = 40, [string]$launch = 'rc=0 (0x00000000)') {
  $h = @{}
  foreach ($k in $ops.Keys) { $h[$k] = @($ops[$k], 'synthetic') }
  $h['__lines'] = @($lines, '')
  $h['__launch'] = @($launch, '')
  return $h
}

$deepOps = @('read-deep-granted', 'require-deep-granted', 'realpath-deep-granted',
  'stat-deep-granted', 'readdir-deepdir', 'write-into-granted', 'chdir-to-deepdir',
  'cwd-after-chdir', 'read-relative-after-chdir')
$rootOps = @('stat-c-root', 'readdir-c-root', 'stat-c-users', 'readdir-c-users',
  'stat-userprofile', 'readdir-userprofile')
$ungrantedOps = @('read-ungranted-sibling-inside-root', 'read-ungranted-sibling-under-profile')
$sysOps = @('read-system32-hosts', 'readdir-system32', 'whoami-groups')
$netOps = @('dns-lookup-registry', 'net-connect-ip', 'net-connect-name', 'net-connect-loopback')
$entryOps = @('entry-as-deep-file', 'entry-cwd', 'entry-realpath', 'entry-require-bare')

function Ops([string]$deep, [string]$root, [string]$ungranted, [string]$sys, [string]$net,
             [string]$entry, [string]$entryRoot, [string]$dacl = 'OK') {
  $h = @{ 'findup-walk' = 'OK'; 'dacl-grants-ac-sid' = $dacl }
  foreach ($o in $deepOps) { $h[$o] = $deep }
  foreach ($o in $rootOps) { $h[$o] = $root }
  foreach ($o in $ungrantedOps) { $h[$o] = $ungranted }
  foreach ($o in $sysOps) { $h[$o] = $sys }
  foreach ($o in $netOps) { $h[$o] = $net }
  foreach ($o in $entryOps) { $h[$o] = $entry }
  $h['entry-read-c-root'] = $entryRoot
  return $h
}

# plain: unconfined, everything works.
$plainOK = Ops 'OK' 'OK' 'OK' 'OK' 'OK' 'OK' 'OK' 'ERR'
# confined, deep reads WORK (bypass-traverse alive), roots denied, ungranted denied, egress denied.
$acWorks = Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR'
# confined, deep reads DENIED (bypass-traverse dead).
$acDenied = Ops 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR'
# confined, the data grant withheld: deep denied AND the deep file's dacl carries no ac sid.
$acNoGrant = Ops 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR' 'ERR'
# THE FALSE NEGATIVE THIS GUARDS AGAINST: the grant was written on an ancestor but never
# propagated into the pre-existing deep file, so every deep read fails for a harness reason that
# looks exactly like "bypass-traverse is dead".
$acGrantNeverLanded = Ops 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR' 'ERR'
# a child that fails at literally everything (harness dead).
$allERR = Ops 'ERR' 'ERR' 'ERR' 'ERR' 'ERR' 'ERR' 'ERR'
# a grant that scopes nothing: even the ungranted paths and the roots read.
$acInert = Ops 'OK' 'OK' 'OK' 'OK' 'ERR' 'OK' 'OK'

$worlds = @{
  'works' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
  }
  'denied' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acDenied; 'ac-leaf-grants' = Mk $acDenied
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acDenied; 'ac-entry-deep' = Mk $acDenied
  }
  'harness-dead' = @{
    plain = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-root-grant' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-leaf-grants' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-data-ungranted' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-cwd-deep' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-entry-deep' = Mk $allERR 0 'launch-error CreateProcessW err=2'
  }
  'ace-inert' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acInert; 'ac-leaf-grants' = Mk $acInert
    'ac-data-ungranted' = Mk $acInert; 'ac-cwd-deep' = Mk $acInert; 'ac-entry-deep' = Mk $acInert
  }
  'grant-never-landed' = @{
    plain = Mk $plainOK
    'ac-root-grant' = Mk $acGrantNeverLanded; 'ac-leaf-grants' = Mk $acGrantNeverLanded
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acGrantNeverLanded
    'ac-entry-deep' = Mk $acGrantNeverLanded
  }
}

# Property name -> expected outcome, per world. A property missing from a world's map is not
# asserted; every property the world is ABOUT is asserted explicitly.
$expect = @{
  'works' = @{
    'baseline-plain-launched' = 'PASS'; 'baseline-plain-reads-deep' = 'PASS'
    'baseline-plain-reads-c-root' = 'PASS'; 'baseline-plain-egress-works' = 'PASS'
    'appcontainer-gate-is-live' = 'PASS'; 'appcontainer-positive-control' = 'PASS'
    'bypass-traverse-deep-read-root-grant' = 'PASS'
    'bypass-traverse-deep-read-leaf-grants' = 'PASS'; 'bypass-traverse-require-deep' = 'PASS'
    'bypass-traverse-launch-cwd-deep' = 'PASS'; 'bypass-traverse-child-chdir-deep' = 'PASS'
    'bypass-traverse-node-entry-deep' = 'PASS'
    'harness-grant-reached-decisive-target' = 'PASS'
    'control-ace-absent-denies-deep-read' = 'PASS'
    'control-ungranted-sibling-under-profile-denies' = 'PASS'
    'control-ungranted-sibling-inside-root-denies' = 'PASS'
    'egress-denied-with-zero-capabilities' = 'PASS'
  }
  'denied' = @{
    # A CLEAN NEGATIVE: the decisive cells fail while every control still holds.
    'baseline-plain-launched' = 'PASS'; 'baseline-plain-reads-deep' = 'PASS'
    'baseline-plain-reads-c-root' = 'PASS'; 'baseline-plain-egress-works' = 'PASS'
    'appcontainer-gate-is-live' = 'PASS'; 'appcontainer-positive-control' = 'PASS'
    'bypass-traverse-deep-read-root-grant' = 'FAIL'
    'bypass-traverse-deep-read-leaf-grants' = 'FAIL'; 'bypass-traverse-require-deep' = 'FAIL'
    'bypass-traverse-child-chdir-deep' = 'FAIL'; 'bypass-traverse-node-entry-deep' = 'FAIL'
    # THE distinguishing cell: the ace DID land on the deep file, so the denial is the kernel's
    # answer and not a harness slip. This is what makes the negative clean.
    'harness-grant-reached-decisive-target' = 'PASS'
    'control-ace-absent-denies-deep-read' = 'PASS'
    'control-ungranted-sibling-under-profile-denies' = 'PASS'
    'egress-denied-with-zero-capabilities' = 'PASS'
  }
  'harness-dead' = @{
    # The baseline must catch it. A negative reported under these props is worthless.
    'baseline-plain-launched' = 'FAIL'; 'baseline-plain-reads-deep' = 'FAIL'
    'baseline-plain-reads-c-root' = 'FAIL'; 'baseline-plain-egress-works' = 'FAIL'
    'appcontainer-positive-control' = 'FAIL'
    'bypass-traverse-deep-read-leaf-grants' = 'FAIL'
  }
  'grant-never-landed' = @{
    # Looks identical to 'denied' on every read cell, and must NOT be reportable as a clean
    # negative: the grant-reached control is what tells the two apart.
    'baseline-plain-launched' = 'PASS'; 'appcontainer-gate-is-live' = 'PASS'
    'appcontainer-positive-control' = 'PASS'
    'harness-grant-reached-decisive-target' = 'FAIL'
    'bypass-traverse-deep-read-leaf-grants' = 'FAIL'
    'control-ace-absent-denies-deep-read' = 'PASS'
  }
  'ace-inert' = @{
    # Every deep read passes, but so does every path that was never granted — so the CONTROLS
    # must fail and the run must not be reportable as a positive.
    'appcontainer-gate-is-live' = 'FAIL'
    'control-ace-absent-denies-deep-read' = 'FAIL'
    'control-ungranted-sibling-under-profile-denies' = 'FAIL'
    'control-ungranted-sibling-inside-root-denies' = 'FAIL'
  }
}

$bad = 0
foreach ($name in @('works', 'denied', 'harness-dead', 'ace-inert', 'grant-never-landed')) {
  $script:fails = 0
  $captured = Invoke-Verdict -Cells $worlds[$name] 6>&1
  $got = @{}
  foreach ($line in $captured) {
    if ("$line" -match '^\s*prop:([^=]+)=(PASS|FAIL)') { $got[$Matches[1]] = $Matches[2] }
  }
  Write-Host ''
  Write-Host "=== selftest world '$name' ==="
  foreach ($k in ($expect[$name].Keys | Sort-Object)) {
    $want = $expect[$name][$k]
    $have = if ($got.ContainsKey($k)) { $got[$k] } else { 'ABSENT' }
    if ($have -eq $want) {
      Write-Host "  ok    $k = $have"
    } else {
      Write-Host "  BAD   $k = $have (expected $want)"
      $bad++
    }
  }
}

Write-Host ''
if ($bad -eq 0) {
  Write-Host "SELFTEST OK - the verdict discriminates in all five worlds"
  exit 0
}
Write-Host "SELFTEST FAILED - $bad expectation(s) wrong"
exit 1
