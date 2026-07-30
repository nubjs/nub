# Driver for the UNIQUE-RESTRICTING-SID probe.
#
#   powershell -File usid.ps1 -Stage <gate|dacl|check|launch|ops|all>
#
# The unique sid is generated ONCE per run and exported as NUB_JAIL_UNIQUE_SID so
# that `grant` and `--restrict unique` provably name the SAME binary sid; without
# that, each Jail.exe invocation would mint its own and every grant would be
# written for a sid no token ever holds -- which would read exactly like
# "ACL writes do not work".
param([string]$Stage = 'all')
$ErrorActionPreference = 'Continue'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$jail = Join-Path $here 'Jail.exe'
$root = Join-Path $env:USERPROFILE 'jail'
$proj = Join-Path $root 'project'
$sib  = Join-Path $root 'sibling'
$node = (Get-Command node -ErrorAction SilentlyContinue).Source
if (-not $node) { $node = 'C:\node\node.exe' }

# NOTE: do NOT name this `H` -- PowerShell resolves aliases BEFORE functions, so `H`
# hits the built-in Get-History alias and every header line errors out.
function Hdr($s) { Write-Output ''; Write-Output "########## $s ##########" }

if (-not $env:NUB_JAIL_UNIQUE_SID) {
  $out = & $jail sidgen
  $line = $out | Select-String -Pattern '^unique_sid=' | Select-Object -First 1
  $env:NUB_JAIL_UNIQUE_SID = ($line -split '=', 2)[1].Trim()
  Write-Output "generated NUB_JAIL_UNIQUE_SID=$env:NUB_JAIL_UNIQUE_SID"
}
$U = $env:NUB_JAIL_UNIQUE_SID

# Paths whose read outcome decides the verdict. Order: roots first so an
# ancestor result is visible above the leaf that depends on it.
$paths = @(
  'C:\',
  'C:\Users',
  $env:USERPROFILE,
  (Join-Path $env:USERPROFILE '.ssh'),
  (Join-Path $env:USERPROFILE '.ssh\id_rsa'),
  (Join-Path $env:USERPROFILE '.npmrc'),
  $proj,
  (Join-Path $proj 'package.json'),
  (Join-Path $proj 'node_modules\nested-pkg\a\b\c\probe.js'),
  $sib,
  (Join-Path $sib 'sibling.txt'),
  'C:\Windows',
  'C:\Windows\System32',
  'C:\Windows\System32\ntdll.dll',
  $node
)

if ($Stage -in @('gate','all')) {
  Hdr 'GATE Q1/Q3 -- arbitrary sid, zero registration'
  & $jail sidgen

  Hdr 'GATE Q2 -- does SetNamedSecurityInfoW accept an ace for an unresolvable sid?'
  Write-Output '--- DACL of the project dir BEFORE ---'
  & $jail dacl $proj
  & $jail grant $proj $U
  Write-Output '--- DACL of the project dir AFTER ---'
  & $jail dacl $proj
  Write-Output '--- a leaf file ---'
  & $jail grant (Join-Path $proj 'package.json') $U
  & $jail dacl (Join-Path $proj 'package.json')
  Write-Output '--- NEGATIVE CONTROL: the same write on paths we do not own ---'
  & $jail grant 'C:\' $U
  & $jail grant 'C:\Windows' $U
  Write-Output '--- CONTROL: the sibling dir stays UNGRANTED on purpose ---'
  & $jail dacl $sib
}

if ($Stage -in @('dacl','all')) {
  Hdr 'DACLs of every decision path (what trustees exist where)'
  foreach ($p in $paths) { & $jail dacl $p }
}

if ($Stage -in @('check','all')) {
  Hdr 'ACCESSCHECK MATRIX'
  # Arm 1 is the mandatory baseline: an unmodified token must be GRANTED
  # everywhere, or a DENIED elsewhere is the harness, not the mechanism.
  & $jail check --il none -- $paths
  & $jail check --il low  -- $paths                        # proven prior design: reads pass
  & $jail check --il low --restrict unique -- $paths       # the candidate
  & $jail check --il low --restrict unique,restricted -- $paths
  & $jail check --il low --restrict unique,world -- $paths
  & $jail check --il low --restrict unique,users -- $paths # permissive control
}

if ($Stage -in @('launch','all')) {
  Hdr 'REAL LAUNCH -- can a child even start with the unique sid as the only restrictor?'
  foreach ($spec in @($null, 'unique', 'unique,restricted', 'unique,world', 'unique,users')) {
    $tag = if ($spec) { $spec } else { '(none)' }
    Write-Output "---- restrict=$tag ----"
    $o = Join-Path $env:TEMP "usid-launch-$([Math]::Abs($tag.GetHashCode())).txt"
    if ($spec) { & $jail launch --il low --restrict $spec --out $o --cwd $proj -- cmd.exe /c "echo CHILD_ALIVE && whoami" }
    else       { & $jail launch --il low                 --out $o --cwd $proj -- cmd.exe /c "echo CHILD_ALIVE && whoami" }
  }
}

if ($Stage -in @('ops','all')) {
  Hdr 'REAL NODE OPS'
  Write-Output "node=$node"
  foreach ($arm in @(@{il='none';r=$null}, @{il='low';r='unique'}, @{il='low';r='unique,restricted'})) {
    $tag = "il=$($arm.il) restrict=$(if ($arm.r) { $arm.r } else { '(none)' })"
    Write-Output "---- $tag ----"
    $o = Join-Path $env:TEMP ("usid-ops-" + ($tag -replace '[^a-z0-9]','') + '.txt')
    if ($arm.r) {
      & $jail launch --il $arm.il --restrict $arm.r --out $o --cwd $proj -- $node (Join-Path $here 'ops-usid.js') $proj $sib
    } else {
      & $jail launch --il $arm.il --out $o --cwd $proj -- $node (Join-Path $here 'ops-usid.js') $proj $sib
    }
  }
}
