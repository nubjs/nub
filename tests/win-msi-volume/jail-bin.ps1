# QUESTION 3: does a nub-OWNED, ACE-able executable directory plus a SANITIZED `PATH` close the gap
# MECHANISM-FACTS §5j left open?
#
# §5j measured a dissociation on a stock MSI-installed Node. An AppContainer child CAN exec
# `C:\Program Files\nodejs\node.exe` — `CreateProcessW` opens the image in the CALLER's context, so
# the LowBox token's lack of rights on it cannot block creation — but it can read NOTHING in that
# tree afterwards. One variable, same interpreter: unconfined `npm --version` prints `11.16.0`;
# confined dies `Cannot find module '…\npm\bin\npm-cli.js'`, rc=1. And that tree cannot be ACE'd
# unprivileged: it is protected, owner SYSTEM, and carries no `S-1-15-2-*` at all.
#
# THE MECHANISM UNDER TEST. A directory under nub's own cache — which the user owns, so nub can ACE
# it with no privilege — holding the executables a jailed lifecycle script may invoke, with the
# child's `PATH` REWRITTEN to point only there. This is not a new idea so much as a deliberate version
# of an accident: `tests/win-bypass-traverse/probe.ps1:360` already copies `node.exe` into a granted
# directory, which is exactly why no prior arm ever hit §5j's defect.
#
# WHAT THIS PROBE HAS TO ESTABLISH, in the order the design depends on it:
#
#   1. WHETHER `node.exe` NEEDS COPYING AT ALL. §5j says the ambient image execs anyway, so the
#      hybrid arm pairs the AMBIENT MSI `node.exe` with a nub-owned npm tree. If that runs, the copy
#      payload is the ~12 MiB npm tree, not the ~88 MiB binary. The two arms differ ONLY in PATH
#      ORDER, and the child reports `process.execPath`, so which interpreter ran is measured rather
#      than assumed.
#
#   2. THAT THE ACE IS WHAT DOES THE WORK. The ungranted arm runs first, before any grant exists, on
#      the identical command line. Without it, a green jail-bin arm could be bypass-traverse, the
#      runner image, or an inherited ace — anything but the mechanism.
#
#   3. THAT A STABLE TRUSTEE SUFFICES. The grant is `S-1-15-2-1` (ALL APPLICATION PACKAGES), written
#      ONCE on the EMPTY directory before it is populated, so the 2,374 npm entries inherit it at
#      creation. If a zero-capability LowBox token reads through that, the per-launch ACE cost is
#      zero instead of a tree walk. The `ac-aap-only` arm withholds the per-run package sid entirely
#      and reads the DACL back to prove it was withheld.
#
#   4. THE READ QUESTION SEPARATED FROM THE PIPE QUESTION. Stock `npm.cmd` resolves its CLI through
#      `FOR /F ('CALL "%NODE_EXE%" npm-prefix.js')` — a PIPED sub-process — and §5h measured a piped
#      spawn under an AppContainer hanging indefinitely. So `…-npm-direct` runs `node npm-cli.js`
#      with no pipe anywhere; that is the read question alone. `…-npm-stock` is the pipe question and
#      is allowed to hang without taking the table with it.
#
#   5. WHAT AN ABSENT TOOL LOOKS LIKE. A sanitized PATH turns "MSVC/python is not granted" into
#      "python is not on PATH". The failure SHAPE is recorded because it is what nub must map to a
#      useful message.
#
#   6. WHETHER HARD LINKING IS A CHEAPER POPULATE. An NTFS hard link is a second directory entry on
#      the SAME MFT record and the security descriptor lives on the record — so an ace on the link
#      is an ace on the original. Measured in both directions rather than recalled.
#
# EVERY OP THE CONTROL SCRIPT PERFORMS IS `fs` OR `process.env` ONLY — no `child_process`, because a
# piped spawn under an AppContainer hangs indefinitely (§5h) and an unbounded probe has already
# destroyed a run. The controls also run BEFORE the tool under test on every arm, for the same
# reason: §5h lost a whole table to a hang and recovered it by moving the hanging op last.
#
# CI IS THE ONLY VENUE. An AppContainer cannot launch over OpenSSH — sshd lands in services session
# 0, which has no window station a LowBox token can attach to, and every launch returns 0xC0000142
# (§5e). The standing `nub-win` VM cannot answer this.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

. (Join-Path $PSScriptRoot 'jailbin-verdict.ps1')   # also dot-sources W/Fact/Prop/Cell via verdict.ps1
. (Join-Path $PSScriptRoot 'acjail.ps1')

W ''
W 'PROBE windows jail-bin (question 3: does a nub-owned ACE-able bin dir + sanitized PATH close 5j?)'
W ''
W '== host =='
$osci = Get-CimInstance Win32_OperatingSystem
Fact 'os' ($osci.Caption + ' build ' + [Environment]::OSVersion.Version.ToString())
Fact 'os-producttype' $osci.ProductType
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'whoami' (whoami)
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
# The privilege context, reported because a result without it settles nothing (two prior lanes
# differed on this and it invalidated a zero-setup claim). `admin=` is the ACCESS CHECK, which is the
# gate that matters; `TokenIsElevated` is NOT the gate (CreateRestrictedToken copies that flag).
Fact 'admin' $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Fact 'token-owner' $id.Owner
Fact 'integrity-level' (((whoami /groups 2>&1) | Select-String 'Mandatory Label') -join ' ')
Fact 'privileges-held' ([AcJail]::PrivsOf($id.Token.ToInt64()))
Fact 'session-id' (Get-Process -Id $PID).SessionId

$facts = @{}
$cells = @{}
$progFiles = 'C:\Program Files\nodejs'
$cmdExe = Join-Path $env:SystemRoot 'System32\cmd.exe'
$AAP = 'S-1-15-2-1'    # ALL APPLICATION PACKAGES — a STABLE trustee, unlike a per-run package sid
$ARAP = 'S-1-15-2-2'   # ALL RESTRICTED APPLICATION PACKAGES — used only as the de-elevated marker

# ─────────────── install a REAL MSI, so the ambient tree is genuinely un-ACE-able ───────────────
# Duplicated from `msi-node-acl.ps1` deliberately: this job must not depend on a sibling job's side
# effects, and the attribution arm is worthless against anything but the real protected descriptor.
# THIS STEP NEEDS ADMIN and that is not a finding about nub — a user consents to the same install.
# Nothing else in this probe needs it, which the de-elevated arm at the end measures.

W ''
W '== install a stock MSI (the un-ACE-able ambient tree the mechanism has to work around) =='
$msiOk = $false; $msiWhy = ''
try {
  $arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
  $idx = Invoke-RestMethod -Uri 'https://nodejs.org/dist/index.json' -TimeoutSec 60
  # `index.json` never lists `win-arm64-msi` for ANY release yet the artifact is served, so the index
  # picks the VERSION and a HEAD request decides whether it exists (msi-node-acl.ps1's run-1 bug).
  $ver = $null
  foreach ($r in @($idx | Where-Object { $_.lts })) {
    $v = $r.version.TrimStart('v')
    try {
      $h = Invoke-WebRequest -Uri "https://nodejs.org/dist/v$v/node-v$v-$arch.msi" -Method Head -TimeoutSec 30
      if ($h.StatusCode -eq 200) { $ver = $v; break }
    } catch { }
  }
  if (-not $ver) { throw "no LTS release serves a $arch .msi" }
  Fact 'msi-version' $ver
  $msiPath = Join-Path $env:TEMP "node-$ver-$arch.msi"
  Invoke-WebRequest -Uri "https://nodejs.org/dist/v$ver/node-v$ver-$arch.msi" -OutFile $msiPath -TimeoutSec 300
  $p = Start-Process -FilePath msiexec.exe -Wait -PassThru -ArgumentList @(
    '/i', "`"$msiPath`"", '/qn', '/norestart')
  Fact 'msiexec-exit' $p.ExitCode
  $msiOk = ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) -and (Test-Path -LiteralPath (Join-Path $progFiles 'node.exe'))
  if (-not $msiOk) { $msiWhy = "msiexec exit=$($p.ExitCode)" }
} catch { $msiWhy = "$_"; Fact 'msi-install-error' $msiWhy }
Fact 'msi-installed' "$msiOk $msiWhy"
$facts['msi-installed'] = [bool]$msiOk

$ambientNode = Join-Path $progFiles 'node.exe'
$ambientNpmCli = Join-Path $progFiles 'node_modules\npm\bin\npm-cli.js'
$null = Show-Dacl 'progfiles-after-msi' $progFiles

# ─────────────── the fixture: a nub-owned jail-bin, granted BEFORE it is populated ───────────────
# The order is the design: grant the EMPTY directory, then copy into it, so every one of the 2,374
# npm entries inherits the ace AT CREATION (§5e/§5h: `OICI`, flags=0x3). That turns the per-launch
# cost from a tree walk into nothing, and it is measured below as two timings rather than asserted.

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$root = Join-Path $env:USERPROFILE "nubjb-$nonce"
$jailBin = Join-Path $root 'jail-bin'
$scriptsDir = Join-Path $root 'scripts'
$ungrantedDir = Join-Path $root 'ungranted'
$deelevDir = Join-Path $root 'deelev-control'
New-Item -ItemType Directory -Force -Path $jailBin, $scriptsDir, $ungrantedDir, $deelevDir | Out-Null
Set-Content -LiteralPath (Join-Path $ungrantedDir 'secret.txt') -Value 'CANARY-UNGRANTED' -Encoding ascii
Fact 'fixture-root' $root

W ''
W '== cost: grant the EMPTY dir, then populate so children inherit =='
$swEmpty = [Diagnostics.Stopwatch]::StartNew()
Grant-AcAce $jailBin $AAP 'ReadAndExecute'
$swEmpty.Stop()
$facts['grant-empty-ms'] = $swEmpty.ElapsedMilliseconds
Fact 'grant-on-empty-jailbin-ms' $swEmpty.ElapsedMilliseconds
Grant-AcAce $scriptsDir $AAP 'ReadAndExecute'

# POPULATE. `node.exe` is copied so the "copy" variant exists at all; the hybrid arm is what decides
# whether that copy is actually necessary. The stock `npm.cmd`/`npx.cmd` come across verbatim because
# they are `%~dp0`-relative and therefore self-consistent inside the new dir.
$srcRoot = if ($msiOk) { $progFiles } else { Split-Path (Get-Command node -EA SilentlyContinue).Source }
Fact 'populate-source' $srcRoot
$swPop = [Diagnostics.Stopwatch]::StartNew()
Copy-Item -LiteralPath (Join-Path $srcRoot 'node.exe') -Destination (Join-Path $jailBin 'node.exe') -Force
foreach ($f in @('npm.cmd', 'npx.cmd')) {
  $s = Join-Path $srcRoot $f
  if (Test-Path -LiteralPath $s) { Copy-Item -LiteralPath $s -Destination (Join-Path $jailBin $f) -Force }
}
Copy-Item -LiteralPath (Join-Path $srcRoot 'node_modules') -Destination (Join-Path $jailBin 'node_modules') -Recurse -Force
$swPop.Stop()
$facts['populate-cold-ms'] = $swPop.ElapsedMilliseconds
$popItems = @(Get-ChildItem -LiteralPath $jailBin -Recurse -Force -EA SilentlyContinue)
$popBytes = ($popItems | Where-Object { -not $_.PSIsContainer } | Measure-Object -Property Length -Sum).Sum
Fact 'populate-cold-ms' $swPop.ElapsedMilliseconds
Fact 'populate-entries' $popItems.Count
Fact 'populate-bytes' $popBytes
Fact 'populate-node-exe-bytes' (Get-Item -LiteralPath (Join-Path $jailBin 'node.exe')).Length
$npmTree = @(Get-ChildItem -LiteralPath (Join-Path $jailBin 'node_modules') -Recurse -Force -EA SilentlyContinue)
Fact 'populate-npm-tree-entries' $npmTree.Count
Fact 'populate-npm-tree-bytes' (($npmTree | Where-Object { -not $_.PSIsContainer } | Measure-Object -Property Length -Sum).Sum)

# WARM: what a second install pays when the dir is already populated. Measured as the real skip
# decision a keyed cache makes (test-and-skip per entry), not as a re-copy.
$swWarm = [Diagnostics.Stopwatch]::StartNew()
$skipped = 0
foreach ($it in $popItems) { if (Test-Path -LiteralPath $it.FullName) { $skipped++ } }
$swWarm.Stop()
$facts['populate-warm-ms'] = $swWarm.ElapsedMilliseconds
Fact 'populate-warm-verify-ms' "$($swWarm.ElapsedMilliseconds) ($skipped entries confirmed present)"

# DID INHERITANCE ACTUALLY REACH THE DEEPEST ENTRY? The single control that separates "the design
# works" from "the grant silently covered only the directory".
$deepNpmCli = Join-Path $jailBin 'node_modules\npm\bin\npm-cli.js'
$facts['inherited-ace-on-deep-npm-cli'] = Get-AceRightsFor $deepNpmCli $AAP
Fact 'inherited-ace-on-deep-npm-cli' $facts['inherited-ace-on-deep-npm-cli']
Fact 'inherited-ace-on-jailbin-node-exe' (Get-AceRightsFor (Join-Path $jailBin 'node.exe') $AAP)

# For comparison only: re-granting the SAME ace on the now-populated tree, i.e. what a per-run
# package sid would cost on every launch.
$swReGrant = [Diagnostics.Stopwatch]::StartNew()
Grant-AcAce $jailBin $ARAP 'ReadAndExecute'
$swReGrant.Stop()
Fact 'grant-on-populated-tree-ms' $swReGrant.ElapsedMilliseconds
Revoke-AcAce $jailBin $ARAP

# nub-authored npm shim: no `FOR /F` prefix probe, so no piped sub-process and no PATH re-search.
# This is the shippable shape; stock `npm.cmd` is measured beside it as the thing it replaces.
@'
@ECHO OFF
"%~dp0node.exe" "%~dp0node_modules\npm\bin\npm-cli.js" %*
'@ | Set-Content -LiteralPath (Join-Path $jailBin 'npm-nub.cmd') -Encoding ascii

# The uniform control script every arm runs FIRST. `fs` + `process.env` ONLY — no `child_process`,
# because a piped spawn under an AppContainer hangs indefinitely (§5h). Authored as a LITERAL
# here-string with a placeholder: a double-quoted here-string would treat every backtick as an escape
# and expand every `$`, which silently produces a script that measures nothing.
$controlSrc = @'
const fs = require('fs');
function op(n, f) {
  try { const d = f(); process.stdout.write('op:' + n + '=OK ' + (d === undefined ? '' : d) + '\n'); }
  catch (e) { process.stdout.write('op:' + n + '=ERR ' + e.code + ' ' + e.message + '\n'); }
}
op('interp-execpath', function () { return process.execPath; });
op('path-raw', function () { return process.env.PATH; });
op('stat-c-root', function () { fs.statSync('C:\\'); return 'ok'; });
op('read-system32-hosts', function () {
  return fs.readFileSync(process.env.SystemRoot + '\\System32\\drivers\\etc\\hosts').length;
});
op('read-ungranted', function () { return fs.readFileSync('@@UNGRANTED@@', 'utf8').trim(); });
'@
$controlSrc = $controlSrc.Replace('@@UNGRANTED@@',
  (Join-Path $ungrantedDir 'secret.txt').Replace('\', '\\'))
Set-Content -LiteralPath (Join-Path $scriptsDir 'control.js') -Value $controlSrc -Encoding ascii
$controlJs = Join-Path $scriptsDir 'control.js'

# A real lifecycle script, in the shape a dependency ships one: a `.cmd` that shells `node -e` and
# then reaches for npm. `%~dp0`-relative to nothing outside the fixture.
@"
@ECHO OFF
echo op:lc-start=OK
node -e "process.stdout.write('op:lc-node-e=OK '+process.version+String.fromCharCode(10))"
node "$deepNpmCli" --version
if errorlevel 1 (echo op:lifecycle=ERR npm-cli-exited-nonzero) else (echo op:lifecycle=OK)
"@ | Set-Content -LiteralPath (Join-Path $scriptsDir 'lifecycle.cmd') -Encoding ascii
$lifecycleCmd = Join-Path $scriptsDir 'lifecycle.cmd'

# ─────────────── the sanitized environment ───────────────
# What the child gets INSTEAD of inheriting ours. `SystemRoot` is not optional — without it the
# loader and half of Win32 misbehave in ways that read as "the tool is broken".
# `PATH` is the whole point and is a REPLACEMENT, never a prepend: stock `npm.cmd`'s
# `IF NOT EXIST "%~dp0\node.exe" SET "NODE_EXE=node"` re-searches PATH, which is the same shim
# class that defeated a caller-side allowlist on macOS. A prepend would leave the original path
# reachable by exactly that fallback.
$osFloor = @(
  (Join-Path $env:SystemRoot 'System32'),
  $env:SystemRoot,
  (Join-Path $env:SystemRoot 'System32\Wbem'),
  (Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0')
) -join ';'
Fact 'os-floor-path' $osFloor

function New-SanitizedEnv([string]$pathValue) {
  # A deliberately small block. Anything not here is absent from the child by construction, which is
  # the env half of the same allowlist discipline `compiler/defaults.rs` applies on the nub side.
  @(
    "PATH=$pathValue",
    'PATHEXT=.COM;.EXE;.BAT;.CMD',
    "SystemRoot=$env:SystemRoot",
    "windir=$env:windir",
    "ComSpec=$cmdExe",
    "SystemDrive=$env:SystemDrive",
    "NUMBER_OF_PROCESSORS=$env:NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE=$env:PROCESSOR_ARCHITECTURE",
    'OS=Windows_NT',
    "TEMP=$scriptsDir",
    "TMP=$scriptsDir",
    # The profile POINTERS are present deliberately. They are not a hole: §5h measured `~/.ssh/id_rsa`
    # and `~/.npmrc` denied EPERM in every granting arm, so it is the ACE that denies the secrets, not
    # the absence of a variable naming their directory. Withholding them would instead make npm's own
    # `os.homedir()`/config bootstrap fail for a reason that has nothing to do with the mechanism —
    # an arm failing for the wrong reason is worse than an arm carrying one more variable.
    "USERPROFILE=$env:USERPROFILE",
    "APPDATA=$env:APPDATA",
    "LOCALAPPDATA=$env:LOCALAPPDATA"
  ) -join "`n"
}

# ─────────────── the arm driver ───────────────

New-Item -ItemType Directory -Force -Path (Join-Path $PWD 'jb-logs') | Out-Null

function Invoke-JbArm {
  param(
    [string]$Name,
    [bool]$AppContainer,
    [string]$PathValue,
    [string]$Tool,            # the cmd expression under test; must emit its own op: line
    [switch]$NoControl,       # PATH has no node, so the control script cannot run
    [switch]$SkipJailBinAce,  # withhold the per-run package sid from jail-bin (AAP-only arm)
    [switch]$NoAceAtAll,      # jail-bin carries NO appcontainer ace at all (the attribution arm)
    [int]$TimeoutMs = 90000
  )
  W ''
  W "== arm $Name =="
  $sid = ''; $profileName = ''
  if ($AppContainer) {
    $profileName = "nubjb_${nonce}_$($Name -replace '[^A-Za-z0-9]','_')"
    if ($profileName.Length -gt 60) { $profileName = $profileName.Substring(0, 60) }
    $sid = [AcJail]::CreateProfile($profileName)
    Fact "$Name/profile" "$profileName -> $sid"
    if ($sid -like 'ERR*') { $script:fails++; return }
  }
  $granted = @()
  $aapPulled = $false
  try {
    if ($AppContainer) {
      if ($NoAceAtAll) {
        # The attribution arm: strip the standing AAP grant for the duration of this launch, so the
        # ONE variable against every other arm is whether jail-bin carries any appcontainer ace.
        Revoke-AcAce $jailBin $AAP
        $aapPulled = $true
      } elseif (-not $SkipJailBinAce) {
        try { Grant-AcAce $jailBin $sid 'ReadAndExecute'; $granted += $jailBin } catch { Fact "$Name/grant-failed" "$jailBin : $_" }
      }
      # `scripts` always carries the per-run sid: the control script and the lifecycle `.cmd` must be
      # reachable in every arm, or a missing control would be indistinguishable from a failed one.
      try { Grant-AcAce $scriptsDir $sid 'ReadAndExecute'; $granted += $scriptsDir } catch { Fact "$Name/grant-failed" "$scriptsDir : $_" }
      Fact "$Name/jailbin-ace-aap" (Get-AceRightsFor $jailBin $AAP)
      Fact "$Name/jailbin-ace-perrun" (Get-AceRightsFor $jailBin $sid)
      # The mandatory read-back on the DECISIVE target, five levels down: without it a propagation
      # slip is indistinguishable from a kernel denial.
      Fact "$Name/deep-npm-cli-ace" ((Get-AceRightsFor $deepNpmCli $AAP) + ' | perrun=' + (Get-AceRightsFor $deepNpmCli $sid))
      if ($Name -eq 'ac-aap-only') {
        $facts['aap-only-perrun-ace'] = Get-AceRightsFor $jailBin $sid
        $facts['aap-only-aap-ace'] = Get-AceRightsFor $jailBin $AAP
      }
      if ($Name -eq 'ac-jailbin-ungranted') {
        $facts['ungranted-arm-aap-ace'] = Get-AceRightsFor $jailBin $AAP
        $facts['ungranted-arm-deep-ace'] = (Get-AceRightsFor $deepNpmCli $AAP) +
          ' | perrun=' + (Get-AceRightsFor $deepNpmCli $sid)
      }
    }

    $inner = if ($NoControl) { $Tool } else { 'node "' + $controlJs + '" & ' + $Tool }
    $cmdline = '"' + $cmdExe + '" /s /c "' + $inner + '"'
    $envLf = New-SanitizedEnv $PathValue
    $log = Join-Path (Join-Path $PWD 'jb-logs') "$Name.log"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $status = [AcJail]::Launch($sid, $cmdExe, $cmdline, $scriptsDir, $log, [uint32]$TimeoutMs, '', '', $envLf)
    $sw.Stop()
    Fact "$Name/launch" "$status in $($sw.ElapsedMilliseconds)ms"
    Fact "$Name/path-given" $PathValue
    Fact "$Name/cmdline" $cmdline

    $lines = @()
    if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -EA SilentlyContinue) }
    Fact "$Name/log-lines" $lines.Count
    $arm = @{}
    foreach ($l in $lines) {
      W ("    | " + $l)
      if ($l -match '^op:([^=]+)=(OK|ERR)\s*(.*)$') { $arm[$Matches[1]] = @($Matches[2], $Matches[3]) }
    }
    # `path-seen` is DERIVED, not reported by the child: the child cannot know what the arm intended.
    # OK means the jail-bin dir is present on the PATH the child actually saw AND the MSI install dir
    # is absent from it — the two halves that make "resolved from jail-bin" a measurement.
    if ($arm.ContainsKey('path-raw')) {
      $raw = [string]$arm['path-raw'][1]
      $hasJb = ($raw -like "*$jailBin*")
      $hasPf = ($raw -like "*$progFiles*")
      # OK means the child's PATH is EXACTLY the block we handed it — i.e. the environment block took
      # effect and nothing was inherited. The jailbin/progfiles booleans ride along as the detail so a
      # reader can see the composition without re-deriving it, but they are not the pass condition:
      # the hybrid arm deliberately carries BOTH and must not read as a control failure.
      $arm['path-seen'] = @($(if ($raw -eq $PathValue) { 'OK' } else { 'ERR' }),
        "jailbin=$hasJb progfiles=$hasPf")
    }
    $arm['__launch'] = @($status, '')
    $arm['__lines'] = @($lines.Count, '')
    $cells[$Name] = $arm
    return ($lines -join "`n")
  }
  finally {
    foreach ($p in $granted) { try { Revoke-AcAce $p $sid } catch { W "    revoke failed on $p : $_" } }
    if ($aapPulled) { Grant-AcAce $jailBin $AAP 'ReadAndExecute' }
    if ($profileName) { Fact "$Name/profile-deleted" ([AcJail]::DeleteProfile($profileName)) }
  }
}

function T([string]$op, [string]$expr) { '( ' + $expr + ' ) && echo op:' + $op + '=OK || echo op:' + $op + '=ERR' }

$pathJailBin = "$jailBin;$osFloor"
$pathJailBinOnly = $jailBin
$pathAmbient = "$progFiles;$osFloor"
$pathHybrid = "$progFiles;$jailBin;$osFloor"
$pathFloor = $osFloor
$directNpm = T 'npm-direct' ('node "' + $deepNpmCli + '" --version')

# 1-2. UNCONFINED baselines. Same command lines, same sanitized env. If these fail the fixture is
#      broken and nothing below is readable.
$null = Invoke-JbArm -Name 'plain-jailbin' -AppContainer $false -PathValue $pathJailBin -Tool (T 'npm-version' 'npm --version')
$null = Invoke-JbArm -Name 'plain-ambient' -AppContainer $false -PathValue $pathAmbient -Tool (T 'npm-version' 'npm --version')

# 3. Does `cmd.exe` run confined at all? Every cell here is resolved BY cmd, so a dead shell would
#    read as a missing tool. No control script — this arm's PATH has no node by design.
$null = Invoke-JbArm -Name 'ac-cmd-floor' -AppContainer $true -PathValue $pathFloor -Tool 'echo op:cmd-runs=OK' -NoControl

# 4. ATTRIBUTION. §5j's failure, re-measured in THIS run: confined, PATH = the MSI install dir.
$null = Invoke-JbArm -Name 'ac-ambient' -AppContainer $true -PathValue $pathAmbient -Tool (T 'npm-version' 'npm --version')

# 5. ATTRIBUTION FOR THE MECHANISM ITSELF. Identical to arm 8 except jail-bin carries NO
#    appcontainer ace. Must FAIL, or a green arm 8 is not the grant's doing.
$null = Invoke-JbArm -Name 'ac-jailbin-ungranted' -AppContainer $true -PathValue $pathJailBin -Tool $directNpm -NoAceAtAll

# 6. `node` resolved from jail-bin with NO OS floor on PATH at all — the tightest PATH the design
#    could ship, measured rather than assumed to be sufficient.
$null = Invoke-JbArm -Name 'ac-jailbin-node' -AppContainer $true -PathValue $pathJailBinOnly -Tool (T 'node-version' 'node --version')

# 7. THE READ CELL, isolated from the pipe question. This is what §5j measured FAILING on the MSI tree.
$null = Invoke-JbArm -Name 'ac-jailbin-npm-direct' -AppContainer $true -PathValue $pathJailBin -Tool $directNpm

# 8. ⭐ THE HYBRID: the AMBIENT MSI node.exe as interpreter (PATH order is the only variable vs arm 7)
#    with a nub-owned npm tree. A pass here means the ~88 MiB binary does not need copying.
$null = Invoke-JbArm -Name 'ac-hybrid-npm-direct' -AppContainer $true -PathValue $pathHybrid -Tool $directNpm

# 9. The zero-per-launch-cost shape: the standing AAP ace only, per-run package sid WITHHELD.
$null = Invoke-JbArm -Name 'ac-aap-only' -AppContainer $true -PathValue $pathJailBin -Tool $directNpm -SkipJailBinAce

# 10. Stock npm.cmd — the `FOR /F` piped prefix probe. Allowed to hang; the controls ran first.
$null = Invoke-JbArm -Name 'ac-jailbin-npm-stock' -AppContainer $true -PathValue $pathJailBin -Tool (T 'npm-version' 'npm --version')

# 11. The nub-authored one-line shim: same job, no pipe, no PATH re-search.
$null = Invoke-JbArm -Name 'ac-jailbin-npm-nubshim' -AppContainer $true -PathValue $pathJailBin -Tool (T 'npm-version' 'npm-nub --version')

# 12. NEGATIVE: a tool nub did not put in jail-bin. The failure SHAPE is the deliverable.
$pyOut = Invoke-JbArm -Name 'ac-absent-python' -AppContainer $true -PathValue $pathJailBin -Tool (T 'python-version' 'python --version')
$facts['absent-tool-shape'] = 'other'
if ($pyOut -match 'is not recognized') { $facts['absent-tool-shape'] = 'not-recognized' }
elseif ($pyOut -match 'Access is denied|EPERM|EACCES') { $facts['absent-tool-shape'] = 'access-denied' }
elseif ($pyOut -match 'cannot find|ENOENT') { $facts['absent-tool-shape'] = 'not-found' }
Fact 'absent-tool-failure-shape' $facts['absent-tool-shape']

# 13. A real lifecycle `.cmd`, end to end, under the sanitized PATH.
$null = Invoke-JbArm -Name 'ac-lifecycle' -AppContainer $true -PathValue $pathJailBin -Tool ('"' + $lifecycleCmd + '"')

# The ungranted-sibling cell is read off the uniform control in arm 6 rather than costing its own
# launch — every arm performs it, so any arm's row proves the grant still scopes.
if ($cells.ContainsKey('ac-jailbin-node')) {
  $cells['ac-ungranted'] = @{
    'read-ungranted' = $cells['ac-jailbin-node']['read-ungranted']
    '__launch' = @('(derived from ac-jailbin-node)', '')
    '__lines' = @(0, '')
  }
}

# ─────────────── the hardlink hazard, no launch needed ───────────────
# An NTFS hard link is a second directory entry on the SAME MFT record, and the security descriptor
# lives on the record. So an ace written "on the link" is an ace on the ORIGINAL path. If that holds,
# populating jail-bin by hard link would silently extend the confined child's read grant to wherever
# else the file lives — a grant LEAK, not a saving.

W ''
W '== hardlink: does an ace on the link reach the original? =='
$hlOrig = Join-Path $ungrantedDir 'hl-origin.txt'
Set-Content -LiteralPath $hlOrig -Value 'HL-CANARY' -Encoding ascii
$hlLink = Join-Path $jailBin 'hl-link.txt'
$facts['hardlink-create'] = [AcJail]::HardLink($hlLink, $hlOrig)
Fact 'hardlink-create' $facts['hardlink-create']
$facts['hardlink-original-ace'] = '(not attempted)'
$facts['hardlink-link-ace'] = '(not attempted)'
$facts['hardlink-leak'] = $null
if ($facts['hardlink-create'] -eq 'OK') {
  # BEFORE, so a pre-existing inherited ace cannot be mistaken for the leak.
  Fact 'hardlink-original-ace-before' (Get-AceRightsFor $hlOrig $ARAP)
  try { Grant-AcAce $hlLink $ARAP 'ReadAndExecute'; Fact 'hardlink-ace-write' 'OK' }
  catch { Fact 'hardlink-ace-write' "ERR $($_.Exception.GetType().Name)" }
  $facts['hardlink-link-ace'] = Get-AceRightsFor $hlLink $ARAP
  $facts['hardlink-original-ace'] = Get-AceRightsFor $hlOrig $ARAP
  $facts['hardlink-leak'] = ($facts['hardlink-original-ace'] -ne 'none')
  Fact 'hardlink-link-ace-after' $facts['hardlink-link-ace']
  Fact 'hardlink-original-ace-after' $facts['hardlink-original-ace']
  try { Revoke-AcAce $hlLink $ARAP } catch { }
}
# The direction that would matter for a real populate: a hard link to the MSI `node.exe`. Same
# volume, so the link itself may well be creatable; the ace on it is the question, and a SUCCESS here
# would be a hole in the MSI's protected descriptor rather than a feature.
$hlPf = Join-Path $jailBin 'hl-progfiles-node.exe'
$facts['hardlink-pf-create'] = if ($msiOk) { [AcJail]::HardLink($hlPf, $ambientNode) } else { '(no msi)' }
Fact 'hardlink-to-progfiles-node' $facts['hardlink-pf-create']
$facts['hardlink-pf-ace-write'] = '(not attempted)'
$facts['hardlink-pf-original-ace'] = '(not attempted)'
if ($facts['hardlink-pf-create'] -eq 'OK') {
  try { Grant-AcAce $hlPf $ARAP 'ReadAndExecute'; $facts['hardlink-pf-ace-write'] = 'OK' }
  catch { $facts['hardlink-pf-ace-write'] = "ERR $($_.Exception.GetType().Name)" }
  $facts['hardlink-pf-original-ace'] = Get-AceRightsFor $ambientNode $ARAP
  if ($facts['hardlink-pf-ace-write'] -eq 'OK') { try { Revoke-AcAce $hlPf $ARAP } catch { } }
  Remove-Item -LiteralPath $hlPf -Force -EA SilentlyContinue
}
Fact 'hardlink-progfiles-ace-write' $facts['hardlink-pf-ace-write']
Fact 'hardlink-progfiles-original-ace-after' $facts['hardlink-pf-original-ace']

# ─────────────── zero setup: can nub ACE its own jail-bin with NO admin? ───────────────
# Measured under an impersonated admin-STRIPPED token whose privileges are DELETED (not merely
# disabled — `DISABLE_MAX_PRIVILEGE` leaves them held and .NET's Set-Acl re-enables what it needs,
# which produced a false positive in `msi-node-acl.ps1`'s run 1). The control that makes it
# attributable is in the same impersonation: the same write on `C:\Program Files\nodejs` must be
# REFUSED, and the descriptor is read back rather than the return value trusted.

W ''
W '== de-elevated: the jail-bin ace write with admin stripped and privileges deleted =='
$facts['deelev-jailbin-grant'] = '(not attempted)'
$facts['deelev-jailbin-landed'] = '(not attempted)'
$facts['deelev-pf-grant'] = '(not attempted)'
$facts['deelev-admin'] = '(not attempted)'
$facts['deelev-privs'] = '(not attempted)'
$tokPtr = [AcJail]::DeElevatedToken($true)
Fact 'deelev/token' $(if ($tokPtr -ne 0) { 'created' } else { "FAILED err=$([AcJail]::LastErr)" })
if ($tokPtr -ne 0) {
  $facts['deelev-privs'] = [AcJail]::PrivsOf($tokPtr)
  $h = New-Object Microsoft.Win32.SafeHandles.SafeAccessTokenHandle([IntPtr]$tokPtr)
  try {
    $res = [Security.Principal.WindowsIdentity]::RunImpersonated($h, [Func[object]]{
      $out = @{}
      $wp = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
      $out['admin'] = $wp.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
      try { Grant-AcAce $deelevDir $ARAP 'ReadAndExecute'; $out['own'] = 'OK' }
      catch { $out['own'] = "ERR $($_.Exception.GetType().Name)" }
      try { Grant-AcAce $progFiles $ARAP 'ReadAndExecute'; $out['pf'] = 'OK' }
      catch { $out['pf'] = "ERR $($_.Exception.GetType().Name)" }
      return $out
    })
    $facts['deelev-admin'] = $res['admin']
    $facts['deelev-jailbin-grant'] = $res['own']
    $facts['deelev-pf-grant'] = $res['pf']
  } catch { Fact 'deelev/impersonation-error' "$_" }
  finally { $h.Dispose() }
  # From OUTSIDE the impersonation: what the descriptor SAYS, not what the write returned.
  $facts['deelev-jailbin-landed'] = Get-AceRightsFor $deelevDir $ARAP
  Fact 'deelev/admin-under-impersonation' $facts['deelev-admin']
  Fact 'deelev/privileges-held' $facts['deelev-privs']
  Fact 'deelev/write-on-nub-owned-dir' $facts['deelev-jailbin-grant']
  Fact 'deelev/ace-actually-on-disk' $facts['deelev-jailbin-landed']
  Fact 'deelev/write-on-program-files-nodejs' $facts['deelev-pf-grant']
  Fact 'deelev/progfiles-ace-on-disk' (Get-AceRightsFor $progFiles $ARAP)
  if ($facts['deelev-pf-grant'] -eq 'OK') {
    try { Revoke-AcAce $progFiles $ARAP; Fact 'deelev/progfiles-reverted' 'yes' }
    catch { Fact 'deelev/progfiles-reverted' "FAILED $_" }
  }
}

# ── residue: nothing this probe did may leave an appcontainer ace on an admin-owned path.
$residue = @()
try {
  $residue = @((Get-Acl -LiteralPath $progFiles).Access |
    Where-Object { $_.IdentityReference.Value -like 'S-1-15-*' } |
    ForEach-Object { "$($_.IdentityReference.Value)=$($_.FileSystemRights)" })
} catch { $residue = @("unreadable:$_") }
$facts['pf-residue'] = $(if ($residue.Count) { ($residue -join ' ; ') } else { 'none' })
Fact 'progfiles-appcontainer-aces-after-probe' $facts['pf-residue']

# ─────────────── verdict ───────────────

$null = Invoke-JailBinVerdict -Cells $cells -Facts $facts

W ''
W '== cost summary =='
Fact 'cost/grant-empty-dir-ms' $facts['grant-empty-ms']
Fact 'cost/populate-cold-ms' $facts['populate-cold-ms']
Fact 'cost/populate-warm-verify-ms' $facts['populate-warm-ms']
Fact 'cost/payload-bytes' $popBytes
Fact 'cost/payload-entries' $popItems.Count

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED" }

Remove-Item -LiteralPath $root -Recurse -Force -EA SilentlyContinue
W ''
W 'PROBE END'
