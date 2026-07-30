# Does the privileged step have to happen on EVERY install, or once?
# Server runs elevated (the stand-in for a one-time-installed helper): it creates the silo and the
# bindings, then DuplicateHandles the job into the unprivileged client. Client is the standard
# account; it launches node into that silo. The client's own BfSetupFilter attempt is the in-run
# control that it is genuinely unprivileged.

$ErrorActionPreference = 'Continue'
$pw   = 'Pr0be!778899'
$sync = 'C:\silo-probe\handoff'
$exe  = 'C:\silo-probe\silo-probe.exe'

Remove-Item C:\silo-probe\out-handoff-server.txt, C:\silo-probe\out-handoff-client.txt `
            -ErrorAction SilentlyContinue
Remove-Item $sync -Recurse -Force -ErrorAction SilentlyContinue

Start-Process -FilePath $exe -ArgumentList @(
  '--role','server','--sync',$sync,'--out','C:\silo-probe\out-handoff-server.txt') -NoNewWindow

$deadline = (Get-Date).AddMinutes(2)
while ((Get-Date) -lt $deadline -and -not (Test-Path "$sync\fixture-ready.txt")) { Start-Sleep -Milliseconds 300 }
icacls C:\silo-probe /grant 'Users:(OI)(CI)F' /T 2>&1 | Out-Null
Write-Host "fixture-ready = $(Test-Path "$sync\fixture-ready.txt")"

Set-Content -Encoding ASCII C:\silo-probe\run-handoff-client.cmd (
  "$exe --role client --sync $sync --node C:\node\node.exe " +
  "--out C:\silo-probe\out-handoff-client.txt > C:\silo-probe\log-handoff-client.txt 2>&1")
schtasks /delete /tn siloprobe-handoff /f 2>&1 | Out-Null
schtasks /create /tn siloprobe-handoff /tr C:\silo-probe\run-handoff-client.cmd /sc ONCE /st 23:59 `
         /ru siloprobe /rp $pw /rl LIMITED /f | Out-Null
schtasks /run /tn siloprobe-handoff | Out-Null

$deadline = (Get-Date).AddMinutes(8)
while ((Get-Date) -lt $deadline -and -not (Test-Path C:\silo-probe\out-handoff-server.txt)) { Start-Sleep -Seconds 3 }
Start-Sleep -Seconds 3

Write-Host ''
Write-Host '########## HANDOFF SERVER ##########'
Get-Content C:\silo-probe\out-handoff-server.txt -ErrorAction SilentlyContinue
Write-Host ''
Write-Host '########## HANDOFF CLIENT ##########'
if (Test-Path C:\silo-probe\out-handoff-client.txt) { Get-Content C:\silo-probe\out-handoff-client.txt }
else { Write-Host 'NO CLIENT REPORT'; Get-Content C:\silo-probe\log-handoff-client.txt -ErrorAction SilentlyContinue }
