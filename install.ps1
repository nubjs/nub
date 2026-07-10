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
# Both paths are normalized so the "does nub own this directory?" test at the
# cleanup step below is exact rather than a string compare of two spellings.
$DefaultInstallDir = [System.IO.Path]::GetFullPath("$env:USERPROFILE\.nub")
$InstallDir = if ($env:NUB_INSTALL_DIR) { $env:NUB_INSTALL_DIR } else { $DefaultInstallDir }

try {
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
} catch {
    Write-Error "Failed to create install directory: $InstallDir"
    exit 1
}
$InstallDir = (Resolve-Path -LiteralPath $InstallDir).Path

$InstallBinDir = "$InstallDir\bin"
$InstallExe = "$InstallBinDir\nub.exe"

# Download the per-platform archive and extract it into the install dir. nub is a
# single self-contained binary that embeds its runtime (preload + vendored
# node_modules + native addon) and JIT-extracts it to the user cache on first run.
# The archive ships bin\ plus a vestigial empty runtime\ (kept only to satisfy the
# sidecar-era `nub upgrade`; the binary ignores it — see release.yml).
$Url = "https://github.com/nubjs/nub/releases/download/v$Version/nub-$Target.zip"
Write-Host "Downloading from $Url..."

$TmpZip = Join-Path $env:TEMP "nub-$Target-$PID.zip"
# Suppress the per-chunk progress bar — it re-renders on every received byte
# and dominates the total download time in PowerShell.
$prevProgressPreference = $ProgressPreference
try {
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $Url -OutFile $TmpZip -UseBasicParsing
    # nub owns the default dir outright, so replace any prior bin\ for a clean
    # upgrade and drop a stale runtime\ from a pre-single-binary install. A
    # user-supplied NUB_INSTALL_DIR may hold foreign files, so there we remove
    # only the two executables we wrote. Then extract bin\.
    if ($InstallDir -ieq $DefaultInstallDir) {
        if (Test-Path $InstallBinDir) { Remove-Item -Recurse -Force $InstallBinDir }
        if (Test-Path "$InstallDir\runtime") { Remove-Item -Recurse -Force "$InstallDir\runtime" }
    } else {
        Remove-Item -Force -ErrorAction SilentlyContinue -LiteralPath "$InstallBinDir\nub.exe", "$InstallBinDir\nubx.exe"
    }
    Expand-Archive -Path $TmpZip -DestinationPath $InstallDir -Force
} catch {
    Write-Error "Failed to download/extract nub: $_"
    exit 1
} finally {
    $ProgressPreference = $prevProgressPreference
    if (Test-Path $TmpZip) { Remove-Item -Force $TmpZip }
}

if (-not (Test-Path $InstallExe)) {
    Write-Error "Archive did not contain bin\nub.exe"
    exit 1
}

# `nubx` is the same binary as `nub`, dispatched on argv[0] (cli.rs reads
# args_os()[0].file_stem(): "nubx" -> exec). The release archive ships only
# bin\nub.exe, so create the nubx alias. On Windows we COPY rather than symlink:
# symlinks require admin/Developer Mode, and a copy reliably yields argv[0]
# "nubx.exe". Re-extract on upgrade wipes bin\, so this is recreated each run.
$InstallExex = "$InstallBinDir\nubx.exe"
Copy-Item -Path $InstallExe -Destination $InstallExex -Force

Write-Host "Installed nub (with nubx) to $InstallExe" -ForegroundColor Green

# --- PATH setup ---
$NoModifyPath = if ($env:NUB_NO_MODIFY_PATH) { $env:NUB_NO_MODIFY_PATH.ToLowerInvariant() } else { "0" }
if ($NoModifyPath -in @("1", "yes", "true", "on")) {
    Write-Host "Please add the nub bin path to your PATH:"
    Write-Host "  $InstallBinDir" -ForegroundColor White
    exit 0
} elseif ($NoModifyPath -notin @("0", "no", "false", "off")) {
    Write-Error "Invalid NUB_NO_MODIFY_PATH: $env:NUB_NO_MODIFY_PATH"
    exit 1
}

# Split on the separator rather than `-like "*$InstallBinDir*"`: a user-supplied
# NUB_INSTALL_DIR can carry wildcard characters, which -like would interpret.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ';') -notcontains $InstallBinDir) {
    [Environment]::SetEnvironmentVariable("Path", "$InstallBinDir;$UserPath", "User")
    $env:Path = "$InstallBinDir;$env:Path"
    Write-Host "Added $InstallBinDir to PATH" -ForegroundColor Green
} else {
    Write-Host "Already in PATH" -ForegroundColor Green
}

Write-Host ""
Write-Host "To get started, open a new terminal and run:" -ForegroundColor Cyan
Write-Host "  nub --version" -ForegroundColor White
