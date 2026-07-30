# Privilege arms for the silo+bindflt probe. The interactive/SSH context is already an ELEVATED
# ADMIN, so the two arms that decide "does nub need privilege, and how often" are run out of band:
# a SYSTEM token (holds SeTcbPrivilege, the privilege Quarkslab says silo creation needs) and a
# freshly-created STANDARD USER (holds nothing). Both go through Task Scheduler because that is the
# only way to get a different primary token here without an interactive logon.

param(
  [string]$Exe  = 'C:\silo-probe\silo-probe.exe',
  [string]$Node = 'C:\node\node.exe',
  [string]$Base = 'C:\silo-probe'
)

$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force -Path $Base | Out-Null

function Invoke-Arm {
  param([string]$Name, [string]$RunAs, [string]$Password, [string]$RunLevel = 'LIMITED')

  $root = Join-Path $Base $Name
  $out  = Join-Path $Base "out-$Name.txt"
  $log  = Join-Path $Base "log-$Name.txt"
  Remove-Item $out, $log -ErrorAction SilentlyContinue
  New-Item -ItemType Directory -Force -Path $root | Out-Null

  $cmd = Join-Path $Base "run-$Name.cmd"
  "`"$Exe`" --arm $Name --root `"$root`" --node `"$Node`" --out `"$out`" > `"$log`" 2>&1" |
    Set-Content -Encoding ASCII $cmd

  schtasks /delete /tn "siloprobe-$Name" /f *> $null
  if ($Password) {
    schtasks /create /tn "siloprobe-$Name" /tr "$cmd" /sc ONCE /st 23:59 /ru $RunAs /rp $Password /rl $RunLevel /f
  } else {
    schtasks /create /tn "siloprobe-$Name" /tr "$cmd" /sc ONCE /st 23:59 /ru $RunAs /rl $RunLevel /f
  }
  if ($LASTEXITCODE -ne 0) { Write-Host "===== ARM $Name : TASK CREATE FAILED ====="; return }

  schtasks /run /tn "siloprobe-$Name" | Out-Null
  $deadline = (Get-Date).AddMinutes(8)
  while ((Get-Date) -lt $deadline) {
    Start-Sleep -Seconds 5
    $status = (schtasks /query /tn "siloprobe-$Name" /fo LIST | Select-String '^Status:') -join ''
    if ($status -notmatch 'Running') { break }
  }

  Write-Host ""
  Write-Host "########## ARM $Name  (RunAs=$RunAs RunLevel=$RunLevel) ##########"
  if (Test-Path $out) { Get-Content $out }
  elseif (Test-Path $log) { Write-Host '--- no report file; task log follows ---'; Get-Content $log }
  else { Write-Host 'NO OUTPUT AT ALL (task never produced a file)' }
}

# The standard user must be able to read the probe and write its fixtures.
icacls $Base /grant 'Users:(OI)(CI)F' /T *> $null

$pw = 'Pr0be!' + (Get-Random -Minimum 100000 -Maximum 999999)
net user siloprobe $pw /add *> $null
net localgroup Administrators siloprobe /delete *> $null
Write-Host ('standard-user in Administrators = ' +
  [bool]((net localgroup Administrators) -match '^siloprobe$'))

Invoke-Arm -Name 'system'   -RunAs 'SYSTEM'    -RunLevel 'HIGHEST'
Invoke-Arm -Name 'stduser'  -RunAs 'siloprobe' -Password $pw -RunLevel 'LIMITED'
