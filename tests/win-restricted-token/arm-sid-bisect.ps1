$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
$node = 'C:\node\node.exe'
$base = 'unique,world,restricted,users'
$cands = @(
  @('INTERACTIVE','S-1-5-4'), @('AUTH_USERS','S-1-5-11'), @('THIS_ORG','S-1-5-15'),
  @('NTLM_AUTH','S-1-5-64-10'), @('LOCAL_ACCOUNT','S-1-5-113'), @('LOCAL','S-1-2-0'),
  @('CONSOLE_LOGON','S-1-2-1'), @('PRINCIPAL_SELF','S-1-5-10')
)
$i = 0
foreach ($c in $cands) {
  $i++
  $o = "$env:TEMP\l5-$i.txt"
  $r = & $jail launch --il low --restrict "$base,$($c[1])" --out $o --cwd $proj -- $node -e 'console.log("NODE_ALIVE")' 2>&1
  $exit = @($r | Select-String 'child_exit=')
  $body = @(Get-Content $o -ErrorAction SilentlyContinue) -join ' | '
  Write-Output ("{0,-16} {1}  out=[{2}]" -f $c[0], "$exit", "$body")
}
