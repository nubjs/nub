# Appended to a hand-made Windows VM's startup script: the box powers itself off once nothing has
# happened for 30 minutes. A guest shutdown reports as TERMINATED, so the disk survives and `start`
# resumes it. Keep this file ASCII: PowerShell 5.1 reads a BOM-less script in the ANSI codepage.
$nubIdleDir = 'C:\ProgramData\nub-idle'
New-Item -ItemType Directory -Force -Path $nubIdleDir | Out-Null

@'
$IdleMinutes = 30
$Stamp = 'C:\ProgramData\nub-idle\since'

function Test-Busy {
  # Sockets rather than process names, which change across OpenSSH versions. 3389 is RDP.
  if (Get-NetTCPConnection -LocalPort 22,3389 -State Established -ErrorAction SilentlyContinue) { return $true }
  # Work outlives a connection only as a scheduled task, since a detached process dies with its SSH
  # session. Windows' own tasks sit under \Microsoft\. CPU is deliberately not a signal: one Defender
  # spike anywhere in the window would restart it.
  return [bool](Get-ScheduledTask -TaskPath '\' | Where-Object { $_.State -eq 'Running' -and $_.TaskName -ne 'nub-idle-stop' })
}

if (Test-Busy) {
  Remove-Item -Force -ErrorAction SilentlyContinue $Stamp
  exit 0
}

# The stamp survives a shutdown, and this task can tick before the startup script runs, so a stamp
# from before the last boot has to be recognised here rather than cleared at startup.
$now = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$boot = ([DateTimeOffset](Get-CimInstance Win32_OperatingSystem).LastBootUpTime).ToUnixTimeSeconds()
$since = if (Test-Path $Stamp) { [int64](Get-Content -Path $Stamp -TotalCount 1) } else { 0 }
if ($since -lt $boot) {
  Set-Content -Path $Stamp -Value $now -Encoding ascii
  exit 0
}
if (($now - $since) / 60 -ge $IdleMinutes) { Stop-Computer -Force }
'@ | Set-Content -Path "$nubIdleDir\check.ps1" -Encoding ascii

schtasks /Create /TN nub-idle-stop /TR "powershell -NoProfile -ExecutionPolicy Bypass -File $nubIdleDir\check.ps1" /SC MINUTE /MO 5 /RU SYSTEM /RL HIGHEST /F | Out-Null
