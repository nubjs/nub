param([string]$Stage)
$ErrorActionPreference = 'Stop'
$profileRoot = [Environment]::GetFolderPath('UserProfile')
if (!$profileRoot -or $profileRoot -like '*systemprofile*') { throw 'Normal user profile required' }
$root = Join-Path $profileRoot 'restricted-lowbox-fixture'
New-Item -ItemType Directory $root | Out-Null
Copy-Item "$Stage\launcher.exe", "$Stage\child.exe", "$Stage\native-child.exe" $root
Set-Location $root
whoami /all
Write-Output "FIXTURE_ROOT=$root"
foreach ($name in @('launcher.exe', 'child.exe', 'native-child.exe')) {
    $sha = [Security.Cryptography.SHA256]::Create()
    $stream = [IO.File]::OpenRead((Join-Path $root $name))
    try {
        $hash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
        Write-Output "DEPLOYED_SHA256 file=$name hash=$hash"
    } finally { $stream.Dispose(); $sha.Dispose() }
}
& .\launcher.exe $root
$code = $LASTEXITCODE
foreach ($log in Get-ChildItem $root -Filter '*.log') {
    Write-Output "CHILD_LOG_BEGIN=$($log.Name)"
    Get-Content $log.FullName
    Write-Output "CHILD_LOG_END=$($log.Name)"
}
if (!(Select-String -Path "$root\ordinary.log" -Pattern '^CHILD_COMPLETE label=ordinary$')) { $code = 93 }
if (!(Select-String -Path "$root\ordinary-native.log" -Pattern '^NATIVE_READY$')) { $code = 94 }
# Copy as the owner into a new, normally inherited directory for artifact collection.
# No descriptor is broadened to collect evidence.
$evidence = Join-Path $profileRoot 'restricted-lowbox-evidence'
New-Item -ItemType Directory $evidence | Out-Null
Get-ChildItem $root -File | Copy-Item -Destination $evidence
Set-Location $profileRoot
Remove-Item $root -Recurse -Force
Write-Output "STANDARD_USER_TEST_EXIT=$code"
exit $code
