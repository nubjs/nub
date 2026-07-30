# Chromium's EXACT two-token shape: the initial impersonation token is built at
# the SAME integrity level as the lockdown token and includes the user's own sid
# (USER_RESTRICTED_SAME_ACCESS); the lockdown primary token omits it
# (USER_LIMITED). The 0xC00000A5 STATUS_BAD_IMPERSONATION_LEVEL in the previous
# round came from impersonating a MEDIUM-IL token on a LOW-IL process.
$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
$sib  = 'C:\Users\probe2.NUB-WIN-USID\jail\sibling'
$node = 'C:\node\node.exe'
$ops  = 'C:\Users\probe2.NUB-WIN-USID\probe\ops-usid.js'
$lock = 'unique,world,restricted,users'
$init = 'unique,world,restricted,users,self,logon'

Write-Output "======== A: does node START with same-IL initial impersonation, never reverted? ========"
& $jail launch --il low --restrict $lock --startup-impersonate $init --keep-logon-sid `
  --out "$env:TEMP\l8a.txt" --cwd $proj -- $node $ops $proj $sib 0

Write-Output ""
Write-Output "======== B: parent strips impersonation at 1000ms; child reads at 3000ms ========"
& $jail launch --il low --restrict $lock --startup-impersonate $init --keep-logon-sid --revert-after 1000 `
  --out "$env:TEMP\l8b.txt" --cwd $proj -- $node $ops $proj $sib 3000

Write-Output ""
Write-Output "======== C: BASELINE last -- same delayed ops file, no confinement ========"
& $jail launch --il none --out "$env:TEMP\l8c.txt" --cwd $proj -- $node $ops $proj $sib 3000
