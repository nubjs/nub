# Chromium's two-token startup, driven entirely from the parent.
#   arm 1: impersonate a permissive token on the child's main thread, never revert
#          -> does the child START at all?
#   arm 2: same, then the PARENT strips the impersonation after N ms
#          -> is the confinement actually restored, i.e. do the secret reads FAIL?
#   arm 3: revert-after-0 (immediately) -> control: too early should reproduce the crash
$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
$sib  = 'C:\Users\probe2.NUB-WIN-USID\jail\sibling'
$node = 'C:\node\node.exe'
$ops  = 'C:\Users\probe2.NUB-WIN-USID\probe\ops-usid.js'
$set  = 'unique,world,restricted,users'
foreach ($arm in @(@{n='no-revert';ms=$null}, @{n='revert-2000ms';ms=2000}, @{n='revert-0ms';ms=0})) {
  Write-Output ""
  Write-Output "======== [$($arm.n)] primary={$set}  startup-impersonate=self-full ========"
  $o = "$env:TEMP\l7-$($arm.n).txt"
  if ($null -ne $arm.ms) {
    & $jail launch --il low --restrict $set --startup-impersonate self-full --revert-after $arm.ms --out $o --cwd $proj -- $node $ops $proj $sib
  } else {
    & $jail launch --il low --restrict $set --startup-impersonate self-full --out $o --cwd $proj -- $node $ops $proj $sib
  }
}
Write-Output ""
Write-Output "======== [BASELINE last] il=none, no restricting set, same ops file ========"
& $jail launch --il none --out "$env:TEMP\l7-base.txt" --cwd $proj -- $node $ops $proj $sib
