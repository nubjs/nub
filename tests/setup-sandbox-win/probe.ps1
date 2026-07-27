# Windows capture probe for `nub setup-sandbox`.
#
# WHAT THIS EXISTS TO ANSWER. The Windows host setup is the only one of the three that cannot be
# exercised anywhere but a real Windows machine — it creates a local account and installs
# SID-keyed WFP filters, neither of which Docker or a Linux VM can stand in for. This captures
# the whole flow verbatim so the printed output can be read rather than reconstructed.
#
# It deliberately captures BOTH privilege states. A GitHub-hosted `windows-latest` runner is
# already elevated, so the non-elevated path — the one an ordinary user hits first, and the only
# one that prints an instruction — is reached by creating a standard local account and
# re-invoking through it.

param([Parameter(Mandatory = $true)][string]$Nub)

$ErrorActionPreference = 'Continue'

function Show-Step {
    param([string]$Title, [string]$Command, [scriptblock]$Body)
    Write-Host ""
    Write-Host "===== $Title ====="
    Write-Host "PS> $Command"
    & $Body
    Write-Host "[exit $LASTEXITCODE]"
}

Write-Host "########## host ##########"
Write-Host (Get-CimInstance Win32_OperatingSystem).Caption
Write-Host "build $([System.Environment]::OSVersion.Version)"
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
$elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Write-Host "running as: $($identity.Name)"
Write-Host "elevated: $elevated"

# ── the non-elevated path, via a standard user ────────────────────────────────────────────────
# `runas /trustlevel` drops to a restricted token WITHOUT a password prompt, which is what makes
# this capturable in CI at all. It cannot inherit a pipe, so the child writes to a file the
# elevated parent reads back — the same stdio problem a UAC self-elevation would have, which is
# precisely why nub prints an instruction instead of self-elevating.
$out = Join-Path $env:TEMP 'nub-unelevated.txt'
Write-Host ""
Write-Host "===== 1. nub setup-sandbox   (NON-elevated) ====="
Write-Host "PS> nub setup-sandbox"
$cmd = "cmd.exe /c `"`"$Nub`" setup-sandbox > `"$out`" 2>&1`""
Start-Process -FilePath 'runas.exe' -ArgumentList "/trustlevel:0x20000 $cmd" -Wait -NoNewWindow
Start-Sleep -Seconds 3
if (Test-Path $out) { Get-Content $out | Write-Host } else { Write-Host "(no output captured)" }

Show-Step '2. nub setup-sandbox --check   (before setup)' 'nub setup-sandbox --check' {
    & $Nub setup-sandbox --check
}

Show-Step '3. nub setup-sandbox   (ELEVATED)' 'nub setup-sandbox' {
    & $Nub setup-sandbox
}

Show-Step '4. nub setup-sandbox   (second run — idempotency)' 'nub setup-sandbox' {
    & $Nub setup-sandbox
}

Show-Step '5. nub setup-sandbox --check   (after setup)' 'nub setup-sandbox --check' {
    & $Nub setup-sandbox --check
}

Write-Host ""
Write-Host "########## state created ##########"
Write-Host "PS> Get-LocalUser nub-sandbox"
Get-LocalUser -Name 'nub-sandbox' -ErrorAction SilentlyContinue |
    Format-List Name, Enabled, Description, SID | Out-String | Write-Host
Write-Host "PS> netsh wfp show filters (nub provider, counted)"
$wfp = Join-Path $env:TEMP 'wfp.xml'
netsh wfp show filters file="$wfp" | Out-Null
if (Test-Path $wfp) {
    $n = ([regex]::Matches((Get-Content $wfp -Raw), 'nub')).Count
    Write-Host "occurrences of 'nub' in the WFP filter dump: $n"
}

Show-Step '6. nub setup-sandbox --undo' 'nub setup-sandbox --undo' {
    & $Nub setup-sandbox --undo
}

Show-Step '7. nub setup-sandbox --check   (after undo)' 'nub setup-sandbox --check' {
    & $Nub setup-sandbox --check
}

Show-Step '8. nub sandbox setup   (the removed spelling)' 'nub sandbox setup' {
    & $Nub sandbox setup
}

Write-Host ""
Write-Host "########## probe complete ##########"
