#Requires -Version 5
# Token scanner: given a mechanism's decoded output, report which workload
# shapes it named. `alpha.txt` is the POSITIVE CONTROL -- if it is absent the
# mechanism did not observe the workload at all and every other row is noise.
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [Parameter(Mandatory = $true)][string]$Mechanism
)

$tokens = [ordered]@{
    'CONTROL create (alpha.txt)'      = 'alpha\.txt'
    'rename SOURCE (alpha.txt)'       = 'alpha\.txt'
    'rename DEST (beta_RENAMEDEST)'   = 'beta_RENAMEDEST'
    'move DEST (gamma_MOVEDEST)'      = 'gamma_MOVEDEST'
    'HARDLINK DEST (delta_LINKDEST)'  = 'delta_LINKDEST'
    'symlink DEST (zeta_SYMDEST)'     = 'zeta_SYMDEST'
    'mmap in-trace (epsilon_MMAP)'    = 'epsilon_MMAP'
    'mmap PRE-EXISTING (omega_PREMMAP)' = 'omega_PREMMAP'
    'handle write (theta_HANDLE)'     = 'theta_HANDLE'
    'mkdir/rmdir (iota_DIRDEST)'      = 'iota_DIRDEST'
    'CHILD-proc rename DEST (lambda_CHILDDEST)' = 'lambda_CHILDDEST'
    'via 8.3 short path (mu_SHORTNAME)' = 'mu_SHORTNAME'
    '8.3 spelling present (RUNNER~1)' = 'RUNNER~1'
    'device path (\\Device\\Harddisk)' = '\\\\Device\\\\Harddisk'
    'NT path prefix (\\??\\)'          = '\\\\\?\?\\\\'
}

if (-not (Test-Path -LiteralPath $Path)) {
    Write-Host "### $Mechanism -- NO OUTPUT FILE ($Path)"
    return
}
$len = (Get-Item -LiteralPath $Path).Length
$text = Get-Content -LiteralPath $Path -Raw -ErrorAction SilentlyContinue
if ($null -eq $text) { $text = '' }

Write-Host ''
Write-Host "### $Mechanism  (decoded output $len bytes)"
foreach ($k in $tokens.Keys) {
    $n = ([regex]::Matches($text, $tokens[$k], 'IgnoreCase')).Count
    $mark = if ($n -gt 0) { 'YES' } else { ' NO' }
    '{0}  {1,-42} hits={2}' -f $mark, $k, $n | Write-Host
}
$storm = ([regex]::Matches($text, 'storm_\d{4}', 'IgnoreCase') | ForEach-Object { $_.Value } | Sort-Object -Unique).Count
Write-Host ("     {0,-42} distinct={1}/500" -f 'storm files named', $storm)
