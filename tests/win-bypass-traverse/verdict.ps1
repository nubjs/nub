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
  'read-ungranted-sibling-under-profile',
  'read-ssh-private-key', 'readdir-dot-ssh', 'stat-ssh-private-key', 'read-npmrc',
  'read-system32-hosts', 'readdir-system32',
  'dns-lookup-registry', 'net-connect-ip', 'net-connect-name', 'net-connect-loopback',
  'spawn-piped-whoami')
$script:ProbeArms = @('plain', 'ac-root-grant', 'ac-leaf-grants', 'ac-data-ungranted', 'ac-cwd-deep',
  'ac-noflags', 'ac-derive-only')
# `ac-noflags` and `ac-entry-deep` are excluded from the gate check below: the first emits no op
# lines at all by design (it dies in Node's bootstrap), and the second reports the root cell under
# a different op name.
$script:ProbeAcArms = @('ac-root-grant', 'ac-leaf-grants', 'ac-data-ungranted', 'ac-cwd-deep',
  'ac-entry-deep')
$script:SecretArms = @('ac-root-grant', 'ac-leaf-grants', 'ac-cwd-deep')

# The object table is separate from the fs one because it has different arms and a different
# question. `spawn-piped` is LAST on purpose: in a hanging arm it is the only cell reported as
# MISSING-OP, and that absence is the measurement.
$script:ObjOps = @(
  'null-dacl-grants-ac-sid', 'npfs-dacl-grants-ac-sid',
  'nul-open-read', 'nul-open-write',
  'pipe-listen-global', 'pipe-listen-local',
  'spawn-inherit', 'spawn-ignore', 'spawn-fdfile', 'spawn-piped')
$script:ObjArms = @('obj-plain', 'obj-ac-baseline', 'obj-ac-nulfix', 'obj-ac-npfsfix',
  'obj-ac-baseline-again')

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
  foreach ($a in @('ac-root-grant', 'ac-leaf-grants', 'ac-cwd-deep')) {
    $egressWhy += "$a/ip=$(Cell $a 'net-connect-ip')"
    if ((Cell $a 'net-connect-ip') -ne 'ERR') { $egressDenied = $false }
    if ((Cell $a 'net-connect-name') -ne 'ERR') { $egressDenied = $false }
  }
  Prop 'egress-denied-with-zero-capabilities' $egressDenied `
    "internetClient withheld => connect must fail in every AppContainer arm while the plain arm connects ($($egressWhy -join ' '))"

  Invoke-ObjectVerdict -Cells $cells

  return $script:fails
}

# ── THE OBJECT VERDICT: the NUL device, the pipe namespaces, and the piped-spawn hang ──────────
#
# Kept in its own function for the same reason the whole file exists: `selftest.ps1` drives it with
# synthetic worlds in both directions, so a verdict that cannot tell "the repair fixed it" from "the
# repair was never applied" fails the selftest instead of shipping a wrong conclusion.
#
# THE SHAPE OF THE ANSWER THIS ASSERTS. The piped-spawn hang and the NUL denial are DIFFERENT
# defects that a single `ERROR_ACCESS_DENIED` story conflates. libuv opens `NUL` only in the
# `UV_IGNORE` branch of `uv__stdio_create` (`deps/uv/src/win/process-stdio.c`), so `stdio:'ignore'`
# touches the device and `stdio:'pipe'` does not: the piped path goes to `uv__create_stdio_pipe_pair`
# and spins in `uv__pipe_server`. So a `\Device\Null` repair is predicted to fix `spawn-ignore` and
# NOT `spawn-piped`, and the two are asserted separately rather than as one "the repair works" cell.
function Invoke-ObjectVerdict {
  param([hashtable]$Cells)
  $cells = $Cells

  W ''
  W '== object table (NUL device, pipe namespaces, stdio shapes) =='
  W ("  {0,-28} {1}" -f 'op', (($script:ObjArms | ForEach-Object { "{0,-22}" -f $_ }) -join ''))
  foreach ($op in $script:ObjOps) {
    W ("  {0,-28} {1}" -f $op,
      (($script:ObjArms | ForEach-Object { "{0,-22}" -f (Cell $_ $op) }) -join ''))
  }
  W '  launch status per arm:'
  foreach ($a in $script:ObjArms) { W ("    {0,-24} {1}" -f $a, (LaunchOf $a)) }

  W ''
  W '== object verdict =='

  # ── CONTROL. The unconfined arm must pass EVERY cell, including the piped spawn. A confined
  # table of failures means nothing without it.
  $plainOk = $true
  $plainWhy = @()
  foreach ($o in @('nul-open-read', 'nul-open-write', 'pipe-listen-global', 'pipe-listen-local',
                   'spawn-inherit', 'spawn-ignore', 'spawn-fdfile', 'spawn-piped')) {
    $c = Cell 'obj-plain' $o
    $plainWhy += "$o=$c"
    if ($c -ne 'OK') { $plainOk = $false }
  }
  Prop 'obj-baseline-unconfined-passes-everything' $plainOk `
    "the unconfined control: every object cell must succeed, or a confined failure is the harness ($($plainWhy -join ' '))"

  # ── CONTROL. The confined arms really are confined, and the repairs really landed.
  Prop 'obj-confined-arms-launched' (((Lines 'obj-ac-baseline') -gt 0) -and
    ((Lines 'obj-ac-nulfix') -gt 0) -and ((Lines 'obj-ac-npfsfix') -gt 0)) `
    "each confined object arm must produce a log: baseline=$(Lines 'obj-ac-baseline') nulfix=$(Lines 'obj-ac-nulfix') npfsfix=$(Lines 'obj-ac-npfsfix')"
  Prop 'obj-device-repair-reached-the-object' `
    (((Cell 'obj-ac-nulfix' 'null-dacl-grants-ac-sid') -eq 'OK') -and
     ((Cell 'obj-ac-baseline' 'null-dacl-grants-ac-sid') -eq 'ERR') -and
     ((Cell 'obj-ac-npfsfix' 'npfs-dacl-grants-ac-sid') -eq 'OK') -and
     ((Cell 'obj-ac-baseline' 'npfs-dacl-grants-ac-sid') -eq 'ERR')) `
    "read back from the device's own DACL: the ace must be PRESENT in the repaired arm and ABSENT in the baseline, or the differential varies something other than the repair (nulfix-null=$(Detail 'obj-ac-nulfix' 'null-dacl-grants-ac-sid') baseline-null=$(Detail 'obj-ac-baseline' 'null-dacl-grants-ac-sid') npfsfix-npfs=$(Detail 'obj-ac-npfsfix' 'npfs-dacl-grants-ac-sid') baseline-npfs=$(Detail 'obj-ac-baseline' 'npfs-dacl-grants-ac-sid'))"

  # ── THE AS-SHIPPED BLOCKERS, restated as first-class cells of THIS run rather than recalled.
  Prop 'obj-nul-device-refused-as-shipped' `
    (((Cell 'obj-ac-baseline' 'nul-open-read') -eq 'ERR') -and
     ((Cell 'obj-ac-baseline' 'nul-open-write') -eq 'ERR')) `
    "the candidate's premise, measured: read=$(Cell 'obj-ac-baseline' 'nul-open-read') write=$(Cell 'obj-ac-baseline' 'nul-open-write')"
  Prop 'obj-global-pipe-namespace-refused-local-permitted' `
    (((Cell 'obj-ac-baseline' 'pipe-listen-global') -eq 'ERR') -and
     ((Cell 'obj-ac-baseline' 'pipe-listen-local') -eq 'OK')) `
    "the fallback theory, measured through libuv's own CreateNamedPipeW: global=$(Cell 'obj-ac-baseline' 'pipe-listen-global') LOCAL=$(Cell 'obj-ac-baseline' 'pipe-listen-local')"
  Prop 'obj-piped-spawn-hangs-as-shipped' ((Cell 'obj-ac-baseline' 'spawn-piped') -eq 'MISSING-OP') `
    "the blocker: the op never returns, so its line is absent while every earlier cell reported. launch=$(LaunchOf 'obj-ac-baseline')"

  # ── THE DECISIVE CELLS. Which repair fixes which shape.
  Prop 'nul-repair-fixes-the-nul-device' `
    (((Cell 'obj-ac-nulfix' 'nul-open-read') -eq 'OK') -and
     ((Cell 'obj-ac-nulfix' 'nul-open-write') -eq 'OK')) `
    "does granting the AC sid on \Device\Null re-open it: read=$(Cell 'obj-ac-nulfix' 'nul-open-read') write=$(Cell 'obj-ac-nulfix' 'nul-open-write')"
  Prop 'nul-repair-fixes-stdio-ignore' ((Cell 'obj-ac-nulfix' 'spawn-ignore') -eq 'OK') `
    "stdio:'ignore' is the shape that opens NUL, so it is the spawn the repair should fix: baseline=$(Cell 'obj-ac-baseline' 'spawn-ignore') repaired=$(Cell 'obj-ac-nulfix' 'spawn-ignore')"
  Prop 'nul-repair-does-not-fix-the-piped-hang' ((Cell 'obj-ac-nulfix' 'spawn-piped') -eq 'MISSING-OP') `
    "PREDICTED from libuv source — the piped path never opens NUL, so this repair must NOT be what unhangs it. A PASS here means the candidate is correctly scoped; a FAIL means it also fixed the hang: $(Cell 'obj-ac-nulfix' 'spawn-piped') launch=$(LaunchOf 'obj-ac-nulfix')"
  Prop 'npfs-repair-fixes-the-piped-hang' ((Cell 'obj-ac-npfsfix' 'spawn-piped') -eq 'OK') `
    "THE PRIZE: if the NPFS root's DACL admits the AC sid, libuv's global pipe name works and the spawn returns with no libuv change: $(Cell 'obj-ac-npfsfix' 'spawn-piped') global-listen=$(Cell 'obj-ac-npfsfix' 'pipe-listen-global') launch=$(LaunchOf 'obj-ac-npfsfix')"

  # ── THE MITIGATION FLOOR the repairs have to beat. Measured in the same run so "the repair is
  # unnecessary" and "the repair is insufficient" are distinguishable.
  Prop 'fdfile-mitigation-works-under-the-jail' ((Cell 'obj-ac-baseline' 'spawn-fdfile') -eq 'OK') `
    "file-backed descriptors in place of pipes, the measured floor: $(Cell 'obj-ac-baseline' 'spawn-fdfile') $(Detail 'obj-ac-baseline' 'spawn-fdfile')"
  Prop 'inherit-stdio-works-under-the-jail' ((Cell 'obj-ac-baseline' 'spawn-inherit') -eq 'OK') `
    "liveness: the confined child CAN spawn, so a piped failure is about the stdio shape and not about spawning: $(Cell 'obj-ac-baseline' 'spawn-inherit')"

  # ── THE BASELINE-REPEAT GUARD. §5e's `core-js` false positive is why this exists, and a
  # machine-global device DACL makes it sharper: a repair whose revoke failed would leave the
  # device open and make this arm read as repaired.
  # ── THE RESIDUAL. `fork`'s IPC channel is a pipe in the same global namespace, and no `stdio`
  # option removes it, so the file-descriptor mitigation has a hole. Both halves in one run: the
  # unconfined arm must fork, the confined one is the question.
  W ("    {0,-24} plain={1} confined={2}" -f 'fork-ipc', (Cell 'obj-plain-fork' 'fork-ipc'),
    (Cell 'obj-ac-fork' 'fork-ipc'))
  Prop 'fork-baseline-unconfined-forks' ((Cell 'obj-plain-fork' 'fork-ipc') -eq 'OK') `
    "the allow half: an unconfined child must fork, or a confined failure is the harness: $(Cell 'obj-plain-fork' 'fork-ipc') launch=$(LaunchOf 'obj-plain-fork')"
  Prop 'fork-ipc-hangs-under-the-jail' ((Cell 'obj-ac-fork' 'fork-ipc') -eq 'MISSING-OP') `
    "PREDICTED: fork's IPC channel is a pipe in the refused namespace and no stdio option removes it, so the fd mitigation cannot cover it. A FAIL means fork survives and the residual is smaller than thought: $(Cell 'obj-ac-fork' 'fork-ipc') launch=$(LaunchOf 'obj-ac-fork')"

  # ── THE CANDIDATE FIX. Read bottom-up: each property is a precondition of the next, so a FAIL
  # high in the list explains every FAIL below it and the ones below carry no independent news.
  W ''
  W '    -- the container-private pipe namespace, from userland --'
  foreach ($o in @('pipe-connect-local', 'spawn-wrap-local', 'shim-fork-roundtrip', 'shim-fork-nested')) {
    $armPlain = if ($o -like 'shim-*') { 'obj-plain-shimfork' } else { 'obj-plain-localpipe' }
    $armAc = if ($o -like 'shim-*') { 'obj-ac-shimfork' } else { 'obj-ac-localpipe' }
    W ("    {0,-24} plain={1} confined={2}  {3}" -f $o, (Cell $armPlain $o), (Cell $armAc $o),
      (Detail $armAc $o))
  }

  # CONTROL FIRST. All four cells are ordinary `net`/`child_process` code with nothing
  # Windows-security about them, so an unconfined failure is a harness fault and makes the confined
  # column unreadable — the same discipline as `obj-baseline-unconfined-passes-everything`.
  $lpPlainOk = ((Cell 'obj-plain-localpipe' 'pipe-connect-local') -eq 'OK') -and
    ((Cell 'obj-plain-localpipe' 'spawn-wrap-local') -eq 'OK') -and
    ((Cell 'obj-plain-shimfork' 'shim-fork-roundtrip') -eq 'OK') -and
    ((Cell 'obj-plain-shimfork' 'shim-fork-nested') -eq 'OK')
  Prop 'localpipe-baseline-unconfined-passes-everything' $lpPlainOk `
    "the unconfined control for the candidate fix: connect=$(Cell 'obj-plain-localpipe' 'pipe-connect-local') wrap=$(Cell 'obj-plain-localpipe' 'spawn-wrap-local') shimfork=$(Cell 'obj-plain-shimfork' 'shim-fork-roundtrip') nested=$(Cell 'obj-plain-shimfork' 'shim-fork-nested')"

  # The preload must actually be loaded, or the shim arms measure stock Node under a longer timeout
  # and a hang there would be blamed on the fix. This is the grant-never-landed guard, for a preload.
  Prop 'shim-preload-was-active' `
    (((Cell 'obj-ac-shimfork' 'shim-active') -eq 'OK') -and ((Cell 'obj-plain-shimfork' 'shim-active') -eq 'OK')) `
    "the child reports whether the preload installed itself, so a shim that never loaded cannot read as a shim that did not help: plain=$(Cell 'obj-plain-shimfork' 'shim-active') confined=$(Cell 'obj-ac-shimfork' 'shim-active')"

  Prop 'localpipe-connect-works-under-the-jail' ((Cell 'obj-ac-localpipe' 'pipe-connect-local') -eq 'OK') `
    "creating a LOCAL\ name was already measured; CONNECTING to it duplex is the untested half every emulated channel needs: $(Cell 'obj-ac-localpipe' 'pipe-connect-local') $(Detail 'obj-ac-localpipe' 'pipe-connect-local')"

  Prop 'inherit-stream-stdio-avoids-the-hang' ((Cell 'obj-ac-localpipe' 'spawn-wrap-local') -eq 'OK') `
    "PREDICTED from libuv source: a Stream in the stdio array takes UV_INHERIT_STREAM, which only DuplicateHandle's an existing end and never calls CreateNamedPipe, so the namespace cannot refuse it. A PASS means ordinary piped spawn is repairable in userland WITH streaming semantics, not just file-backed capture: $(Cell 'obj-ac-localpipe' 'spawn-wrap-local') $(Detail 'obj-ac-localpipe' 'spawn-wrap-local')"

  Prop 'shimmed-fork-converses-under-the-jail' ((Cell 'obj-ac-shimfork' 'shim-fork-roundtrip') -eq 'OK') `
    "THE PRIZE, and the residual the fd mitigation cannot reach: unmodified fork + process.send under the jail, with the IPC channel emulated over a container-private pipe. As shipped obj-ac-fork/fork-ipc=$(Cell 'obj-ac-fork' 'fork-ipc'); shimmed: $(Cell 'obj-ac-shimfork' 'shim-fork-roundtrip') $(Detail 'obj-ac-shimfork' 'shim-fork-roundtrip')"

  Prop 'shimmed-fork-survives-nesting' ((Cell 'obj-ac-shimfork' 'shim-fork-nested') -eq 'OK') `
    "a fix that works one level down and not two is not a fix — node-gyp and jest both fork from forked children. The preload must reach the grandchild and a second private pipe must be creatable from inside an already-confined descendant: $(Cell 'obj-ac-shimfork' 'shim-fork-nested') $(Detail 'obj-ac-shimfork' 'shim-fork-nested')"

  Prop 'baseline-repeats-after-every-repair' `
    (((Cell 'obj-ac-baseline-again' 'nul-open-read') -eq (Cell 'obj-ac-baseline' 'nul-open-read')) -and
     ((Cell 'obj-ac-baseline-again' 'pipe-listen-global') -eq (Cell 'obj-ac-baseline' 'pipe-listen-global')) -and
     ((Cell 'obj-ac-baseline-again' 'null-dacl-grants-ac-sid') -eq 'ERR') -and
     ((Cell 'obj-ac-baseline-again' 'npfs-dacl-grants-ac-sid') -eq 'ERR')) `
    "re-run LAST: the as-shipped arm must reproduce itself, or a repair leaked past its revoke and every cell after it is suspect (nul: $(Cell 'obj-ac-baseline' 'nul-open-read') -> $(Cell 'obj-ac-baseline-again' 'nul-open-read'); global pipe: $(Cell 'obj-ac-baseline' 'pipe-listen-global') -> $(Cell 'obj-ac-baseline-again' 'pipe-listen-global'))"
}
