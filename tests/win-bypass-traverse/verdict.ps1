# The operations table and the verdict, factored out of `probe.ps1` for ONE reason: so
# `selftest.ps1` can drive it with SYNTHETIC cells and prove it discriminates in BOTH directions.
#
# Every prior arm of this Windows effort that produced a wrong answer produced it here — a table
# of denials that could not be told from a broken harness, a control that "confirmed" a hypothesis
# because it varied more than one thing. A verdict block that has only ever been exercised against
# the world it expects is a verdict block that will report PASS on a harness that measured nothing.
# `selftest.ps1` feeds it a bypass-traverse-WORKS world and a bypass-traverse-FAILS world and
# requires the opposite answer from each.
#
# Dot-source this; the functions then read `$cells` and write `$script:fails` in the caller's scope.

$script:fails = 0
function W([string]$s) { Write-Host $s }
function Fact([string]$k, $v) { W("  fact:$k = $v") }
function Prop([string]$k, [bool]$ok, [string]$why) {
  W("  prop:$k=" + $(if ($ok) { 'PASS' } else { 'FAIL' }) + "  $why")
  if (-not $ok) { $script:fails++ }
}

function Cell([string]$arm, [string]$op) {
  if (-not $cells.ContainsKey($arm)) { return 'MISSING-ARM' }
  if (-not $cells[$arm].ContainsKey($op)) { return 'MISSING-OP' }
  return $cells[$arm][$op][0]
}
function Detail([string]$arm, [string]$op) {
  if (-not $cells.ContainsKey($arm)) { return '' }
  if (-not $cells[$arm].ContainsKey($op)) { return '' }
  return $cells[$arm][$op][1]
}
function Lines([string]$arm) {
  if (-not $cells.ContainsKey($arm)) { return -1 }
  return [int]$cells[$arm]['__lines'][0]
}
function LaunchOf([string]$arm) {
  if (-not $cells.ContainsKey($arm)) { return 'MISSING-ARM' }
  return [string]$cells[$arm]['__launch'][0]
}
function OpCount([string]$arm) {
  if (-not $cells.ContainsKey($arm)) { return -1 }
  if (-not $cells[$arm].ContainsKey('__opcount')) { return -1 }
  return [int]$cells[$arm]['__opcount'][0]
}

$script:ProbeOps = @(
  'dacl-grants-ac-sid',
  'read-deep-granted', 'require-deep-granted', 'realpath-deep-granted', 'stat-deep-granted',
  'readdir-deepdir', 'write-into-granted', 'chdir-to-deepdir', 'cwd-after-chdir',
  'read-relative-after-chdir', 'lstat-c-root', 'realpath-c-root',
  'stat-c-root', 'readdir-c-root', 'stat-c-users', 'readdir-c-users',
  'stat-userprofile', 'readdir-userprofile', 'findup-walk', 'read-ungranted-sibling-inside-root',
  'read-ungranted-sibling-under-profile', 'read-system32-hosts', 'readdir-system32',
  'whoami-groups', 'dns-lookup-registry', 'net-connect-ip', 'net-connect-name',
  'net-connect-loopback')
$script:ProbeArms = @('plain', 'ac-root-grant', 'ac-leaf-grants', 'ac-data-ungranted', 'ac-cwd-deep',
  'ac-noflags')
# `ac-noflags` and `ac-entry-deep` are excluded from the gate check below: the first emits no op
# lines at all by design (it dies in Node's bootstrap), and the second reports the root cell under
# a different op name.
$script:ProbeAcArms = @('ac-root-grant', 'ac-leaf-grants', 'ac-data-ungranted', 'ac-cwd-deep',
  'ac-entry-deep')

function Invoke-Verdict {
  param([hashtable]$Cells)
  $cells = $Cells

  W ''
  W '== operations table =='
  W ("  {0,-38} {1}" -f 'op', (($script:ProbeArms | ForEach-Object { "{0,-18}" -f $_ }) -join ''))
  foreach ($op in $script:ProbeOps) {
    W ("  {0,-38} {1}" -f $op,
      (($script:ProbeArms | ForEach-Object { "{0,-18}" -f (Cell $_ $op) }) -join ''))
  }
  W ''
  W '  entry-point arm (ac-entry-deep):'
  foreach ($op in @('entry-as-deep-file', 'entry-cwd', 'entry-realpath', 'entry-require-bare',
                    'entry-read-c-root')) {
    W ("    {0,-24} {1}  {2}" -f $op, (Cell 'ac-entry-deep' $op), (Detail 'ac-entry-deep' $op))
  }

  W ''
  W '== verdict =='

  # ── CONTROL: the plain arm. Identical child, identical paths, no SECURITY_CAPABILITIES, no
  # aces. If any of these is red the harness or the host is the story and nothing else in the
  # table is attributable to the token.
  Prop 'baseline-plain-launched' ((Lines 'plain') -gt 0) `
    "log-lines=$(Lines 'plain') launch=$(LaunchOf 'plain')"
  Prop 'baseline-plain-reads-deep' ((Cell 'plain' 'read-deep-granted') -eq 'OK') `
    "unconfined child must read the deep file with no ace anywhere: $(Detail 'plain' 'read-deep-granted')"
  Prop 'baseline-plain-reads-c-root' ((Cell 'plain' 'stat-c-root') -eq 'OK') `
    "unconfined child must stat C:\ — a failure here is the host, not the token"
  # Tolerant of a blocked literal-IP dial: the differential needs the plain arm to reach the
  # network SOMEHOW; pinning it to 1.1.1.1 would let a runner egress policy pose as evidence.
  Prop 'baseline-plain-egress-works' (((Cell 'plain' 'net-connect-ip') -eq 'OK') -or
    ((Cell 'plain' 'net-connect-name') -eq 'OK') -or ((Cell 'plain' 'dns-lookup-registry') -eq 'OK')) `
    "the allow half of the egress differential: ip=$(Cell 'plain' 'net-connect-ip') name=$(Cell 'plain' 'net-connect-name') dns=$(Cell 'plain' 'dns-lookup-registry')"

  # ── CONTROL: the AppContainer arms must really be confined, and must not be failing at
  # everything. `C:\` denied proves the LowBox gate is live; System32 granted proves the gate is
  # passable, so a column of ERRs is a statement about DACLs rather than about a dead child.
  $gateLive = $true
  $gateWhy = @()
  foreach ($a in $script:ProbeAcArms) {
    $c = if ($a -eq 'ac-entry-deep') { Cell $a 'entry-read-c-root' } else { Cell $a 'stat-c-root' }
    $gateWhy += "$a=$c"
    if ($c -ne 'ERR') { $gateLive = $false }
  }
  Prop 'appcontainer-gate-is-live' $gateLive `
    "every AppContainer arm must be DENIED on C:\ ($($gateWhy -join ' ')) — else the token is not confined and no pass below means anything"
  Prop 'appcontainer-positive-control' (((Cell 'ac-leaf-grants' 'read-system32-hosts') -eq 'OK') -and
    ((Cell 'ac-leaf-grants' 'readdir-system32') -eq 'OK')) `
    "System32 carries an ALL APPLICATION PACKAGES ace, so a confined child must still read it: hosts=$(Cell 'ac-leaf-grants' 'read-system32-hosts') readdir=$(Cell 'ac-leaf-grants' 'readdir-system32')"

  # ── THE FLAG DIFFERENTIAL, one variable. An unflagged confined `node` dies in `resolveMainPath`
  # at `EPERM lstat 'C:\'` because Node's JS realpath opens the volume ROOT as a TARGET, which
  # bypass-traverse does not cover. `--preserve-symlinks-main --preserve-symlinks` skip that walk.
  # Both directions are required: the defect must reproduce WITHOUT the flags in this very run
  # (or the flagged arms prove nothing about the flags), and the table must run WITH them.
  Prop 'realpath-defect-reproduces-without-flags' `
    (((Cell 'ac-noflags' 'node-died-realpath-c-root') -eq 'OK') -and ((OpCount 'ac-noflags') -eq 0)) `
    "no-flag arm must die in Node's realpath walk before any user code: died=$(Cell 'ac-noflags' 'node-died-realpath-c-root') ops=$(OpCount 'ac-noflags') launch=$(LaunchOf 'ac-noflags')"
  Prop 'preserve-symlinks-flags-unblock-the-child' (((OpCount 'ac-leaf-grants') -gt 10) -and
    ((Cell 'ac-leaf-grants' 'node-died-realpath-c-root') -eq 'ERR')) `
    "the SAME grants WITH the flags must reach the operations table: ops=$(OpCount 'ac-leaf-grants') died-in-realpath=$(Cell 'ac-leaf-grants' 'node-died-realpath-c-root')"

  # ── CONTROL: the grant physically REACHED the decisive target. The ace is written on an
  # ancestor as an inheritable one and relies on propagation into the already-existing deep file.
  # If it never landed, the deep read fails for a reason with nothing to do with traverse — a
  # false negative indistinguishable from "AppContainer is dead". Required in BOTH directions:
  # present where granted, ABSENT in the ace-absent arm.
  Prop 'harness-grant-reached-decisive-target' `
    (((Cell 'ac-leaf-grants' 'dacl-grants-ac-sid') -eq 'OK') -and
     ((Cell 'ac-root-grant' 'dacl-grants-ac-sid') -eq 'OK') -and
     ((Cell 'ac-data-ungranted' 'dacl-grants-ac-sid') -eq 'ERR')) `
    "the deep file's own DACL must carry the AC sid where granted and NOT where withheld: leaf=$(Detail 'ac-leaf-grants' 'dacl-grants-ac-sid') root=$(Detail 'ac-root-grant' 'dacl-grants-ac-sid') withheld=$(Detail 'ac-data-ungranted' 'dacl-grants-ac-sid')"

  # ── THE DECISIVE CELLS. Each is a deep open THROUGH un-ACE'd C:\ and C:\Users.
  Prop 'bypass-traverse-deep-read-root-grant' ((Cell 'ac-root-grant' 'read-deep-granted') -eq 'OK') `
    "deep read with ONE grant at a project root under %USERPROFILE%: $(Cell 'ac-root-grant' 'read-deep-granted') $(Detail 'ac-root-grant' 'read-deep-granted')"
  Prop 'bypass-traverse-deep-read-leaf-grants' ((Cell 'ac-leaf-grants' 'read-deep-granted') -eq 'OK') `
    "deep read with leaf-only grants, test root ALSO ungranted: $(Cell 'ac-leaf-grants' 'read-deep-granted') $(Detail 'ac-leaf-grants' 'read-deep-granted')"
  Prop 'bypass-traverse-require-deep' ((Cell 'ac-leaf-grants' 'require-deep-granted') -eq 'OK') `
    "require() realpaths every prefix as a TARGET, not as an intermediate component: $(Cell 'ac-leaf-grants' 'require-deep-granted')"
  # A launch-time cwd may be opened in the PARENT's context, so this arm is weaker than it looks;
  # `bypass-traverse-child-chdir-deep` is the unambiguous child-context open.
  Prop 'bypass-traverse-launch-cwd-deep' ((Lines 'ac-cwd-deep') -gt 0) `
    "launch with cwd five components below the last grant: launch=$(LaunchOf 'ac-cwd-deep')"
  Prop 'bypass-traverse-child-chdir-deep' (((Cell 'ac-leaf-grants' 'chdir-to-deepdir') -eq 'OK') -and
    ((Cell 'ac-leaf-grants' 'read-relative-after-chdir') -eq 'OK')) `
    "the CONFINED process itself opens the deep dir: chdir=$(Cell 'ac-leaf-grants' 'chdir-to-deepdir') relread=$(Cell 'ac-leaf-grants' 'read-relative-after-chdir')"
  Prop 'bypass-traverse-node-entry-deep' ((Cell 'ac-entry-deep' 'entry-as-deep-file') -eq 'OK') `
    "node <deep file> as entry — resolveMainPath ran before any user code: $(Cell 'ac-entry-deep' 'entry-as-deep-file')"

  # ── MANDATORY CONTROLS ON THE GRANT. Without a FAIL here a pass above is unfalsifiable.
  Prop 'control-ace-absent-denies-deep-read' ((Cell 'ac-data-ungranted' 'read-deep-granted') -eq 'ERR') `
    "the same launch with the data grant WITHHELD must fail: $(Cell 'ac-data-ungranted' 'read-deep-granted') $(Detail 'ac-data-ungranted' 'read-deep-granted')"
  Prop 'control-ungranted-sibling-under-profile-denies' `
    (((Cell 'ac-leaf-grants' 'read-ungranted-sibling-under-profile') -eq 'ERR') -and
     ((Cell 'ac-root-grant' 'read-ungranted-sibling-under-profile') -eq 'ERR')) `
    "a sibling under %USERPROFILE% that never got a grant must fail, or the grant scopes nothing: leaf=$(Cell 'ac-leaf-grants' 'read-ungranted-sibling-under-profile') root=$(Cell 'ac-root-grant' 'read-ungranted-sibling-under-profile')"
  Prop 'control-ungranted-sibling-inside-root-denies' `
    ((Cell 'ac-leaf-grants' 'read-ungranted-sibling-inside-root') -eq 'ERR') `
    "leaf-grant arm: a sibling inside the test root but outside both grants must fail: $(Cell 'ac-leaf-grants' 'read-ungranted-sibling-inside-root')"

  # ── EGRESS. Both halves must hold in the SAME token or the design does not exist.
  $egressDenied = $true
  $egressWhy = @()
  foreach ($a in @('ac-root-grant', 'ac-leaf-grants', 'ac-cwd-deep')) {
    $egressWhy += "$a/ip=$(Cell $a 'net-connect-ip')"
    if ((Cell $a 'net-connect-ip') -ne 'ERR') { $egressDenied = $false }
    if ((Cell $a 'net-connect-name') -ne 'ERR') { $egressDenied = $false }
  }
  Prop 'egress-denied-with-zero-capabilities' $egressDenied `
    "internetClient withheld => connect must fail in every AppContainer arm while the plain arm connects ($($egressWhy -join ' '))"

  return $script:fails
}
