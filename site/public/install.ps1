#!/usr/bin/env pwsh
# Nub installer for Windows (PowerShell)
# Usage: irm https://raw.githubusercontent.com/nubjs/nub/main/install.ps1 | iex

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
    $Release = Invoke-RestMethod "https://api.github.com/repos/nubjs/nub/releases/latest"
    $Version = $Release.tag_name -replace "^v", ""
}

Write-Host "Installing nub v$Version for $Target..." -ForegroundColor Cyan

# --- Install ---
$InstallDir = "$env:USERPROFILE\.nub"
$BinDir = "$InstallDir\bin"
$Exe = "$BinDir\nub.exe"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# Download the per-platform archive (binary + runtime) and extract it into the
# install dir. The archive ships bin\nub.exe alongside runtime\ (preload.mjs +
# vendored node_modules); without runtime\, nub cannot transpile at all (A30).
$Url = "https://github.com/nubjs/nub/releases/download/v$Version/nub-$Target.zip"
Write-Host "Downloading from $Url..."

$TmpZip = Join-Path $env:TEMP "nub-$Target-$PID.zip"
try {
    Invoke-WebRequest -Uri $Url -OutFile $TmpZip -UseBasicParsing
    # Replace any prior bin\ + runtime\ for a clean upgrade, then extract.
    if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }
    if (Test-Path "$InstallDir\runtime") { Remove-Item -Recurse -Force "$InstallDir\runtime" }
    Expand-Archive -Path $TmpZip -DestinationPath $InstallDir -Force
} catch {
    Write-Error "Failed to download/extract nub: $_"
    exit 1
} finally {
    if (Test-Path $TmpZip) { Remove-Item -Force $TmpZip }
}

if (-not (Test-Path $Exe)) {
    Write-Error "Archive did not contain bin\nub.exe"
    exit 1
}

Write-Host "Installed nub to $Exe" -ForegroundColor Green

# --- PATH setup ---
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$BinDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$BinDir;$UserPath", "User")
    $env:Path = "$BinDir;$env:Path"
    Write-Host "Added $BinDir to PATH" -ForegroundColor Green
} else {
    Write-Host "Already in PATH" -ForegroundColor Green
}

Write-Host ""
Write-Host "To get started, open a new terminal and run:" -ForegroundColor Cyan
Write-Host "  nub --version" -ForegroundColor White
