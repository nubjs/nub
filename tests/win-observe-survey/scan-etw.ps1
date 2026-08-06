#Requires -Version 5
# Per-EVENT-ID attribution scanner for a tracerpt XML dump.
#
# ⛔ Why this exists: scanning the whole dump for a destination token answers
# "does this string appear anywhere", which is NOT the question. Every
# destination except the ABANDON pair gets reopened later, so its path leaks in
# through an ordinary Create. Only the event ID that named it says whether the
# mechanism recorded the rename/link ITSELF.
param(
    [Parameter(Mandatory = $true)][string]$Path,
    [Parameter(Mandatory = $true)][string]$Label
)

if (-not (Test-Path -LiteralPath $Path)) { Write-Host "### $Label -- NO OUTPUT ($Path)"; return }

$tokens = @('alpha.txt', 'beta_RENAMEDEST', 'gamma_MOVEDEST', 'delta_LINKDEST', 'zeta_SYMDEST',
    'epsilon_MMAP', 'omega_PREMMAP', 'theta_HANDLE', 'lambda_CHILDDEST', 'mu_SHORTNAME',
    'nu_ABANDONRENAME', 'xi_ABANDONLINK')

# One streaming pass: remember the last EventID seen, attribute each token hit
# to it. tracerpt writes <EventID> before <EventData>, so this is well-ordered.
$hits = @{}
$cur = '?'
$rdr = [System.IO.File]::OpenText($Path)
try {
    while ($null -ne ($line = $rdr.ReadLine())) {
        if ($line -match '<EventID[^>]*>(\d+)</EventID>') { $cur = $Matches[1]; continue }
        foreach ($t in $tokens) {
            if ($line.IndexOf($t, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                $k = "$t|$cur"
                if ($hits.ContainsKey($k)) { $hits[$k]++ } else { $hits[$k] = 1 }
            }
        }
    }
} finally { $rdr.Close() }

Write-Host ""
Write-Host "### $Label -- token -> event IDs that NAMED it"
foreach ($t in $tokens) {
    $ids = $hits.Keys | Where-Object { $_ -like "$t|*" } |
        ForEach-Object { $p = $_ -split '\|'; "id$($p[1])x$($hits[$_])" } | Sort-Object
    $mark = if ($ids) { 'YES' } else { ' NO' }
    '{0}  {1,-20} {2}' -f $mark, $t, ($ids -join ' ') | Write-Host
}
