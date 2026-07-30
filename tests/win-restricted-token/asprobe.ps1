# Run a command as the Users-only `probe` account.
#
# Every measured arm must run de-elevated, or the whole result is worthless: an
# admin token holds SeImpersonate/SeAssignPrimaryToken and would prove nothing
# about what an ordinary user can do. This wrapper is the de-elevation boundary.
#
# `powershell -Command -` over SSH silently produces NO output for a script
# containing Start-Process -Credential — scp this file and use `powershell -File`.
#
#   powershell -File asprobe.ps1 -Cmd 'powershell' -Arg '-File','C:\Users\probe\p\gyp.ps1','-Mode','arm'
param(
  [string]$Cmd = 'cmd.exe',
  [string[]]$Arg = @('/c', 'whoami'),
  [int]$TimeoutSec = 5400
)
$ErrorActionPreference = 'Stop'
$pw = (Get-Content C:\probe-pw.txt -Raw).Trim()
$cred = New-Object System.Management.Automation.PSCredential('probe', (ConvertTo-SecureString $pw -AsPlainText -Force))
# Deliberately NOT %USERPROFILE%: this wrapper's first invocation is what creates
# probe's profile (an interactive logon — a standard user has no
# SeBatchLogonRight, so a scheduled task cannot do it), so C:\Users\probe does
# not exist yet on run one and a working directory pointing there fails.
$wd = 'C:\probe'
New-Item -ItemType Directory -Force -Path $wd | Out-Null
icacls $wd /grant 'probe:(OI)(CI)F' /T /Q | Out-Null
$so = Join-Path $wd 'asprobe.out'
$se = Join-Path $wd 'asprobe.err'
Remove-Item $so, $se -ErrorAction SilentlyContinue

$p = Start-Process -FilePath $Cmd -ArgumentList $Arg -Credential $cred -WorkingDirectory $wd `
  -RedirectStandardOutput $so -RedirectStandardError $se -PassThru
if (-not $p.WaitForExit($TimeoutSec * 1000)) { Write-Output "TIMEOUT after ${TimeoutSec}s"; $p.Kill() }
Write-Output "== exit=$($p.ExitCode)"
Get-Content $so -ErrorAction SilentlyContinue
$err = Get-Content $se -ErrorAction SilentlyContinue
if ($err) { Write-Output '== stderr'; $err }
