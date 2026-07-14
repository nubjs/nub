$ErrorActionPreference = "Stop"
if (-not $env:TEMP) { $env:TEMP = [System.IO.Path]::GetTempPath() }

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$Installer = Join-Path $RepoRoot "install.ps1"
$Pwsh = (Get-Command pwsh).Source
$Target = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "win32-x64" }
    "Arm64" { "win32-arm64" }
    default { throw "Unsupported test architecture" }
}
$ArchiveName = "nub-$Target.zip"
$Fixture = Join-Path ([System.IO.Path]::GetTempPath()) "nub-installer-checksums-$([guid]::NewGuid())"
$Assets = Join-Path $Fixture "assets"
$Build = Join-Path $Fixture "build"
$Wrapper = Join-Path $Fixture "invoke-installer.ps1"

try {
    New-Item -ItemType Directory -Force -Path $Assets, (Join-Path $Build "bin"), (Join-Path $Build "runtime") | Out-Null
    [System.IO.File]::WriteAllText((Join-Path $Build "bin\nub.exe"), "NEW-NUB`n")
    [System.IO.File]::WriteAllText((Join-Path $Build "runtime\from-archive"), "FROM-ARCHIVE`n")
    Compress-Archive -Path (Join-Path $Build "bin"), (Join-Path $Build "runtime") -DestinationPath (Join-Path $Assets $ArchiveName)
    $Digest = (Get-FileHash -LiteralPath (Join-Path $Assets $ArchiveName) -Algorithm SHA256).Hash.ToLowerInvariant()

    @'
param(
    [string]$Installer,
    [string]$Assets,
    [string]$HomeDir,
    [string]$TempDir,
    [int]$NoModifyPath
)
$ErrorActionPreference = "Stop"
function global:Invoke-WebRequest {
    param(
        [uri]$Uri,
        [string]$OutFile,
        [switch]$UseBasicParsing
    )
    $Name = [System.IO.Path]::GetFileName($Uri.AbsolutePath)
    Copy-Item -LiteralPath (Join-Path $Assets $Name) -Destination $OutFile -Force
}
$env:USERPROFILE = $HomeDir
$env:NUB_INSTALL_DIR = $null
$env:TEMP = $TempDir
if ($NoModifyPath) {
    $env:NUB_NO_MODIFY_PATH = "1"
} else {
    $env:NUB_NO_MODIFY_PATH = $null
}
& $Installer 9.9.9
'@ | Set-Content -LiteralPath $Wrapper -Encoding utf8

    function Invoke-Case {
        param(
            [string]$Name,
            [AllowNull()][string]$Sidecar,
            [string]$ExpectedMessage,
            [bool]$ExpectSuccess
        )
        $CaseRoot = Join-Path $Fixture "case-$script:CaseIndex"
        $script:CaseIndex++
        $HomeDir = Join-Path $CaseRoot "home"
        $CaseTemp = Join-Path $CaseRoot "tmp"
        $InstallDir = Join-Path $HomeDir ".nub"
        New-Item -ItemType Directory -Force -Path (Join-Path $InstallDir "bin"), (Join-Path $InstallDir "runtime"), $CaseTemp | Out-Null
        [System.IO.File]::WriteAllText((Join-Path $InstallDir "bin\nub.exe"), "OLD-NUB`n")
        [System.IO.File]::WriteAllText((Join-Path $InstallDir "runtime\existing"), "EXISTING-RUNTIME`n")

        $SidecarPath = Join-Path $Assets "$ArchiveName.sha256"
        Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath $SidecarPath
        if ($null -ne $Sidecar) {
            [System.IO.File]::WriteAllBytes($SidecarPath, [System.Text.Encoding]::ASCII.GetBytes($Sidecar))
        }

        $PriorPreference = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        $UserPathBefore = [Environment]::GetEnvironmentVariable("Path", "User")
        $Output = (& $Pwsh -NoProfile -File $Wrapper -Installer $Installer -Assets $Assets -HomeDir $HomeDir -TempDir $CaseTemp -NoModifyPath ([int]$ExpectSuccess) 2>&1 | Out-String)
        $Status = $LASTEXITCODE
        $UserPathAfter = [Environment]::GetEnvironmentVariable("Path", "User")
        if (-not [string]::Equals($UserPathBefore, $UserPathAfter, [System.StringComparison]::Ordinal)) {
            [Environment]::SetEnvironmentVariable("Path", $UserPathBefore, "User")
        }
        $ErrorActionPreference = $PriorPreference

        if ($ExpectSuccess) {
            if ($Status -ne 0) { throw "$Name expected success, got ${Status}: $Output" }
            if ((Get-Content -Raw (Join-Path $InstallDir "bin\nub.exe")) -ne "NEW-NUB`n") { throw "$Name did not install the new binary" }
            if (-not (Test-Path (Join-Path $InstallDir "runtime\from-archive"))) { throw "$Name did not extract the verified archive" }
        } else {
            if ($Status -eq 0) { throw "$Name expected failure" }
            if ($Output -notlike "*$ExpectedMessage*") { throw "$Name missing error '$ExpectedMessage': $Output" }
            if ((Get-Content -Raw (Join-Path $InstallDir "bin\nub.exe")) -ne "OLD-NUB`n") { throw "$Name changed the existing binary before verification" }
            if ((Get-Content -Raw (Join-Path $InstallDir "runtime\existing")) -ne "EXISTING-RUNTIME`n") { throw "$Name changed the existing runtime before verification" }
            if (Test-Path (Join-Path $InstallDir "runtime\from-archive")) { throw "$Name extracted the archive before verification" }
            if (Test-Path (Join-Path $InstallDir ".nub-receipt")) { throw "$Name wrote a receipt after verification failure" }
            if (-not [string]::Equals($UserPathBefore, $UserPathAfter, [System.StringComparison]::Ordinal)) { throw "$Name changed the User PATH before verification" }
            if (Get-ChildItem -Force -LiteralPath $CaseTemp) { throw "$Name did not clean up temporary download files" }
        }
        Write-Host "ok - $Name"
    }

    $script:CaseIndex = 0
    $Valid = "$Digest  $ArchiveName`n"
    Invoke-Case "matching checksum" $Valid "" $true
    Invoke-Case "mismatched checksum" "$('0' * 64)  $ArchiveName`n" "Checksum mismatch" $false
    Invoke-Case "missing sidecar" $null "Failed to download/verify/extract nub" $false

    $MalformedCases = @(
        @{ Name = "wrong basename"; Sidecar = "$Digest  wrong.zip`n" },
        @{ Name = "wrong basename case"; Sidecar = "$Digest  $($ArchiveName.ToUpperInvariant())`n" },
        @{ Name = "extra record"; Sidecar = "$Valid$Valid" },
        @{ Name = "extra field"; Sidecar = "$Digest  $ArchiveName extra`n" },
        @{ Name = "invalid digest"; Sidecar = "g$($Digest.Substring(1))  $ArchiveName`n" },
        @{ Name = "short digest"; Sidecar = "$($Digest.Substring(0, 63))  $ArchiveName`n" },
        @{ Name = "CRLF ending"; Sidecar = "$Digest  $ArchiveName`r`n" },
        @{ Name = "missing LF"; Sidecar = "$Digest  $ArchiveName" },
        @{ Name = "excess trailing newline"; Sidecar = "$Digest  $ArchiveName`n`n" }
    )
    foreach ($Malformed in $MalformedCases) {
        Invoke-Case $Malformed.Name $Malformed.Sidecar "Malformed checksum" $false
    }
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue -LiteralPath $Fixture
}
