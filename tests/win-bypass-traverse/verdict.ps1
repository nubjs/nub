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
  'open-deep-granted', 'native-deep-granted', 'native-deep-granted-held',
  'native-deepdir-granted', 'native-runtime-granted', 'native-system32-hosts',
  'native-longpath-granted', 'native-c-root',
  'realpath-shim-installed', 'isolated-layout-version', 'isolated-layout-resolved-main',
  'readdir-deepdir', 'write-into-granted', 'chdir-to-deepdir', 'cwd-after-chdir',
  'read-relative-after-chdir', 'lstat-c-root', 'realpath-c-root',
  'stat-c-root', 'readdir-c-root', 'stat-c-users', 'readdir-c-users',
  'stat-userprofile', 'readdir-userprofile', 'findup-walk', 'read-ungranted-sibling-inside-root',
  'read-ungranted-sibling-under-profile',
  'read-ssh-private-key', 'readdir-dot-ssh', 'stat-ssh-private-key', 'read-npmrc',
  'read-system32-hosts', 'readdir-system32',
  'dns-lookup-registry', 'net-connect-ip', 'net-connect-name', 'net-connect-loopback',
  'spawn-piped-whoami')
$script:ProbeArms = @('plain', 'plain-noflags', 'ac-root-grant', 'ac-leaf-grants',
  'ac-data-ungranted', 'ac-cwd-deep', 'ac-noflags', 'ac-derive-only', 'ac-shim')
# `ac-noflags` and `ac-entry-deep` are excluded from the gate check below: the first emits no op
# lines at all by design (it dies in Node's bootstrap), and the second reports the root cell under
# a different op name.
$script:ProbeAcArms = @('ac-root-grant', 'ac-leaf-grants', 'ac-data-ungranted', 'ac-cwd-deep',
  'ac-entry-deep', 'ac-shim')
# `ac-shim` is in here deliberately: a repair that quietly widened the jail would otherwise show
# up as a green repair. Every secret cell must still be refused in the arm carrying the shim.
$script:SecretArms = @('ac-root-grant', 'ac-leaf-grants', 'ac-cwd-deep', 'ac-shim')
$script:EgressArms = @('ac-root-grant', 'ac-leaf-grants', 'ac-cwd-deep', 'ac-shim')

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
  foreach ($arm in @('ac-entry-deep', 'ac-shim-entry-deep')) {
    W "  entry-point arm ($arm):"
    foreach ($op in @('entry-as-deep-file', 'entry-cwd', 'entry-realpath', 'entry-require-bare',
                      'entry-read-c-root')) {
      W ("    {0,-24} {1}  {2}" -f $op, (Cell $arm $op), (Detail $arm $op))
    }
  }

  # The `.native` cells carry their attribution in the DETAIL (errno/syscall), not in OK/ERR, so
  # they are printed in full rather than collapsed into the table above.
  W ''
  W '  fs.realpathSync.native, verbatim per arm:'
  foreach ($a in @('plain', 'ac-leaf-grants', 'ac-shim')) {
    foreach ($op in @('open-deep-granted', 'native-deep-granted', 'native-deep-granted-held',
                      'native-deepdir-granted', 'native-system32-hosts', 'native-longpath-granted')) {
      W ("    {0,-16} {1,-26} {2,-4} {3}" -f $a, $op, (Cell $a $op), (Detail $a $op))
    }
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

  # ── THE PROPERTY THIS WHOLE EXERCISE EXISTS TO GET. A confined lifecycle script must not be
  # able to read `%USERPROFILE%\.ssh\id_rsa` or `.npmrc`. Landlock and Seatbelt deliver this;
  # Windows's current restricted-token design does NOT, because that token keeps the user's sid so
  # every DACL granting the user still applies. Asserted on CONTENT and on metadata, and in every
  # granting arm — including the one whose grant sits at a project root directly under the profile,
  # which is the shape most likely to over-reach.
  $secretsDenied = $true
  $secretsWhy = @()
  foreach ($a in $script:SecretArms) {
    foreach ($o in @('read-ssh-private-key', 'read-npmrc', 'readdir-dot-ssh', 'stat-ssh-private-key')) {
      $c = Cell $a $o
      if ($c -ne 'ERR') { $secretsDenied = $false; $secretsWhy += "$a/$o=$c" }
    }
  }
  Prop 'secrets-under-profile-are-denied' $secretsDenied `
    "~/.ssh/id_rsa and ~/.npmrc must be unreachable — content, listing and metadata — in every granting arm$(if ($secretsWhy.Count) { ' VIOLATIONS: ' + ($secretsWhy -join ' ') } else { ' (all ERR)' })"
  Prop 'secrets-baseline-plain-can-read-them' (((Cell 'plain' 'read-ssh-private-key') -eq 'OK') -and
    ((Cell 'plain' 'read-npmrc') -eq 'OK')) `
    "the differential's allow half: an UNCONFINED child must read both, or the denial above is about the files not existing rather than about the jail: ssh=$(Cell 'plain' 'read-ssh-private-key') npmrc=$(Cell 'plain' 'read-npmrc')"

  # ── EGRESS. Both halves must hold in the SAME token or the design does not exist.
  $egressDenied = $true
  $egressWhy = @()
  foreach ($a in $script:EgressArms) {
    $egressWhy += "$a/ip=$(Cell $a 'net-connect-ip')"
    if ((Cell $a 'net-connect-ip') -ne 'ERR') { $egressDenied = $false }
    if ((Cell $a 'net-connect-name') -ne 'ERR') { $egressDenied = $false }
  }
  Prop 'egress-denied-with-zero-capabilities' $egressDenied `
    "internetClient withheld => connect must fail in every AppContainer arm while the plain arm connects ($($egressWhy -join ' '))"

  # ── TASK 1: IS `fs.realpathSync.native` REALLY REFUSED, AND BY WHICH CALL? ──
  # A prior lane recorded it refused and closed the search. Re-measured here on LOCAL NTFS with the
  # attribution the earlier cell lacked. `open-deep-granted` is the bound: a `.native` refusal on a
  # leaf that cannot even be opened says nothing about realpath, and the plain arm's `.native`
  # working is the allow half without which a denial is not a differential.
  Prop 'native-realpath-battery-is-attributable' `
    (((Cell 'ac-leaf-grants' 'open-deep-granted') -eq 'OK') -and
     ((Cell 'plain' 'native-deep-granted') -eq 'OK')) `
    "the confined arm must OPEN the leaf and the unconfined arm must resolve it natively, or the cells below are unreadable: confined-open=$(Cell 'ac-leaf-grants' 'open-deep-granted') plain-native=$(Cell 'plain' 'native-deep-granted') $(Detail 'plain' 'native-deep-granted')"
  # POLARITY IS DELIBERATE: the STANDING expectation is "refused", so this PASSes on the known
  # answer and FAILs loudly the day a Windows build starts granting
  # `GetFinalPathNameByHandleW` under an AppContainer — which would make the whole repair below
  # unnecessary and is exactly the flip nobody should discover by accident.
  Prop 'native-realpath-refused-under-appcontainer' `
    ((Cell 'ac-leaf-grants' 'native-deep-granted') -eq 'ERR') `
    "if this FAILS, .native now works in the jail and pointing fs.realpathSync at it is a one-line fix — read the verbatim block above. deep=$(Cell 'ac-leaf-grants' 'native-deep-granted') $(Detail 'ac-leaf-grants' 'native-deep-granted') / held=$(Cell 'ac-leaf-grants' 'native-deep-granted-held') $(Detail 'ac-leaf-grants' 'native-deep-granted-held') / system32=$(Cell 'ac-leaf-grants' 'native-system32-hosts') $(Detail 'ac-leaf-grants' 'native-system32-hosts') / longpath=$(Cell 'ac-leaf-grants' 'native-longpath-granted') $(Detail 'ac-leaf-grants' 'native-longpath-granted')"

  # ── TASK 2: THE PORTED lstat-TOLERANCE REPAIR ──
  # Skipped entirely when the arm is absent so a checkout without the shim reports MISSING rather
  # than a spurious FAIL — the workflow requires these names, so absence is still loud.
  if ($cells.ContainsKey('ac-shim')) {
    Prop 'shim-preload-arrived-in-jail' ((Cell 'ac-shim' 'realpath-shim-installed') -eq 'OK') `
      "the data: --import must EVALUATE inside the jail, or every cell below is about a preload that never ran: $(Cell 'ac-shim' 'realpath-shim-installed') $(Detail 'ac-shim' 'realpath-shim-installed')"
    # The repair, against the arm that has neither flags nor shim. Both directions in one run.
    Prop 'shim-repairs-resolution-in-jail' `
      (((Cell 'ac-shim' 'require-deep-granted') -eq 'OK') -and
       ((Cell 'ac-shim' 'realpath-deep-granted') -eq 'OK') -and
       ((Cell 'ac-noflags' 'node-died-realpath-c-root') -eq 'OK')) `
      "with NO tree-wide flag, the shim must make require() and realpathSync work where the same grants without it die: require=$(Cell 'ac-shim' 'require-deep-granted') realpath=$(Cell 'ac-shim' 'realpath-deep-granted') noflag-arm-died=$(Cell 'ac-noflags' 'node-died-realpath-c-root')"
    # THE CONTROL THAT DECIDES WHETHER THIS MAY SHIP. Three cells, one fixture: the unconfined
    # unflagged truth, the disqualified flag's silent wrong answer, and the shim's answer. The
    # shim is only acceptable if it matches the first and differs from the second.
    $isoTruth = Detail 'plain-noflags' 'isolated-layout-version'
    $isoFlag = Detail 'ac-leaf-grants' 'isolated-layout-version'
    $isoShim = Detail 'ac-shim' 'isolated-layout-version'
    Prop 'shim-preserves-isolated-layout-resolution' `
      (($isoShim -match 'bar@2\.0\.0') -and ($isoTruth -match 'bar@2\.0\.0') -and
       ($isoFlag -match 'bar@1\.0\.0')) `
      "truth(plain,no flags)=$isoTruth / --preserve-symlinks(ac-leaf-grants)=$isoFlag / shim(ac-shim)=$isoShim — the shim must equal the truth AND the flag arm must show the wrong version, or this control proves nothing"
    Prop 'shim-does-not-widen-the-jail' `
      (((Cell 'ac-shim' 'read-ungranted-sibling-under-profile') -eq 'ERR') -and
       ((Cell 'ac-shim' 'read-ssh-private-key') -eq 'ERR') -and
       ((Cell 'ac-shim' 'read-npmrc') -eq 'ERR') -and
       ((Cell 'ac-shim' 'stat-c-root') -eq 'ERR')) `
      "a realpath repair must not turn into a read grant: sibling=$(Cell 'ac-shim' 'read-ungranted-sibling-under-profile') ssh=$(Cell 'ac-shim' 'read-ssh-private-key') npmrc=$(Cell 'ac-shim' 'read-npmrc') c-root=$(Cell 'ac-shim' 'stat-c-root')"
    Prop 'shim-entry-point-runs-on-main-flag-alone' `
      ((Cell 'ac-shim-entry-deep' 'entry-as-deep-file') -eq 'OK') `
      "--import runs after resolveMainPath, so the entry point rides --preserve-symlinks-main; a deep file as entry must still start: $(Cell 'ac-shim-entry-deep' 'entry-as-deep-file')"
  }

  return $script:fails
}
