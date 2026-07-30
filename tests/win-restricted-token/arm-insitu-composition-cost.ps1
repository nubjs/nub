$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$prof = 'C:\Users\probe2.NUB-WIN-USID'
$proj = "$prof\jail\project"
$sib  = "$prof\jail\sibling"
$node = 'C:\node\node.exe'
$lock = 'unique,world,restricted,users'
$init = 'unique,world,restricted,users,self,logon'

Write-Output "######## 1. IN-SITU confinement via cmd builtins (no impersonation at all) ########"
# `type` and `dir` are cmd BUILTINS, so this arm needs no grandchild process and
# measures the restricted token's own read behaviour end to end. Each read is on
# its own line so one denial cannot short-circuit the rest.
$c = 'echo ALIVE' +
     ' & echo [granted project] & type package.json' +
     ' & echo [ungranted sibling] & type ..\sibling\sibling.txt' +
     ' & echo [profile npmrc] & type "' + $prof + '\.npmrc"' +
     ' & echo [profile ssh key] & type "' + $prof + '\.ssh\id_rsa"' +
     ' & echo [profile listing] & dir /b "' + $prof + '"' +
     ' & echo [C:\ listing] & dir /b C:\' +
     ' & echo [deep granted] & type node_modules\nested-pkg\a\b\c\probe.js'
& $jail launch --il low --restrict $lock --out "$env:TEMP\l9-1.txt" --cwd $proj -- cmd.exe /c $c
Write-Output ""
Write-Output "######## 1b. BASELINE, identical command, il=none ########"
& $jail launch --il none --out "$env:TEMP\l9-1b.txt" --cwd $proj -- cmd.exe /c $c

Write-Output ""
Write-Output "######## 2. GRANDCHILD spawn -- can the jail run another exe? ########"
& $jail launch --il low --restrict $lock --out "$env:TEMP\l9-2.txt" --cwd $proj -- cmd.exe /c 'whoami & echo AFTER_WHOAMI'
Write-Output "-- baseline --"
& $jail launch --il none --out "$env:TEMP\l9-2b.txt" --cwd $proj -- cmd.exe /c 'whoami & echo AFTER_WHOAMI'

Write-Output ""
Write-Output "######## 3. Q5 COMPOSITION: low mandatory label + restricting sids ########"
& $jail label $proj low -r
& $jail showlabel $proj
& $jail check --il low --restrict $lock -- $proj "$proj\package.json" $sib "$prof\.npmrc"
Write-Output "-- and in situ: write into the labeled+granted project from the jail --"
& $jail launch --il low --restrict $lock --out "$env:TEMP\l9-3.txt" --cwd $proj -- cmd.exe /c 'echo hi> composed.txt & type composed.txt & echo hi2> "' + $prof + '\escape.txt"'

Write-Output ""
Write-Output "######## 4. Q6 COST: recursive grant over a real node_modules ########"
$big = "$prof\jail\big"
if (-not (Test-Path $big)) {
  New-Item -ItemType Directory -Force -Path $big | Out-Null
  Set-Content "$big\package.json" '{"name":"big","version":"1.0.0"}'
  Push-Location $big
  & C:\node\npm.cmd install --ignore-scripts --no-audit --no-fund typescript esbuild react react-dom lodash chalk 2>&1 | Select-Object -Last 3
  Pop-Location
}
$cnt = (Get-ChildItem -Recurse -Force $big -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Output "entries_under_big=$cnt"
& $jail grant $big unique -r
Write-Output "-- second pass (already-granted, measures steady-state re-run cost) --"
& $jail grant $big unique -r
