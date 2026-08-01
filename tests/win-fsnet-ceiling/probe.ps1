# THE QUESTION (maintainer, 2026-07-31): can nub grant a lifecycle script the WIDEST filesystem
# access obtainable without elevation while still denying it the network?
#
# The two axes are coupled on Windows only through one mechanism: coarse egress denial IS a
# withheld AppContainer capability (`internetClient`), so declining the LowBox token to widen the
# filesystem also hands the package the network. Whether a "widest grantable" tier exists that
# keeps the token is a measurement, not an argument.
#
# ────────────────── THE CONTRADICTION THIS PROBE EXISTS TO SETTLE ──────────────────
#
# MECHANISM-FACTS §5i reports `ERR 5 ACCESS_DENIED` de-elevated at `C:\`, `C:\Users` and
# `C:\ProgramData`, and in the same breath that `C:\` and `C:\ProgramData` are "both of which a
# standard user CAN do WITHOUT the token". Those describe TWO DIFFERENT OPERATIONS — installing an
# ACE (`SetNamedSecurityInfoW`, needs WRITE_DAC) versus creating an object (needs a write right on
# the parent) — and the earlier harness measured `mkdir` for one and a DACL write for the other,
# never a file CREATE at the same target under both instruments. So this probe runs, at every
# location, all four of: create a FILE, create a DIRECTORY, read, list — through raw Win32 (exact
# error numbers) AND through a real launched child (libuv codes), in every arm.
#
# ────────────────────── WHAT THIS PROBE MUST NOT GET WRONG ──────────────────────
#
# 1. A UNIFORMLY-DENIED MATRIX IS USUALLY A BROKEN HARNESS, and that misread has already cost this
#    effort two runs. Every arm carries a POSITIVE control that must succeed (`armtmp`, granted in
#    every arm) and a NEGATIVE control that must fail (`C:\Windows` create). An arm that fails
#    either is VOID and is not interpreted.
# 2. AN ARM WITHOUT AN ENGAGEMENT PROOF IS NOT AN ARM. Each launch reads back the CHILD's own
#    primary token — `IsAppContainer`, package sid, capability list, integrity level — through the
#    child's process handle. `tests/win-bypass-traverse/launcher.ps1` silently dropped its
#    capability array, so every arm it ran was a zero-capability arm; only this read-back makes
#    that class of bug impossible to repeat.
# 3. THE RUNNER IS ELEVATED AND A USER IS NOT, and `CreateRestrictedToken` COPIES the integrity
#    level — so a "de-elevated" token on CI is still HIGH IL unless it is explicitly lowered.
#    Every de-elevated context here is lowered to MEDIUM, the level a standard user runs at.
# 4. ONE VARIABLE. `ac-broad-net` and `ac-broad-nonet` share ONE AppContainer profile, ONE sid and
#    ONE installed ACE set — the ACEs are literally the same bytes on disk, not merely equivalent —
#    and the DACL is read back before each launch to prove nothing moved between them. The only
#    difference is the capability array.
# 5. THE CHILD MUST REACH USER CODE. An unflagged confined `node` dies in `resolveMainPath`'s
#    realpath before running a line (MECHANISM-FACTS §5h). `--preserve-symlinks-main` repairs the
#    entry point and `child.js` imports only `node:`-prefixed builtins, which never enter
#    `_findPath`, so the rejected tree-wide `--preserve-symlinks` is not needed.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $here 'launcher.ps1')

$FAILURES = 0
function Check($name, $cond, $detail) {
  if ($cond) { Write-Host "CONTROL $name = PASS $detail" }
  else { Write-Host "CONTROL $name = FAIL $detail"; $script:FAILURES++ }
}

# Generic rights, as production writes them. `[Convert]::ToUInt32` rather than a `0x…` literal:
# PowerShell parses `0x80000000` as a SIGNED int, so `$GR -bor $GE` comes out negative and every
# marshalled call dies — producing a table of blanks that a "denied" control then reads as a PASS.
$GR = [Convert]::ToUInt32('80000000', 16)
$GW = [Convert]::ToUInt32('40000000', 16)
$GE = [Convert]::ToUInt32('20000000', 16)
$DEL = [Convert]::ToUInt32('00010000', 16)
$READ_MASK = [uint32]($GR -bor $GE)
$WRITE_MASK = [uint32]($GR -bor $GW -bor $GE -bor $DEL)
$GRANT = 1; $REVOKE = 4
# The well-known `internetClient` capability — the ONE variable between the two broad arms.
$INTERNET_CLIENT = 'S-1-15-3-1'

$tag = 'fx' + (Get-Random -Maximum 999999)
$proj = Join-Path $env:USERPROFILE "fxproj-$tag"
$stage = Join-Path $proj 'stage'
$armtmp = Join-Path $proj 'armtmp'

Write-Host '=============================== PROVENANCE ==============================='
Write-Host "os=$([System.Environment]::OSVersion.VersionString) arch=$env:PROCESSOR_ARCHITECTURE"
Write-Host "user=$env:USERNAME profile=$env:USERPROFILE"
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
Write-Host "elevated=$($pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
Write-Host "session=$((Get-Process -Id $PID).SessionId)"
$nodeExe = (Get-Command node).Source
Write-Host "node=$nodeExe $(& $nodeExe -v)"
Write-Host "tag=$tag"

# ─────────────────────────────────── FIXTURES ───────────────────────────────────

New-Item -ItemType Directory -Force -Path $stage, $armtmp | Out-Null
Copy-Item -Path (Join-Path $here 'child.js') -Destination $stage -Force
$childJs = Join-Path $stage 'child.js'

# The locations, in the order the maintainer named them plus the three that DISCRIMINATE: a shared
# directory a standard user can write but cannot re-ACL (`C:\Users\Public`, `C:\Windows\Temp`), and
# the two controls (`armtmp` must always pass, `C:\Windows` must always fail).
# THE LEAD THE FIRST RUN TURNED UP. A standard user cannot re-ACL `C:\` or `C:\ProgramData` — but
# `mkdir` there succeeds, and the creator OWNS what it creates, which should carry `WRITE_DAC`. If
# so, "the ceiling is `%USERPROFILE%`" is too narrow: it is `%USERPROFILE%` PLUS anything the user
# creates. These two directories are created BY THE DE-ELEVATED USER and then flow through the
# whole matrix — canary, raw Win32 pass, DACL reach, broad-grant target selection, every arm.
$ownRoot = "C:\fxown-$tag"
$ownPd = "C:\ProgramData\fxown-$tag"
$impF = [Fx]::BeginDeelevated()
$mkOwn1 = [Fx]::TryMkdir($ownRoot)
$mkOwn2 = [Fx]::TryMkdir($ownPd)
[void][Fx]::EndDeelevated()
Write-Host "user-created dirs (de-elevated): $ownRoot -> $mkOwn1 ; $ownPd -> $mkOwn2 (impersonate=$impF)"

$locs = [ordered]@{
  'C:\'                 = 'C:\'
  'C:\ProgramData'      = 'C:\ProgramData'
  'C:\<user-made>'      = $ownRoot
  'C:\ProgramData\<um>' = $ownPd
  'C:\Users'            = 'C:\Users'
  'C:\Users\Public'     = 'C:\Users\Public'
  'C:\Windows\Temp'     = 'C:\Windows\Temp'
  'C:\Program Files'    = 'C:\Program Files'
  'C:\Windows'          = 'C:\Windows'
  '%USERPROFILE%'       = $env:USERPROFILE
  '%USERPROFILE%\.cache' = (Join-Path $env:USERPROFILE '.cache')
  '%TEMP%'              = $env:TEMP
  '%LOCALAPPDATA%'      = $env:LOCALAPPDATA
  'armtmp (POSITIVE)'   = $armtmp
}
New-Item -ItemType Directory -Force -Path (Join-Path $env:USERPROFILE '.cache') | Out-Null

# A canary in every location, planted by the ELEVATED parent, so a read cell that comes back
# denied is a denial and not a missing file. Recorded per location: a location whose canary could
# not be planted has its read row reported as N/A rather than silently read as confinement.
Write-Host ''
Write-Host '========== FIXTURES — canary placement (elevated) =========='
$canary = @{}
foreach ($k in $locs.Keys) {
  $p = Join-Path $locs[$k] "fxcanary-$tag.txt"
  try {
    [IO.File]::WriteAllText($p, "CANARY-$tag")
    $canary[$k] = $p
    Write-Host ("{0,-22} canary=OK" -f $k)
  } catch {
    $canary[$k] = $null
    Write-Host ("{0,-22} canary=ERR {1}" -f $k, $_.Exception.Message)
  }
}
Check 'every-location-has-a-canary' (@($locs.Keys | Where-Object { -not $canary[$_] }).Count -eq 0) `
  "missing=$(($locs.Keys | Where-Object { -not $canary[$_] }) -join ',')"

# A SECOND canary, planted by the DE-ELEVATED USER wherever it can. Run 30689267117 showed the AC
# arm creating and listing freely inside a user-created directory under `C:\` while READ of the
# canary there came back EPERM — which reads as a kernel limit and is not one. An inheritable ACE
# propagates into an existing child only where the GRANTOR holds WRITE_DAC on that child, and the
# first canary is planted by the ELEVATED parent, so under `C:\<user-made>` it is an
# Administrators-owned file inside a user-owned directory and the de-elevated grant skips it.
# Measuring the read row against BOTH a canary the grantor can re-ACL and one it cannot is what
# tells a fixture artefact from confinement.
Write-Host ''
Write-Host '========== FIXTURES — second canary, planted DE-ELEVATED =========='
$canary2 = @{}
$imp0 = [Fx]::BeginDeelevated()
foreach ($k in $locs.Keys) {
  $p = Join-Path $locs[$k] "fxcanary2-$tag.txt"
  $r = [Fx]::TryCreateFile($p)
  $canary2[$k] = if ($r -eq 'OK') { $p } else { $null }
  Write-Host ("{0,-22} canary2(de-elevated)={1}" -f $k, $r)
}
[void][Fx]::EndDeelevated()

# ═══════════ SECTION 1 — RAW WIN32, EXACT ERROR NUMBERS, ELEVATED vs DE-ELEVATED ═══════════
# libuv collapses several distinct Win32 statuses onto one errno, so the child's `EPERM` does not
# by itself name the OS error. These calls run in the parent and report `GetLastError()` verbatim.
# The de-elevated pass is the authoritative no-token matrix: `SetNamedSecurityInfoW`, `CreateFileW`
# and `CreateDirectoryW` all access-check against the THREAD's effective token, so impersonating a
# restricted MEDIUM-IL token gives the standard-user answer in-process.

Write-Host ''
Write-Host '========== SECTION 1 — RAW WIN32 FS MATRIX + DACL REACH =========='
Write-Host "impersonation-check(before) = $([Fx]::WhoAmI())"

$probeName = "nubfx-probe-$tag"
$probeSid = [Fx]::CreateProfile($probeName)
Write-Host "probe-sid=$probeSid"
Check 'probe-profile-created' ($probeSid -notlike 'ERR*') $probeSid

function Win32-Pass($ctx) {
  $out = @{}
  foreach ($k in $locs.Keys) {
    $d = $locs[$k]
    $out["$k|create"] = [Fx]::TryCreateFile((Join-Path $d "fxw32-$ctx-$tag.txt"))
    $out["$k|mkdir"] = [Fx]::TryMkdir((Join-Path $d "fxw32d-$ctx-$tag"))
    $out["$k|read"] = if ($canary[$k]) { [Fx]::TryReadFile($canary[$k]) } else { 'N/A' }
    $out["$k|read2"] = if ($canary2[$k]) { [Fx]::TryReadFile($canary2[$k]) } else { 'N/A' }
    $out["$k|list"] = [Fx]::TryListDir($d)
    # The DACL question, at the same target in the same context, so the two halves of the
    # contradiction are answered side by side rather than in two different runs.
    $out["$k|dacl"] = [Fx]::SetAce($d, $probeSid, $READ_MASK, $GRANT, $false)
    if ($out["$k|dacl"] -eq 'OK') { [void][Fx]::SetAce($d, $probeSid, 0, $REVOKE, $false) }
  }
  return $out
}

$w32elev = Win32-Pass 'elev'
$imp = [Fx]::BeginDeelevated()
Write-Host "impersonate = $imp"
Write-Host "impersonation-check(during) = $([Fx]::WhoAmI())"
$w32deelev = if ($imp -eq 'OK') { Win32-Pass 'deelev' } else { @{} }
[void][Fx]::EndDeelevated()
Check 'impersonation-engaged' ($imp -eq 'OK') "= $imp"

Write-Host ''
Write-Host 'location               | op     | elevated   | DE-ELEVATED (a standard user)'
foreach ($k in $locs.Keys) {
  foreach ($op in @('create', 'mkdir', 'read', 'read2', 'list', 'dacl')) {
    $e = $w32elev["$k|$op"]; $d = $w32deelev["$k|$op"]
    if ($null -eq $e) { $e = 'BLANK' }
    if ($null -eq $d) { $d = 'BLANK' }
    Write-Host ("{0,-22} | {1,-6} | {2,-10} | {3}" -f $k, $op, $e, $d)
  }
}

# ENGAGEMENT: a blank cell means the call never reached the OS, and "not OK" would then be
# indistinguishable from "denied" — the exact false PASS an earlier revision of this harness
# produced. Elevated-can-ACE-`C:\` is what makes the de-elevated refusal about privilege.
$blank = @($locs.Keys | ForEach-Object { $k = $_; @('create', 'mkdir', 'read', 'read2', 'list', 'dacl') |
      Where-Object { -not $w32elev["$k|$_"] -or -not $w32deelev["$k|$_"] } })
Check 'win32-rows-all-answered' ($blank.Count -eq 0) "blanks=$($blank.Count)"
Check 'runner-really-is-elevated' ($w32elev['C:\|dacl'] -eq 'OK') "elevated-dacl-on-C:\ = $($w32elev['C:\|dacl'])"
Check 'deelev-is-really-deelevated' ($w32deelev['C:\Windows|create'] -ne 'OK') `
  "deelev-create-in-C:\Windows = $($w32deelev['C:\Windows|create'])"
Check 'deelev-can-still-write-its-own-profile' ($w32deelev['%USERPROFILE%|create'] -eq 'OK') `
  "= $($w32deelev['%USERPROFILE%|create'])"
Check 'user-created-dirs-were-created-de-elevated' (($mkOwn1 -eq 'OK') -and ($mkOwn2 -eq 'OK')) `
  "C:\=$mkOwn1 C:\ProgramData\=$mkOwn2"
# THE LEAD, reported not asserted: does OWNING a directory under a root you cannot ACE let you ACE
# the directory? If yes, the ceiling is not the profile — it is the profile plus whatever the user
# makes, anywhere it can make it.
Write-Host "PROPERTY deelev-dacl-write-on-user-created-C-root = $($w32deelev['C:\<user-made>|dacl'])"
Write-Host "PROPERTY deelev-dacl-write-on-user-created-ProgramData = $($w32deelev['C:\ProgramData\<um>|dacl'])"

Write-Host ''
Write-Host '--- THE CONTRADICTION, both halves at the same target, same context ---'
foreach ($k in @('C:\', 'C:\ProgramData', 'C:\Users')) {
  Write-Host ("PROPERTY {0,-16} deelev: create-file={1,-10} mkdir={2,-10} dacl-write={3}" -f `
      $k, $w32deelev["$k|create"], $w32deelev["$k|mkdir"], $w32deelev["$k|dacl"])
}

# ═══════════════════ SECTION 2 — THE ARMS: real launches, one variable apart ═══════════════════

Write-Host ''
Write-Host '========== SECTION 2 — REAL LAUNCHES =========='

function Build-Ops($arm) {
  $o = @()
  foreach ($k in $locs.Keys) {
    $d = $locs[$k]
    $o += @{ id = "$k|create"; kind = 'create'; path = (Join-Path $d "fxnew-$arm-$tag.txt") }
    $o += @{ id = "$k|mkdir"; kind = 'mkdir'; path = (Join-Path $d "fxdir-$arm-$tag") }
    if ($canary[$k]) { $o += @{ id = "$k|read"; kind = 'read'; path = $canary[$k] } }
    if ($canary2[$k]) { $o += @{ id = "$k|read2"; kind = 'read'; path = $canary2[$k] } }
    $o += @{ id = "$k|list"; kind = 'list'; path = $d }
  }
  return $o
}

# THE BROAD GRANT: an inheritable write ACE on every location Section 1 measured as DACL-writable
# BY A DE-ELEVATED CALLER, plus the project. Built from the measurement rather than from a guess,
# so "the widest a real user could grant" is true by construction and not by assertion.
$broadTargets = @()
foreach ($k in $locs.Keys) { if ($w32deelev["$k|dacl"] -eq 'OK') { $broadTargets += $locs[$k] } }
$broadTargets += $proj
$broadTargets = $broadTargets | Select-Object -Unique
Write-Host "broad-grant targets (measured de-elevated-writable): $($broadTargets -join ' ; ')"
Check 'broad-grant-is-non-empty' ($broadTargets.Count -ge 1) "n=$($broadTargets.Count)"

# Two profiles: P1 backs the ungranted floor arm, P2 backs all four broad arms so their ACE set is
# literally one set of bytes rather than four equivalent ones.
$bareName = "nubfx-bare-$tag"
$bareSid = [Fx]::CreateProfile($bareName)
$broadName = "nubfx-broad-$tag"
$broadSid = [Fx]::CreateProfile($broadName)
Write-Host "bare-sid=$bareSid"
Write-Host "broad-sid=$broadSid"
Check 'arm-profiles-created' (($bareSid -notlike 'ERR*') -and ($broadSid -notlike 'ERR*')) `
  "bare=$bareSid broad=$broadSid"

$nodeDir = Split-Path -Parent $nodeExe
$installed = @()   # (path, sid) pairs to revoke at teardown

# SCAFFOLDING, not a measured grant, and IDENTICAL on both profiles — so it can never explain a
# difference between arms. A confined child must read its interpreter and its entry point or it
# dies before user code, which presents as a filesystem denial. Node lives under `hostedtoolcache`,
# the one toolchain layout on these images carrying no ALL APPLICATION PACKAGES ace.
function Short($s) { if ($s.Length -gt 26) { $s.Substring(0, 26) + '…' } else { $s } }
foreach ($sid in @($bareSid, $broadSid)) {
  $r1 = [Fx]::SetAce($nodeDir, $sid, $READ_MASK, $GRANT, $true)
  $r2 = [Fx]::SetAce($stage, $sid, $READ_MASK, $GRANT, $true)
  Write-Host "scaffold sid=$(Short $sid) node=$r1 stage=$r2"
  if ($r1 -eq 'OK') { $installed += , @($nodeDir, $sid) }
  if ($r2 -eq 'OK') { $installed += , @($stage, $sid) }
}
# `armtmp` is the POSITIVE CONTROL and must be reachable in every arm, confined or not.
foreach ($sid in @($bareSid, $broadSid)) {
  $r = [Fx]::SetAce($armtmp, $sid, $WRITE_MASK, $GRANT, $true)
  Write-Host "positive-control-grant armtmp sid=$(Short $sid) -> $r"
  if ($r -eq 'OK') { $installed += , @($armtmp, $sid) }
}

# The broad ACEs, installed ONCE, DE-ELEVATED — nub would install them as the user, so installing
# them as the elevated runner would measure a grant no user could make.
Write-Host ''
$imp2 = [Fx]::BeginDeelevated()
Write-Host "broad-grant impersonate = $imp2"
foreach ($t in $broadTargets) {
  $sw = [Diagnostics.Stopwatch]::StartNew()
  $r = [Fx]::SetAce($t, $broadSid, $WRITE_MASK, $GRANT, $true)
  $sw.Stop()
  Write-Host "broad ace $t -> $r ($($sw.ElapsedMilliseconds) ms)"
  if ($r -eq 'OK') { $installed += , @($t, $broadSid) }
}
[void][Fx]::EndDeelevated()

# The ACE read-back probe set: a PRE-EXISTING file in each location. A propagation slip and a
# kernel denial are otherwise indistinguishable in a child's output.
$readbackPaths = @($canary['%USERPROFILE%'], $canary['%TEMP%'], $canary['%LOCALAPPDATA%'],
  $canary['C:\'], $canary['C:\ProgramData'], $canary['C:\Users\Public'], $canary['armtmp (POSITIVE)'],
  $canary['C:\<user-made>'], $canary2['C:\<user-made>'],
  $canary['C:\ProgramData\<um>'], $canary2['C:\ProgramData\<um>'])
function Readback($sid) {
  ($readbackPaths | ForEach-Object { if ($_) { [Fx]::ReadAceMask($_, $sid) } else { 'N/A' } }) -join ' '
}

$arms = @(
  @{ name = 'plain-elev'; sid = ''; caps = @(); deelev = $false }
  @{ name = 'no-token'; sid = ''; caps = @(); deelev = $true }
  @{ name = 'ac-bare'; sid = $bareSid; caps = @(); deelev = $false }
  @{ name = 'ac-broad-net'; sid = $broadSid; caps = @($INTERNET_CLIENT); deelev = $false }
  @{ name = 'ac-broad-nonet'; sid = $broadSid; caps = @(); deelev = $false }
  @{ name = 'ac-broad-net-dv'; sid = $broadSid; caps = @($INTERNET_CLIENT); deelev = $true }
  @{ name = 'ac-broad-nonet-dv'; sid = $broadSid; caps = @(); deelev = $true }
  # §5e's lesson: re-run the baseline LAST. A persistent side effect of an earlier arm would
  # otherwise present as confinement.
  @{ name = 'plain-elev-2'; sid = ''; caps = @(); deelev = $false }
)

$results = @{}
$tokenInfo = @{}
$readbacks = @{}
foreach ($arm in $arms) {
  $name = $arm.name
  Write-Host ''
  Write-Host "--- arm $name ---"
  if ($arm.sid) {
    $rb = Readback $arm.sid
    $readbacks[$name] = $rb
    Write-Host "$name ace-readback = $rb"
  }

  $opsFile = Join-Path $stage "ops-$name.json"
  Build-Ops $name | ConvertTo-Json -Depth 4 | Set-Content -Path $opsFile -Encoding UTF8
  $env:FX_OPS = $opsFile
  $log = Join-Path $proj "log-$name.txt"
  $cmd = "`"$nodeExe`" --preserve-symlinks-main `"$childJs`""
  $rc = [Fx]::LaunchEx($arm.sid, $nodeExe, $cmd, $proj, $log, 120000, $arm.caps, $arm.deelev)
  foreach ($l in ($rc -split "`n")) { Write-Host "$name launch $l" }
  $tokenInfo[$name] = $rc

  $lines = if (Test-Path $log) { Get-Content $log } else { @() }
  $map = @{}
  foreach ($l in $lines) { if ($l -match '^([^=]+)=(.*)$') { $map[$matches[1]] = $matches[2] } }
  $results[$name] = $map
  Write-Host "$name lines=$($lines.Count) done=$($map['child:done'])"
  foreach ($l in $lines) { if ($l -notlike 'op:*') { Write-Host "  $name | $l" } }
}

# ═══════════════════════════════ SECTION 3 — THE MATRIX ═══════════════════════════════

Write-Host ''
Write-Host '========== SECTION 3 — ARM x LOCATION x OP =========='
$armNames = $arms | ForEach-Object { $_.name }
$hdr = 'location'.PadRight(23) + 'op'.PadRight(16)
foreach ($n in $armNames) { $hdr += $n.PadRight(19) }
Write-Host $hdr
foreach ($k in $locs.Keys) {
  foreach ($op in @('create', 'mkdir', 'read', 'read2', 'list')) {
    $row = $k.PadRight(23) + $op.PadRight(16)
    foreach ($n in $armNames) {
      $v = $results[$n]["op:$k|$op"]
      if ($null -eq $v) { $v = 'MISSING' }
      $row += ($v.Substring(0, [Math]::Min(18, $v.Length))).PadRight(19)
    }
    Write-Host $row
  }
}
foreach ($nk in @('net:tcp-1.1.1.1:443', 'net:tcp-127.0.0.1:135', 'net:dns', 'net:https-get')) {
  $row = 'NETWORK'.PadRight(23) + ($nk -replace '^net:', '').PadRight(16)
  foreach ($n in $armNames) {
    $v = $results[$n][$nk]; if ($null -eq $v) { $v = 'MISSING' }
    $row += ($v.Substring(0, [Math]::Min(18, $v.Length))).PadRight(19)
  }
  Write-Host $row
}

# ══════════════════════════════ SECTION 4 — CONTROLS ══════════════════════════════

Write-Host ''
Write-Host '========== SECTION 4 — CONTROLS =========='

# The CORE arms carry the verdict and are asserted. The two `-dv` arms compose an AppContainer
# attribute onto a de-elevated `CreateProcessAsUserW` base — a combination this repo has modelled
# (§5d) but never LAUNCHED, so they are reported rather than asserted: if that launch turns out to
# be unavailable, that is itself a finding and must not void the arms that answered the question.
$coreArms = @('plain-elev', 'no-token', 'ac-bare', 'ac-broad-net', 'ac-broad-nonet', 'plain-elev-2')
$fidelityArms = @('ac-broad-net-dv', 'ac-broad-nonet-dv')

foreach ($n in $coreArms) {
  Check "$n-reached-user-code" ($results[$n]['child:done'] -eq 'ok') "done=$($results[$n]['child:done'])"
}
foreach ($n in $fidelityArms) {
  Write-Host "PROPERTY $n-reached-user-code = $($results[$n]['child:done'])"
}
# THE POSITIVE CONTROL. A location every arm MUST succeed at — without it a column of denials
# reads as "the ceiling is zero" when the arm is simply broken.
foreach ($n in $coreArms) {
  Check "$n-positive-control-armtmp-create" ($results[$n]['op:armtmp (POSITIVE)|create'] -like 'OK*') `
    "= $($results[$n]['op:armtmp (POSITIVE)|create'])"
}
# THE NEGATIVE CONTROL. A location no de-elevated or confined arm may succeed at. `plain-elev` is
# excluded BY DESIGN: it is elevated and must succeed there, which is what proves the path is real.
foreach ($n in $coreArms | Where-Object { $_ -notlike 'plain-elev*' }) {
  Check "$n-negative-control-windows-create" ($results[$n]['op:C:\Windows|create'] -notlike 'OK*') `
    "= $($results[$n]['op:C:\Windows|create'])"
}
Check 'plain-elev-negative-control-is-reachable-when-elevated' `
  ($results['plain-elev']['op:C:\Windows|create'] -like 'OK*') `
  "= $($results['plain-elev']['op:C:\Windows|create'])"
# Every confined arm must prove the AppContainer gate is live, or its passes mean nothing.
foreach ($a in $arms | Where-Object { $_.sid -and ($coreArms -contains $_.name) }) {
  Check "$($a.name)-gate-is-live" ($results[$a.name]['op:C:\|list'] -notlike 'OK*') `
    "C:\ list = $($results[$a.name]['op:C:\|list'])"
}
foreach ($n in $fidelityArms) {
  Write-Host "PROPERTY $n-gate-is-live C:\ list = $($results[$n]['op:C:\|list'])"
  Write-Host "PROPERTY $n-childtoken = $(($tokenInfo[$n] -split "`n" | Where-Object { $_ -like 'childtoken:*' -or $_ -like 'launch-error*' }) -join ' ')"
  Write-Host "PROPERTY $n-egress tcp=$($results[$n]['net:tcp-1.1.1.1:443']) https=$($results[$n]['net:https-get'])"
}
# The floor arm proves the broad ACEs are what open the profile, not confinement being absent.
Check 'ac-bare-denies-the-profile' ($results['ac-bare']['op:%USERPROFILE%|create'] -notlike 'OK*') `
  "= $($results['ac-bare']['op:%USERPROFILE%|create'])"
Check 'ac-broad-allows-the-profile' ($results['ac-broad-nonet']['op:%USERPROFILE%|create'] -like 'OK*') `
  "= $($results['ac-broad-nonet']['op:%USERPROFILE%|create'])"
# ONE VARIABLE: the two broad arms must have seen the identical ACE state.
Check 'broad-arms-share-one-ace-set' ($readbacks['ac-broad-net'] -eq $readbacks['ac-broad-nonet']) `
  "net=[$($readbacks['ac-broad-net'])] nonet=[$($readbacks['ac-broad-nonet'])]"
# ENGAGEMENT: the capability must be visible in the CHILD's token, not merely requested.
$netTok = $tokenInfo['ac-broad-net']
$nonetTok = $tokenInfo['ac-broad-nonet']
Check 'net-arm-really-holds-internetClient' ($netTok -match 'capabilities=\[[^\]]*S-1-15-3-1') `
  "childtoken caps line: $(($netTok -split "`n" | Where-Object { $_ -like 'childtoken:capabilities*' }) -join '')"
Check 'nonet-arm-really-holds-no-capability' ($nonetTok -match 'capabilities=\[\]') `
  "childtoken caps line: $(($nonetTok -split "`n" | Where-Object { $_ -like 'childtoken:capabilities*' }) -join '')"
Check 'confined-arms-really-are-appcontainers' `
  (($netTok -match 'isAppContainer=1') -and ($nonetTok -match 'isAppContainer=1')) `
  "net/nonet isAppContainer"
Check 'no-token-arm-is-not-an-appcontainer' ($tokenInfo['no-token'] -match 'isAppContainer=0') `
  "$(($tokenInfo['no-token'] -split "`n" | Where-Object { $_ -like 'childtoken:*' }) -join ' ')"
# THE ANSWER, as two controls rather than as prose.
Check 'broad-nonet-is-denied-egress' `
  (($results['ac-broad-nonet']['net:tcp-1.1.1.1:443'] -notlike 'OK*') -and
   ($results['ac-broad-nonet']['net:https-get'] -notlike 'OK*')) `
  "tcp=$($results['ac-broad-nonet']['net:tcp-1.1.1.1:443']) https=$($results['ac-broad-nonet']['net:https-get'])"
Check 'broad-net-reaches-egress' `
  ($results['ac-broad-net']['net:tcp-1.1.1.1:443'] -eq 'OK') `
  "tcp=$($results['ac-broad-net']['net:tcp-1.1.1.1:443']) https=$($results['ac-broad-net']['net:https-get'])"
# The repeat baseline: a persistent side effect of an earlier arm would show up here as a
# difference from the first `plain-elev` run. Compared by OK/ERR CLASS, not by exact value — the
# arms in between legitimately add entries, so a directory listing's COUNT is expected to move and
# comparing it would manufacture a failure. (Caught by a stubbed dry run before this ever shipped.)
function Cls($v) { if ($v -like 'OK*') { 'OK' } elseif ($null -eq $v) { 'MISSING' } else { $v } }
$drift = @()
foreach ($k in $locs.Keys) {
  foreach ($op in @('create', 'mkdir', 'read', 'read2', 'list')) {
    if ((Cls $results['plain-elev']["op:$k|$op"]) -ne (Cls $results['plain-elev-2']["op:$k|$op"])) {
      $drift += "$k|$op"
    }
  }
}
Check 'baseline-is-stable-when-re-run-last' ($drift.Count -eq 0) "drift=$($drift -join ',')"

Write-Host ''
Write-Host '--- REPORTED PROPERTIES (not controls: whichever way they fall IS the finding) ---'
foreach ($k in $locs.Keys) {
  Write-Host ("PROPERTY gap-vs-no-token {0,-22} create: no-token={1,-12} ac-broad-nonet={2}" -f `
      $k, $results['no-token']["op:$k|create"], $results['ac-broad-nonet']["op:$k|create"])
  Write-Host ("PROPERTY gap-vs-no-token {0,-22} mkdir:  no-token={1,-12} ac-broad-nonet={2}" -f `
      $k, $results['no-token']["op:$k|mkdir"], $results['ac-broad-nonet']["op:$k|mkdir"])
  Write-Host ("PROPERTY gap-vs-no-token {0,-22} read:   no-token={1,-12} ac-broad-nonet={2}" -f `
      $k, $results['no-token']["op:$k|read"], $results['ac-broad-nonet']["op:$k|read"])
}

# ─────────────────────────────────── TEARDOWN ───────────────────────────────────

foreach ($pair in $installed) { [void][Fx]::SetAce($pair[0], $pair[1], 0, $REVOKE, $true) }
[void][Fx]::DeleteProfile($bareName)
[void][Fx]::DeleteProfile($broadName)
[void][Fx]::DeleteProfile($probeName)
Write-Host ''
Write-Host "teardown broad-readback-after-revoke = $(Readback $broadSid)"
foreach ($k in $locs.Keys) {
  $d = $locs[$k]
  Get-ChildItem -Path $d -Filter "fx*$tag*" -Force -ErrorAction SilentlyContinue |
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
}
Remove-Item -Recurse -Force $proj -ErrorAction SilentlyContinue
Remove-Item -Force (Join-Path $env:USERPROFILE ".cache\fxcanary-$tag.txt") -ErrorAction SilentlyContinue

Write-Host ''
Write-Host "FAILURES = $FAILURES"
Write-Host 'PROBE-EXIT-MARKER'
