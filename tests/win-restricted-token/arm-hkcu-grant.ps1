$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
$node = 'C:\node\node.exe'
$base = 'unique,world,restricted,users'
Write-Output "--- HKCU root dacl BEFORE, then grant unique KEY_READ (container-inherit) ---"
& $jail grantreg 'CURRENT_USER' unique
Write-Output ""
Write-Output "--- relaunch node with the SAME restricting set as the failing arm ---"
$o = "$env:TEMP\l6.txt"
& $jail launch --il low --restrict $base --out $o --cwd $proj -- $node -e 'console.log("NODE_ALIVE")'
