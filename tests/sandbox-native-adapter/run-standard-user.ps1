param(
    [Parameter(Mandatory=$true)][string]$TestBinary,
    [Parameter(Mandatory=$true)][string]$ReportDirectory
)

$ErrorActionPreference = 'Stop'
$TestBinary = (Resolve-Path $TestBinary).Path
New-Item -ItemType Directory -Force $ReportDirectory | Out-Null
$ReportDirectory = (Resolve-Path $ReportDirectory).Path
$name = 'sbx' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$stage = Join-Path $env:PUBLIC $name
$password = ConvertTo-SecureString ([guid]::NewGuid().ToString('N') + '!aA1') -AsPlainText -Force
$user = $null
$exitCode = 1
try {
    $user = New-LocalUser -Name $name -Password $password -AccountNeverExpires -PasswordNeverExpires
    $users = Get-LocalGroup -SID 'S-1-5-32-545'
    Add-LocalGroupMember -Group $users -Member $name
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item $TestBinary (Join-Path $stage 'probe.exe')
    @'
param([string]$Stage)
$ErrorActionPreference = 'Stop'
$profileRoot = [Environment]::GetFolderPath('UserProfile')
if (!$profileRoot -or $profileRoot -like '*systemprofile*') { throw 'A normal user profile is required' }
$env:USERPROFILE = $profileRoot
$env:HOME = $profileRoot
$env:APPDATA = Join-Path $profileRoot 'AppData\Roaming'
$env:LOCALAPPDATA = Join-Path $profileRoot 'AppData\Local'
$env:TEMP = Join-Path $env:LOCALAPPDATA 'Temp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Force $env:TEMP | Out-Null
$owned = Join-Path $profileRoot 'sandbox-production-probe'
New-Item -ItemType Directory -Force $owned | Out-Null
Copy-Item (Join-Path $Stage 'probe.exe') (Join-Path $owned 'probe.exe')
Set-Location $owned
whoami /all
Write-Host "USERPROFILE=$env:USERPROFILE"
Get-FileHash '.\probe.exe'
& '.\probe.exe' --ignored --nocapture --test-threads=1
$code = $LASTEXITCODE
Write-Host "STANDARD_USER_TEST_EXIT=$code"
exit $code
'@ | Set-Content -Encoding ascii (Join-Path $stage 'run.ps1')
    # Only the runner artifacts are shared. The user copies its executable into
    # its own home so ACL preparation is exercised without administrative rights.
    icacls $stage /grant "${name}:(OI)(CI)RX" /T | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Granting read access to probe artifacts failed' }
    $credential = New-Object System.Management.Automation.PSCredential("$env:COMPUTERNAME\$name", $password)
    Start-Service seclogon
    $process = Start-Process powershell.exe -Credential $credential -LoadUserProfile `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "$stage\run.ps1", $stage) `
        -WorkingDirectory $stage -PassThru `
        -RedirectStandardOutput "$ReportDirectory\standard-user.log" `
        -RedirectStandardError "$ReportDirectory\standard-user-error.log"
    if (!$process.WaitForExit(900000)) {
        taskkill /PID $process.Id /T /F | Out-Null
        throw 'Standard-user probes exceeded the 15-minute deadline'
    }
    $process.WaitForExit()
    Get-Content "$ReportDirectory\standard-user.log"
    Get-Content "$ReportDirectory\standard-user-error.log"
    # Read the exact child exit marker as well as the outer test summary. Nested
    # reentry tests also print successful summaries; they cannot bless a failed run.
    $marker = Select-String -Path "$ReportDirectory\standard-user.log" -Pattern '^STANDARD_USER_TEST_EXIT=([0-9]+)$' | Select-Object -Last 1
    if ($marker) { $exitCode = [int]$marker.Matches[0].Groups[1].Value }
    if (!(Select-String -Path "$ReportDirectory\standard-user.log" -Pattern '^test result: ok\. 7 passed; 0 failed;')) { $exitCode = 1 }
} finally {
    if ($user) {
        $sid = $user.SID.Value
        Get-CimInstance Win32_UserProfile -Filter "SID='$sid'" | Remove-CimInstance
        Remove-LocalUser -Name $name
    }
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
}
exit $exitCode
