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

# ── the non-elevated path, via a real standard user ───────────────────────────────────────────
# A GitHub runner is already elevated, so the path an ordinary user hits first — the only one
# that prints an instruction — needs a genuine standard account to reach.
#
# The child's output is REDIRECTED TO A FILE rather than inherited, because a `-Credential`
# launch gets a new console and cannot write to this one. That is the same constraint a UAC
# self-elevation would face, and the concrete reason nub prints an instruction instead of
# elevating itself: the setup's report is the useful part, and a re-launched process cannot put
# it back in front of the person who typed the command.
$stdout = Join-Path $env:TEMP 'nub-unelevated-out.txt'
$stderr = Join-Path $env:TEMP 'nub-unelevated-err.txt'
$pw = 'Pr0be!' + [guid]::NewGuid().ToString('N').Substring(0, 12)
$secure = ConvertTo-SecureString $pw -AsPlainText -Force
New-LocalUser -Name 'nubprobe' -Password $secure -Description 'probe: unelevated capture' `
    -AccountNeverExpires -PasswordNeverExpires -ErrorAction SilentlyContinue | Out-Null
# The standard user must be able to read and execute the binary under the runner's work dir.
icacls (Split-Path $Nub -Parent) /grant 'nubprobe:(OI)(CI)RX' /T | Out-Null
icacls 'D:\a' /grant 'nubprobe:(RX)' | Out-Null
icacls 'D:\a\nub' /grant 'nubprobe:(RX)' | Out-Null
icacls 'D:\a\nub\nub' /grant 'nubprobe:(RX)' | Out-Null
icacls 'D:\a\nub\nub\target' /grant 'nubprobe:(RX)' | Out-Null

Write-Host ""
Write-Host "===== 1. nub setup-sandbox   (NON-elevated, as a standard user) ====="
Write-Host "PS> nub setup-sandbox"
$cred = New-Object System.Management.Automation.PSCredential('nubprobe', $secure)
try {
    Start-Process -FilePath $Nub -ArgumentList 'setup-sandbox' -Credential $cred `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
        -WorkingDirectory $env:TEMP -Wait -ErrorAction Stop
} catch {
    Write-Host "(could not launch as the standard user: $_)"
}
foreach ($f in @($stdout, $stderr)) {
    if ((Test-Path $f) -and (Get-Item $f).Length -gt 0) { Get-Content $f | Write-Host }
}
if (-not ((Test-Path $stdout) -and (Get-Item $stdout).Length -gt 0) -and
    -not ((Test-Path $stderr) -and (Get-Item $stderr).Length -gt 0)) {
    Write-Host "(no output captured)"
}

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

Remove-LocalUser -Name 'nubprobe' -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "########## probe complete ##########"

# The last capture step deliberately exercises a command that exits 1, so end on a clean status
# rather than letting PowerShell propagate it and mark the whole run failed.
exit 0
