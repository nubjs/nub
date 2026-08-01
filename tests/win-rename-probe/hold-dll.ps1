# Map a DLL into THIS process (as Node does when it dlopen's the addon), announce
# readiness, then idle so a sibling process can attempt the rename.
param([Parameter(Mandatory=$true)][string]$Dll)
$null = [System.Runtime.InteropServices.NativeLibrary]::Load($Dll)
Write-Host "HOLDER_READY"
while ($true) { Start-Sleep -Seconds 1 }
