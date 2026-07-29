# Read the DEFAULT DACLs an AppContainer child is access-checked against, and modify nothing.
#
# THE QUESTION. A confined `node` dies on `EPERM: lstat 'C:\'` because Node's `realpathSync`
# opens every path prefix as a target, and a LowBox token's open must satisfy TWO checks: the
# ordinary DACL check, and a LowBox check that needs an ace naming ALL APPLICATION PACKAGES,
# ALL RESTRICTED APPLICATION PACKAGES, a capability, or the package sid. `C:\` grants
# BUILTIN\Users read+execute, so the ordinary half passes for any local user; whether the LowBox
# half passes is a property of the IMAGE's default DACL. That makes this survey decisive: if a
# stock desktop already grants AAP traverse on the ancestor chain, no repair is needed there and
# the refusal is an artefact of the CI runner image.
#
# WHY IT IS LABELLED. Default ACLs are image-dependent — a GitHub Actions `windows-latest` runner
# is a SERVER image and is not representative of the developer desktops that actually matter — so
# every result carries its exact build and edition. An unlabelled row from this script is useless.
#
# READ-ONLY BY CONSTRUCTION: `Get-Acl` only. Nothing here writes a DACL, which is what makes it
# safe to run on system roots and what keeps the measurement about the STOCK image.

$ErrorActionPreference = 'Stop'

# The SIDs that satisfy a LowBox check without nub granting anything. Capability sids
# (S-1-15-3-*) are matched by prefix: which ones a machine carries is itself a finding.
$AAP  = 'S-1-15-2-1'
$ARAP = 'S-1-15-2-2'

# FILE_READ_DATA / FILE_TRAVERSE / FILE_READ_ATTRIBUTES. `lstat` needs read-attributes on the
# object; walking THROUGH to a descendant needs traverse.
$TRAVERSE       = 0x20
$READ_ATTRS     = 0x80
$LIST_DIRECTORY = 0x1
# WRITE_DAC — whether a standard user could author the repair on this path at all.
$WRITE_DAC = 0x40000

function Write-Label {
  $os = Get-CimInstance Win32_OperatingSystem
  $cs = Get-CimInstance Win32_ComputerSystem
  $type = switch ($os.ProductType) { 1 { 'workstation' } 2 { 'domain-controller' } 3 { 'server' } default { "unknown($($os.ProductType))" } }
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  $elevated = (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
  Write-Output "  fact:image-caption=$($os.Caption)"
  Write-Output "  fact:image-build=$($os.Version).$($os.BuildNumber)"
  Write-Output "  fact:image-ubr=$((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR)"
  Write-Output "  fact:image-displayversion=$((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').DisplayVersion)"
  Write-Output "  fact:image-producttype=$type"
  Write-Output "  fact:image-arch=$($os.OSArchitecture) / $($cs.SystemType)"
  Write-Output "  fact:runner-user=$($id.Name)"
  # Elevation is recorded because it decides whether the REPAIR is available, while the rows
  # below decide whether it is NEEDED. Conflating the two is what made an earlier green run
  # look like it had settled the unprivileged case.
  Write-Output "  fact:runner-elevated=$elevated"
}

function Survey-Path([string]$path) {
  if (-not (Test-Path -LiteralPath $path)) {
    Write-Output "  fact:absent[$path]=true"
    return
  }
  $acl = Get-Acl -LiteralPath $path
  Write-Output "  fact:sddl[$path]=$($acl.Sddl)"

  $selfTraverse = $false
  $selfReadAttrs = $false
  $descendantTraverse = $false
  # Tracked apart from the capability-inclusive counters above: only AAP/ARAP grants an
  # AppContainer child access nub does not have to obtain.
  $aapTraverse = $false
  $aapReadAttrs = $false
  foreach ($ace in $acl.Access) {
    $sid = $null
    try {
      $sid = $ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
    } catch {
      $sid = $ace.IdentityReference.Value
    }
    $isLowBoxTrustee = ($sid -eq $AAP) -or ($sid -eq $ARAP) -or ($sid -like 'S-1-15-3-*')
    if (-not $isLowBoxTrustee) { continue }

    $mask = [int]$ace.FileSystemRights
    $inheritOnly = ($ace.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -ne 0
    $inheritable = ($ace.InheritanceFlags -ne [Security.AccessControl.InheritanceFlags]::None)
    $allow = ($ace.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow)
    Write-Output ("  fact:ace[$path]=sid=$sid type=$($ace.AccessControlType) mask=0x{0:x} inherited={1} inheritable={2} inherit-only={3} rights={4}" -f `
      $mask, $ace.IsInherited, $inheritable, $inheritOnly, $ace.FileSystemRights)

    if (-not $allow) { continue }
    # An inherit-only ace governs children, never the object it sits on — which is exactly the
    # distinction that decides whether `lstat` of THIS path succeeds.
    if (-not $inheritOnly) {
      if ($mask -band $TRAVERSE)   { $selfTraverse = $true }
      if ($mask -band $READ_ATTRS) { $selfReadAttrs = $true }
    }
    if ($inheritable -and ($mask -band $TRAVERSE)) { $descendantTraverse = $true }
    if ((-not $inheritOnly) -and (($sid -eq $AAP) -or ($sid -eq $ARAP))) {
      if ($mask -band $TRAVERSE)   { $aapTraverse = $true }
      if ($mask -band $READ_ATTRS) { $aapReadAttrs = $true }
    }
  }

  # THE VERDICT THAT MATTERS is the UNGRANTED one. A capability ace only helps a child that
  # HOLDS that capability, and nub cannot obtain these: `CreateProcessW` refuses the
  # `S-1-15-3-65536-...` form outright (ERROR_INVALID_PARAMETER), and Chromium's AppContainer
  # never requests a harvested ace's sid either. So AAP/ARAP is the only trustee that grants an
  # AppContainer child anything for free, and the two verdicts are reported separately —
  # collapsing them reads a `C:\` that only carries a capability ace as already reachable.
  #
  # `lstat` needs read-attributes ON the object; walking THROUGH to a descendant needs traverse.
  # Both are reported because a chain that is traversable but not stat-able fails exactly the way
  # the defect does.
  Write-Output "  prop:lstat-without-a-grant[$path]=$(if ($aapReadAttrs) { 'YES' } else { 'NO' })"
  Write-Output "  prop:traverse-without-a-grant[$path]=$(if ($aapTraverse) { 'YES' } else { 'NO' })"
  Write-Output "  fact:lstat-if-capability-were-holdable[$path]=$(if ($selfReadAttrs) { 'YES' } else { 'NO' })"
  Write-Output "  fact:traverse-descendants-any-lowbox-trustee[$path]=$(if ($descendantTraverse) { 'YES' } else { 'NO' })"

  # IS THE REPAIR EVEN AVAILABLE HERE? Writing an ace needs WRITE_DAC, so a standard user can
  # only repair a chain member whose DACL grants it (or that it owns). This is the read-only
  # answer to that, alongside the owner — together with the rows above it decides whether the
  # ancestor repair is a candidate at all on this image.
  $standard = @('S-1-5-32-545', 'S-1-5-11', 'S-1-1-0')
  $userWriteDac = $false
  foreach ($ace in $acl.Access) {
    if ($ace.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow) { continue }
    $sid = $null
    try { $sid = $ace.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value } catch { $sid = $ace.IdentityReference.Value }
    if ($standard -notcontains $sid) { continue }
    if (([int]$ace.FileSystemRights) -band $WRITE_DAC) { $userWriteDac = $true }
  }
  Write-Output "  fact:owner[$path]=$($acl.Owner)"
  Write-Output "  prop:standard-user-can-write-dacl[$path]=$(if ($userWriteDac) { 'YES' } else { 'NO' })"
}

Write-Output 'SURVEY windows default ancestor DACLs (read-only)'
Write-Label

# The ancestor chain a real lifecycle script is walked through. `%TEMP%` is on it, which is why
# this reaches further than `C:\` alone.
$paths = @(
  'C:\',
  'C:\Users',
  $env:USERPROFILE,
  (Join-Path $env:USERPROFILE 'AppData'),
  $env:LOCALAPPDATA,
  $env:TEMP,
  'C:\Program Files'
) | Where-Object { $_ } | Select-Object -Unique

foreach ($p in $paths) { Survey-Path $p }
Write-Output 'SURVEY end'
