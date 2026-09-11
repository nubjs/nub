param([Parameter(Mandatory=$true)][string]$SourceRoot)
$ErrorActionPreference = 'Stop'

# Diagnostic copy only. Never patch the installed runtime or share its file inodes.
$source = (Resolve-Path $SourceRoot).Path
$target = Join-Path (Get-Location).Path '.native-adapter/git-dynamicbase'
if (Test-Path $target) { throw "Refusing to modify an existing copy: $target" }
$original = Join-Path $source 'usr/bin/msys-2.0.dll'
$before = (Get-FileHash $original).Hash
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
if ((Get-FileHash $original).Hash -ne $before) { throw 'Installed MSYS runtime changed' }
@{ source = $original; copy = $dll; sourceSha256 = $before;
   copySha256 = (Get-FileHash $dll).Hash; dllCharacteristicsBefore = $old;
   dllCharacteristicsAfter = ($old -bor 0x40); changedOffset = $offset;
   sourceSignature = (Get-AuthenticodeSignature $original).Status.ToString();
   copySignature = (Get-AuthenticodeSignature $dll).Status.ToString()
} | ConvertTo-Json > reports/msys-dynamicbase-copy.json
# The runtime's source enables ASLR; MSYS packaging reverts it for Docker fork
# failures. This one-bit copy tests that distinction without relaxing ASLR.
"NUB_NATIVE_DYNAMICBASE_GIT=$target" >> $env:GITHUB_ENV
exit 0
