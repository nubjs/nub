param([Parameter(Mandatory=$true)][string]$SourceRoot, [switch]$IncludeExecutables)
$ErrorActionPreference = 'Stop'

# Diagnostic copy only. Never patch the installed runtime or share its file inodes.
$source = (Resolve-Path $SourceRoot).Path
$copyName = if ($IncludeExecutables) { 'git-dynamicbase-allimages' } else { 'git-dynamicbase' }
$target = Join-Path (Get-Location).Path ".native-adapter/$copyName"
if (Test-Path $target) { throw "Refusing to modify an existing copy: $target" }
$original = Join-Path $source 'usr/bin/msys-2.0.dll'
$before = (Get-FileHash -LiteralPath $original).Hash
if ($before -ne 'E55BE2D89F2756F540A8F2ACF18CD14BF275D1187A32D5189946EE5C896BCF03') {
  throw "Unrecognized MSYS runtime: $before"
}
& robocopy $source $target /E /COPY:DAT /DCOPY:DAT /R:0 /W:0 /NFL /NDL /NJH /NJS
if ($LASTEXITCODE -ge 8) { throw "Copying the disposable Git tree failed: $LASTEXITCODE" }
$dll = Join-Path $target 'usr/bin/msys-2.0.dll'
$bytes = [IO.File]::ReadAllBytes($dll)
$pe = [BitConverter]::ToUInt32($bytes, 60)
if ([BitConverter]::ToUInt32($bytes, $pe) -ne 0x4550 -or
    [BitConverter]::ToUInt16($bytes, $pe + 4) -ne 0x8664) { throw 'Expected AMD64 PE image' }
$offset = $pe + 24 + 70
$old = [BitConverter]::ToUInt16($bytes, $offset)
if ($old -band 0x40) { throw 'Control already has DYNAMIC_BASE enabled' }
$updated = [BitConverter]::GetBytes([uint16]($old -bor 0x40))
[Array]::Copy($updated, 0, $bytes, $offset, 2)
[IO.File]::WriteAllBytes($dll, $bytes)
if ((Get-FileHash -LiteralPath $original).Hash -ne $before) { throw 'Installed MSYS runtime changed' }
@{ source = $original; copy = $dll; sourceSha256 = $before;
   copySha256 = (Get-FileHash -LiteralPath $dll).Hash; dllCharacteristicsBefore = $old;
   dllCharacteristicsAfter = ($old -bor 0x40); changedOffset = $offset;
   sourceSignature = (Get-AuthenticodeSignature $original).Status.ToString();
   copySignature = (Get-AuthenticodeSignature $dll).Status.ToString()
} | ConvertTo-Json > "reports/$copyName-copy.json"
if ($IncludeExecutables) {
  $records = @()
  foreach ($file in Get-ChildItem (Join-Path $target 'usr/bin') -Filter '*.exe') {
    $originalExe = Join-Path $source "usr/bin/$($file.Name)"
    $originalHash = (Get-FileHash -LiteralPath $originalExe).Hash
    $image = [IO.File]::ReadAllBytes($file.FullName)
    $imagePe = [BitConverter]::ToUInt32($image, 60)
    if ([BitConverter]::ToUInt32($image, $imagePe) -ne 0x4550 -or
        [BitConverter]::ToUInt16($image, $imagePe + 4) -ne 0x8664) { throw "Expected AMD64 executable: $file" }
    $imageOffset = $imagePe + 24 + 70
    $flags = [BitConverter]::ToUInt16($image, $imageOffset)
    if (!($flags -band 0x40)) {
      $replacement = [BitConverter]::GetBytes([uint16]($flags -bor 0x40))
      [Array]::Copy($replacement, 0, $image, $imageOffset, 2)
      [IO.File]::WriteAllBytes($file.FullName, $image)
    }
    if ((Get-FileHash -LiteralPath $originalExe).Hash -ne $originalHash) { throw "Installed executable changed: $originalExe" }
    $records += @{ name = $file.Name; sourceSha256 = $originalHash; copySha256 = (Get-FileHash -LiteralPath $file.FullName).Hash;
      characteristicsBefore = $flags; characteristicsAfter = ($flags -bor 0x40); changedOffset = $imageOffset }
  }
  $records | ConvertTo-Json > reports/msys-dynamicbase-executables.json
  "NUB_NATIVE_DYNAMICBASE_ALLIMAGES_GIT=$target" >> $env:GITHUB_ENV
} else {
  "NUB_NATIVE_DYNAMICBASE_GIT=$target" >> $env:GITHUB_ENV
}
# The runtime's source enables ASLR; MSYS packaging reverts it for Docker fork
# failures. This one-bit copy tests that distinction without relaxing ASLR.
exit 0
