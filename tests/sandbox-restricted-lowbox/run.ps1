param([string]$BinaryDirectory, [string]$ReportDirectory)
$ErrorActionPreference = 'Stop'
$BinaryDirectory = (Resolve-Path $BinaryDirectory).Path
New-Item -ItemType Directory -Force $ReportDirectory | Out-Null
$ReportDirectory = (Resolve-Path $ReportDirectory).Path
$name = 'rlb' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$stage = Join-Path $env:PUBLIC $name
$password = ConvertTo-SecureString ([guid]::NewGuid().ToString('N') + '!aA1') -AsPlainText -Force
$user = $null; $process = $null; $exitCode = 1
$cleanup = @{ errors = @() }
try {
    $user = New-LocalUser -Name $name -Password $password -AccountNeverExpires -PasswordNeverExpires
    Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-545') -Member $name
    New-Item -ItemType Directory $stage | Out-Null
    Copy-Item "$BinaryDirectory\launcher.exe", "$BinaryDirectory\child.exe", "$BinaryDirectory\native-child.exe", "$PSScriptRoot\ordinary-user.ps1" $stage
    icacls $stage /grant "${name}:(OI)(CI)RX" /T | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Fixture artifact read grant failed' }
    $credential = New-Object System.Management.Automation.PSCredential("$env:COMPUTERNAME\$name", $password)
    Start-Service seclogon
    $process = Start-Process powershell.exe -Credential $credential -LoadUserProfile `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "$stage\ordinary-user.ps1", $stage) `
        -WorkingDirectory $stage -PassThru -RedirectStandardOutput "$ReportDirectory\standard-user.log" `
        -RedirectStandardError "$ReportDirectory\standard-user-error.log"
    if (!$process.WaitForExit(240000)) { throw 'Ordinary-user fixture exceeded four minutes' }
    $process.WaitForExit()
    $marker = Select-String "$ReportDirectory\standard-user.log" -Pattern '^STANDARD_USER_TEST_EXIT=([0-9]+)$' | Select-Object -Last 1
    if ($marker) { $exitCode = [int]$marker.Matches[0].Groups[1].Value }
} finally {
    try {
        if ($process -and !$process.HasExited) {
            taskkill /PID $process.Id /T /F | Out-Null
            if (!$process.WaitForExit(30000)) { throw 'Probe did not exit after taskkill' }
        }
    } catch { $cleanup.errors += "process: $_"; $exitCode = 1 }
    foreach ($log in @('standard-user.log', 'standard-user-error.log')) {
        if (Test-Path "$ReportDirectory\$log") { Get-Content "$ReportDirectory\$log" }
    }
    if ($user) {
        try {
            $profile = Get-CimInstance Win32_UserProfile -Filter "SID='$($user.SID.Value)'"
            if ($profile) {
                $fixture = Join-Path $profile.LocalPath 'restricted-lowbox-evidence'
                if (Test-Path $fixture) { Copy-Item $fixture "$ReportDirectory\fixture" -Recurse }
            }
        } catch { $cleanup.errors += "profile: $_"; $exitCode = 1 }
        try { Get-CimInstance Win32_UserProfile -Filter "SID='$($user.SID.Value)'" | Remove-CimInstance }
        catch { $cleanup.errors += "profile removal: $_"; $exitCode = 1 }
        try { Remove-LocalUser -Name $name }
        catch { $cleanup.errors += "account: $_"; $exitCode = 1 }
    }
    try { if (Test-Path $stage) { Remove-Item $stage -Recurse -Force } }
    catch { $cleanup.errors += "stage: $_"; $exitCode = 1 }
    $cleanup | ConvertTo-Json -Depth 4 | Set-Content "$ReportDirectory\cleanup.json"
}
exit $exitCode
