# LEAD (adjacent to this lane's route, same harness): can the SECRETS be denied
# by mandatory policy instead, keeping the proven low-IL token that boots fine?
$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$prof = 'C:\Users\probe2.NUB-WIN-USID'
$proj = "$prof\jail\project"
$c = 'echo ALIVE & echo [npmrc] & type "' + $prof + '\.npmrc" & echo [sshkey] & type "' + $prof + '\.ssh\id_rsa" & echo [project] & type package.json'
Write-Output "---- BEFORE labeling: proven low-IL token reads the secrets (the gap) ----"
& $jail launch --il low --out "$env:TEMP\l11a.txt" --cwd $proj -- cmd.exe /c $c
Write-Output ""
Write-Output "---- label the secrets Medium/NO_READ_UP as their unprivileged OWNER ----"
& $jail label "$prof\.npmrc" noreadup
& $jail label "$prof\.ssh" noreadup -r
& $jail showlabel "$prof\.npmrc"
Write-Output "-- NEGATIVE CONTROL: the same label write on a path we do not own --"
& $jail label 'C:\Windows' noreadup
Write-Output ""
Write-Output "---- AFTER labeling: same token, same command ----"
& $jail launch --il low --out "$env:TEMP\l11b.txt" --cwd $proj -- cmd.exe /c $c
Write-Output ""
Write-Output "---- CONTROL: a MEDIUM-il child (a normal app) must still read them ----"
& $jail launch --il medium --out "$env:TEMP\l11c.txt" --cwd $proj -- cmd.exe /c $c
