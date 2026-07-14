#!/usr/bin/env pwsh
# Nub installer for Windows (PowerShell)
# Usage: irm https://raw.githubusercontent.com/nubjs/nub/main/install.ps1 | iex
#
# Customization (env vars):
#   NUB_INSTALL_DIR      install location, absolute path (default: %USERPROFILE%\.nub)
#   NUB_NO_MODIFY_PATH   truthy (1/yes/true/on) to skip editing the User PATH

$ErrorActionPreference = "Stop"

# --- Platform detection ---
$Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
switch ($Arch) {
    "X64"   { $Target = "win32-x64" }
    "Arm64" { $Target = "win32-arm64" }
    default { Write-Error "Unsupported architecture: $Arch"; exit 1 }
}

# --- Version ---
$Version = if ($args.Count -gt 0) { $args[0] } else { "latest" }
if ($Version -eq "latest") {
    # Authenticate the GitHub API call when a token is available: CI runners share
    # an IP and hit the 60/hr unauthenticated rate limit (403). Real users without
    # GITHUB_TOKEN use the anonymous path unchanged.
    $apiHeaders = @{}
    if ($env:GITHUB_TOKEN) { $apiHeaders["Authorization"] = "token $env:GITHUB_TOKEN" }
    $Release = Invoke-RestMethod "https://api.github.com/repos/nubjs/nub/releases/latest" -Headers $apiHeaders
    $Version = $Release.tag_name -replace "^v", ""
}

Write-Host "Installing nub v$Version for $Target..." -ForegroundColor Cyan

# --- Install ---
# The install location is overridable via NUB_INSTALL_DIR (default %USERPROFILE%\.nub).
# Both the resolved dir and the default are normalized to full paths so the
# "is this the default location?" test below is an exact comparison.
$DefaultInstallDir = [System.IO.Path]::GetFullPath("$env:USERPROFILE\.nub")
$InstallDir = if ($env:NUB_INSTALL_DIR) { $env:NUB_INSTALL_DIR } else { $DefaultInstallDir }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$InstallDir = (Resolve-Path -LiteralPath $InstallDir).Path
$BinDir = "$InstallDir\bin"
$Exe = "$BinDir\nub.exe"

# Download the per-platform archive and extract it into the install dir. nub is a
# single self-contained binary that embeds its runtime (preload + vendored
# node_modules + native addon) and JIT-extracts it to the user cache on first run.
# The archive ships bin\ plus a vestigial empty runtime\ (kept only to satisfy the
# sidecar-era `nub upgrade`; the binary ignores it — see release.yml).
$ArchiveName = "nub-$Target.zip"
$Url = "https://github.com/nubjs/nub/releases/download/v$Version/$ArchiveName"
$ChecksumUrl = "$Url.sha256"
Write-Host "Downloading from $Url..."

$TmpZip = Join-Path $env:TEMP "nub-$Target-$PID.zip"
$TmpChecksum = Join-Path $env:TEMP "nub-$Target-$PID.zip.sha256"
# Suppress the per-chunk progress bar — it re-renders on every received byte
# and dominates the total download time in PowerShell.
$prevProgressPreference = $ProgressPreference
try {
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $Url -OutFile $TmpZip -UseBasicParsing
    Invoke-WebRequest -Uri $ChecksumUrl -OutFile $TmpChecksum -UseBasicParsing

    # The sidecar detects corrupt, truncated, stale-cache, or mismatched assets. It
    # is not an independent authenticity check because both files share an origin.
    $ChecksumBytes = [System.IO.File]::ReadAllBytes($TmpChecksum)
    $ChecksumText = [System.Text.Encoding]::ASCII.GetString($ChecksumBytes)
    $ChecksumPattern = "\A(?<Digest>[0-9A-Fa-f]{64})  $([regex]::Escape($ArchiveName))`n\z"
    if ($ChecksumText -cnotmatch $ChecksumPattern) {
        throw "Malformed checksum from: $ChecksumUrl"
    }
    $ExpectedSha256 = $Matches["Digest"]
    $ActualSha256 = (Get-FileHash -LiteralPath $TmpZip -Algorithm SHA256).Hash
    if (-not [string]::Equals($ActualSha256, $ExpectedSha256, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Checksum mismatch for $Url (expected $ExpectedSha256, got $ActualSha256). Refusing to install a corrupt or mismatched archive."
    }

    # Replace any prior nub artifacts for a clean upgrade. In the default ~\.nub —
    # which nub owns outright — drop the whole bin\ and a stale runtime\ from a
    # pre-single-binary install. A user-supplied NUB_INSTALL_DIR may hold unrelated
    # files, so there remove only the two executables we wrote. Then extract bin\.
    if ($InstallDir -ieq $DefaultInstallDir) {
        if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }
        if (Test-Path "$InstallDir\runtime") { Remove-Item -Recurse -Force "$InstallDir\runtime" }
    } else {
        Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath "$BinDir\nub.exe", "$BinDir\nubx.exe"
    }
    Expand-Archive -Path $TmpZip -DestinationPath $InstallDir -Force
} catch {
    Write-Error "Failed to download/verify/extract nub: $_"
    exit 1
} finally {
    $ProgressPreference = $prevProgressPreference
    if (Test-Path $TmpZip) { Remove-Item -Force $TmpZip }
    if (Test-Path $TmpChecksum) { Remove-Item -Force $TmpChecksum }
}

if (-not (Test-Path $Exe)) {
    Write-Error "Archive did not contain bin\nub.exe"
    exit 1
}

# `nubx` is the same binary as `nub`, dispatched on argv[0] (cli.rs reads
# args_os()[0].file_stem(): "nubx" -> exec). The release archive ships only
# bin\nub.exe, so create the nubx alias. On Windows we COPY rather than symlink:
# symlinks require admin/Developer Mode, and a copy reliably yields argv[0]
# "nubx.exe". Re-extract on upgrade wipes bin\, so this is recreated each run.
$Exex = "$BinDir\nubx.exe"
Copy-Item -Path $Exe -Destination $Exex -Force

# Install receipt: marks this dir as a nub self-managed install so `nub upgrade`
# recognizes it as in-place-upgradeable even when NUB_INSTALL_DIR relocated it out
# of the default ~\.nub (cli.rs detect_channel checks for this file).
$Receipt = @'
# This file marks a nub self-managed install so `nub upgrade` can update it in
# place. Created by the nub installer; safe to delete (deleting it disables
# in-place self-update for a non-default install location).
'@
Set-Content -LiteralPath "$InstallDir\.nub-receipt" -Value $Receipt

Write-Host "Installed nub (with nubx) to $Exe" -ForegroundColor Green

# --- PATH setup ---
# Honor NUB_NO_MODIFY_PATH: skip the User PATH edit and just print the dir to add
# (rustup/uv convention).
$NoModifyPath = "$env:NUB_NO_MODIFY_PATH".ToLowerInvariant()
if ($NoModifyPath -in @("1", "yes", "true", "on")) {
    Write-Host "Add the nub bin path to your PATH:"
    Write-Host "  $BinDir" -ForegroundColor White
    exit 0
} elseif ($NoModifyPath -notin @("", "0", "no", "false", "off")) {
    Write-Error "Invalid NUB_NO_MODIFY_PATH: $env:NUB_NO_MODIFY_PATH (expected 1/yes/true/on or 0/no/false/off)"
    exit 1
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
# Compare against the exact segments rather than a substring match: a custom
# NUB_INSTALL_DIR could be a substring of an unrelated existing entry.
if (($UserPath -split ';') -notcontains $BinDir) {
    [Environment]::SetEnvironmentVariable("Path", "$BinDir;$UserPath", "User")
    $env:Path = "$BinDir;$env:Path"
    Write-Host "Added $BinDir to PATH" -ForegroundColor Green
} else {
    Write-Host "Already in PATH" -ForegroundColor Green
}

Write-Host ""
Write-Host "To get started, open a new terminal and run:" -ForegroundColor Cyan
Write-Host "  nub --version" -ForegroundColor White
