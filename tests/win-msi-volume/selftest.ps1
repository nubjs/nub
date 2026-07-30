# Drives BOTH verdict blocks against synthetic worlds and requires the exact expected set of FAILing
# properties from each. No Windows APIs, no launch, no filesystem — so it runs anywhere pwsh does and
# can be validated before a CI minute is spent.
#
# WHY THIS EXISTS AND IS NOT OPTIONAL. Every arm of this Windows effort that produced a wrong answer
# produced it in a verdict block: a table of denials that could not be told from a broken harness, a
# control that "confirmed" a hypothesis because it varied more than one thing. A verdict that has only
# ever been exercised against the world it expects will report PASS on a harness that measured
# nothing. So each world below asserts the EXACT set of failing property names — not merely "some
# property failed" — which is what catches a property that fires for the wrong reason.
#
# The worlds that matter most are the deceptive ones, and they are here deliberately:
#   msi/grant-never-landed        the repaired arm ALSO fails, so the DACL attribution collapses —
#                                 read-cell-identical to "the kernel refuses regardless of the ace".
#   msi/impersonation-inert       the de-elevated control write fails too, so "nub cannot repair it
#                                 unprivileged" is unproven rather than established.
#   vol/unc-unreachable           deepgrant red because the volume is unreachable at all, NOT because
#                                 traverse is enforced. Indistinguishable at the read cell.
#   vol/grant-never-landed        deepgrant red with the ace never propagated. Same read cell as a
#                                 kernel denial.
#   vol/cn-differential-inert     the treatment token still holds the privilege, so the differential
#                                 varied nothing while looking like a clean result.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Off

. (Join-Path $PSScriptRoot 'msi-verdict.ps1')
. (Join-Path $PSScriptRoot 'vol-verdict.ps1')

# Capture instead of print: `Prop` resolves `W` at call time, so redefining it here routes the whole
# verdict into a buffer the assertions can parse.
$script:captured = @()
function W([string]$s) { $script:captured += $s }

$script:selftestFails = 0
function Check([string]$world, [string[]]$expectedFails) {
  $got = @($script:captured | Where-Object { $_ -match '^\s*prop:(\S+)=FAIL' } |
    ForEach-Object { ($_ -replace '^\s*prop:(\S+)=FAIL.*$', '$1') } | Sort-Object)
  $want = @($expectedFails | Sort-Object)
  $missing = @($want | Where-Object { $got -notcontains $_ })
  $extra = @($got | Where-Object { $want -notcontains $_ })
  if ($missing.Count -eq 0 -and $extra.Count -eq 0) {
    Write-Host "  selftest OK   $world  ($($got.Count) expected failures)"
  } else {
    Write-Host "  selftest FAIL $world"
    if ($missing.Count) { Write-Host "      expected to FAIL but did not: $($missing -join ', ')" }
    if ($extra.Count) { Write-Host "      failed unexpectedly:           $($extra -join ', ')" }
    $script:selftestFails++
  }
  $script:captured = @()
  $script:fails = 0
}

function C($v, $d = '') { return @($v, $d) }

# ───────────────────────────── MSI worlds ─────────────────────────────

# Every MSI world starts from the #63590 world and mutates ONE thing, so a world's expected failures
# are attributable to that mutation.
function New-MsiWorld {
  return @{
    'plain-progfiles'           = @{ 'exec-inline' = (C 'OK'); '__launch' = (C 'rc=0'); '__lines' = (C 1) }
    'ac-exec-progfiles'         = @{ '__launch' = (C 'launch-error CreateProcessW err=5'); '__lines' = (C 0) }
    'ac-exec-progfiles-repaired' = @{ 'exec-inline' = (C 'OK'); '__launch' = (C 'rc=0 isAC=1'); '__lines' = (C 1) }
    'ac-exec-userstore'         = @{
      'cwd-at-entry' = (C 'OK'); 'stat-c-root' = (C 'ERR'); 'read-system32-hosts' = (C 'OK')
      '__launch' = (C 'rc=0 isAC=1'); '__lines' = (C 4)
    }
    'ac-exec-toolcache'         = @{ 'exec-inline' = (C 'OK'); '__launch' = (C 'rc=0 isAC=1'); '__lines' = (C 1) }
    'plain-progfiles-npm'       = @{ 'entry-ran' = (C 'OK' 'log:10.9.4'); '__launch' = (C 'rc=0'); '__lines' = (C 1) }
    'ac-exec-progfiles-npm'     = @{ 'entry-ran' = (C 'ERR' 'log:MODULE_NOT_FOUND'); '__launch' = (C 'rc=1 isAC=1'); '__lines' = (C 6) }
    'ac-read-progfiles'         = @{
      'cwd-at-entry' = (C 'OK'); 'stat-c-root' = (C 'ERR'); 'read-system32-hosts' = (C 'OK')
      'read-progfiles-node-exe' = (C 'ERR'); 'stat-progfiles-node-exe' = (C 'ERR')
      'readdir-progfiles-dir' = (C 'ERR'); 'read-npm-package-json' = (C 'ERR')
      'readdir-npm-tree' = (C 'ERR'); 'walk-from-progfiles' = (C 'OK')
      '__launch' = (C 'rc=0 isAC=1'); '__lines' = (C 12)
    }
  }
}
function New-MsiFacts {
  return @{
    'msi-install-succeeded' = $true; 'msi-install-why' = 'True '
    'msi-has-aap' = $false; 'msi-has-arap' = $false; 'msi-protected' = $true
    'msi-owner' = 'NT SERVICE\TrustedInstaller'; 'toolcache-has-aap' = $true
    'sibling-has-aap' = $true; 'sibling-name' = '7/12'
    'deelev-admin' = $false; 'deelev-own-profile-write' = 'OK'
    'deelev-progfiles-write' = 'ERR UnauthorizedAccessException'
    'deelev-disable-artifact' = 'disable-variant pf-write=OK'
    'deelev-delete-privs' = 'SeChangeNotifyPrivilege:on'
    'deelev-progfiles-landed' = 'none'
  }
}

Write-Host ''
Write-Host 'MSI verdict — synthetic worlds'

# 1. The world #63590 predicts, with every control green. Nothing may fail.
$null = Invoke-MsiVerdict -Cells (New-MsiWorld) -Facts (New-MsiFacts)
Check 'msi/defect-is-real' @()

# 2. The MSI grants the ace and exec works: the DEFECT-CLAIMING properties must all flip.
$w = New-MsiWorld
$w['ac-exec-progfiles'] = @{ 'exec-inline' = (C 'OK'); '__launch' = (C 'rc=0 isAC=1'); '__lines' = (C 1) }
$w['ac-read-progfiles']['read-progfiles-node-exe'] = (C 'OK')
$w['ac-read-progfiles']['read-npm-package-json'] = (C 'OK')
$w['ac-exec-progfiles-npm']['entry-ran'] = (C 'OK' 'log:10.9.4')
$f = New-MsiFacts
$f['msi-has-aap'] = $true; $f['msi-protected'] = $false
$null = Invoke-MsiVerdict -Cells $w -Facts $f
Check 'msi/no-defect' @(
  'msi-install-dir-lacks-all-application-packages', 'msi-install-dir-dacl-is-protected',
  'msi-node-cannot-be-executed-by-an-appcontainer', 'msi-tree-cannot-be-read-by-an-appcontainer',
  'bundled-npm-cannot-run-inside-the-jail',
  'ci-provisioned-node-and-msi-node-disagree-on-exec')

# 3. THE DECEPTIVE ONE. The repaired arm fails too, so the refusal is NOT attributable to the DACL —
#    read-cell-identical to the real defect at `ac-exec-progfiles`.
$w = New-MsiWorld
$w['ac-exec-progfiles-repaired'] = @{ '__launch' = (C 'launch-error CreateProcessW err=5'); '__lines' = (C 0) }
$null = Invoke-MsiVerdict -Cells $w -Facts (New-MsiFacts)
Check 'msi/grant-never-landed' @('msi-exec-is-restored-by-adding-the-ace')

# 4. A DEAD HARNESS: nothing ran anywhere. The controls must be what fails, so the defect is never
#    credited off a table of blanks.
$null = Invoke-MsiVerdict -Cells @{} -Facts (New-MsiFacts)
Check 'msi/dead-harness' @(
  'msi-baseline-plain-execs-progfiles-node', 'msi-appcontainer-gate-is-live',
  'msi-appcontainer-positive-control', 'msi-exec-is-restored-by-adding-the-ace',
  'npm-baseline-plain-runs-bundled-npm',
  'nub-provisioned-node-shape-execs-under-appcontainer',
  'ci-provisioned-node-and-msi-node-disagree-on-exec')

# 5. The impersonation never took effect, so "cannot repair unprivileged" is unproven.
$f = New-MsiFacts
$f['deelev-own-profile-write'] = 'ERR UnauthorizedAccessException'
$null = Invoke-MsiVerdict -Cells (New-MsiWorld) -Facts $f
Check 'msi/impersonation-inert' @('unprivileged-repair-control-own-profile-write-succeeds')

# 6. A gate that is NOT live — the "confined" child can stat C:\, so every denial below is vacuous.
$w = New-MsiWorld
$w['ac-read-progfiles']['stat-c-root'] = (C 'OK')
$null = Invoke-MsiVerdict -Cells $w -Facts (New-MsiFacts)
Check 'msi/gate-not-live' @('msi-appcontainer-gate-is-live')

# ───────────────────────────── volume worlds ─────────────────────────────

function New-VolWorld {
  $w = @{}
  foreach ($v in @('local-c', 'vhd', 'subst', 'unc')) {
    $w["$v-plain"] = @{ 'read-deep' = (C 'OK'); '__launch' = (C 'rc=0'); '__lines' = (C 9) }
    foreach ($a in @('rootgrant', 'deepgrant')) {
      $w["$v-$a"] = @{
        'read-deep' = (C 'OK'); 'stat-deep' = (C 'OK'); 'readdir-deepdir' = (C 'OK')
        'require-deep' = (C 'OK'); 'read-ungranted-sibling' = (C 'ERR')
        'walk-up-from-deepdir' = (C 'OK'); 'stat-c-root' = (C 'ERR')
        'read-system32-hosts' = (C 'OK'); 'dacl-grants-ac-sid' = (C 'OK' 'dacl:Modify, Synchronize')
        '__launch' = (C 'rc=0 isAC=1 privs=[SeChangeNotifyPrivilege:on]'); '__lines' = (C 10)
      }
    }
  }
  $w['cn-kept'] = @{
    'read-deep' = (C 'OK'); 'stat-c-root' = (C 'ERR'); 'read-system32-hosts' = (C 'OK')
    'read-ungranted-sibling' = (C 'ERR'); 'dacl-grants-ac-sid' = (C 'OK' 'dacl:Modify')
    '__launch' = (C 'rc=0 isAC=1 privs=[SeChangeNotifyPrivilege:on]'); '__lines' = (C 10)
  }
  $w['cn-deleted'] = @{
    'read-deep' = (C 'ERR'); 'stat-c-root' = (C 'ERR'); 'read-system32-hosts' = (C 'OK')
    'read-ungranted-sibling' = (C 'ERR'); 'dacl-grants-ac-sid' = (C 'OK' 'dacl:Modify')
    '__launch' = (C 'rc=1 isAC=1 privs=[(none)]'); '__lines' = (C 10)
  }
  return $w
}
function New-VolFacts {
  $f = @{
    'sdk-const-source' = 'C:\...\devioctl.h'; 'sdk-const-value' = '#define FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL 0x00020000'
    'traverse-flag-varies' = $true; 'traverse-flag-summary' = 'True,False,True,False'
    'cn-kept-has-privilege' = $true; 'cn-deleted-has-privilege' = $false
  }
  foreach ($v in @('local-c', 'vhd', 'subst', 'unc')) {
    $f["$v-available"] = $true; $f["$v-why"] = 'created'; $f["$v-root"] = "$v-root"
    $f["$v-volchar"] = 'type=0x7 chars=0x00020020'; $f["$v-traverse-flag"] = $true
  }
  return $f
}

Write-Host ''
Write-Host 'volume verdict — synthetic worlds'

# 1. The privilege IS the mechanism and traverse holds on every volume. `cn-deleted` reached the table
#    and was REFUSED (read-deep=ERR), which is what makes the differential positive rather than
#    inconclusive. Nothing may fail.
$null = Invoke-VolVerdict -Cells (New-VolWorld) -Facts (New-VolFacts)
Check 'vol/privilege-is-the-mechanism' @()

# 2. THE COMPAT FINDING: the VHD volume enforces traverse while remaining reachable when the whole
#    chain is granted. Only the vhd treatment cell may fail.
$w = New-VolWorld
$w['vhd-deepgrant']['read-deep'] = (C 'ERR' 'EPERM')
$null = Invoke-VolVerdict -Cells $w -Facts (New-VolFacts)
Check 'vol/vhd-enforces-traverse' @('vhd-bypass-traverse-holds')

# 3. THE DECEPTIVE ONE. The UNC volume is unreachable under an AppContainer AT ALL, so its red
#    deepgrant cell says nothing about traverse. The reachability control must be what flags it.
$w = New-VolWorld
$w['unc-rootgrant']['read-deep'] = (C 'ERR' 'EPERM')
$w['unc-deepgrant']['read-deep'] = (C 'ERR' 'EPERM')
$null = Invoke-VolVerdict -Cells $w -Facts (New-VolFacts)
Check 'vol/unc-unreachable' @('unc-reachable-when-whole-chain-granted', 'unc-bypass-traverse-holds')

# 4. THE OTHER DECEPTIVE ONE. The ace never propagated to the deep file, so the denial is a
#    propagation slip. Same read cell as a kernel denial.
$w = New-VolWorld
$w['subst-deepgrant']['dacl-grants-ac-sid'] = (C 'ERR' 'dacl:none')
$w['subst-deepgrant']['read-deep'] = (C 'ERR' 'EPERM')
$null = Invoke-VolVerdict -Cells $w -Facts (New-VolFacts)
Check 'vol/grant-never-landed' @('subst-grant-reached-the-deep-target', 'subst-bypass-traverse-holds')

# 5. The privilege differential varied nothing: the treatment token still holds it.
$f = New-VolFacts
$f['cn-deleted-has-privilege'] = $true
$null = Invoke-VolVerdict -Cells (New-VolWorld) -Facts $f
Check 'vol/cn-differential-inert' @('cn-control-deleted-token-really-lacks-the-privilege')

# 6. Deleting the privilege changes nothing, so the VOLUME FLAG is the remaining candidate — and if
#    the flag does not vary across the volumes measured, the volume rows cannot discriminate either.
$w = New-VolWorld
$w['cn-deleted']['read-deep'] = (C 'OK')
$f = New-VolFacts
$f['traverse-flag-varies'] = $false
$null = Invoke-VolVerdict -Cells $w -Facts $f
Check 'vol/flag-is-the-candidate-but-uniform' @(
  'device-flag-varies-across-the-volumes-measured', 'deleting-changenotify-breaks-the-deep-read')

# 6b. RUN 1'S ACTUAL WORLD, and the one the old predicate got wrong: the treatment child never started
#     (rc=0xC0000022, zero-byte log), so its read cell is MISSING-OP. The differential must read as
#     INCONCLUSIVE — `cn-deleted-child-started-at-all` fails and the treatment property fails too,
#     rather than the treatment property PASSing off a read that was never attempted.
$w = New-VolWorld
$w['cn-deleted'].Remove('read-deep')
$null = Invoke-VolVerdict -Cells $w -Facts (New-VolFacts)
Check 'vol/cn-treatment-never-ran' @(
  'cn-deleted-child-started-at-all', 'deleting-changenotify-breaks-the-deep-read')

# 7. A volume that could not be provisioned must surface as a GAP, never as a silent pass.
$w = New-VolWorld
foreach ($k in @('vhd-plain', 'vhd-rootgrant', 'vhd-deepgrant')) { $w.Remove($k) }
$f = New-VolFacts
$f['vhd-available'] = $false; $f['vhd-why'] = 'not created (admin=False)'
$null = Invoke-VolVerdict -Cells $w -Facts $f
Check 'vol/volume-missing' @(
  'vhd-was-actually-tested', 'vhd-plain-baseline-reads-deep',
  'vhd-reachable-when-whole-chain-granted', 'vhd-grant-reached-the-deep-target',
  'vhd-ungranted-sibling-denied', 'vhd-bypass-traverse-holds')

# 8. A dead harness on the anchor: nothing may be concluded about any volume.
$null = Invoke-VolVerdict -Cells @{} -Facts (New-VolFacts)
Check 'vol/dead-harness' @(
  'anchor-local-c-deep-read-still-works', 'anchor-local-c-gate-is-live',
  'anchor-local-c-positive-control',
  'local-c-plain-baseline-reads-deep', 'local-c-reachable-when-whole-chain-granted',
  'local-c-grant-reached-the-deep-target', 'local-c-ungranted-sibling-denied',
  'local-c-bypass-traverse-holds',
  'vhd-plain-baseline-reads-deep', 'vhd-reachable-when-whole-chain-granted',
  'vhd-grant-reached-the-deep-target', 'vhd-ungranted-sibling-denied', 'vhd-bypass-traverse-holds',
  'subst-plain-baseline-reads-deep', 'subst-reachable-when-whole-chain-granted',
  'subst-grant-reached-the-deep-target', 'subst-ungranted-sibling-denied', 'subst-bypass-traverse-holds',
  'unc-plain-baseline-reads-deep', 'unc-reachable-when-whole-chain-granted',
  'unc-grant-reached-the-deep-target', 'unc-ungranted-sibling-denied', 'unc-bypass-traverse-holds',
  'cn-control-restricted-launch-path-works', 'cn-deleted-child-started-at-all',
  'deleting-changenotify-breaks-the-deep-read')

Write-Host ''
if ($script:selftestFails -eq 0) {
  Write-Host 'SELFTEST OK — both verdicts discriminate in both directions'
  exit 0
}
Write-Host "SELFTEST FAILED — $($script:selftestFails) world(s) misjudged"
exit 1
