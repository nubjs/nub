param([string]$Stage)
$ErrorActionPreference = 'Stop'
$profileRoot = [Environment]::GetFolderPath('UserProfile')
if (!$profileRoot -or $profileRoot -like '*systemprofile*') { throw 'Normal user profile required' }
$root = Join-Path $profileRoot 'restricted-lowbox-fixture'
New-Item -ItemType Directory $root | Out-Null
Copy-Item "$Stage\launcher.exe", "$Stage\child.exe" $root
Set-Location $root
whoami /all
Write-Output "FIXTURE_ROOT=$root"
Get-FileHash launcher.exe, child.exe -Algorithm SHA256 | Format-List
& .\launcher.exe $root
$code = $LASTEXITCODE
foreach ($log in Get-ChildItem $root -Filter '*.log') {
    Write-Output "CHILD_LOG_BEGIN=$($log.Name)"
    Get-Content $log.FullName
    Write-Output "CHILD_LOG_END=$($log.Name)"
}
if (!(Select-String -Path "$root\ordinary.log" -Pattern '^CHILD_COMPLETE label=ordinary$')) { $code = 93 }
Write-Output "STANDARD_USER_TEST_EXIT=$code"
exit $code
