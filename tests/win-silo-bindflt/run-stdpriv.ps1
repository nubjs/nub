# Is the bindflt ACCESS_DENIED a PRIVILEGE gate or an admin-membership gate?
# That distinction decides whether nub can ship "one elevated setup command, then unprivileged
# forever": a user right is grantable once by policy and persists; group membership + elevation
# is not something a per-install unprivileged process can reach.
#
# Arm 1 (stdpriv): the same standard account, granted every plausible user right, all enabled
#                  in-process. If BfSetupFilter still returns 0x80070005, no privilege buys it.
# Arm 2 (stdadmin): the same account added to Administrators and run /rl HIGHEST. This is the
#                   positive control — without it, an arm-1 failure cannot be told apart from a
#                   broken harness.

$ErrorActionPreference = 'Continue'
$pw = 'Pr0be!778899'
$sid = (New-Object System.Security.Principal.NTAccount('siloprobe')).Translate(
         [System.Security.Principal.SecurityIdentifier]).Value

$rights = @(
  'SeTcbPrivilege', 'SeCreateSymbolicLinkPrivilege', 'SeManageVolumePrivilege',
  'SeSecurityPrivilege', 'SeBackupPrivilege', 'SeRestorePrivilege', 'SeLoadDriverPrivilege',
  'SeCreateGlobalPrivilege', 'SeImpersonatePrivilege', 'SeTakeOwnershipPrivilege',
  'SeDebugPrivilege', 'SeCreatePermanentPrivilege', 'SeAssignPrimaryTokenPrivilege',
  'SeSystemEnvironmentPrivilege', 'SeCreatePagefilePrivilege'
)

secedit /export /cfg C:\secpol2.cfg /areas USER_RIGHTS | Out-Null
$cfg = [System.Collections.ArrayList](Get-Content C:\secpol2.cfg)
foreach ($rgt in $rights) {
  $idx = ($cfg | Select-String "^$rgt\s*=" | Select-Object -First 1).LineNumber
  if ($idx) {
    if ($cfg[$idx - 1] -notmatch [regex]::Escape($sid)) { $cfg[$idx - 1] = $cfg[$idx - 1] + ",*$sid" }
  } else {
    $priv = ($cfg | Select-String '^\[Privilege Rights\]' | Select-Object -First 1).LineNumber
    $cfg.Insert($priv, "$rgt = *$sid")
  }
}
Set-Content C:\secpol2.cfg $cfg
secedit /configure /db C:\Windows\security\local.sdb /cfg C:\secpol2.cfg /areas USER_RIGHTS | Out-Null

function Invoke-Arm($name, $level) {
  $out = "C:\silo-probe\out-$name.txt"
  Remove-Item $out -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force "C:\silo-probe\$name" | Out-Null
  Set-Content -Encoding ASCII "C:\silo-probe\run-$name.cmd" (
    "C:\silo-probe\silo-probe.exe --arm $name --root C:\silo-probe\$name " +
    "--node C:\node\node.exe --out $out > C:\silo-probe\log-$name.txt 2>&1")
  schtasks /delete /tn "siloprobe-$name" /f 2>&1 | Out-Null
  schtasks /create /tn "siloprobe-$name" /tr "C:\silo-probe\run-$name.cmd" /sc ONCE /st 23:59 `
           /ru siloprobe /rp $pw /rl $level /f | Out-Null
  schtasks /run /tn "siloprobe-$name" | Out-Null
  $deadline = (Get-Date).AddMinutes(6)
  while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    if (((schtasks /query /tn "siloprobe-$name" /fo LIST | Select-String '^Status:') -join '') -notmatch 'Running') { break }
  }
  Write-Host ""
  Write-Host "########## ARM $name (/rl $level) ##########"
  if (Test-Path $out) {
    Get-Content $out | Select-String 'token-user|token-elevated|token-privileges|SeTcb|enable-all|PROMOTE-TO-SILO|BfSetupFilter|canary-exists|marker-exists|GLOBAL BfSetupFilter'
  } else { Write-Host 'NO OUTPUT'; Get-Content "C:\silo-probe\log-$name.txt" -ErrorAction SilentlyContinue }
}

Invoke-Arm 'stdpriv' 'LIMITED'

net localgroup Administrators siloprobe /add 2>&1 | Out-Null
Invoke-Arm 'stdadmin' 'HIGHEST'
net localgroup Administrators siloprobe /delete 2>&1 | Out-Null
