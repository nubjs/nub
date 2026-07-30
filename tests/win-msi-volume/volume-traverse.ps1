# QUESTION 2: does the AppContainer bypass-traverse skip hold on a NON-LOCAL volume?
#
# MECHANISM-FACTS §5h measured a confined child reading five components deep under `%USERPROFILE%`
# with `C:\` and `C:\Users` carrying no ace, and closed with: "NOT ESTABLISHED HERE: which mechanism
# does the traverse skip. `windows.rs:412-423` credits `SeChangeNotifyPrivilege` AND
# `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL` on the volume device; both predict this observable and
# this probe cannot separate them. It also does not measure a non-`C:` volume, a network path, or a
# filter-driver device — the volume-flag hypothesis predicts traverse WOULD be enforced there."
#
# The shipping backend goes further than the measurement does, asserting the volume-flag reading
# outright: "Traverse would only be enforced on the rare device LACKING the volume flag — a custom
# filter-driver/redirector device, not where user/build files live." Developers keep projects on
# mapped drives and mounted volumes, so if that assertion is wrong the jail fails silently for them.
#
# THREE INDEPENDENT WAYS AT IT, because no single one is conclusive alone:
#   1. Run the SAME deep-read shape on four surfaces — local `C:`, a freshly formatted VHD volume, a
#      `subst`-mapped drive, and an SMB share — with the local `C:` arm in the same run as the
#      known-good anchor, so a failure elsewhere is attributable to the volume rather than the
#      harness.
#   2. READ the device Characteristics bitmask per volume
#      (`NtQueryVolumeInformationFile(FileFsDeviceInformation)`) and decode it against the constant
#      grepped out of the Windows SDK header ON THE RUNNER. The flag hypothesis is only
#      DISCRIMINATING if the flag actually differs across the volumes measured; if every one carries
#      it, a uniformly green table proves nothing about the mechanism and must not be read as if it
#      did.
#   3. DELETE `SeChangeNotifyPrivilege` from the launching token and re-run the ANCHOR arm. This
#      separates the two mechanisms without needing a second volume at all: if the skip is the
#      privilege, deep reads must stop working on the very volume where they currently work.
#
# CONTROLS — each rules out a specific alternative explanation, and the volume table is
# uninterpretable without them:
#   <vol>-plain       unconfined child, same path. Rules out a broken fixture / absent volume.
#   <vol>-rootgrant   confined child with an inheritable ace at the VOLUME ROOT, so no ancestor is
#                     ungranted. Rules out "this volume is unreachable under an AppContainer for a
#                     reason other than traverse" — network isolation on an SMB path, a per-session
#                     DOS device map on subst. Red here makes the treatment row meaningless.
#   <vol>-deepgrant   THE TREATMENT: ace only on the deep dir. Red WITH a green rootgrant is
#                     traverse being enforced.
#   per arm           the decisive target's DACL read back (a propagation slip must never be
#                     mistakable for a kernel denial) and an ungranted sibling on the same volume
#                     that must be DENIED (or the grant scopes nothing and a green read is vacuous).
#   cn-kept           the privilege differential's control: identical CreateProcessAsUser +
#                     restricted-token launch with the privilege RETAINED.
#
# The interpreter is a copy of `node.exe` in a granted directory under `%USERPROFILE%` for EVERY arm,
# including the non-local ones, so the volume under test is the only thing that varies. Putting the
# interpreter on the volume under test would confound its reachability with the target's.
#
# CI IS THE ONLY VENUE (AppContainer cannot launch over OpenSSH — session 0 has no window station,
# every launch returns 0xC0000142, §5e). Every launch is timeout-bounded and the child never spawns a
# piped process, because a piped spawn under an AppContainer hangs indefinitely (§5h).
#
# WHAT NEEDS ADMIN, reported per arm rather than glossed: creating and attaching a VHD (Virtual Disk
# Service) and creating an SMB share both do. That is NOT a claim about nub's own zero-setup
# requirement — the question is what the kernel does on a volume a developer ALREADY has, not whether
# nub can conjure one. `subst` needs no privilege at all.

$ErrorActionPreference = 'Continue'
Set-StrictMode -Off

. (Join-Path $PSScriptRoot 'vol-verdict.ps1')   # also dot-sources W/Fact/Prop/Cell
. (Join-Path $PSScriptRoot 'acjail.ps1')

W ''
W 'PROBE windows volume-traverse (question 2: does bypass-traverse hold off the local C: volume?)'
W ''
W '== host =='
$osci = Get-CimInstance Win32_OperatingSystem
Fact 'os' ($osci.Caption + ' build ' + [Environment]::OSVersion.Version.ToString())
Fact 'os-producttype' $osci.ProductType
Fact 'arch' $env:PROCESSOR_ARCHITECTURE
Fact 'whoami' (whoami)
$pr = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
$elevated = $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
# The privilege context, reported because a result without it cannot settle anything. `admin=` is the
# ACCESS CHECK, which is the gate that matters — never the `TokenIsElevated` flag, which
# `CreateRestrictedToken` copies (§5h methodology note).
Fact 'admin' $elevated
Fact 'session-id' (Get-Process -Id $PID).SessionId
Fact 'parent-token-privileges' ((whoami /priv 2>&1 | Select-String 'Se\w+Privilege' |
  ForEach-Object { ($_ -split '\s+')[0] }) -join ',')

# The SDK's own definition of the flag, so the decode below is checked against the header rather than
# against anyone's memory of the constant.
$facts = @{}
$facts['sdk-const-source'] = '(not found)'
$facts['sdk-const-value'] = '(not found)'
$hdrs = @(Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Include' -Recurse -Filter '*.h' `
  -EA SilentlyContinue -Include 'devioctl.h', 'ntifs.h', 'winioctl.h', 'wdm.h')
foreach ($h in $hdrs) {
  $m = Select-String -LiteralPath $h.FullName -Pattern 'FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL' -EA SilentlyContinue
  if ($m) {
    $facts['sdk-const-source'] = $h.FullName
    $facts['sdk-const-value'] = ($m[0].Line.Trim())
    break
  }
}
Fact 'sdk-const-source' $facts['sdk-const-source']
Fact 'sdk-const-value' $facts['sdk-const-value']
# Fallback decode used only when the header is unavailable on the image. Reported as such so a reader
# never mistakes a hardcoded guess for the SDK's answer.
$traverseBit = 0x00020000
if ($facts['sdk-const-value'] -match '0x([0-9a-fA-F]+)') { $traverseBit = [Convert]::ToUInt32($Matches[1], 16) }
Fact 'traverse-bit-used-for-decode' ('0x' + $traverseBit.ToString('x8') +
  $(if ($facts['sdk-const-value'] -eq '(not found)') { ' (FALLBACK — header not on this image)' } else { ' (from the SDK header)' }))

$nonce = [Guid]::NewGuid().ToString('N').Substring(0, 8)
$logDir = Join-Path $PWD 'vol-logs'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null

# ─────────────── the interpreter, held constant across every arm ───────────────
$runtimeDir = Join-Path $env:USERPROFILE "nubvt-$nonce-runtime"
New-Item -ItemType Directory -Force -Path $runtimeDir | Out-Null
$srcNode = (Get-Command node -EA SilentlyContinue).Source
if (-not $srcNode) { $srcNode = 'C:\Program Files\nodejs\node.exe' }
Copy-Item -LiteralPath $srcNode -Destination (Join-Path $runtimeDir 'node.exe') -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'child.js') -Destination (Join-Path $runtimeDir 'child.js') -Force
$jailNode = Join-Path $runtimeDir 'node.exe'
$jailChild = Join-Path $runtimeDir 'child.js'
Fact 'interpreter' "$jailNode (copied from $srcNode)"

# ─────────────── provision the volumes ───────────────
# Each volume ends up with an IDENTICAL fixture shape so the only variable is the volume:
#   <root>\projroot\proj\node_modules\dep\index.js     the decisive deep target
#   <root>\projroot\secret.txt                         the ungranted sibling (same volume)
# `deep grant` = an inheritable ace on `…\node_modules\dep` only; every ancestor stays ungranted.
# `root grant` = an inheritable ace at `<root>` itself, i.e. nothing ungranted above the target.

$vols = @{}   # name -> @{ root; deep; deepdir; sib; rootGrantPath; available; why; aclPath }

function New-Fixture([string]$name, [string]$root, [string]$aclRoot) {
  # $aclRoot is where ACEs are WRITTEN (the local backing path for a UNC volume, where it differs
  # from the path the child reads through). Reading the DACL back on the same object it was written
  # to is what makes the read-back a check on propagation rather than on the redirector.
  $projroot = Join-Path $root 'projroot'
  $deepdir = Join-Path $projroot 'proj\node_modules\dep'
  $deep = Join-Path $deepdir 'index.js'
  $sib = Join-Path $projroot 'secret.txt'
  $aclProjroot = Join-Path $aclRoot 'projroot'
  $aclDeepdir = Join-Path $aclProjroot 'proj\node_modules\dep'
  New-Item -ItemType Directory -Force -Path $aclDeepdir | Out-Null
  New-Item -ItemType Directory -Force -Path (Join-Path $aclProjroot 'proj\node_modules\dep2') | Out-Null
  "module.exports={marker:'deep-$name'};" |
    Set-Content -LiteralPath (Join-Path $aclDeepdir 'index.js') -Encoding ASCII
  "module.exports={marker:'bare'};" |
    Set-Content -LiteralPath (Join-Path $aclProjroot 'proj\node_modules\dep2\index.js') -Encoding ASCII
  "top-secret-$name" | Set-Content -LiteralPath (Join-Path $aclProjroot 'secret.txt') -Encoding ASCII
  return @{
    root = $root; deep = $deep; deepdir = $deepdir; sib = $sib
    aclRoot = $aclRoot; aclDeep = (Join-Path $aclDeepdir 'index.js'); aclDeepdir = $aclDeepdir
    available = $true; why = 'created'
  }
}

# ── local C: — the ANCHOR. §5h's known-good shape, re-measured in this run.
# NOTE the honest limit: `C:\` and `C:\Users` cannot be ACE'd, so this volume's "root grant" arm
# grants at the fixture root (the highest directory the user owns) rather than at the volume root.
# It is therefore §5h's shape rather than a true whole-chain grant, and it is labelled as such.
$localRoot = Join-Path $env:USERPROFILE "nubvt-$nonce-local"
$vols['local-c'] = New-Fixture 'local-c' $localRoot $localRoot
$vols['local-c']['rootGrantPath'] = $localRoot
$vols['local-c']['why'] = 'C:\ and C:\Users are un-ACE-able by design, so the root-grant arm grants at the fixture root — §5h shape, not a whole-chain grant'

# ── a freshly formatted VHD volume. `create vdisk` needs the Virtual Disk Service, i.e. admin.
W ''
W '== provision a VHD volume =='
$vhdPath = Join-Path $env:USERPROFILE "nubvt-$nonce.vhdx"
$vhdLetter = $null
foreach ($L in @('V', 'W', 'Y', 'Z')) { if (-not (Test-Path "${L}:\")) { $vhdLetter = $L; break } }
Fact 'vhd-letter-chosen' $vhdLetter
if ($elevated -and $vhdLetter) {
  $dp = @(
    "create vdisk file=`"$vhdPath`" maximum=512 type=expandable",
    "select vdisk file=`"$vhdPath`"",
    'attach vdisk',
    'convert mbr',
    'create partition primary',
    'format fs=ntfs quick label=NUBVT',
    "assign letter=$vhdLetter"
  ) -join "`r`n"
  $dpFile = Join-Path $env:TEMP "nubvt-$nonce.dp"
  Set-Content -LiteralPath $dpFile -Value $dp -Encoding ASCII
  $out = (& diskpart.exe /s $dpFile 2>&1) -join ' | '
  Fact 'diskpart-output' $out
  Start-Sleep -Seconds 3
}
if ($vhdLetter -and (Test-Path "${vhdLetter}:\")) {
  $vhdRoot = "${vhdLetter}:\"
  $vols['vhd'] = New-Fixture 'vhd' $vhdRoot $vhdRoot
  $vols['vhd']['rootGrantPath'] = $vhdRoot
  $null = Show-Dacl 'vhd-volume-root' $vhdRoot
} else {
  $vols['vhd'] = @{ available = $false; why = "not created (admin=$elevated letter=$vhdLetter)"; root = "${vhdLetter}:\" }
}

# ── a subst-mapped drive. NO privilege needed (DefineDosDevice on the user's own device map), which
# also makes it the arm that most closely matches a developer's everyday mapped project drive.
# It resolves to the SAME NTFS volume as C:, so a failure here isolates the AppContainer's DOS device
# map rather than any property of the volume.
W ''
W '== provision a subst drive =='
$substBack = Join-Path $env:USERPROFILE "nubvt-$nonce-subst"
New-Item -ItemType Directory -Force -Path $substBack | Out-Null
$substLetter = $null
foreach ($L in @('S', 'T', 'U', 'X')) { if (-not (Test-Path "${L}:\")) { $substLetter = $L; break } }
Fact 'subst-letter-chosen' $substLetter
if ($substLetter) {
  $so = (& cmd.exe /c "subst ${substLetter}: `"$substBack`"" 2>&1) -join ' | '
  Fact 'subst-output' $(if ($so) { $so } else { '(silent success)' })
}
if ($substLetter -and (Test-Path "${substLetter}:\")) {
  # ACEs are written through the BACKING path and read through the subst letter, which is the whole
  # point: same object, two namespaces.
  $vols['subst'] = New-Fixture 'subst' "${substLetter}:\" $substBack
  $vols['subst']['rootGrantPath'] = $substBack
} else {
  $vols['subst'] = @{ available = $false; why = "subst failed (letter=$substLetter)"; root = "${substLetter}:\" }
}

# ── an SMB path. Needs admin to share. KNOWN CONFOUND, stated up front: an AppContainer is denied
# NETWORK access, and `\\localhost\…` is loopback, which Windows blocks for AppContainers unless
# explicitly exempted. So a denial here may be about the network and not about traverse — which is
# exactly what the `unc-rootgrant` reachability control exists to distinguish. The capability SIDs
# and the loopback exemption are both attempted so the arm has the best chance of being conclusive.
W ''
W '== provision an SMB share =='
$uncBack = Join-Path $env:USERPROFILE "nubvt-$nonce-unc"
New-Item -ItemType Directory -Force -Path $uncBack | Out-Null
$shareName = "nubvt$nonce"
$uncRoot = "\\localhost\$shareName\"
if ($elevated) {
  try {
    $null = New-SmbShare -Name $shareName -Path $uncBack -FullAccess 'Everyone' -EA Stop
    Fact 'smb-share-created' "$shareName -> $uncBack"
  } catch { Fact 'smb-share-error' "$_" }
}
if (Test-Path -LiteralPath $uncRoot) {
  $vols['unc'] = New-Fixture 'unc' $uncRoot $uncBack
  $vols['unc']['rootGrantPath'] = $uncBack
} else {
  $vols['unc'] = @{ available = $false; why = "share unreachable (admin=$elevated)"; root = $uncRoot }
}

# ─────────────── the device characteristics, read rather than assumed ───────────────
W ''
W '== device characteristics per volume =='
$flagVals = @()
foreach ($v in @('local-c', 'vhd', 'subst', 'unc')) {
  $root = if ($vols[$v].ContainsKey('root')) { $vols[$v]['root'] } else { $null }
  $probeRoot = if ($v -eq 'local-c') { 'C:\' } else { $root }
  $vc = if ($probeRoot -and (Test-Path -LiteralPath $probeRoot)) { [AcJail]::VolumeChar($probeRoot) } else { 'ERR unavailable' }
  $facts["$v-volchar"] = "$probeRoot => $vc"
  $set = $null
  if ($vc -match 'chars=0x([0-9a-fA-F]+)') {
    $chars = [Convert]::ToUInt32($Matches[1], 16)
    $set = (($chars -band $traverseBit) -ne 0)
    $flagVals += $set
  }
  $facts["$v-traverse-flag"] = $set
  Fact "volchar/$v" "$probeRoot => $vc traverse-flag-set=$set"
}
$facts['traverse-flag-varies'] = ((@($flagVals | Sort-Object -Unique).Count) -gt 1)
$facts['traverse-flag-summary'] = ($flagVals -join ',')
Fact 'traverse-flag-varies-across-measured-volumes' $facts['traverse-flag-varies']

foreach ($v in @('local-c', 'vhd', 'subst', 'unc')) {
  $facts["$v-available"] = [bool]$vols[$v]['available']
  $facts["$v-why"] = $vols[$v]['why']
  $facts["$v-root"] = $vols[$v]['root']
}

# ─────────────── the arms ───────────────

$cells = @{}

function Invoke-VolArm {
  param(
    [string]$Name,
    [bool]$AppContainer,
    [hashtable]$Vol,
    [string]$GrantPath = '',      # where the inheritable ace goes; '' => no grant at all
    [string]$CapSids = '',
    [string]$DeletePrivs = ''
  )
  W ''
  W "== arm $Name =="
  if (-not $Vol['available']) { Fact "$Name/skipped" $Vol['why']; return }
  $sid = ''
  $profileName = ''
  if ($AppContainer) {
    $profileName = "nubvt_${nonce}_$($Name -replace '[^A-Za-z0-9]','_')"
    if ($profileName.Length -gt 60) { $profileName = $profileName.Substring(0, 60) }
    $sid = [AcJail]::CreateProfile($profileName)
    Fact "$Name/profile" "$profileName -> $sid"
    if ($sid -like 'ERR*') { $script:fails++; return }
  }
  $granted = @()
  try {
    # The interpreter always gets a grant: it must never be the variable under test.
    if ($AppContainer) {
      try { Grant-AcAce $runtimeDir $sid 'ReadAndExecute'; $granted += $runtimeDir }
      catch { Fact "$Name/interpreter-grant-failed" "$_" }
    }
    if ($AppContainer -and $GrantPath) {
      try { Grant-AcAce $GrantPath $sid 'Modify'; $granted += $GrantPath }
      catch { Fact "$Name/target-grant-failed" "$GrantPath : $_" }
    }
    Fact "$Name/grants" $(if ($granted.Count) { ($granted -join ' ; ') } else { '(none)' })

    # LOOPBACK EXEMPTION for the SMB arms, attempted and REPORTED either way. Needs admin, mutates
    # per-package state, and is undone below — it is here to make the UNC arm conclusive, not because
    # nub would ever ship it.
    $exempted = $false
    if ($AppContainer -and $Name -like 'unc-*') {
      $ci = (& CheckNetIsolation.exe LoopbackExempt -a "-p=$sid" 2>&1) -join ' | '
      Fact "$Name/loopback-exempt" $ci
      $exempted = $true
    }

    # THE MANDATORY READ-BACK. Written on an ancestor as an inheritable ace and relied on to
    # propagate into the already-existing deep file; if it did not land, a denial has nothing to do
    # with traverse and reads exactly like a kernel refusal.
    $deepAce = 'none'
    if ($AppContainer) { $deepAce = Get-AceRightsFor $Vol['aclDeep'] $sid }
    Fact "$Name/deep-file-dacl-for-ac-sid" $deepAce

    $env:AJ_ARM = $Name
    $env:AJ_TARGETS = @(
      "read-deep|read|$($Vol['deep'])",
      "stat-deep|stat|$($Vol['deep'])",
      "readdir-deepdir|readdir|$($Vol['deepdir'])",
      "require-deep|require|$($Vol['deep'])",
      "read-ungranted-sibling|read|$($Vol['sib'])",
      "walk-up-from-deepdir|walk|$($Vol['deepdir'])"
    ) -join ';'

    $log = Join-Path $logDir "$Name.log"
    # cwd is the granted interpreter dir on local C: for EVERY arm — a launch-time cwd on the volume
    # under test may be opened in the PARENT's context and would muddy the one variable.
    $cmdline = '"' + $jailNode + '" --preserve-symlinks-main --preserve-symlinks "' + $jailChild + '"'
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $status = [AcJail]::Launch($sid, $jailNode, $cmdline, $runtimeDir, $log, 60000, $CapSids, $DeletePrivs)
    $sw.Stop()
    Fact "$Name/launch" "$status in $($sw.ElapsedMilliseconds)ms"

    $lines = @()
    if (Test-Path -LiteralPath $log) { $lines = @(Get-Content -LiteralPath $log -EA SilentlyContinue) }
    Fact "$Name/log-lines" $lines.Count
    $arm = @{}
    foreach ($l in $lines) {
      W ("    | " + $l)
      if ($l -match '^op:([^=]+)=(OK|ERR)\s*(.*)$') { $arm[$Matches[1]] = @($Matches[2], $Matches[3]) }
    }
    $arm['dacl-grants-ac-sid'] = @($(if ($deepAce -eq 'none') { 'ERR' } else { 'OK' }), "dacl:$deepAce")
    $arm['__launch'] = @($status, '')
    $arm['__lines'] = @($lines.Count, '')
    $cells[$Name] = $arm

    if ($exempted) { $null = (& CheckNetIsolation.exe LoopbackExempt -d "-p=$sid" 2>&1) }
  }
  finally {
    foreach ($p in $granted) { try { Revoke-AcAce $p $sid } catch { W "    revoke failed on $p : $_" } }
    if ($profileName) { Fact "$Name/profile-deleted" ([AcJail]::DeleteProfile($profileName)) }
  }
}

foreach ($v in @('local-c', 'vhd', 'subst', 'unc')) {
  $vol = $vols[$v]
  $caps = if ($v -eq 'unc') { 'S-1-15-3-1,S-1-15-3-3' } else { '' }   # internetClient, privateNetworkClientServer
  Invoke-VolArm -Name "$v-plain" -AppContainer $false -Vol $vol
  Invoke-VolArm -Name "$v-rootgrant" -AppContainer $true -Vol $vol -GrantPath $vol['rootGrantPath'] -CapSids $caps
  Invoke-VolArm -Name "$v-deepgrant" -AppContainer $true -Vol $vol -GrantPath $vol['aclDeepdir'] -CapSids $caps
}

# ── THE PRIVILEGE DIFFERENTIAL, on the anchor volume. One variable: whether
# `SeChangeNotifyPrivilege` was deleted from the token the child runs on. `cn-kept` takes the
# identical CreateProcessAsUser + CreateRestrictedToken path deleting nothing, so a failure in
# `cn-deleted` cannot be blamed on the launch mechanism.
W ''
W '== the SeChangeNotifyPrivilege differential (anchor volume) =='
Invoke-VolArm -Name 'cn-kept' -AppContainer $true -Vol $vols['local-c'] `
  -GrantPath $vols['local-c']['aclDeepdir'] -DeletePrivs '-'
Invoke-VolArm -Name 'cn-deleted' -AppContainer $true -Vol $vols['local-c'] `
  -GrantPath $vols['local-c']['aclDeepdir'] -DeletePrivs 'SeChangeNotifyPrivilege'

# Read off the token that ACTUALLY ran (the launcher reports the child token's privilege list), not
# off the API call. Without this the differential is an assumption about CreateRestrictedToken.
function Has-Cn([string]$arm) {
  $l = [string](LaunchOf $arm)
  if ($l -like '*MISSING-ARM*' -or -not $l) { return $null }
  return ($l -match 'SeChangeNotifyPrivilege')
}
$facts['cn-kept-has-privilege'] = Has-Cn 'cn-kept'
$facts['cn-deleted-has-privilege'] = Has-Cn 'cn-deleted'
Fact 'cn-kept-token' (LaunchOf 'cn-kept')
Fact 'cn-deleted-token' (LaunchOf 'cn-deleted')

# ─────────────── verdict ───────────────

$null = Invoke-VolVerdict -Cells $cells -Facts $facts

W ''
Fact 'FAILURES' $script:fails
if ($script:fails -eq 0) { W '  RESULT all properties PASS' } else { W "  RESULT $($script:fails) properties FAILED" }

# ─────────────── teardown ───────────────
W ''
W '== teardown =='
if ($elevated) {
  try { Remove-SmbShare -Name $shareName -Force -EA SilentlyContinue; Fact 'smb-share-removed' 'yes' } catch {}
}
if ($substLetter) { $null = (& cmd.exe /c "subst ${substLetter}: /d" 2>&1) }
if ($vhdLetter -and (Test-Path "${vhdLetter}:\")) {
  $dp2 = @("select vdisk file=`"$vhdPath`"", 'detach vdisk') -join "`r`n"
  $f2 = Join-Path $env:TEMP "nubvt-$nonce-d.dp"
  Set-Content -LiteralPath $f2 -Value $dp2 -Encoding ASCII
  Fact 'vhd-detach' ((& diskpart.exe /s $f2 2>&1) -join ' | ')
}
Remove-Item -LiteralPath $vhdPath -Force -EA SilentlyContinue
foreach ($p in @($runtimeDir, $localRoot, $substBack, $uncBack)) {
  Remove-Item -LiteralPath $p -Recurse -Force -EA SilentlyContinue
}
W ''
W 'PROBE END'
