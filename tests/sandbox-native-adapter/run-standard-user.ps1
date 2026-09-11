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
$process = $null
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
$developerMode = @{ registryView = 'Registry64'; key = 'SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock'; name = 'AllowDevelopmentWithoutDevLicense' }
$registry = $null
$key = $null
try {
    $registry = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
    $key = $registry.OpenSubKey($developerMode.key, $false)
    if (!$key) { $developerMode.state = 'missing-key' }
    elseif ($key.GetValueNames() -notcontains $developerMode.name) { $developerMode.state = 'missing-value' }
    else {
        $developerMode.state = 'present'
        $developerMode.value = $key.GetValue($developerMode.name)
        $developerMode.kind = $key.GetValueKind($developerMode.name).ToString()
    }
} catch {
    $developerMode.state = 'read-error'
    $developerMode.error = $_.Exception.Message
} finally {
    if ($key) { $key.Dispose() }
    if ($registry) { $registry.Dispose() }
}
$env:SANDBOX_SYMLINK_DEVELOPER_MODE = $developerMode | ConvertTo-Json -Compress
Write-Host "WINDOWS_SYMLINK_CONTEXT=$env:SANDBOX_SYMLINK_DEVELOPER_MODE"
$hash = [Security.Cryptography.SHA256]::Create()
$binaryStream = [IO.File]::OpenRead((Join-Path $owned 'probe.exe'))
try {
    $digest = [BitConverter]::ToString($hash.ComputeHash($binaryStream)).Replace('-', '').ToLowerInvariant()
    Write-Host "STANDARD_USER_BINARY_SHA256=$digest"
} finally {
    $binaryStream.Dispose()
    $hash.Dispose()
}
& '.\probe.exe' --ignored --nocapture --test-threads=1
$code = $LASTEXITCODE
try {
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $journal = Join-Path $env:ProgramData "nub-sandbox-$sid\registry.json"
    if (Test-Path $journal) {
        Write-Host ('STANDARD_USER_REGISTRY_BASE64=' + [Convert]::ToBase64String([IO.File]::ReadAllBytes($journal)))
    }
} catch { Write-Host "STANDARD_USER_REGISTRY_ERROR=$($_.Exception.Message)" }
Write-Host "STANDARD_USER_TEST_EXIT=$code"
exit $code
'@ | Set-Content -Encoding ascii (Join-Path $stage 'run.ps1')
    Copy-Item (Join-Path $stage 'run.ps1') (Join-Path $ReportDirectory 'standard-user-run.ps1')
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
    # Read the exact child exit marker as well as the outer test summary. Nested
    # reentry tests also print successful summaries; they cannot bless a failed run.
    $marker = Select-String -Path "$ReportDirectory\standard-user.log" -Pattern '^STANDARD_USER_TEST_EXIT=([0-9]+)$' | Select-Object -Last 1
    if ($marker) { $exitCode = [int]$marker.Matches[0].Groups[1].Value }
    if (!(Select-String -Path "$ReportDirectory\standard-user.log" -Pattern '^test result: ok\. 8 passed; 0 failed;')) { $exitCode = 1 }
} finally {
    $cleanup = @{ user = $name; stage = $stage; errors = @() }
    # Keep failure output outside the disposable profile, including timeout paths.
    try {
        if ($process -and !$process.HasExited) {
            taskkill /PID $process.Id /T /F | Out-Null
            if (!$process.WaitForExit(30000)) { throw 'Standard-user process did not exit after taskkill' }
        }
    } catch { $cleanup.errors += "process: $($_.Exception.Message)"; $exitCode = 1 }
    foreach ($log in @('standard-user.log', 'standard-user-error.log')) {
        $path = Join-Path $ReportDirectory $log
        if (Test-Path $path) { Get-Content $path -ErrorAction Continue }
    }
    try {
        $log = Join-Path $ReportDirectory 'standard-user.log'
        if (Test-Path $log) {
            $journal = Select-String -Path $log -Pattern '^STANDARD_USER_REGISTRY_BASE64=(.*)$' | Select-Object -Last 1
            if ($journal) {
                [IO.File]::WriteAllBytes((Join-Path $ReportDirectory 'standard-user-registry.json'), [Convert]::FromBase64String($journal.Matches[0].Groups[1].Value))
                $cleanup.registryCaptured = $true
            }
        }
    } catch { $cleanup.errors += "registry artifact: $($_.Exception.Message)"; $exitCode = 1 }
    if ($user) {
        $sid = $user.SID.Value
        $cleanup.sid = $sid
        try { Get-CimInstance Win32_UserProfile -Filter "SID='$sid'" | Remove-CimInstance }
        catch { $cleanup.errors += "profile: $($_.Exception.Message)"; $exitCode = 1 }
        try { Remove-LocalUser -Name $name }
        catch { $cleanup.errors += "account: $($_.Exception.Message)"; $exitCode = 1 }
    }
    try { if (Test-Path $stage) { Remove-Item -Recurse -Force $stage } }
    catch { $cleanup.errors += "stage: $($_.Exception.Message)"; $exitCode = 1 }
    $cleanup | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 "$ReportDirectory\standard-user-cleanup.json"
    foreach ($cleanupError in $cleanup.errors) { Write-Warning $cleanupError }
}
exit $exitCode
