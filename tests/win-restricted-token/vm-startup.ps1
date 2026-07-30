# GCE windows-startup-script-ps1 for the MSVC from-source build-jail probe box.
# Runs as SYSTEM on every boot; idempotent. Three jobs:
#   1. wire OpenSSH so the box is reachable at all (the guest agent does not do
#      this reliably on the windows-* images — see README).
#   2. create the two accounts the measurement needs: an ADMIN we ssh in as, and
#      a Users-only `probe` account every measured arm actually runs as.
#   3. install the toolchain the prior lane's box lacked: VS Build Tools (MSVC +
#      Windows SDK), Python, Node. This is the whole point of this box.
$ErrorActionPreference = 'Continue'
Start-Transcript -Path C:\startup.log -Append

$PW = 'Pr0be!Jail-2026#msvc'
Set-Content -Path C:\probe-pw.txt -Value $PW

# ---------------- accounts ----------------
# Created BEFORE ssh so the key placement below can rely on them existing.
foreach ($u in @('nub', 'probe')) {
  if (-not (Get-LocalUser -Name $u -ErrorAction SilentlyContinue)) {
    net user $u $PW /add /y
    wmic useraccount where "name='$u'" set PasswordExpires=false
  }
}
net localgroup Administrators nub /add
# probe stays in Users only — that is the de-elevation the measurement rests on.

# NOTHING here may create C:\Users\probe. A pre-existing directory makes Windows
# mint the profile at C:\Users\probe.<HOST> instead, silently breaking every path
# the arms use. The profile is created by asprobe.ps1's first Start-Process
# -Credential (an INTERACTIVE logon). A scheduled task cannot do it: BUILTIN\Users
# holds no SeBatchLogonRight by default, so `schtasks /run` as probe fails —
# measured on this image, do not reintroduce it.

# ---------------- OpenSSH ----------------
$cap = Get-WindowsCapability -Online -Name 'OpenSSH.Server*' | Select-Object -First 1
if ($cap -and $cap.State -ne 'Installed') { Add-WindowsCapability -Online -Name $cap.Name }
Set-Service -Name sshd -StartupType Automatic -ErrorAction SilentlyContinue
New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell `
  -Value 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -PropertyType String -Force -ErrorAction SilentlyContinue

$PUBKEY = '__PUBKEY__'
New-Item -ItemType Directory -Force -Path C:\ProgramData\ssh | Out-Null
# The trailing `Match Group administrators` block redirects admins here, so this
# is the file that matters for the admin account. It also needs no user profile.
$ak = 'C:\ProgramData\ssh\administrators_authorized_keys'
Set-Content -Path $ak -Value $PUBKEY -Encoding ascii
icacls $ak /inheritance:r /grant 'SYSTEM:F' /grant 'BUILTIN\Administrators:F'

# StrictModes off: key-file ownership/ACL pickiness is pure setup friction and
# has nothing to do with what this box measures. Insert BEFORE the first Match
# line — appending lands the directive INSIDE the Match block, which is invalid
# and stops sshd from starting at all (locking you out).
$cfgPath = 'C:\ProgramData\ssh\sshd_config'
if (Test-Path $cfgPath) {
  $lines = @(Get-Content $cfgPath | Where-Object { $_ -notmatch '^\s*StrictModes' })
  $mi = ($lines | Select-String -Pattern '^\s*Match ' | Select-Object -First 1).LineNumber
  if ($mi) { $lines = $lines[0..($mi - 2)] + @('StrictModes no') + $lines[($mi - 1)..($lines.Count - 1)] }
  else { $lines += 'StrictModes no' }
  Set-Content -Path $cfgPath -Value $lines -Encoding ascii
}
Restart-Service sshd -ErrorAction SilentlyContinue
Start-Service sshd -ErrorAction SilentlyContinue
New-NetFirewallRule -Name sshd-in -DisplayName 'sshd' -Enabled True -Direction Inbound `
  -Protocol TCP -Action Allow -LocalPort 22 -ErrorAction SilentlyContinue | Out-Null

# ---------------- toolchain ----------------
# Each step drops a marker so a reboot does not reinstall.
$dl = 'C:\dl'; New-Item -ItemType Directory -Force -Path $dl | Out-Null
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Fetch($url, $out) {
  if (-not (Test-Path $out)) { (New-Object Net.WebClient).DownloadFile($url, $out) }
}

# Node — pinned to a 24.x line; native-addon toolchains track LTS most reliably,
# and the tier question here is MSVC, not the Node version.
if (-not (Test-Path C:\node\node.exe)) {
  $nv = 'v24.11.0'
  Fetch "https://nodejs.org/dist/$nv/node-$nv-win-x64.zip" "$dl\node.zip"
  Expand-Archive "$dl\node.zip" -DestinationPath $dl -Force
  Move-Item "$dl\node-$nv-win-x64" C:\node -Force
}

# Python — node-gyp hard-requires it.
if (-not (Test-Path 'C:\Python312\python.exe')) {
  Fetch 'https://www.python.org/ftp/python/3.12.8/python-3.12.8-amd64.exe' "$dl\py.exe"
  Start-Process "$dl\py.exe" -ArgumentList '/quiet', 'InstallAllUsers=1', 'TargetDir=C:\Python312', `
    'PrependPath=1', 'Include_test=0' -Wait
}

# VS Build Tools — MSVC v143 + Windows SDK. THE reason this box exists.
$vcMarker = 'C:\vsbuildtools.done'
if (-not (Test-Path $vcMarker)) {
  Fetch 'https://aka.ms/vs/17/release/vs_BuildTools.exe' "$dl\vs_BuildTools.exe"
  $p = Start-Process "$dl\vs_BuildTools.exe" -ArgumentList '--quiet', '--wait', '--norestart', '--nocache', `
    '--installPath', 'C:\BuildTools', `
    '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended' -Wait -PassThru
  Write-Output "vs_BuildTools exit=$($p.ExitCode)"
  if ($p.ExitCode -eq 0 -or $p.ExitCode -eq 3010) { Set-Content $vcMarker "exit=$($p.ExitCode)" }
}

# Machine-wide PATH so the probe account inherits the toolchain.
$mp = [Environment]::GetEnvironmentVariable('Path', 'Machine')
foreach ($add in @('C:\node', 'C:\Python312', 'C:\Python312\Scripts')) {
  if ($mp -notlike "*$add*") { $mp = "$mp;$add" }
}
[Environment]::SetEnvironmentVariable('Path', $mp, 'Machine')

Set-Content -Path C:\provision.done -Value @"
node=$(Test-Path C:\node\node.exe)
python=$(Test-Path C:\Python312\python.exe)
vsbuildtools=$(Test-Path $vcMarker)
cl=$( (Get-ChildItem 'C:\BuildTools\VC\Tools\MSVC' -ErrorAction SilentlyContinue | Select-Object -First 1).Name )
"@
Get-Content C:\provision.done
Stop-Transcript
