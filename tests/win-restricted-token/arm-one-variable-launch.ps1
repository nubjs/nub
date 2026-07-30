# One variable at a time. cmd.exe itself starts under every combined set, so the
# 0xC0000142 above came from the GRANDchild (whoami.exe), not from cmd -- these
# arms separate "cmd builtins work" from "a fresh exe can initialize".
$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
$sib  = 'C:\Users\probe2.NUB-WIN-USID\jail\sibling'
$prof = 'C:\Users\probe2.NUB-WIN-USID'
$set  = 'unique,world,restricted,users'
$i = 0
function Arm($label, $spec, $cmdArr, $keepLogon) {
  $script:i++
  Write-Output ""
  Write-Output "---- [$label] restrict=$spec ----"
  $o = "$env:TEMP\l3-$script:i.txt"
  if (-not $spec) { & $jail launch --il low --out $o --cwd $proj -- @cmdArr; return }
  if ($keepLogon) { & $jail launch --il low --restrict $spec --keep-logon-sid --out $o --cwd $proj -- @cmdArr }
  else            { & $jail launch --il low --restrict $spec --out $o --cwd $proj -- @cmdArr }
}
# builtins only: does the ACE'd project read work, and are the negatives denied?
Arm 'cmd builtins' $set @('cmd.exe','/c','echo ALIVE && type package.json && dir /b && type ..\sibling\sibling.txt && type "' + $prof + '\.npmrc"') $false
# a fresh exe from System32
Arm 'whoami.exe'   $set @('cmd.exe','/c','whoami') $false
# node directly as the launched image
Arm 'node -e'      $set @('C:\node\node.exe','-e','console.log("NODE_ALIVE",process.version)') $false
# node with the logon sid also in the restricting set
Arm 'node -e +logon' ($set + ',logon') @('C:\node\node.exe','-e','console.log("NODE_ALIVE",process.version)') $true
# BASELINE re-run LAST: same command, no restricting set, to prove the failures
# above are the restricting set and not the harness or a stale fixture.
Arm 'BASELINE node' $null @('C:\node\node.exe','-e','console.log("NODE_ALIVE",process.version)') $false
