# Combined restricting sets: the kernel-object namespace needs RESTRICTED
# (S-1-5-12) or Everyone, the System32 FILES need BUILTIN\Users, and the user's
# OWN sid is deliberately absent from every set -- that absence is what denies
# %USERPROFILE%.
$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
foreach ($spec in @('unique,restricted,users','unique,world,users','unique,world,restricted,users','unique,world,restricted,users,logon')) {
  Write-Output "---- restrict=$spec ----"
  $o = "$env:TEMP\l2-$($spec.Length)-$([Math]::Abs($spec.GetHashCode())).txt"
  if ($spec -match 'logon') {
    & $jail launch --il low --restrict $spec --keep-logon-sid --out $o --cwd $proj -- cmd.exe /c "echo CHILD_ALIVE && whoami && type package.json"
  } else {
    & $jail launch --il low --restrict $spec --out $o --cwd $proj -- cmd.exe /c "echo CHILD_ALIVE && whoami && type package.json"
  }
}
