# Proves the verdict block DISCRIMINATES — runs anywhere, no Windows APIs, no launch.
#
# WHY THIS EXISTS. A verdict block only ever exercised against the world it expects will happily
# report PASS on a harness that measured nothing; that is precisely how this effort previously
# produced a negative it could not falsify and a "confirmation" from a broken control. So the
# verdict is driven against SIX synthetic worlds and required to give a DIFFERENT answer in each:
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
#   defect-absent the no-flag arm ran fine     => the flag differential FAILS, so the flags are
#                                                never credited with fixing something that was
#                                                not broken in this run.
#
# Run: pwsh -File selftest.ps1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Off
. (Join-Path $PSScriptRoot 'verdict.ps1')

function Mk([hashtable]$ops, [int]$lines = 40, [string]$launch = 'rc=0 (0x00000000)',
            [int]$opcount = 25) {
  $h = @{}
  foreach ($k in $ops.Keys) { $h[$k] = @($ops[$k], 'synthetic') }
  $h['__lines'] = @($lines, '')
  $h['__launch'] = @($launch, '')
  $h['__opcount'] = @($opcount, '')
  return $h
}

$deepOps = @('read-deep-granted', 'require-deep-granted', 'realpath-deep-granted',
  'stat-deep-granted', 'readdir-deepdir', 'write-into-granted', 'chdir-to-deepdir',
  'cwd-after-chdir', 'read-relative-after-chdir')
$rootOps = @('lstat-c-root', 'realpath-c-root', 'stat-c-root', 'readdir-c-root', 'stat-c-users',
  'readdir-c-users', 'stat-userprofile', 'readdir-userprofile')
# The $HOME secrets share the ungranted paths' semantics exactly: readable by an unconfined child,
# refused by a confined one. Modelling them together is the point — if a world makes ungranted
# paths readable it must also make the secrets readable, and the secrets property must then fail.
$ungrantedOps = @('read-ungranted-sibling-inside-root', 'read-ungranted-sibling-under-profile',
  'read-ssh-private-key', 'readdir-dot-ssh', 'stat-ssh-private-key', 'read-npmrc')
$sysOps = @('read-system32-hosts', 'readdir-system32', 'whoami-groups')
$netOps = @('dns-lookup-registry', 'net-connect-ip', 'net-connect-name', 'net-connect-loopback',
  'spawn-piped-whoami')
$entryOps = @('entry-as-deep-file', 'entry-cwd', 'entry-realpath', 'entry-require-bare')

function Ops([string]$deep, [string]$root, [string]$ungranted, [string]$sys, [string]$net,
             [string]$entry, [string]$entryRoot, [string]$dacl = 'OK') {
  # `node-died-realpath-c-root` is ERR for every arm that REACHED the table — an arm that died in
  # Node's realpath walk emits no op lines at all and is modelled separately below.
  $h = @{ 'findup-walk' = 'OK'; 'dacl-grants-ac-sid' = $dacl; 'node-died-realpath-c-root' = 'ERR' }
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
# THE NO-FLAG ARM: node dies in `resolveMainPath` before user code, so there are NO op lines —
# only the log-derived cell. Modelled with zero ops on purpose; that is what the flag differential
# reads, and an arm with zero ops must never be mistaken for one whose reads were denied.
$acNoFlagsArm = Mk @{ 'node-died-realpath-c-root' = 'OK' } 20 'rc=1 (0x00000001)' 0
# the no-flag arm in a world where the defect does NOT reproduce (so the differential must fail)
$acNoFlagsRan = Mk (Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR') 40 'rc=0 (0x00000000)' 25

$worlds = @{
  'works' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  }
  'denied' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acDenied; 'ac-leaf-grants' = Mk $acDenied
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acDenied; 'ac-entry-deep' = Mk $acDenied
    'ac-noflags' = $acNoFlagsArm
  }
  'harness-dead' = @{
    plain = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-root-grant' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-leaf-grants' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-data-ungranted' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-cwd-deep' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-entry-deep' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-noflags' = Mk $allERR 0 'launch-error CreateProcessW err=2' 0
  }
  'ace-inert' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acInert; 'ac-leaf-grants' = Mk $acInert
    'ac-data-ungranted' = Mk $acInert; 'ac-cwd-deep' = Mk $acInert; 'ac-entry-deep' = Mk $acInert
    'ac-noflags' = $acNoFlagsArm
  }
  'grant-never-landed' = @{
    plain = Mk $plainOK
    'ac-root-grant' = Mk $acGrantNeverLanded; 'ac-leaf-grants' = Mk $acGrantNeverLanded
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acGrantNeverLanded
    'ac-entry-deep' = Mk $acGrantNeverLanded; 'ac-noflags' = $acNoFlagsArm
  }
  'defect-absent' = @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsRan
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
    'realpath-defect-reproduces-without-flags' = 'PASS'
    'preserve-symlinks-flags-unblock-the-child' = 'PASS'
    'secrets-under-profile-are-denied' = 'PASS'
    'secrets-baseline-plain-can-read-them' = 'PASS'
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
    # The flags still did their job — the child RAN and the kernel denied the read. That is what
    # separates a real denial from "node never got started".
    'realpath-defect-reproduces-without-flags' = 'PASS'
    'preserve-symlinks-flags-unblock-the-child' = 'PASS'
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
    'preserve-symlinks-flags-unblock-the-child' = 'PASS'
    'control-ace-absent-denies-deep-read' = 'PASS'
  }
  'defect-absent' = @{
    # The no-flag arm ran fine, so the realpath defect did NOT reproduce in this run — which means
    # the flags cannot be credited with fixing it. Guards against attributing an effect to a
    # variable when the thing it supposedly fixes was never there.
    'realpath-defect-reproduces-without-flags' = 'FAIL'
    'preserve-symlinks-flags-unblock-the-child' = 'PASS'
    'bypass-traverse-deep-read-leaf-grants' = 'PASS'
  }
  'ace-inert' = @{
    # Every deep read passes, but so does every path that was never granted — so the CONTROLS
    # must fail and the run must not be reportable as a positive.
    'appcontainer-gate-is-live' = 'FAIL'
    'control-ace-absent-denies-deep-read' = 'FAIL'
    'control-ungranted-sibling-under-profile-denies' = 'FAIL'
    'control-ungranted-sibling-inside-root-denies' = 'FAIL'
    # the whole point: a grant that scopes nothing leaks $HOME secrets, and that must be loud
    'secrets-under-profile-are-denied' = 'FAIL'
  }
}

$bad = 0
foreach ($name in @('works', 'denied', 'harness-dead', 'ace-inert', 'grant-never-landed',
                    'defect-absent')) {
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
  Write-Host "SELFTEST OK - the verdict discriminates in all six worlds"
  exit 0
}
Write-Host "SELFTEST FAILED - $bad expectation(s) wrong"
exit 1
