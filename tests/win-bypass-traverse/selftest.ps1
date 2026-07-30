# Proves the verdict block DISCRIMINATES — runs anywhere, no Windows APIs, no launch.
#
# WHY THIS EXISTS. A verdict block only ever exercised against the world it expects will happily
# report PASS on a harness that measured nothing; that is precisely how this effort previously
# produced a negative it could not falsify and a "confirmation" from a broken control. So the
# verdict is driven against ELEVEN synthetic worlds and required to give a DIFFERENT answer in each.
# The first six are about the AppContainer read mechanism:
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
# And five more for the realpath repair, each one variable off 'works':
#
#   shim-wrong-  the shim loads, every read     => the wrong-version control FAILS. THE ONE THAT
#   version      works, and it binds the OTHER     WOULD ACTUALLY SHIP A DEFECT: nothing else in
#                version of the package            the table looks wrong, so this control is the
#                                                  only thing standing between a repair and the
#                                                  hazard that disqualified --preserve-symlinks.
#   shim-preload the `data:` --import never     => the preload property FAILS and is ATTRIBUTED as
#   -absent      evaluated                        such, rather than read as "the repair fails".
#   shim-widens  resolution repaired, but       => the widen property AND the jail-wide secrets
#   -jail        $HOME secrets now readable        property both FAIL.
#   native-works `.native` works in the jail   => the refusal property FAILS, loudly, because that
#                                                  flip makes the whole shim unnecessary.
#   native-unat- the confined leaf cannot even => the attribution property FAILS, so a `.native`
#   tributable   be opened                        refusal is never over-read.
#
# Run: pwsh -File selftest.ps1

$ErrorActionPreference = 'Stop'
Set-StrictMode -Off
. (Join-Path $PSScriptRoot 'verdict.ps1')

function Mk([hashtable]$ops, [int]$lines = 40, [string]$launch = 'rc=0 (0x00000000)',
            [int]$opcount = 25, [hashtable]$details = @{}) {
  $h = @{}
  foreach ($k in $ops.Keys) {
    $d = if ($details.ContainsKey($k)) { $details[$k] } else { 'synthetic' }
    $h[$k] = @($ops[$k], $d)
  }
  $h['__lines'] = @($lines, '')
  $h['__launch'] = @($launch, '')
  $h['__opcount'] = @($opcount, '')
  return $h
}

$deepOps = @('read-deep-granted', 'require-deep-granted', 'realpath-deep-granted',
  'stat-deep-granted', 'open-deep-granted', 'readdir-deepdir', 'write-into-granted',
  'chdir-to-deepdir', 'cwd-after-chdir', 'read-relative-after-chdir')
# `.native` moves independently of the JS walk — that is the whole point of the battery — so it is
# its own group rather than folded into the deep cells.
$nativeOps = @('native-deep-granted', 'native-deep-granted-held', 'native-deepdir-granted',
  'native-runtime-granted', 'native-system32-hosts', 'native-longpath-granted')
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
             [string]$entry, [string]$entryRoot, [string]$dacl = 'OK', [string]$native = 'ERR',
             [string]$shim = 'ERR', [string]$iso = 'OK') {
  # `node-died-realpath-c-root` is ERR for every arm that REACHED the table — an arm that died in
  # Node's realpath walk emits no op lines at all and is modelled separately below.
  $h = @{ 'findup-walk' = 'OK'; 'dacl-grants-ac-sid' = $dacl; 'node-died-realpath-c-root' = 'ERR' }
  foreach ($o in $deepOps) { $h[$o] = $deep }
  foreach ($o in $nativeOps) { $h[$o] = $native }
  foreach ($o in $rootOps) { $h[$o] = $root }
  foreach ($o in $ungrantedOps) { $h[$o] = $ungranted }
  foreach ($o in $sysOps) { $h[$o] = $sys }
  foreach ($o in $netOps) { $h[$o] = $net }
  foreach ($o in $entryOps) { $h[$o] = $entry }
  $h['entry-read-c-root'] = $entryRoot
  $h['native-c-root'] = $root
  $h['realpath-shim-installed'] = $shim
  $h['isolated-layout-version'] = $iso
  $h['isolated-layout-resolved-main'] = $iso
  return $h
}
# The version an arm's `isolated-layout-version` cell REPORTS lives in the detail, not in OK/ERR,
# so the wrong-version control needs it modelled explicitly.
function IsoDetail([string]$version) { return @{ 'isolated-layout-version' = "bar@$version" } }

# plain: unconfined, everything works — INCLUDING `.native`, which is the allow half of the
# battery's differential, and the flag-carrying arms report the WRONG version because the two
# preserve-symlinks flags are what `plain` runs with.
$plainOK = Ops 'OK' 'OK' 'OK' 'OK' 'OK' 'OK' 'OK' 'ERR' 'OK' 'ERR' 'OK'
# confined, deep reads WORK (bypass-traverse alive), roots denied, ungranted denied, egress denied.
# `.native` refused, which is the standing measured answer.
$acWorks = Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'ERR' 'OK'
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

# ── the arms the realpath work added, and the versions they report ──
# Every flag-carrying arm reports bar@1.0.0 because `--preserve-symlinks` is what it runs with;
# the unflagged baseline and a correct repair report bar@2.0.0. Those three numbers ARE the
# wrong-version control, so they are modelled per arm rather than assumed uniform.
function MkArm([hashtable]$ops, [string]$version) {
  return Mk $ops 40 'rc=0 (0x00000000)' 25 (IsoDetail $version)
}
$plainNoFlagsArm = MkArm $plainOK '2.0.0'
$acShimGood = MkArm (Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'OK') '2.0.0'
# THE FALSE POSITIVE THIS GUARDS AGAINST, and the only one that would actually ship a defect: the
# shim loads, every read works, nothing looks wrong — and it silently binds the OTHER version,
# which is precisely what disqualified `--preserve-symlinks`.
$acShimWrongVersion = MkArm (Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'OK') '1.0.0'
# The `data:` --import never evaluated: reads still fail, and the failure must be attributed to the
# preload rather than reported as "the repair does not work".
$acShimNoPreload = MkArm (Ops 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR') '2.0.0'
# A repair that turned into a read grant: resolution fixed, $HOME secrets now readable.
$acShimWidened = MkArm (Ops 'OK' 'ERR' 'OK' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'OK') '2.0.0'
# `.native` starts WORKING in the jail — the flip that makes the whole repair unnecessary and must
# never pass unnoticed.
$acNativeWorks = MkArm (Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'OK' 'ERR' 'OK') '1.0.0'
# The confined leaf cannot even be OPENED, so a `.native` refusal is unattributable.
$acNativeUnattributable = MkArm (Ops 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR') '1.0.0'
$acShimEntryOK = MkArm (Ops 'OK' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'ERR' 'OK' 'OK') '2.0.0'
# The preload arrived and the jail denies the reads anyway — the shim arm's shape in a world where
# bypass-traverse itself is dead. Distinct from `shim-preload-absent`, whose reads fail for a
# different reason, and keeping them distinct is what stops one being read as the other.
$acShimDenied = MkArm (Ops 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'ERR' 'ERR' 'OK' 'ERR' 'OK' 'OK') '2.0.0'

# Every world must state its shim arms, because `ac-shim` is one of the arms the gate-is-live
# control covers: a world that omitted it would fail that control for a modelling reason rather
# than for anything the world is about.
function WithShim([hashtable]$world, $shimArm, $entryArm = $null, $plainNoFlags = $null) {
  $w = @{}
  foreach ($k in $world.Keys) { $w[$k] = $world[$k] }
  $w['plain-noflags'] = if ($plainNoFlags) { $plainNoFlags } else { $plainNoFlagsArm }
  $w['ac-shim'] = $shimArm
  $w['ac-shim-entry-deep'] = if ($entryArm) { $entryArm } else { $acShimEntryOK }
  # The flag-carrying confined arm reports the WRONG version by construction; without this the
  # wrong-version control has no "and the flag arm shows the hazard" half.
  if ($w.ContainsKey('ac-leaf-grants')) {
    $existing = $w['ac-leaf-grants']
    if ($existing.ContainsKey('isolated-layout-version')) {
      $existing['isolated-layout-version'] = @($existing['isolated-layout-version'][0], 'bar@1.0.0')
    }
  }
  return $w
}

$worlds = @{
  'works' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  } $acShimGood
  # ── the realpath-work worlds: one variable off 'works', each about ONE new property ──
  'shim-wrong-version' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  } $acShimWrongVersion
  'shim-preload-absent' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  } $acShimNoPreload
  'shim-widens-jail' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  } $acShimWidened
  'native-works' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = $acNativeWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  } $acShimGood
  'native-unattributable' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = $acNativeUnattributable
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsArm
  } $acShimGood
  'denied' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acDenied; 'ac-leaf-grants' = Mk $acDenied
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acDenied; 'ac-entry-deep' = Mk $acDenied
    'ac-noflags' = $acNoFlagsArm
  } $acShimDenied
  'harness-dead' = WithShim @{
    plain = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-root-grant' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-leaf-grants' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-data-ungranted' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-cwd-deep' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-entry-deep' = Mk $allERR 0 'launch-error CreateProcessW err=2'
    'ac-noflags' = Mk $allERR 0 'launch-error CreateProcessW err=2' 0
  } (Mk $allERR 0 'launch-error CreateProcessW err=2') `
    (Mk $allERR 0 'launch-error CreateProcessW err=2') (Mk $allERR 0 'launch-error CreateProcessW err=2')
  'ace-inert' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acInert; 'ac-leaf-grants' = Mk $acInert
    'ac-data-ungranted' = Mk $acInert; 'ac-cwd-deep' = Mk $acInert; 'ac-entry-deep' = Mk $acInert
    'ac-noflags' = $acNoFlagsArm
  } (MkArm $acInert '2.0.0')
  'grant-never-landed' = WithShim @{
    plain = Mk $plainOK
    'ac-root-grant' = Mk $acGrantNeverLanded; 'ac-leaf-grants' = Mk $acGrantNeverLanded
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acGrantNeverLanded
    'ac-entry-deep' = Mk $acGrantNeverLanded; 'ac-noflags' = $acNoFlagsArm
  } $acShimDenied
  'defect-absent' = WithShim @{
    plain = Mk $plainOK; 'ac-root-grant' = Mk $acWorks; 'ac-leaf-grants' = Mk $acWorks
    'ac-data-ungranted' = Mk $acNoGrant; 'ac-cwd-deep' = Mk $acWorks; 'ac-entry-deep' = Mk $acWorks
    'ac-noflags' = $acNoFlagsRan
  } $acShimGood
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
    'native-realpath-battery-is-attributable' = 'PASS'
    'native-realpath-refused-under-appcontainer' = 'PASS'
    'shim-preload-arrived-in-jail' = 'PASS'
    'shim-repairs-resolution-in-jail' = 'PASS'
    'shim-preserves-isolated-layout-resolution' = 'PASS'
    'shim-does-not-widen-the-jail' = 'PASS'
    'shim-entry-point-runs-on-main-flag-alone' = 'PASS'
  }
  'shim-wrong-version' = @{
    # The one that would actually ship a defect: everything green except the version. If this ever
    # reads PASS the control is not measuring the hazard and the repair is unfalsifiable.
    'shim-preload-arrived-in-jail' = 'PASS'; 'shim-repairs-resolution-in-jail' = 'PASS'
    'shim-does-not-widen-the-jail' = 'PASS'
    'shim-preserves-isolated-layout-resolution' = 'FAIL'
  }
  'shim-preload-absent' = @{
    # Attribution: the repair did nothing because the preload never evaluated, and that must be
    # reported as a preload failure rather than as a repair that does not work.
    'shim-preload-arrived-in-jail' = 'FAIL'; 'shim-repairs-resolution-in-jail' = 'FAIL'
  }
  'shim-widens-jail' = @{
    # A realpath repair must not become a read grant. Both the shim-specific property and the
    # jail-wide secrets property have to catch it.
    'shim-repairs-resolution-in-jail' = 'PASS'
    'shim-does-not-widen-the-jail' = 'FAIL'
    'secrets-under-profile-are-denied' = 'FAIL'
  }
  'native-works' = @{
    # `.native` granted under an AppContainer makes the entire shim unnecessary. The polarity is
    # deliberately inverted so that discovery is a loud FAIL, never a quiet green.
    'native-realpath-battery-is-attributable' = 'PASS'
    'native-realpath-refused-under-appcontainer' = 'FAIL'
  }
  'native-unattributable' = @{
    # The confined leaf could not be opened, so a `.native` refusal says nothing about realpath.
    'native-realpath-battery-is-attributable' = 'FAIL'
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
                    'defect-absent', 'shim-wrong-version', 'shim-preload-absent',
                    'shim-widens-jail', 'native-works', 'native-unattributable')) {
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
  Write-Host "SELFTEST OK - the verdict discriminates in all eleven worlds"
  exit 0
}
Write-Host "SELFTEST FAILED - $bad expectation(s) wrong"
exit 1
