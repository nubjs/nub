# Why does a fresh exe get 0xC0000142 (STATUS_DLL_INIT_FAILED)? One variable per
# arm. The decisive pair is [no-unique] vs [with-self]: if dropping the unique
# sid does not fix startup but adding the USER's own sid does, then something in
# the startup path is ACL'd to the user specifically -- which is structural, not
# a tuning problem.
$jail = 'C:\Users\probe2.NUB-WIN-USID\probe\Jail.exe'
$proj = 'C:\Users\probe2.NUB-WIN-USID\jail\project'
$node = 'C:\node\node.exe'
$i = 0
function Arm($label, $il, $spec, $extra) {
  $script:i++
  Write-Output ""
  Write-Output "---- [$label] il=$il restrict=$spec $extra ----"
  $o = "$env:TEMP\l4-$script:i.txt"
  $args = @('launch','--il',$il)
  if ($spec) { $args += @('--restrict',$spec) }
  if ($extra) { $args += $extra.Split(' ') }
  $args += @('--out',$o,'--cwd',$proj,'--',$node,'-e','console.log("NODE_ALIVE",process.version)')
  & $jail @args
}
Arm 'medium+unique-set' 'medium' 'unique,world,restricted,users' ''
Arm 'no-unique'         'low'    'world,restricted,users'        ''
Arm 'no-unique+logon'   'low'    'world,restricted,users,logon'  '--keep-logon-sid'
Arm 'with-self'         'low'    'unique,world,restricted,users,self' ''
Arm 'with-self+logon'   'low'    'unique,world,restricted,users,self,logon' '--keep-logon-sid'
Arm 'self-only'         'low'    'self'                          ''
Arm 'BASELINE'          'low'    $null                           ''
