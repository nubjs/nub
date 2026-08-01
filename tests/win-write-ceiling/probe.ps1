# THE QUESTION (maintainer, 2026-07-31): what is the greatest level of DISK permission a LowBox
# token can be granted WITHOUT ELEVATION, and is there a useful "maximum grantable" tier between
# today's narrow leaf grants and declining the token altogether?
#
# WHY IT MATTERS. The shipping full-disk tier renders on Windows by NOT taking the LowBox token,
# because egress denial is an AppContainer CAPABILITY (`internetClient` withheld) — so declining
# the token declines the net axis with it, and a full-disk package reaches the network whether or
# not the catalog admits it to. A maximum-grantable tier that KEEPS the token would keep the two
# axes separable. Whether one exists is a measurement, not an argument.
#
# ─────────────────────────── WHAT THIS PROBE MUST NOT GET WRONG ───────────────────────────
#
# 1. THE RUNNER IS ELEVATED AND A USER IS NOT. "Can nub install this ACE?" answered from an
#    elevated token is a fact about CI. Every DACL-write row is therefore taken TWICE — once as
#    the runner, once under an impersonated de-elevated restricted token — and the elevated row
#    exists only as the control that makes the de-elevated denial attributable to privilege.
# 2. A UNIFORMLY-DENIED MATRIX IS USUALLY A BROKEN HARNESS. Every arm set carries a positive
#    control that MUST succeed (`plain`, and a granted leaf inside each confined arm). A table of
#    denials with a dead positive control is discarded, not reported.
# 3. AN ARM WITHOUT AN ENGAGEMENT ASSERTION IS NOT AN ARM. Each confined arm proves it is really
#    confined (`C:\` list refused) and that its ACEs really landed (DACL read-back), because a
#    propagation slip and a kernel denial are otherwise indistinguishable.
# 4. THE CHILD MUST REACH USER CODE. An unflagged confined `node` dies in `resolveMainPath`'s
#    realpath before running a line (MECHANISM-FACTS §5h), which would render every op as a
#    denial. `--preserve-symlinks-main` fixes the entry point and `child.js` imports only
#    `node:`-prefixed builtins, which never enter `_findPath`, so no realpath shim is needed here.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $here 'launcher.ps1')

$FAILURES = 0
function Check($name, $cond, $detail) {
  if ($cond) { Write-Host "CONTROL $name = PASS $detail" }
  else { Write-Host "CONTROL $name = FAIL $detail"; $script:FAILURES++ }
}

# Generic rights, as production writes them (`windows.rs` leaf grants).
$GR = 0x80000000; $GW = 0x40000000; $GE = 0x20000000; $DEL = 0x00010000
$READ_MASK  = $GR -bor $GE
$WRITE_MASK = $GR -bor $GW -bor $GE -bor $DEL
$GRANT = 1; $REVOKE = 4
# The well-known `internetClient` capability. Present in one arm ONLY, as the control proving the
# egress denial in the others is the withheld capability rather than an artefact of confinement.
$INTERNET_CLIENT = 'S-1-15-3-1'

$tag = 'wc' + (Get-Random -Maximum 999999)
$proj = Join-Path $env:USERPROFILE "wcproj-$tag"
$profPre = Join-Path $env:USERPROFILE "wcpre-$tag"

Write-Host '=============================== PROVENANCE ==============================='
Write-Host "os=$([System.Environment]::OSVersion.VersionString) arch=$env:PROCESSOR_ARCHITECTURE"
Write-Host "user=$env:USERNAME profile=$env:USERPROFILE"
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
Write-Host "elevated=$($pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Write-Host "session=$((Get-Process -Id $PID).SessionId)"
$nodeExe = (Get-Command node).Source
Write-Host "node=$nodeExe $(& $nodeExe -v)"

# ─────────────────────────────────── FIXTURES ───────────────────────────────────
# Everything a confined child touches is created BEFORE any ACE is written, so the "existing file"
# cells are genuinely pre-existing — which is the distinction the whole cost argument rests on.

New-Item -ItemType Directory -Force -Path (Join-Path $proj 'granted\deep') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $proj 'ungranted') | Out-Null
Set-Content -Path (Join-Path $proj 'granted\deep\f.txt') -Value 'granted-preexisting'
Set-Content -Path (Join-Path $proj 'ungranted\f.txt') -Value 'ungranted-preexisting'
New-Item -ItemType Directory -Force -Path (Join-Path $profPre 'deep') | Out-Null
Set-Content -Path (Join-Path $profPre 'deep\f.txt') -Value 'profile-preexisting'
# The canary. Whether a confined child can read a secret sitting in the profile is the property
# the jail exists for, and the `plain` arm reading it is what proves the file is really there.
$secret = Join-Path $env:USERPROFILE ".wcsecret-$tag"
Set-Content -Path $secret -Value 'SECRET-CANARY-DO-NOT-LEAK'

# ═══════════════════════ SECTION 1 — DACL-WRITE REACH, de-elevated ═══════════════════════
# Where can nub install a grant at all? An ACE it cannot write is a tier it cannot build, so this
# section bounds every answer below it. A NON-inheritable ACE is used on the shared roots: it
# answers the WRITE_DAC question exactly while reaching no child object, so nothing is widened
# even momentarily on a root the machine shares.

Write-Host ''
Write-Host '========== SECTION 1 — CAN AN UNELEVATED USER WRITE A DACL HERE? =========='
Write-Host 'root                                   | elevated | de-elevated'

$roots = [ordered]@{
  'C:\'                  = 'C:\'
  'C:\Users'             = 'C:\Users'
  'C:\Program Files'     = 'C:\Program Files'
  'C:\ProgramData'       = 'C:\ProgramData'
  'C:\Windows'           = 'C:\Windows'
  '%USERPROFILE%'        = $env:USERPROFILE
  'project'              = $proj
  'project\granted\deep' = (Join-Path $proj 'granted\deep')
}

# A throwaway profile whose sid is the trustee for every probe ACE in this section.
$probeName = "nubwc-probe-$tag"
$probeSid = [Wc]::CreateProfile($probeName)
Write-Host "probe-sid=$probeSid"
Check 'probe-profile-created' ($probeSid -notlike 'ERR*') $probeSid

$daclReach = @{}
foreach ($k in $roots.Keys) {
  $p = $roots[$k]
  $elev = [Wc]::SetAce($p, $probeSid, $READ_MASK, $GRANT, $false)
  if ($elev -eq 'OK') { [void][Wc]::SetAce($p, $probeSid, 0, $REVOKE, $false) }

  $imp = [Wc]::BeginDeelevated()
  if ($imp -ne 'OK') { $deelev = "IMPERSONATE-$imp" }
  else {
    $deelev = [Wc]::SetAce($p, $probeSid, $READ_MASK, $GRANT, $false)
    if ($deelev -eq 'OK') { [void][Wc]::SetAce($p, $probeSid, 0, $REVOKE, $false) }
  }
  [void][Wc]::EndDeelevated()

  $daclReach[$k] = $deelev
  Write-Host ("{0,-38} | {1,-8} | {2}" -f $k, $elev, $deelev)
}

# The two controls that make this table mean anything. Elevated-on-`C:\` succeeding proves the
# runner really holds admin authority, so a de-elevated refusal is privilege and not a bad path;
# de-elevated-on-the-project succeeding proves impersonation did not simply break every call.
Check 'deelev-can-ace-own-project' ($daclReach['project'] -eq 'OK') "= $($daclReach['project'])"
Check 'deelev-refused-on-C-root' ($daclReach['C:\'] -ne 'OK') "= $($daclReach['C:\'])"

# ═══════════════════ SECTION 2 — PROPAGATION: what does a broad grant COST? ═══════════════════
# The claim under test is `windows.rs`'s: "Windows inheritance is STATIC, copied into a child's
# DACL when the child is created, so an inheritable ACE written without propagation grants nothing
# to a file that already exists." If true, a whole-disk grant is O(existing entries) per launch,
# which is the difference between a viable tier and a 25-minute step.

Write-Host ''
Write-Host '========== SECTION 2 — PROPAGATION AND COST =========='

function New-Tree($root, $dirs, $filesPerDir) {
  New-Item -ItemType Directory -Force -Path $root | Out-Null
  for ($i = 0; $i -lt $dirs; $i++) {
    $d = Join-Path $root "d$i"
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    for ($j = 0; $j -lt $filesPerDir; $j++) { Set-Content -Path (Join-Path $d "f$j.txt") -Value 'x' }
  }
  return (Get-ChildItem -Path $root -Recurse -Force | Measure-Object).Count
}

# (a) NON-propagating: an inheritable ACE at the root written this-object-only. If the deep file
#     comes back with no rights, inheritance really is static and there is no cheap broad grant.
$treeA = Join-Path $proj "treeA"
$nA = New-Tree $treeA 20 10
$deepA = Join-Path $treeA 'd19\f9.txt'
$before = [Wc]::ReadAceMask($deepA, $probeSid)
$swA = [Diagnostics.Stopwatch]::StartNew()
$rA = [Wc]::SetAce($treeA, $probeSid, $WRITE_MASK, $GRANT, $false)
$swA.Stop()
$afterA = [Wc]::ReadAceMask($deepA, $probeSid)
Write-Host "nonpropagating: entries=$nA set=$rA ms=$($swA.ElapsedMilliseconds) deep-mask-before=$before deep-mask-after=$afterA"

# (b) Propagating: the same ACE written INHERITABLE. Production's `set_ace` uses this form.
$treeB = Join-Path $proj "treeB"
$nB = New-Tree $treeB 20 10
$deepB = Join-Path $treeB 'd19\f9.txt'
$swB = [Diagnostics.Stopwatch]::StartNew()
$rB = [Wc]::SetAce($treeB, $probeSid, $WRITE_MASK, $GRANT, $true)
$swB.Stop()
$afterB = [Wc]::ReadAceMask($deepB, $probeSid)
$swBr = [Diagnostics.Stopwatch]::StartNew()
[void][Wc]::SetAce($treeB, $probeSid, 0, $REVOKE, $true)
$swBr.Stop()
Write-Host "propagating:    entries=$nB set=$rB grant-ms=$($swB.ElapsedMilliseconds) revoke-ms=$($swBr.ElapsedMilliseconds) deep-mask-after=$afterB"

Check 'static-inheritance-holds' (($afterA -eq '0x00000000') -and ($afterB -ne '0x00000000')) `
  "nonprop=$afterA prop=$afterB"

# (c) Scaling, so the per-launch cost of a real profile can be stated rather than guessed.
$treeC = Join-Path $proj "treeC"
$nC = New-Tree $treeC 100 40
$swC = [Diagnostics.Stopwatch]::StartNew()
[void][Wc]::SetAce($treeC, $probeSid, $WRITE_MASK, $GRANT, $true)
$swC.Stop()
$swCr = [Diagnostics.Stopwatch]::StartNew()
[void][Wc]::SetAce($treeC, $probeSid, 0, $REVOKE, $true)
$swCr.Stop()
$perEntry = if ($nC) { [math]::Round($swC.ElapsedMilliseconds / $nC, 4) } else { 0 }
Write-Host "scaling:        entries=$nC grant-ms=$($swC.ElapsedMilliseconds) revoke-ms=$($swCr.ElapsedMilliseconds) ms-per-entry=$perEntry"

# (d) The real thing: how big is a profile, and what would granting it actually cost?
$profCount = (Get-ChildItem -Path $env:USERPROFILE -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host "profile-entries=$profCount projected-grant-ms=$([math]::Round($profCount * $perEntry))"
$sysCount = (Get-ChildItem -Path 'C:\Windows\System32' -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Host "system32-entries=$sysCount (a whole-volume grant is this plus everything else)"

# ═══════════════════════ SECTION 3 — THE ARMS: real confined writes ═══════════════════════

Write-Host ''
Write-Host '========== SECTION 3 — REAL LAUNCHES =========='

function Build-Ops($arm) {
  $u = "$arm-$tag"
  @(
    @{ id = 'deep-read';         kind = 'read';   path = (Join-Path $proj 'granted\deep\f.txt') }
    @{ id = 'granted-write';     kind = 'write';  path = (Join-Path $proj 'granted\deep\f.txt') }
    @{ id = 'ungranted-write';   kind = 'write';  path = (Join-Path $proj 'ungranted\f.txt') }
    @{ id = 'profile-pre-write'; kind = 'write';  path = (Join-Path $profPre 'deep\f.txt') }
    @{ id = 'profile-new';       kind = 'create'; path = (Join-Path $profPre "deep\new-$u.txt") }
    @{ id = 'profile-root-new';  kind = 'create'; path = (Join-Path $env:USERPROFILE "wcroot-$u.txt") }
    @{ id = 'driveroot-mkdir';   kind = 'mkdir';  path = "C:\wc-$u" }
    @{ id = 'programdata-mkdir'; kind = 'mkdir';  path = "C:\ProgramData\wc-$u" }
    @{ id = 'programfiles-new';  kind = 'create'; path = "C:\Program Files\wc-$u.txt" }
    @{ id = 'windows-new';       kind = 'create'; path = "C:\Windows\wc-$u.txt" }
    @{ id = 'home-secret-read';  kind = 'read';   path = $secret }
    @{ id = 'sysroot-list';      kind = 'list';   path = 'C:\' }
  )
}

# Each arm names the paths it ACEs. `$null` means "take no token" — the `plain` control, which is
# also exactly what the shipping full-disk tier does on Windows today.
$arms = @(
  @{ name = 'plain';      token = $false; aces = @();                        caps = @() }
  @{ name = 'ac-bare';    token = $true;  aces = @();                        caps = @() }
  @{ name = 'ac-leaf';    token = $true;  aces = @('granted');               caps = @() }
  @{ name = 'ac-max';     token = $true;  aces = @('granted', 'maxgrant');   caps = @() }
  @{ name = 'ac-max-net'; token = $true;  aces = @('granted', 'maxgrant');   caps = @($INTERNET_CLIENT) }
)

$results = @{}
foreach ($arm in $arms) {
  $name = $arm.name
  Write-Host ''
  Write-Host "--- arm $name ---"

  $sid = ''
  $acName = "nubwc-$name-$tag"
  if ($arm.token) {
    $sid = [Wc]::CreateProfile($acName)
    if ($sid -like 'ERR*') { Write-Host "$name profile-create $sid"; $FAILURES++; continue }
    Write-Host "$name sid=$sid"
  }

  # Install this arm's ACEs. `maxgrant` is the CEILING ARM: an inheritable write grant on every
  # root Section 1 proved reachable de-elevated. It is deliberately built from the MEASURED reach
  # rather than from a guess, so the arm is "the most a user could grant", by construction.
  $granted = @()
  if ($arm.token) {
    if ($arm.aces -contains 'granted') {
      $g = Join-Path $proj 'granted'
      Write-Host "$name ace granted -> $([Wc]::SetAce($g, $sid, $WRITE_MASK, $GRANT, $true))"
      $granted += $g
    }
    if ($arm.aces -contains 'maxgrant') {
      foreach ($k in $roots.Keys) {
        if ($daclReach[$k] -ne 'OK') { continue }
        if ($k -eq 'project' -or $k -eq 'project\granted\deep') { continue }
        $p = $roots[$k]
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $r = [Wc]::SetAce($p, $sid, $WRITE_MASK, $GRANT, $true)
        $sw.Stop()
        Write-Host "$name ace maxgrant $k -> $r ($($sw.ElapsedMilliseconds) ms)"
        if ($r -eq 'OK') { $granted += $p }
      }
    }
    # THE ENGAGEMENT ASSERTION. A DACL read-back on a PRE-EXISTING deep file: if the ACE did not
    # reach it, a denial downstream is a propagation slip and not the kernel's answer.
    Write-Host "$name readback granted-deep = $([Wc]::ReadAceMask((Join-Path $proj 'granted\deep\f.txt'), $sid))"
    Write-Host "$name readback profile-deep = $([Wc]::ReadAceMask((Join-Path $profPre 'deep\f.txt'), $sid))"
  }

  $opsFile = Join-Path $proj "ops-$name.json"
  Build-Ops $name | ConvertTo-Json -Depth 4 | Set-Content -Path $opsFile -Encoding UTF8
  $env:WC_OPS = $opsFile
  $log = Join-Path $proj "log-$name.txt"

  # `--preserve-symlinks-main` ONLY: it repairs the entry point's realpath, and it is measured
  # clean on the wrong-version axis for a non-symlinked entry (§5h). The tree-wide flag is not
  # used and is not needed — `child.js` requires only `node:` builtins.
  $cmd = "`"$nodeExe`" --preserve-symlinks-main `"$here\child.js`""
  $rc = [Wc]::Launch($sid, $nodeExe, $cmd, $proj, $log, 90000, $arm.caps)
  Write-Host "$name launch $rc"

  $lines = if (Test-Path $log) { Get-Content $log } else { @() }
  $map = @{}
  foreach ($l in $lines) { if ($l -match '^([^=]+)=(.*)$') { $map[$matches[1]] = $matches[2] } }
  $results[$name] = $map
  Write-Host "$name lines=$($lines.Count) done=$($map['child:done'])"
  foreach ($l in $lines) { Write-Host "  $name | $l" }

  # Teardown: revoke every ACE this arm wrote, then drop the profile. A per-run sid leaves no
  # residue by construction, but the revoke is what makes that true rather than hoped.
  foreach ($p in $granted) { [void][Wc]::SetAce($p, $sid, 0, $REVOKE, $true) }
  if ($arm.token) { [void][Wc]::DeleteProfile($acName) }
}

# ═══════════════════════════════ SECTION 4 — THE TABLE ═══════════════════════════════

Write-Host ''
Write-Host '========== SECTION 4 — WRITE CEILING BY TARGET CLASS =========='
$opIds = (Build-Ops 'x' | ForEach-Object { $_.id })
$hdr = 'op'.PadRight(20)
foreach ($a in $arms) { $hdr += ($a.name).PadRight(14) }
Write-Host $hdr
foreach ($o in $opIds) {
  $row = $o.PadRight(20)
  foreach ($a in $arms) {
    $v = $results[$a.name]["op:$o"]
    if ($null -eq $v) { $v = 'MISSING' }
    $row += ($v.Substring(0, [Math]::Min(13, $v.Length))).PadRight(14)
  }
  Write-Host $row
}
$row = 'net:connect'.PadRight(20)
foreach ($a in $arms) {
  $v = $results[$a.name]['net:connect-1.1.1.1']; if ($null -eq $v) { $v = 'MISSING' }
  $row += ($v.Substring(0, [Math]::Min(13, $v.Length))).PadRight(14)
}
Write-Host $row

# ══════════════════════════════ SECTION 5 — CONTROLS ══════════════════════════════

Write-Host ''
Write-Host '========== SECTION 5 — CONTROLS =========='
foreach ($a in $arms) {
  Check "$($a.name)-reached-user-code" ($results[$a.name]['child:done'] -eq 'ok') `
    "done=$($results[$a.name]['child:done'])"
}
# The unconfined arm must succeed at the things a user can do, or nothing below it is attributable.
Check 'plain-writes-its-own-profile' ($results['plain']['op:profile-pre-write'] -like 'OK*') `
  "= $($results['plain']['op:profile-pre-write'])"
Check 'plain-reads-the-secret' ($results['plain']['op:home-secret-read'] -like 'OK*') `
  "= $($results['plain']['op:home-secret-read'])"
Check 'plain-has-network' ($results['plain']['net:connect-1.1.1.1'] -eq 'OK') `
  "= $($results['plain']['net:connect-1.1.1.1'])"
# Every confined arm must be genuinely confined, or its passes are meaningless.
foreach ($a in $arms | Where-Object { $_.token }) {
  Check "$($a.name)-gate-is-live" ($results[$a.name]['op:sysroot-list'] -notlike 'OK*') `
    "sysroot-list=$($results[$a.name]['op:sysroot-list'])"
}
# The floor and the existing tier, so the ceiling arm is read against a live scale.
Check 'ac-bare-denies-granted-write' ($results['ac-bare']['op:granted-write'] -notlike 'OK*') `
  "= $($results['ac-bare']['op:granted-write'])"
Check 'ac-leaf-allows-granted-write' ($results['ac-leaf']['op:granted-write'] -like 'OK*') `
  "= $($results['ac-leaf']['op:granted-write'])"
# THE SEPARABILITY QUESTION, and the control that makes the answer mean something: the ceiling arm
# must still be denied egress, and the identical arm holding `internetClient` must reach it.
Check 'ac-max-still-denied-egress' ($results['ac-max']['net:connect-1.1.1.1'] -notlike 'OK*') `
  "= $($results['ac-max']['net:connect-1.1.1.1'])"
Check 'ac-max-net-reaches-egress' ($results['ac-max-net']['net:connect-1.1.1.1'] -eq 'OK') `
  "= $($results['ac-max-net']['net:connect-1.1.1.1'])"
# NOT a control — a REPORTED PROPERTY, and the one the recommendation turns on. A ceiling arm that
# grants the whole profile necessarily grants the secrets sitting in it, so this is expected to
# read OK; asserting it would make a correct measurement look like a harness failure. What matters
# is that the number is stated: it is the security cost of the ceiling tier, in one cell.
Write-Host "PROPERTY ac-max-home-secret-read = $($results['ac-max']['op:home-secret-read'])"
Write-Host "PROPERTY ac-leaf-home-secret-read = $($results['ac-leaf']['op:home-secret-read'])"

[void][Wc]::DeleteProfile($probeName)
Remove-Item -Recurse -Force $proj, $profPre, $secret -ErrorAction SilentlyContinue
Get-ChildItem -Path 'C:\', 'C:\ProgramData', 'C:\Program Files', 'C:\Windows' -Filter "wc-*-$tag*" -ErrorAction SilentlyContinue |
  Remove-Item -Recurse -Force -ErrorAction SilentlyContinue

Write-Host ''
Write-Host "FAILURES = $FAILURES"
Write-Host 'PROBE-EXIT-MARKER'
