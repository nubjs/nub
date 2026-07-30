$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$prof = 'C:\Users\probe2.NUB-WIN-USID'
$proj = "$prof\jail\project"
$sib  = "$prof\jail\sibling"
$lock = 'unique,world,restricted,users'
Write-Output "######## Q5 completed: the ace mask must include WRITE for the second check ########"
# The label satisfies the MANDATORY check; the discretionary SECOND check still
# needs the unique sid to be granted write, so a read-only ace denies the write
# even on a Low-labeled dir. Regrant with FILE_GENERIC_WRITE included.
& $jail grant $proj unique --mask 1301bf -r
& $jail check --il low --restrict $lock -- $proj "$proj\package.json" $sib "$prof\.npmrc"
Write-Output "-- in situ --"
& $jail launch --il low --restrict $lock --out "$env:TEMP\l10a.txt" --cwd $proj -- cmd.exe /c ('echo hi> composed.txt & type composed.txt & echo ESCAPE> "' + $prof + '\escape.txt" & type "' + $prof + '\.npmrc"')
Write-Output ""
Write-Output "######## RESIDUE: can the grant be revoked, leaving the dacl as found? ########"
& $jail dacl $proj
& $jail grant $proj unique -revoke -r
& $jail dacl $proj
& $jail dacl "$proj\package.json"
