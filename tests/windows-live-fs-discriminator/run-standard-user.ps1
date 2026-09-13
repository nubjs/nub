param(
    [Parameter(Mandatory=$true)][string]$ProbeBinary,
    [Parameter(Mandatory=$true)][string]$ImageExe,
    [Parameter(Mandatory=$true)][string]$ImageDll,
    [Parameter(Mandatory=$true)][string]$ExistingSource,
    [Parameter(Mandatory=$true)][string]$NearestNonmatch,
    [Parameter(Mandatory=$true)][string]$ReportDirectory
)

$ErrorActionPreference = 'Stop'
$ProbeBinary = (Resolve-Path $ProbeBinary).Path
$ImageExe = (Resolve-Path $ImageExe).Path
$ImageDll = (Resolve-Path $ImageDll).Path
$ExistingSource = (Resolve-Path $ExistingSource).Path
$NearestNonmatch = (Resolve-Path $NearestNonmatch).Path
New-Item -ItemType Directory -Force $ReportDirectory | Out-Null
$ReportDirectory = (Resolve-Path $ReportDirectory).Path
$name = 'lfs' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$stage = Join-Path $env:PUBLIC $name
$password = ConvertTo-SecureString ([guid]::NewGuid().ToString('N') + '!aA1') -AsPlainText -Force
$user = $null
$process = $null
$exitCode = 1
try {
    $user = New-LocalUser -Name $name -Password $password -AccountNeverExpires -PasswordNeverExpires
    Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-545') -Member $name
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item $ProbeBinary (Join-Path $stage 'probe.exe')
    Copy-Item $ImageExe (Join-Path $stage 'image-fixture.exe')
    Copy-Item $ImageDll (Join-Path $stage 'image-fixture.dll')
    Copy-Item $ExistingSource (Join-Path $stage 'existing-readable.txt')
    Copy-Item $NearestNonmatch (Join-Path $stage 'image-nonmatch.txt')
    @'
param([string]$Stage)
$ErrorActionPreference = 'Stop'
$profile = [Environment]::GetFolderPath('UserProfile')
if (!$profile -or $profile -like '*systemprofile*') { throw 'A normal user profile is required' }
$env:TEMP = Join-Path $profile 'AppData\Local\Temp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Force $env:TEMP | Out-Null
$owned = Join-Path $profile 'nub-live-fs-discriminator'
New-Item -ItemType Directory -Force $owned | Out-Null
Copy-Item (Join-Path $Stage 'probe.exe') (Join-Path $owned 'probe.exe')
Copy-Item (Join-Path $Stage 'image-fixture.exe') (Join-Path $owned 'image-fixture.exe')
Copy-Item (Join-Path $Stage 'image-fixture.dll') (Join-Path $owned 'image-fixture.dll')
Copy-Item (Join-Path $Stage 'existing-readable.txt') (Join-Path $owned 'existing-readable.txt')
Copy-Item (Join-Path $Stage 'image-nonmatch.txt') (Join-Path $owned 'image-nonmatch.txt')
Set-Location $owned
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
"ORDINARY_USER=$($identity.Name) SID=$($identity.User.Value) ADMIN=$($principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
whoami /all
Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSArchitecture | ConvertTo-Json -Compress
certutil.exe -hashfile .\probe.exe SHA256
certutil.exe -hashfile .\image-fixture.exe SHA256
certutil.exe -hashfile .\image-fixture.dll SHA256
certutil.exe -hashfile .\existing-readable.txt SHA256
certutil.exe -hashfile .\image-nonmatch.txt SHA256
& .\probe.exe
$code = $LASTEXITCODE
"STANDARD_USER_PROBE_EXIT=$code"
Set-Location $Stage
Remove-Item -Recurse -Force $owned
exit $code
'@ | Set-Content -Encoding ascii (Join-Path $stage 'run.ps1')
    icacls $stage /grant "${name}:(OI)(CI)RX" /T | Out-Null
    $credential = [System.Management.Automation.PSCredential]::new("$env:COMPUTERNAME\$name", $password)
    Start-Service seclogon
    $process = Start-Process powershell.exe -Credential $credential -LoadUserProfile -WorkingDirectory $stage -PassThru `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "$stage\run.ps1", $stage) `
        -RedirectStandardOutput "$ReportDirectory\standard-user.log" -RedirectStandardError "$ReportDirectory\standard-user-error.log"
    if (!$process.WaitForExit(180000)) { taskkill /PID $process.Id /T /F | Out-Null; throw 'Probe exceeded three minutes' }
    $process.WaitForExit()
    $marker = Select-String -Path "$ReportDirectory\standard-user.log" -Pattern '^STANDARD_USER_PROBE_EXIT=([0-9]+)$' | Select-Object -Last 1
    if ($marker) { $exitCode = [int]$marker.Matches[0].Groups[1].Value }
    if (!(Select-String -Path "$ReportDirectory\standard-user.log" -Pattern '^RESULT=PASS(?:\s|$)')) { $exitCode = 1 }
} finally {
    $cleanup = @{ user = $name; stage = $stage; errors = @() }
    try { if ($process -and !$process.HasExited) { taskkill /PID $process.Id /T /F | Out-Null } } catch { $cleanup.errors += "process: $($_.Exception.Message)" }
    foreach ($log in @('standard-user.log', 'standard-user-error.log')) { if (Test-Path "$ReportDirectory\$log") { Get-Content "$ReportDirectory\$log" } }
    if ($user) {
        $sid = $user.SID.Value
        try { Get-CimInstance Win32_UserProfile -Filter "SID='$sid'" | Remove-CimInstance } catch { $cleanup.errors += "profile: $($_.Exception.Message)" }
        try { Remove-LocalUser -Name $name } catch { $cleanup.errors += "account: $($_.Exception.Message)" }
    }
    try { if (Test-Path $stage) { Remove-Item -Recurse -Force $stage } } catch { $cleanup.errors += "stage: $($_.Exception.Message)" }
    $cleanup | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 "$ReportDirectory\standard-user-cleanup.json"
    if ($cleanup.errors.Count) { $exitCode = 1 }
}
exit $exitCode
