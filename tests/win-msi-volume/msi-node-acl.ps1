# QUESTION 1: can an AppContainer child even EXEC `node.exe` on a stock, MSI-installed Node?
#
# `nodejs/node#63590` (open, filed 2026-05-26) reports that the MSI's `SetInstallDirPermission`
# component writes a PROTECTED explicit DACL on `C:\Program Files\nodejs` — WiX `<Permission>` maps
# to the MSI `LockPermissions` table, which REPLACES the descriptor rather than merging — granting
# only `[WIX_ACCOUNT_USERS]`, `[AUTHENTICATED_USERS]`, `[WIX_ACCOUNT_ADMINISTRATORS]` and
# `[WIX_ACCOUNT_LOCALSYSTEM]`. Confirmed in the source at
# `.repos/node/tools/msvs/msi/nodemsi/product.wxs:156-165`. No `S-1-15-2-*` sid appears, and because
# the descriptor is protected, the `ALL APPLICATION PACKAGES` ace that `C:\Program Files` does carry
# cannot propagate in.
#
# WHY THIS CAN INVALIDATE THE WHOLE ROUTE. MECHANISM-FACTS §5h established that an AppContainer
# child reads deep into `%USERPROFILE%` through un-ACE'd `C:\` and `C:\Users`, that `$HOME` secrets
# are denied, and that egress is denied at zero privilege. Every one of those results is downstream
# of a confined `node` actually STARTING. If the kernel refuses to execute the user's `node.exe`,
# none of it matters for that user.
#
# AND WHY §5h COULD NOT HAVE SEEN IT — the precise reason, which is NOT the one that was suspected.
# The suspicion was that a CI-provisioned Node carries different ACLs than an MSI one. That is true
# and is measured here, but it is not what hid the defect: `tests/win-bypass-traverse/probe.ps1:360`
# COPIES `node.exe` into a granted directory inside `%USERPROFILE%` and launches the copy, with an
# in-place comment saying granting `C:\Program Files\nodejs` "would need WRITE_DAC on a path the user
# does not own". So no prior arm ever executed an ambient Node under the jail at all. The question was
# side-stepped, not answered — which is why it needs its own probe rather than a re-read of §5h.
#
# THE ARMS, and why the pair is needed. Two failures must be told apart:
#   (a) `CreateProcessW` refusing to START the MSI node. Measured with `node -e`, so nothing but
#       `node.exe` and the System32 DLLs is read and a denial cannot be about a script file.
#   (b) a confined child being unable to READ that tree (`node_modules\npm`). Measured with a
#       DIFFERENT interpreter — a copy under the user's own store, the shape nub provisions — so the
#       read cells exist even when (a) fails outright.
#
# CI IS THE ONLY VENUE. An AppContainer cannot be launched over OpenSSH: sshd lands in services
# session 0, which has no window station a LowBox token can attach to, and every launch returns
# 0xC0000142 (§5e). So the standing `nub-win` VM cannot answer this and a runner must.
#
# EVERY PROBE IS TIMEOUT-BOUNDED and this child never spawns a piped process, because a piped spawn
# under an AppContainer hangs indefinitely (§5h) and an unbounded probe has already destroyed a run.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

. (Join-Path $PSScriptRoot 'msi-verdict.ps1')   # also dot-sources W/Fact/Prop/Cell
. (Join-Path $PSScriptRoot 'acjail.ps1')

W ''
W 'PROBE windows msi-node-acl (question 1: can an AppContainer exec a stock MSI node?)'
W ''
W '== host =='
$osci = Get-CimInstance Win32_OperatingSystem
Fact 'os' ($osci.Caption + ' build ' + [Environment]::OSVersion.Version.ToString())
Fact 'os-producttype' $osci.ProductType   # 1 = client (desktop), 3 = server
Fact 'ubr' (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -Name UBR -EA SilentlyContinue).UBR
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'whoami' (whoami)
$id = [Security.Principal.WindowsIdentity]::GetCurrent()
$pr = New-Object Security.Principal.WindowsPrincipal($id)
# THE PRIVILEGE CONTEXT, reported for every arm because a result without it cannot settle anything
# (two prior lanes differed on this and it invalidated a zero-setup claim). `admin=` is the ACCESS
# CHECK, which is the gate that matters; `TokenIsElevated` is reported beside it precisely because
# it is NOT the gate — `CreateRestrictedToken` copies that flag, so a de-elevated token still
# reports elevated=1 while holding no admin authority (§5h methodology note).
Fact 'admin' $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Fact 'token-is-elevated-flag' ([bool]([Security.Principal.WindowsIdentity]::GetCurrent().Owner -eq $null) -eq $false)
Fact 'session-id' (Get-Process -Id $PID).SessionId

# ─────────────── survey EVERY Node on the box, BEFORE touching anything ───────────────

W ''
W '== node installs found on this image, BEFORE the MSI install =='
$progFiles = 'C:\Program Files\nodejs'
Fact 'where-node' ((& where.exe node 2>&1) -join ' ; ')
Fact 'progfiles-nodejs-exists-before' (Test-Path -LiteralPath $progFiles)
$null = Show-Dacl 'progfiles-before' $progFiles

# How was the pre-existing one provisioned? An MSI product entry is the evidence; a runner image that
# unzipped a tarball leaves none, and that difference is precisely what could hide this defect.
$uninst = @()
foreach ($hive in @('HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
                    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*')) {
  $uninst += @(Get-ItemProperty $hive -EA SilentlyContinue |
    Where-Object { $_.DisplayName -like '*Node.js*' } |
    ForEach-Object { "$($_.DisplayName) v$($_.DisplayVersion) src=$($_.InstallSource) key=$($_.PSChildName)" })
}
Fact 'node-msi-product-entries-before' $(if ($uninst.Count) { ($uninst -join ' ; ') } else { '(none — not MSI-installed)' })

$toolcacheRoot = 'C:\hostedtoolcache\windows\node'
$toolcacheNode = $null
if (Test-Path -LiteralPath $toolcacheRoot) {
  $cand = @(Get-ChildItem -LiteralPath $toolcacheRoot -Directory -EA SilentlyContinue |
    Sort-Object Name -Descending | ForEach-Object {
      @(Get-ChildItem -LiteralPath $_.FullName -Directory -EA SilentlyContinue |
        ForEach-Object { Join-Path $_.FullName 'node.exe' }) } | Where-Object { Test-Path -LiteralPath $_ })
  if ($cand.Count) { $toolcacheNode = $cand[0] }
}
Fact 'toolcache-node' $(if ($toolcacheNode) { $toolcacheNode } else { '(none — setup-node did not run)' })
$tcAcl = $null
if ($toolcacheNode) { $tcAcl = Show-Dacl 'toolcache' (Split-Path $toolcacheNode) }

# ─────────────── install a REAL MSI, and read the DACL it writes ───────────────
# The version is resolved from nodejs.org's own index so the arch-correct MSI is guaranteed to exist
# rather than guessed at, and so the probe does not rot when a hardcoded version is unpublished.

W ''
W '== install a stock MSI =='
$msiKey = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'win-arm64-msi' } else { 'win-x64-msi' }
Fact 'msi-file-key' $msiKey
$msiOk = $false
$msiWhy = ''
try {
  $idx = Invoke-RestMethod -Uri 'https://nodejs.org/dist/index.json' -TimeoutSec 60
  $pick = @($idx | Where-Object { $_.lts -and ($_.files -contains $msiKey) })[0]
  if (-not $pick) { $pick = @($idx | Where-Object { $_.files -contains $msiKey })[0] }
  if (-not $pick) { throw "no release in index.json lists $msiKey" }
  $ver = $pick.version.TrimStart('v')
  $arch = if ($msiKey -eq 'win-arm64-msi') { 'arm64' } else { 'x64' }
  $url = "https://nodejs.org/dist/v$ver/node-v$ver-$arch.msi"
  Fact 'msi-url' $url
  $msiPath = Join-Path $env:TEMP "node-$ver-$arch.msi"
  Invoke-WebRequest -Uri $url -OutFile $msiPath -TimeoutSec 300
  Fact 'msi-downloaded-bytes' (Get-Item -LiteralPath $msiPath).Length
  # /qn: no UI. THIS STEP NEEDS ADMIN, and that is not a finding about nub — a user installs Node
  # with the same consent. What matters is the DACL the installer leaves behind afterwards.
  $p = Start-Process -FilePath msiexec.exe -Wait -PassThru -ArgumentList @(
    '/i', "`"$msiPath`"", '/qn', '/norestart', '/L*v', "`"$env:TEMP\node-msi.log`"")
  Fact 'msiexec-exit' $p.ExitCode
  $msiOk = ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) -and (Test-Path -LiteralPath (Join-Path $progFiles 'node.exe'))
  if (-not $msiOk) { $msiWhy = "msiexec exit=$($p.ExitCode)" }
} catch {
  $msiWhy = "$_"
  Fact 'msi-install-error' $msiWhy
}
Fact 'msi-installed' "$msiOk $msiWhy"

W ''
W '== the DACL the MSI wrote =='
$msiAcl = Show-Dacl 'progfiles-after-msi' $progFiles
$null = Show-Dacl 'progfiles-node-exe' (Join-Path $progFiles 'node.exe')
$null = Show-Dacl 'progfiles-npm-tree' (Join-Path $progFiles 'node_modules\npm')
$null = Show-Dacl 'program-files-parent' 'C:\Program Files'

function Sids-Of($acl) {
  if ($null -eq $acl) { return @() }
  return @($acl.Access | ForEach-Object {
    try { $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value }
    catch { $_.IdentityReference.Value } })
}
$msiSids = Sids-Of $msiAcl
$facts = @{}
$facts['msi-has-aap'] = ($msiSids -contains 'S-1-15-2-1')
$facts['msi-has-arap'] = ($msiSids -contains 'S-1-15-2-2')
$facts['msi-protected'] = $(if ($msiAcl) { [bool]$msiAcl.AreAccessRulesProtected } else { $null })
$facts['msi-owner'] = $(if ($msiAcl) { $msiAcl.Owner } else { '(unreadable)' })
$facts['toolcache-has-aap'] = $(if ($tcAcl) { ((Sids-Of $tcAcl) -contains 'S-1-15-2-1') } else { $null })

# THE SIBLING CONTROL. "nodejs lacks the ace" is only a defect if sibling directories under the same
# parent HAVE it — otherwise it would just be how `C:\Program Files` works. Several siblings are
# sampled and the count reported, so the control does not hinge on one directory's happening to be
# packaged a particular way.
$sibs = @(Get-ChildItem -LiteralPath 'C:\Program Files' -Directory -EA SilentlyContinue |
  Where-Object { $_.Name -ne 'nodejs' } | Select-Object -First 12)
$sibWith = @()
foreach ($s in $sibs) {
  try { if ((Sids-Of (Get-Acl -LiteralPath $s.FullName)) -contains 'S-1-15-2-1') { $sibWith += $s.Name } } catch {}
}
$facts['sibling-has-aap'] = ($sibWith.Count -gt 0)
$facts['sibling-name'] = "$($sibWith.Count)/$($sibs.Count) sampled siblings carry it: $($sibWith -join ',')"
Fact 'siblings-with-all-application-packages' $facts['sibling-name']

# ─────────────── fixture: a granted script dir + a user-store node copy ───────────────
# The interpreter for the READ arm is a COPY under the user's own store, because that is the shape
# nub itself provisions (`crates/nub-core/src/node/discovery.rs:1104` — `<cache_dir>/node/`, i.e.
# `%USERPROFILE%\.cache\nub\node\<version>\`) and it is a path nub can ACE. It also isolates the
# Program Files denial: if this interpreter runs and the Program Files one does not, the difference
# is the DACL and nothing else.

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$root = Join-Path $env:USERPROFILE "nubmsi-$nonce"
$scriptsDir = Join-Path $root 'scripts'
$storeDir = Join-Path $root 'store\node\24.0.0'
New-Item -ItemType Directory -Force -Path $scriptsDir, $storeDir | Out-Null
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'child.js') -Destination (Join-Path $scriptsDir 'child.js') -Force
$srcNodeForCopy = if (Test-Path -LiteralPath (Join-Path $progFiles 'node.exe')) { Join-Path $progFiles 'node.exe' }
                  elseif ($toolcacheNode) { $toolcacheNode }
                  else { (Get-Command node -EA SilentlyContinue).Source }
Copy-Item -LiteralPath $srcNodeForCopy -Destination (Join-Path $storeDir 'node.exe') -Force
$storeNode = Join-Path $storeDir 'node.exe'
$childJs = Join-Path $scriptsDir 'child.js'
Fact 'fixture-root' $root
Fact 'store-node-copy' "$storeNode (copied from $srcNodeForCopy)"

# ─────────────── the arms ───────────────

$cells = @{}
$flags = '--preserve-symlinks-main --preserve-symlinks'
$inlineJs = "process.stdout.write('op:exec-inline=OK '+process.version+String.fromCharCode(10))"

function Invoke-MsiArm {
  param(
    [string]$Name,
    [bool]$AppContainer,
    [string]$Exe,
    [string]$Mode,             # 'inline' => node -e ; 'script' => node child.js
    [string[]]$GrantRX = @(),
    [string]$Targets = ''
  )
  W ''
  W "== arm $Name =="
  $sid = ''
  $profileName = ''
  if ($AppContainer) {
    $profileName = "nubmsi_${nonce}_$($Name -replace '[^A-Za-z0-9]','_')"
    if ($profileName.Length -gt 60) { $profileName = $profileName.Substring(0, 60) }
    $sid = [AcJail]::CreateProfile($profileName)
    Fact "$Name/profile" "$profileName -> $sid"
    if ($sid -like 'ERR*') { $script:fails++; return }
  }
  $granted = @()
  try {
    foreach ($p in $GrantRX) {
      try { Grant-AcAce $p $sid 'ReadAndExecute'; $granted += $p }
      catch { Fact "$Name/grant-failed" "$p : $_" }
    }
    Fact "$Name/grants" $(if ($granted.Count) { ($granted -join ' ; ') } else { '(none)' })
    # The mandatory read-back: the ace on the interpreter's own directory must have reached the exe.
    if ($AppContainer) { Fact "$Name/exe-dacl-for-ac-sid" (Get-AceRightsFor $Exe $sid) }

    $env:AJ_ARM = $Name
    $env:AJ_TARGETS = $Targets
    $log = Join-Path (Join-Path $PWD 'msi-logs') "$Name.log"
    $cmdline = if ($Mode -eq 'inline') { '"' + $Exe + '" ' + $flags + ' -e "' + $inlineJs + '"' }
               else { '"' + $Exe + '" ' + $flags + ' "' + $childJs + '"' }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    # 60 s, bounded: this child never spawns and never touches the network, so anything slower is a
    # hang and must be terminated rather than allowed to eat the job.
    $status = [AcJail]::Launch($sid, $Exe, $cmdline, $scriptsDir, $log, 60000, '', '')
    $sw.Stop()
    Fact "$Name/launch" "$status in $($sw.ElapsedMilliseconds)ms"
    Fact "$Name/cmdline" $cmdline

    $lines = @()
    if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -EA SilentlyContinue) }
    Fact "$Name/log-lines" $lines.Count
    $arm = @{}
    foreach ($l in $lines) {
      W ("    | " + $l)
      if ($l -match '^op:([^=]+)=(OK|ERR)\s*(.*)$') { $arm[$Matches[1]] = @($Matches[2], $Matches[3]) }
    }
    $arm['__launch'] = @($status, '')
    $arm['__lines'] = @($lines.Count, '')
    $cells[$Name] = $arm
  }
  finally {
    foreach ($p in $granted) { try { Revoke-AcAce $p $sid } catch { W "    revoke failed on $p : $_" } }
    if ($profileName) { Fact "$Name/profile-deleted" ([AcJail]::DeleteProfile($profileName)) }
  }
}

New-Item -ItemType Directory -Force -Path (Join-Path $PWD 'msi-logs') | Out-Null

# 1. CONTROL. Unconfined, same command line, MSI node. Must run.
Invoke-MsiArm -Name 'plain-progfiles' -AppContainer $false -Exe (Join-Path $progFiles 'node.exe') -Mode 'inline'

# 2. THE DECISIVE CELL. Confined, zero capabilities, NO ace anywhere on Program Files.
Invoke-MsiArm -Name 'ac-exec-progfiles' -AppContainer $true -Exe (Join-Path $progFiles 'node.exe') -Mode 'inline'

# 3. ONE VARIABLE vs arm 2: the AC sid granted ReadAndExecute on `C:\Program Files\nodejs`. This
#    REQUIRES ELEVATION (WRITE_DAC on an admin-owned path) and is here only to attribute arm 2's
#    result to the DACL — it is not a shippable repair, which the de-elevated arm below settles.
Invoke-MsiArm -Name 'ac-exec-progfiles-repaired' -AppContainer $true -Exe (Join-Path $progFiles 'node.exe') `
  -Mode 'inline' -GrantRX @($progFiles)

# 4. nub's OWN provisioned-Node shape: a copy under the user's store with an ace. Runs the full
#    child so this arm also carries the gate + positive controls for the whole probe.
Invoke-MsiArm -Name 'ac-exec-userstore' -AppContainer $true -Exe $storeNode -Mode 'script' `
  -GrantRX @($root) -Targets ''

# 5. The CI configuration, if setup-node ran: an unzipped toolcache Node, inheriting the toolcache
#    DACL instead of the MSI's LockPermissions descriptor.
if ($toolcacheNode) {
  Invoke-MsiArm -Name 'ac-exec-toolcache' -AppContainer $true -Exe $toolcacheNode -Mode 'inline'
} else {
  Fact 'ac-exec-toolcache/skipped' 'no toolcache node on this image'
}

# 6. THE READ HALF, with a working interpreter so the cells exist regardless of arm 2's outcome.
$tgt = @(
  "read-progfiles-node-exe|read|$(Join-Path $progFiles 'node.exe')",
  "stat-progfiles-node-exe|stat|$(Join-Path $progFiles 'node.exe')",
  "readdir-progfiles-dir|readdir|$progFiles",
  "read-npm-package-json|read|$(Join-Path $progFiles 'node_modules\npm\package.json')",
  "readdir-npm-tree|readdir|$(Join-Path $progFiles 'node_modules\npm')",
  "walk-from-progfiles|walk|$(Join-Path $progFiles 'node_modules\npm')"
) -join ';'
Invoke-MsiArm -Name 'ac-read-progfiles' -AppContainer $true -Exe $storeNode -Mode 'script' `
  -GrantRX @($root) -Targets $tgt

# ─────────────── could nub repair the DACL WITHOUT elevation? ───────────────
# Measured under an impersonated admin-stripped token rather than inferred from the DACL, with the
# control that makes a refusal attributable: the SAME impersonated write inside `%USERPROFILE%` must
# SUCCEED. Without that control, a failure could just mean the impersonation never took effect.

W ''
W '== unprivileged repair attempt (impersonated, admin stripped) =='
$facts['deelev-admin'] = '(not attempted)'
$facts['deelev-own-profile-write'] = '(not attempted)'
$facts['deelev-progfiles-write'] = '(not attempted)'
$tokPtr = [AcJail]::DeElevatedToken()
Fact 'deelev-token' $(if ($tokPtr -ne 0) { "created" } else { "FAILED err=$([AcJail]::LastErr)" })
if ($tokPtr -ne 0) {
  $probeSid = 'S-1-15-2-1'   # ALL APPLICATION PACKAGES: the exact ace #63590 says is missing
  $ownDir = Join-Path $root 'deelev-control'
  New-Item -ItemType Directory -Force -Path $ownDir | Out-Null
  $h = New-Object Microsoft.Win32.SafeHandles.SafeAccessTokenHandle([IntPtr]$tokPtr)
  try {
    $res = [Security.Principal.WindowsIdentity]::RunImpersonated($h, [Func[object]]{
      $out = @{}
      $wi = [Security.Principal.WindowsIdentity]::GetCurrent()
      $wp = New-Object Security.Principal.WindowsPrincipal($wi)
      $out['admin'] = $wp.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
      foreach ($pair in @(@('own', $ownDir), @('pf', $progFiles))) {
        try { Grant-AcAce $pair[1] $probeSid 'ReadAndExecute'; $out[$pair[0]] = 'OK' }
        catch { $out[$pair[0]] = "ERR $($_.Exception.GetType().Name): $($_.Exception.Message)" }
      }
      return $out
    })
    $facts['deelev-admin'] = $res['admin']
    $facts['deelev-own-profile-write'] = $res['own']
    $facts['deelev-progfiles-write'] = $res['pf']
  } catch { Fact 'deelev-impersonation-error' "$_" }
  finally { $h.Dispose() }
  # If the impersonated write DID land on Program Files, undo it — the probe must not leave a machine
  # more permissive than it found it.
  if ($facts['deelev-progfiles-write'] -eq 'OK') {
    try { Revoke-AcAce $progFiles $probeSid; Fact 'deelev-progfiles-ace-reverted' 'yes' }
    catch { Fact 'deelev-progfiles-ace-reverted' "FAILED $_" }
  }
}
Fact 'deelev/admin-under-impersonation' $facts['deelev-admin']
Fact 'deelev/write-inside-userprofile' $facts['deelev-own-profile-write']
Fact 'deelev/write-on-program-files-nodejs' $facts['deelev-progfiles-write']

# ── residue: the repaired arm's grant must be gone, or the probe left the box more permissive ──
$residue = Get-AceRightsFor $progFiles 'S-1-15-2-1'
Fact 'progfiles-all-application-packages-ace-after-probe' $residue
Prop 'probe-left-no-appcontainer-ace-on-program-files' ($residue -eq 'none') `
  "the repaired arm and the de-elevated attempt must both be fully reverted: $residue"

# ─────────────── verdict ───────────────

$null = Invoke-MsiVerdict -Cells $cells -Facts $facts

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED" }

Remove-Item -LiteralPath $root -Recurse -Force -EA SilentlyContinue
W ''
W 'PROBE END'
