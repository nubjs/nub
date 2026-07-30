# GCE windows-startup-script-ps1 for the restricted-token probe box.
#
# The guest agent's `enable-windows-ssh=TRUE` is NOT sufficient on the
# windows-2022 / windows-2025 images: OpenSSH Server is absent, the agent's
# version probe fails ("could not find version: The system cannot find the file
# specified"), and port 22 never opens. This script installs it and wires keys
# for two accounts, because the probe needs BOTH:
#   nub    - Administrators, for one-time box setup (node, fixtures)
#   probe2 - Users ONLY, the de-elevated account every measurement runs as
$ErrorActionPreference = 'Continue'
$log = 'C:\startup-log.txt'
function L($m) { "$(Get-Date -Format o) $m" | Out-File -Append -FilePath $log -Encoding utf8 }
L "=== startup begin ==="

# 1. OpenSSH Server. Prefer the in-image Feature-on-Demand; fall back to the
#    GitHub release, which needs no Windows Update reachability.
if (-not (Test-Path 'C:\Windows\System32\OpenSSH\sshd.exe') -and -not (Test-Path 'C:\Program Files\OpenSSH\sshd.exe')) {
  try {
    L "Add-WindowsCapability OpenSSH.Server"
    Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0 -ErrorAction Stop | Out-Null
    L "capability ok"
  } catch {
    L "capability failed: $_ ; falling back to GitHub release"
    try {
      [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
      $zip = 'C:\Windows\Temp\openssh.zip'
      Invoke-WebRequest -UseBasicParsing -Uri 'https://github.com/PowerShell/Win32-OpenSSH/releases/download/v9.5.0.0p1-Beta/OpenSSH-Win64.zip' -OutFile $zip
      Expand-Archive -Path $zip -DestinationPath 'C:\Program Files' -Force
      Rename-Item 'C:\Program Files\OpenSSH-Win64' 'C:\Program Files\OpenSSH' -ErrorAction SilentlyContinue
      & powershell -ExecutionPolicy Bypass -File 'C:\Program Files\OpenSSH\install-sshd.ps1' | Out-Null
      L "github install ok"
    } catch { L "github install failed: $_" }
  }
}

# 2. Accounts. probe2 is the measurement identity and must be in Users ONLY --
#    every arm's de-elevation claim rests on that.
$pw = ConvertTo-SecureString 'Pr0be!Jail#2026' -AsPlainText -Force
foreach ($u in @(@{n='nub';g='Administrators'}, @{n='probe2';g='Users'})) {
  if (-not (Get-LocalUser -Name $u.n -ErrorAction SilentlyContinue)) {
    New-LocalUser -Name $u.n -Password $pw -PasswordNeverExpires -AccountNeverExpires | Out-Null
    L "created $($u.n)"
  }
  try { Add-LocalGroupMember -Group $u.g -Member $u.n -ErrorAction Stop; L "$($u.n) -> $($u.g)" } catch { L "group add $($u.n): $_" }
}
# Force a profile directory to exist so .ssh has a home before first logon.
foreach ($u in @('nub','probe2')) {
  $h = "C:\Users\$u"
  if (-not (Test-Path $h)) { New-Item -ItemType Directory -Force -Path $h | Out-Null }
  New-Item -ItemType Directory -Force -Path "$h\.ssh" | Out-Null
}

# 3. Keys. Administrators authenticate via administrators_authorized_keys (the
#    Match Group administrators block in the shipped sshd_config redirects
#    there); a standard user uses their own ~/.ssh/authorized_keys.
$key = (Get-ItemProperty -Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Group Policy\State' -ErrorAction SilentlyContinue) | Out-Null
$pub = (Invoke-RestMethod -Headers @{'Metadata-Flavor'='Google'} -Uri 'http://metadata.google.internal/computeMetadata/v1/instance/attributes/probe-pubkey')
New-Item -ItemType Directory -Force -Path 'C:\ProgramData\ssh' | Out-Null
Set-Content -Path 'C:\ProgramData\ssh\administrators_authorized_keys' -Value $pub -Encoding ascii
icacls 'C:\ProgramData\ssh\administrators_authorized_keys' /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
foreach ($u in @('nub','probe2')) {
  $ak = "C:\Users\$u\.ssh\authorized_keys"
  Set-Content -Path $ak -Value $pub -Encoding ascii
  icacls $ak /inheritance:r /grant "${u}:F" /grant 'SYSTEM:F' /grant 'Administrators:F' | Out-Null
}
L "keys placed"

# 4. sshd_config: the shipped file ends with `Match Group administrators`, so
#    appending directives lands them INSIDE that block and sshd refuses to
#    start. Insert before the first Match line.
$cfgPath = 'C:\ProgramData\ssh\sshd_config'
if (Test-Path $cfgPath) {
  $lines = Get-Content $cfgPath
  $idx = ($lines | Select-String -Pattern '^\s*Match\s' | Select-Object -First 1).LineNumber
  $inject = @('PubkeyAuthentication yes', 'PasswordAuthentication yes', 'StrictModes no')
  if ($idx) { $new = $lines[0..($idx-2)] + $inject + $lines[($idx-1)..($lines.Count-1)] }
  else      { $new = $lines + $inject }
  Set-Content -Path $cfgPath -Value $new -Encoding ascii
  L "sshd_config patched at line $idx"
}
New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell `
  -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -PropertyType String -Force | Out-Null

Set-Service -Name sshd -StartupType Automatic -ErrorAction SilentlyContinue
Restart-Service sshd -ErrorAction SilentlyContinue
Start-Service sshd -ErrorAction SilentlyContinue
New-NetFirewallRule -Name sshd22 -DisplayName 'sshd 22' -Enabled True -Direction Inbound `
  -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue | Out-Null
L "sshd status=$((Get-Service sshd -ErrorAction SilentlyContinue).Status)"
L "=== startup end ==="
