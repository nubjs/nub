# The decisive privilege arm: a freshly-created LOCAL STANDARD USER (no admin, no SeTcbPrivilege).
# Task Scheduler is the only way here to obtain a different primary token without an interactive
# logon; /rl LIMITED keeps the task on the account's own unelevated token. A fresh local account
# has no SeBatchLogonRight, so the task silently reports SCHED_S_TASK_HAS_NOT_RUN (267011) until
# the right is granted through secedit — that grant is test scaffolding, NOT a probe result.

$ErrorActionPreference = 'Continue'
$pw = 'Pr0be!778899'

net user siloprobe $pw /add 2>&1 | Out-Null
net user siloprobe $pw 2>&1 | Out-Null
net localgroup Administrators siloprobe /delete 2>&1 | Out-Null

$sid = (New-Object System.Security.Principal.NTAccount('siloprobe')).Translate(
         [System.Security.Principal.SecurityIdentifier]).Value
Write-Host "siloprobe SID = $sid"

secedit /export /cfg C:\secpol.cfg /areas USER_RIGHTS | Out-Null
$cfg = Get-Content C:\secpol.cfg
if ($cfg -match '^SeBatchLogonRight') {
  $cfg = $cfg -replace '^(SeBatchLogonRight\s*=\s*.*)$', "`$1,*$sid"
} else {
  $cfg += "SeBatchLogonRight = *$sid"
}
Set-Content C:\secpol.cfg $cfg
secedit /configure /db C:\Windows\security\local.sdb /cfg C:\secpol.cfg /areas USER_RIGHTS | Out-Null
Write-Host ("SeBatchLogonRight now = " + ((Get-Content C:\secpol.cfg) -match '^SeBatchLogonRight'))

icacls C:\silo-probe /grant 'Users:(OI)(CI)F' /T  2>&1 | Out-Null
icacls C:\node       /grant 'Users:(OI)(CI)RX' /T 2>&1 | Out-Null
New-Item -ItemType Directory -Force C:\silo-probe\stduser | Out-Null
Remove-Item C:\silo-probe\out-stduser.txt, C:\silo-probe\log-stduser.txt -ErrorAction SilentlyContinue

$line = 'C:\silo-probe\silo-probe.exe --arm stduser --root C:\silo-probe\stduser ' +
        '--node C:\node\node.exe --out C:\silo-probe\out-stduser.txt ' +
        '> C:\silo-probe\log-stduser.txt 2>&1'
Set-Content -Encoding ASCII C:\silo-probe\run-stduser.cmd $line

schtasks /delete /tn siloprobe-stduser /f 2>&1 | Out-Null
schtasks /create /tn siloprobe-stduser /tr C:\silo-probe\run-stduser.cmd /sc ONCE /st 23:59 /ru siloprobe /rp $pw /rl LIMITED /f | Out-Null
schtasks /run /tn siloprobe-stduser | Out-Null

$deadline = (Get-Date).AddMinutes(6)
while ((Get-Date) -lt $deadline) {
  Start-Sleep -Seconds 5
  $status = (schtasks /query /tn siloprobe-stduser /fo LIST | Select-String '^Status:') -join ''
  if ($status -notmatch 'Running') { break }
}
schtasks /query /tn siloprobe-stduser /fo LIST /v | Select-String 'Status|Last Result|Run As User'

Write-Host ''
Write-Host '########## ARM stduser ##########'
if (Test-Path C:\silo-probe\out-stduser.txt) { Get-Content C:\silo-probe\out-stduser.txt }
elseif (Test-Path C:\silo-probe\log-stduser.txt) { Write-Host '--- task log ---'; Get-Content C:\silo-probe\log-stduser.txt }
else { Write-Host 'NO OUTPUT' }
