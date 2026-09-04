# W1 -- can an AppContainer with NO capabilities reach a loopback listener running in ANOTHER
# AppContainer that bears the SAME package sid, at zero privilege?
#
# THE HYPOTHESIS (Forshaw, "Understanding Network Access in Windows AppContainers", Project Zero
# 2021): AppContainer loopback is blocked at FWPM_LAYER_ALE_AUTH_RECV_ACCEPT by a filter keyed on
# the IsLoopback condition, and beside it sits a PERMIT filter keyed on IsAppContainerLoopback,
# which -- per Forshaw's own testing -- is set only when both endpoints share a package sid.
#
# WHY THE PRIOR DATA DOES NOT ALREADY ANSWER IT. MECHANISM-FACTS 5l 4 measured
# `connect 127.0.0.1:135` -> ETIMEDOUT from an AppContainer both WITH and WITHOUT internetClient,
# where a real outbound denial was EACCES. Two different errors mean two different layers: the
# loopback failure is a receive-side drop, not an outbound denial. But the listener in that
# measurement was the ordinary RPC endpoint mapper -- a NON-AppContainer process, which is exactly
# the cell IsAppContainerLoopback can never fire for. The AppContainer-to-same-package-AppContainer
# cell has never been run.
#
# WHAT MAKES THE ANSWER TRUSTWORTHY, and it is not the arm table:
#
#   POSITIVE CONTROL (arm d) -- plain listener, plain connector, same de-elevated base. It must
#     connect. A run where every arm fails looks like confinement and is a broken harness.
#   NEGATIVE CONTROL (arm c) -- plain listener, AppContainer connector. It must FAIL, reproducing
#     5l 4 on this image rather than importing it.
#   ONE VARIABLE between arms a and b: the listener's package sid, nothing else. Same program,
#     same base token, same grants, same capability set (empty).
#   CHILD-TOKEN READ-BACK, twice per launch: the launcher reads TokenAppContainerSid /
#     TokenCapabilities off the child's process handle, and the child reads the same four values
#     from INSIDE itself. MECHANISM-FACTS 5i: `tests/win-bypass-traverse/launcher.ps1` declared a
#     capability parameter and passed CapabilityCount = 0, so every arm it ever ran was a
#     zero-capability arm and nothing in its output could have said so.
#   NO ELEVATION ANYWHERE, asserted by ACCESS CHECK. CreateRestrictedToken COPIES TokenIsElevated
#     (MECHANISM-FACTS 5h, run 30423750288), so the flag still reads 1 on a de-elevated token;
#     CheckTokenMembership(NULL, Administrators) is the only honest assertion.
#   NO LoopbackExempt. `CheckNetIsolation LoopbackExempt -s` is captured before and after and the
#     package sids are searched for in it, so "we did not use the admin exemption" is evidence
#     rather than a claim.
#
# W2 rides along (arms e and f), because it is two more rows in the same table: Forshaw's
# loopback-exemption capability sid, the package sid with its first RID changed from 2 to 3.
#
# NOTHING IS WRITTEN ABOVE %USERPROFILE%. Every ACE goes on a stage directory this run creates.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
. (Join-Path $here 'launcher.ps1')

$script:FAILURES = 0
function Check([string]$name, [bool]$cond, [string]$detail) {
  if ($cond) { Write-Host ("CONTROL {0,-42} PASS  {1}" -f $name, $detail) }
  else { $script:FAILURES++; Write-Host ("CONTROL {0,-42} FAIL  {1}" -f $name, $detail) }
}

function Read-LogSafe([string]$p) {
  # The child holds this file open with FILE_SHARE_READ|FILE_SHARE_WRITE, so the reader must
  # share write too; the .NET convenience readers do not, and fail with a sharing violation.
  if (-not (Test-Path -LiteralPath $p)) { return '' }
  try {
    $fs = [System.IO.File]::Open($p, 'Open', 'Read', 'ReadWrite')
    $sr = New-Object System.IO.StreamReader($fs)
    $t = $sr.ReadToEnd()
    $sr.Close(); $fs.Close()
    return $t
  } catch { return '' }
}

function Get-FreePort {
  $l = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
  $l.Start()
  $p = $l.LocalEndpoint.Port
  $l.Stop()
  return $p
}

function Short([string]$sid) { if ($sid.Length -gt 26) { $sid.Substring(0, 26) + '...' } else { $sid } }

# ---------------------------------------------------------------- environment

Write-Host "=== W1 same-package AppContainer loopback probe ==="
Write-Host ("os              = {0}" -f [System.Environment]::OSVersion.VersionString)
Write-Host ("arch            = {0}" -f $env:PROCESSOR_ARCHITECTURE)
Write-Host ("caption         = {0}" -f (Get-CimInstance Win32_OperatingSystem -ErrorAction SilentlyContinue).Caption)
Write-Host ("pwsh            = {0}" -f $PSVersionTable.PSVersion)
$nodeCmd = Get-Command node -ErrorAction SilentlyContinue
if ($nodeCmd) { Write-Host ("node            = {0} {1}" -f $nodeCmd.Source, (& $nodeCmd.Source -v)) }

# ---------------------------------------------------------------- stage + child build

$stage = Join-Path $env:USERPROFILE ("w1ac-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Write-Host "stage           = $stage"

$childExe = Join-Path $stage 'child.exe'
$childCs = Join-Path $here 'child.cs'
$childJs = Join-Path $stage 'child.js'
Copy-Item -LiteralPath (Join-Path $here 'child.js') -Destination $childJs -Force

# The .NET Framework compiler, not Roslyn: the output must be a classic framework exe with no
# runtimeconfig and no host resolution, because it has to start inside a LowBox token.
$csc = Get-ChildItem -Path 'C:\Windows\Microsoft.NET\Framework64', 'C:\Windows\Microsoft.NET\Framework' `
  -Filter csc.exe -Recurse -ErrorAction SilentlyContinue |
  Sort-Object FullName -Descending | Select-Object -First 1
$exeOk = $false
if ($csc) {
  Write-Host "csc             = $($csc.FullName)"
  & $csc.FullName /nologo /target:exe /platform:anycpu /out:"$childExe" "$childCs" 2>&1 | ForEach-Object { Write-Host "  csc: $_" }
  $exeOk = (Test-Path -LiteralPath $childExe)
} else {
  Write-Host "csc             = NOT-FOUND (child.exe unavailable; node fallback only)"
}
Write-Host "child.exe built = $exeOk"

# node.exe is COPIED into the stage rather than granted in place: its install directory may not be
# ACE-writable by a de-elevated user, and a self-contained node.exe needs nothing else from it.
$stageNode = ''
if ($nodeCmd) {
  $stageNode = Join-Path $stage 'node.exe'
  Copy-Item -LiteralPath $nodeCmd.Source -Destination $stageNode -Force -ErrorAction SilentlyContinue
  if (-not (Test-Path -LiteralPath $stageNode)) { $stageNode = '' }
}
Write-Host "stage node      = $stageNode"

# ---------------------------------------------------------------- exemption evidence (before)

function Show-Exempt([string]$when) {
  $out = ''
  try { $out = (& CheckNetIsolation.exe LoopbackExempt -s 2>&1 | Out-String) } catch { $out = "ERR $_" }
  Write-Host "--- CheckNetIsolation LoopbackExempt -s ($when) ---"
  Write-Host $out.Trim()
  Write-Host "--- end ($when) ---"
  return $out
}
$exemptBefore = Show-Exempt 'before'

# ---------------------------------------------------------------- de-elevated setup

Write-Host "effective(before impersonation) = $([Fx]::WhoAmI()) admin=$([Fx]::IsAdmin())"

$imp = [Fx]::BeginDeelevated()
Write-Host "impersonate     = $imp"
$deelevWho = [Fx]::WhoAmI()
$deelevAdmin = [Fx]::IsAdmin()
Write-Host "effective(deelevated)           = $deelevWho admin=$deelevAdmin"

$nonce = [guid]::NewGuid().ToString('N').Substring(0, 10)
$name1 = "nubw1a_$nonce"
$name2 = "nubw1b_$nonce"
$P1 = [Fx]::CreateProfile($name1)
$P2 = [Fx]::CreateProfile($name2)
$D1 = [Fx]::DeriveSid($name1)
$D2 = [Fx]::DeriveSid($name2)
Write-Host "package1 create = $P1"
Write-Host "package1 derive = $D1"
Write-Host "package2 create = $P2"
Write-Host "package2 derive = $D2"

# FILE_GENERIC_READ | FILE_GENERIC_EXECUTE. The children need to load their own image and, for
# the node runner, read child.js; nothing else is granted anywhere.
$READ_EXEC = 0x001200A9
$GRANT = 1
$REVOKE = 4
$aceOk = $true
foreach ($sid in @($P1, $P2)) {
  if ($sid -notlike 'S-1-15-2-*') { $aceOk = $false; continue }
  $r = [Fx]::SetAce($stage, $sid, $READ_EXEC, $GRANT, $true)
  Write-Host ("ace grant {0} = {1}" -f (Short $sid), $r)
  if ($r -ne 'OK') { $aceOk = $false }
}
# Read the mask back off the child's own image, not the directory: without this a propagation
# slip and a kernel denial are indistinguishable in the child's output.
if ($exeOk) {
  Write-Host ("ace readback child.exe P1 = {0}  P2 = {1}" -f [Fx]::ReadAceMask($childExe, $P1), [Fx]::ReadAceMask($childExe, $P2))
}
if ($stageNode) {
  Write-Host ("ace readback node.exe  P1 = {0}  P2 = {1}" -f [Fx]::ReadAceMask($stageNode, $P1), [Fx]::ReadAceMask($stageNode, $P2))
}
[void][Fx]::EndDeelevated()
Write-Host "effective(after revert)         = $([Fx]::WhoAmI()) admin=$([Fx]::IsAdmin())"

Check 'deelevated-context-holds-no-admin' ($deelevAdmin -eq '0') "admin=$deelevAdmin who=$deelevWho"
Check 'profiles-created-deelevated' (($P1 -like 'S-1-15-2-*') -and ($P2 -like 'S-1-15-2-*')) "p1=$P1 p2=$P2"
Check 'package-sids-differ' ($P1 -ne $P2) "p1=$(Short $P1) p2=$(Short $P2)"
Check 'derived-sid-equals-created-sid' (($D1 -eq $P1) -and ($D2 -eq $P2)) "derive1=$($D1 -eq $P1) derive2=$($D2 -eq $P2)"
Check 'aces-installed-deelevated' $aceOk "stage=$stage"

# Forshaw's loopback-exemption capability sid: the package sid with its first RID changed 2 -> 3.
$INTERNET_CLIENT = 'S-1-15-3-1'
$PRIVATE_NET_SERVER = 'S-1-15-3-3'
$capOfP1 = $P1 -replace '^S-1-15-2-', 'S-1-15-3-'
$capOfP2 = $P2 -replace '^S-1-15-2-', 'S-1-15-3-'
Write-Host "loopback-exempt cap of P1 = $capOfP1"
Write-Host "loopback-exempt cap of P2 = $capOfP2"

# ---------------------------------------------------------------- launch helpers

$script:runners = @{}
if ($exeOk) { $script:runners['exe'] = @{ exe = $childExe; pre = '' } }
if ($stageNode) { $script:runners['node'] = @{ exe = $stageNode; pre = "--preserve-symlinks-main `"$childJs`" " } }

function Cmd-For($runner, [string]$mode, [int]$port) {
  return ('"{0}" {1}{2} {3}' -f $runner.exe, $runner.pre, $mode, $port)
}

function Start-Child($runnerKey, [string]$acSid, [string[]]$caps, [string]$mode, [int]$port, [string]$log) {
  $r = $script:runners[$runnerKey]
  $cmd = Cmd-For $r $mode $port
  $out = [Fx]::Start($acSid, $r.exe, $cmd, $stage, $log, [string[]]$caps, $true)
  $h = -1
  foreach ($line in ($out -split "`n")) {
    if ($line -match '^h=(\d+)') { $h = [int]$Matches[1] }
  }
  return @{ h = $h; text = $out.Trim() }
}

# ---------------------------------------------------------------- viability ladder
#
# The smallest payload first (probe-platforms): does the child run at all, then does it run
# confined. A ladder step is seconds; discovering the answer inside the arm table is a wasted run.

function Ladder([string]$label, [string]$runnerKey, [string]$acSid) {
  if (-not $script:runners.ContainsKey($runnerKey)) { Write-Host ("LADDER {0,-24} SKIP (no runner)" -f $label); return $false }
  $log = Join-Path $stage "ladder-$label.log"
  $s = Start-Child $runnerKey $acSid @() 'selftest' 0 $log
  if ($s.h -lt 0) { Write-Host ("LADDER {0,-24} LAUNCH-FAIL {1}" -f $label, $s.text); return $false }
  $w = [Fx]::Wait($s.h, 30000)
  [void][Fx]::Kill($s.h)
  $body = (Read-LogSafe $log).Trim()
  Write-Host ("LADDER {0,-24} {1}" -f $label, $w)
  foreach ($l in ($s.text -split "`n")) { if ($l -match 'childtoken:') { Write-Host "         $($l.Trim())" } }
  foreach ($l in ($body -split "`n")) { if ($l.Trim()) { Write-Host "         $($l.Trim())" } }
  return ($w -eq 'rc=0 (0x00000000)')
}

Write-Host "=== viability ladder ==="
$ladPlainExe = Ladder 'plain-exe' 'exe' ''
$ladAcExe = Ladder 'ac-exe' 'exe' $P1
$ladPlainNode = Ladder 'plain-node' 'node' ''
$ladAcNode = Ladder 'ac-node' 'node' $P1

$primary = ''
if ($ladAcExe) { $primary = 'exe' } elseif ($ladAcNode) { $primary = 'node' }
$cross = ''
if ($primary -eq 'exe' -and $ladAcNode) { $cross = 'node' }
$primaryShow = 'NONE'; if ($primary) { $primaryShow = $primary }
$crossShow = 'none'; if ($cross) { $crossShow = $cross }
Write-Host "primary runner  = $primaryShow"
Write-Host "cross-check     = $crossShow"
Check 'a-confined-child-runs-at-all' ($primary -ne '') "exe=$ladAcExe node=$ladAcNode"

# ---------------------------------------------------------------- egress gate control
#
# THE PREMISE ARM a WOULD OTHERWISE ASSUME. If arm a connects, the reading "the same package sid
# opened loopback" is only available once "the AppContainer confinement was actually engaged at
# the NETWORK layer" is separately shown. The child-token read-back proves the token; this proves
# the token is doing something. A zero-capability child must not reach 1.1.1.1:443, and
# MECHANISM-FACTS 5l 4 says the refusal is EACCES -- WSAEACCES 10013 -- and fast.

function Run-Single([string]$label, [string]$runnerKey, [string]$acSid, [string[]]$caps, [string]$mode) {
  $log = Join-Path $stage "single-$label.log"
  $s = Start-Child $runnerKey $acSid $caps $mode 0 $log
  if ($s.h -lt 0) { Write-Host ("SINGLE {0,-22} LAUNCH-FAIL {1}" -f $label, $s.text); return '' }
  $w = [Fx]::Wait($s.h, 60000)
  [void][Fx]::Kill($s.h)
  $body = (Read-LogSafe $log).Trim()
  Write-Host ("SINGLE {0,-22} {1}" -f $label, $w)
  foreach ($l in ($s.text -split "`n")) { if ($l -match 'childtoken:') { Write-Host "         $($l.Trim())" } }
  foreach ($l in ($body -split "`n")) { if ($l.Trim()) { Write-Host "         $($l.Trim())" } }
  return $body
}

$egPlain = ''
$egAc = ''
if ($primary) {
  Write-Host ""
  Write-Host "=== egress gate control ==="
  $egPlain = Run-Single 'egress-plain' $primary '' @() 'egress'
  $egAc = Run-Single 'egress-ac-nocap' $primary $P1 @() 'egress'
}
$egAcBlocked = ($egAc -and ($egAc -notmatch 'connect:result=CONNECTED'))
Check 'egress-gate-live-zero-cap-ac-denied' $egAcBlocked `
  ("ac=" + (($egAc -split "`n" | Where-Object { $_ -match '^connect:result=' }) -join ' '))
# The unconfined half is INFORMATIONAL: a CI runner may sit behind an egress filter of its own,
# and a failure there says nothing about the token. Reported, never gating.
Write-Host ("egress-plain (informational) = " + (($egPlain -split "`n" | Where-Object { $_ -match '^connect:result=' }) -join ' '))

# ---------------------------------------------------------------- the arm table

$arms = @(
  @{ id = 'd-plain-to-plain'; lsid = ''; lcaps = @(); csid = ''; ccaps = @(); expect = 'CONNECT'; note = 'positive control' }
  @{ id = 'c-plain-to-ac'; lsid = ''; lcaps = @(); csid = $P1; ccaps = @(); expect = 'BLOCK'; note = 'negative control, reproduces 5l section 4' }
  @{ id = 'b-diff-package-sid'; lsid = $P1; lcaps = @(); csid = $P2; ccaps = @(); expect = 'BLOCK'; note = 'one variable from arm a' }
  @{ id = 'a-same-package-sid'; lsid = $P1; lcaps = @(); csid = $P1; ccaps = @(); expect = 'W1'; note = 'THE QUESTION' }
  @{ id = 'a2-same-sid-conn-netcap'; lsid = $P1; lcaps = @(); csid = $P1; ccaps = @($INTERNET_CLIENT); expect = 'W1'; note = 'does internetClient change loopback' }
  @{ id = 'a3-same-sid-listener-srvcap'; lsid = $P1; lcaps = @($PRIVATE_NET_SERVER); csid = $P1; ccaps = @(); expect = 'W1'; note = 'fallback if a zero-cap bind cannot listen' }
  @{ id = 'e-w2-selfcap-to-plain'; lsid = ''; lcaps = @(); csid = $P2; ccaps = @($capOfP2); expect = 'W2'; note = 'Forshaw loopback exemption, own package' }
  @{ id = 'f-w2-peercap-cross-sid'; lsid = $P1; lcaps = @(); csid = $P2; ccaps = @($capOfP1); expect = 'W2'; note = 'exemption naming the peer package' }
)

$script:results = @()

function Run-Arm($arm, [string]$runnerKey) {
  $id = "$($arm.id)/$runnerKey"
  Write-Host ""
  Write-Host "=== ARM $id  ($($arm.note)) ==="
  $port = Get-FreePort
  $llog = Join-Path $stage "$($arm.id).$runnerKey.listen.log"
  $clog = Join-Path $stage "$($arm.id).$runnerKey.connect.log"
  $lsidShow = 'none'; if ($arm.lsid) { $lsidShow = Short $arm.lsid }
  $csidShow = 'none'; if ($arm.csid) { $csidShow = Short $arm.csid }
  Write-Host ("  listener  ac={0} caps=[{1}]" -f $lsidShow, ($arm.lcaps -join ','))
  Write-Host ("  connector ac={0} caps=[{1}]" -f $csidShow, ($arm.ccaps -join ','))
  Write-Host "  port      = $port"

  $ls = Start-Child $runnerKey $arm.lsid $arm.lcaps 'listen' $port $llog
  foreach ($l in ($ls.text -split "`n")) { Write-Host "  L:$($l.Trim())" }
  if ($ls.h -lt 0) {
    $script:results += @{ id = $id; verdict = 'LISTENER-LAUNCH-FAIL'; detail = $ls.text; expect = $arm.expect }
    return
  }

  # Sequenced on the listener's OWN ready line, never on a sleep: a connect fired before the
  # listen() would fail for a reason that has nothing to do with the token.
  $ready = $false
  for ($i = 0; $i -lt 150; $i++) {
    if ((Read-LogSafe $llog) -match 'listen:listening=1') { $ready = $true; break }
    Start-Sleep -Milliseconds 100
  }
  Write-Host "  listener-ready = $ready"

  $cs = Start-Child $runnerKey $arm.csid $arm.ccaps 'connect' $port $clog
  foreach ($l in ($cs.text -split "`n")) { Write-Host "  C:$($l.Trim())" }
  if ($cs.h -lt 0) {
    [void][Fx]::Kill($ls.h)
    $script:results += @{ id = $id; verdict = 'CONNECTOR-LAUNCH-FAIL'; detail = $cs.text; expect = $arm.expect }
    return
  }

  $wc = [Fx]::Wait($cs.h, 60000)
  $wl = [Fx]::Wait($ls.h, 35000)
  [void][Fx]::Kill($cs.h)
  [void][Fx]::Kill($ls.h)

  $lbody = (Read-LogSafe $llog).Trim()
  $cbody = (Read-LogSafe $clog).Trim()
  Write-Host "  listener exit  = $wl"
  foreach ($l in ($lbody -split "`n")) { if ($l.Trim()) { Write-Host "    L| $($l.Trim())" } }
  Write-Host "  connector exit = $wc"
  foreach ($l in ($cbody -split "`n")) { if ($l.Trim()) { Write-Host "    C| $($l.Trim())" } }

  $verdict = 'BLOCKED'
  $detail = ''
  if ($cbody -match 'connect:roundtrip=OK') { $verdict = 'CONNECTED' }
  elseif ($cbody -match 'connect:result=CONNECTED') { $verdict = 'CONNECTED-NO-DATA' }
  foreach ($l in ($cbody -split "`n")) {
    if ($l -match '^connect:result=') { $detail = $l.Trim() }
  }
  if (-not $ready) { $detail = "listener-never-ready; $detail" }
  Write-Host "  VERDICT   = $verdict  $detail"

  $script:results += @{
    id = $id; verdict = $verdict; detail = $detail; expect = $arm.expect
    ready = $ready; lbody = $lbody; cbody = $cbody
    lstart = $ls.text; cstart = $cs.text
    lsid = $arm.lsid; csid = $arm.csid; ccaps = ($arm.ccaps -join ',')
  }
}

if ($primary) {
  foreach ($a in $arms) { Run-Arm $a $primary }
  if ($cross) {
    # A second reading of the decisive cells through a different runtime. The exe reports the raw
    # Winsock number and node reports libuv's translation; agreement across both is what makes
    # the answer independent of one implementation's error handling.
    foreach ($a in ($arms | Where-Object { $_.id -in @('d-plain-to-plain', 'c-plain-to-ac', 'b-diff-package-sid', 'a-same-package-sid') })) {
      Run-Arm $a $cross
    }
  }
}

# ---------------------------------------------------------------- teardown

$exemptAfter = Show-Exempt 'after'

# The per-arm child logs are copied out before the stage is removed, so the run's raw evidence
# survives as an artifact and not only as inline transcript.
$outDir = Join-Path (Get-Location).Path 'w1-logs'
New-Item -ItemType Directory -Path $outDir -Force | Out-Null
Copy-Item -Path (Join-Path $stage '*.log') -Destination $outDir -Force -ErrorAction SilentlyContinue

[void][Fx]::BeginDeelevated()
foreach ($sid in @($P1, $P2)) {
  if ($sid -like 'S-1-15-2-*') { Write-Host ("ace revoke {0} = {1}" -f (Short $sid), [Fx]::SetAce($stage, $sid, 0, $REVOKE, $true)) }
}
Write-Host "profile1 delete = $([Fx]::DeleteProfile($name1))"
Write-Host "profile2 delete = $([Fx]::DeleteProfile($name2))"
[void][Fx]::EndDeelevated()
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue

# ---------------------------------------------------------------- controls and verdict

function Res([string]$id) { return ($script:results | Where-Object { $_.id -eq $id } | Select-Object -First 1) }

Write-Host ""
Write-Host "=== ARM TABLE ==="
Write-Host ("{0,-34} {1,-10} {2,-20} {3}" -f 'arm', 'expect', 'verdict', 'detail')
foreach ($r in $script:results) {
  Write-Host ("{0,-34} {1,-10} {2,-20} {3}" -f $r.id, $r.expect, $r.verdict, $r.detail)
}

Write-Host ""
Write-Host "=== CONTROLS ==="
$rd = Res "d-plain-to-plain/$primary"
$rc = Res "c-plain-to-ac/$primary"
$rb = Res "b-diff-package-sid/$primary"
$ra = Res "a-same-package-sid/$primary"

$rdShow = 'MISSING'; if ($rd) { $rdShow = "$($rd.verdict) $($rd.detail)" }
$rcShow = 'MISSING'; if ($rc) { $rcShow = "$($rc.verdict) $($rc.detail)" }
Check 'positive-control-plain-loopback-works' ($rd -and $rd.verdict -eq 'CONNECTED') "verdict=$rdShow"
Check 'negative-control-plain-listener-refuses-ac' ($rc -and $rc.verdict -notlike 'CONNECTED*') "verdict=$rcShow"
$notReady = @($script:results | Where-Object { -not $_.ready -and $_.verdict -notlike '*LAUNCH-FAIL' })
Check 'listeners-became-ready' ($notReady.Count -eq 0) ("not-ready=" + (($notReady | ForEach-Object { $_.id }) -join ','))

# Every AppContainer arm must SHOW the package sid it claims, from inside the child and from the
# launcher's read of the child's process handle.
$tokOk = $true
$tokDetail = @()
foreach ($r in $script:results) {
  if (-not $r.cbody) { continue }
  $wantSid = $r.csid
  $selfSid = ''
  foreach ($l in (($r.cbody + "`n" + $r.cstart) -split "`n")) {
    if ($l -match '^self:packageSid=(\S+)') { $selfSid = $Matches[1] }
  }
  $handleSid = ''
  foreach ($l in ($r.cstart -split "`n")) {
    if ($l -match 'childtoken:packageSid=(\S+)') { $handleSid = $Matches[1] }
  }
  $want = if ($wantSid) { $wantSid } else { 'none' }
  if ($handleSid -ne $want) { $tokOk = $false; $tokDetail += "$($r.id):handle=$handleSid want=$want" }
  if ($selfSid -and $selfSid -ne 'UNAVAILABLE-node-child' -and $selfSid -ne $want) {
    $tokOk = $false; $tokDetail += "$($r.id):self=$selfSid want=$want"
  }
}
Check 'connector-token-matches-its-arm' $tokOk ($tokDetail -join ' ')

# A requested capability that was silently dropped is the bug 5i exists to prevent.
$capOk = $true
$capDetail = @()
foreach ($r in $script:results) {
  if (-not $r.ccaps) { continue }
  $held = ''
  foreach ($l in ($r.cstart -split "`n")) { if ($l -match 'childtoken:capabilities=\[(.*)\]') { $held = $Matches[1] } }
  foreach ($c in ($r.ccaps -split ',')) {
    if ($held -notlike "*$c*") { $capOk = $false; $capDetail += "$($r.id):missing=$c held=[$held]" }
  }
}
Check 'requested-capabilities-really-held' $capOk ($capDetail -join ' ')

$exemptClean = ($exemptBefore -notmatch [regex]::Escape($P1)) -and ($exemptBefore -notmatch [regex]::Escape($P2)) `
  -and ($exemptAfter -notmatch [regex]::Escape($P1)) -and ($exemptAfter -notmatch [regex]::Escape($P2))
Check 'no-loopback-exemption-for-our-packages' $exemptClean "sids absent from CheckNetIsolation output before and after"

Write-Host ""
Write-Host "=== W1 VERDICT ==="
if ($ra -and $rb) {
  $w1 = ($ra.verdict -like 'CONNECTED*') -and ($rb.verdict -notlike 'CONNECTED*')
  $w1Show = 'DOES NOT HOLD'; if ($w1) { $w1Show = 'HOLDS' }
  Write-Host ("W1 same-package-sid loopback = {0}" -f $w1Show)
  Write-Host ("  a same sid  : {0}  {1}" -f $ra.verdict, $ra.detail)
  Write-Host ("  b diff sid  : {0}  {1}" -f $rb.verdict, $rb.detail)
} else {
  Write-Host "W1 = INCONCLUSIVE (arm a or b did not produce a result)"
}
$re = Res "e-w2-selfcap-to-plain/$primary"
$rf = Res "f-w2-peercap-cross-sid/$primary"
$reShow = 'MISSING'; if ($re) { $reShow = "$($re.verdict)  $($re.detail)" }
$rfShow = 'MISSING'; if ($rf) { $rfShow = "$($rf.verdict)  $($rf.detail)" }
Write-Host "W2 self-cap to plain listener   = $reShow"
Write-Host "W2 peer-cap across package sids = $rfShow"

Write-Host ""
Write-Host "FAILURES = $script:FAILURES"
Write-Host "NOTE: a BLOCKED arm is a RESULT, not a job failure. Only the CONTROL lines above decide"
Write-Host "      whether the table can be read at all."
