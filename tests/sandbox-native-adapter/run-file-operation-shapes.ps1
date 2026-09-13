param(
    [ValidateSet('x64', 'arm64')][string]$Architecture = 'x64',
    [switch]$CompileOnly
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$detours = Join-Path $root 'crates\nub-sandbox\native\detours'
$source = Join-Path $PSScriptRoot 'file-operation-shapes.cpp'
$outputDirectory = Join-Path $root '.native-adapter'
$output = Join-Path $outputDirectory "file-operation-shapes-$Architecture.exe"
if (!(Test-Path $detours) -or !(Test-Path $source)) { throw 'Expected the vendored Detours source and fixture source.' }
New-Item -ItemType Directory -Force $outputDirectory | Out-Null

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vs = & $vswhere -latest -products '*' -property installationPath
if (!$vs) { throw 'Visual Studio C++ tools were not found.' }
$vcArchitecture = if ($Architecture -eq 'arm64') { 'amd64_arm64' } else { 'amd64' }
$detourSources = @('detours.cpp', 'modules.cpp', 'disasm.cpp', 'image.cpp', 'creatwth.cpp',
    'disolx86.cpp', 'disolx64.cpp', 'disolia64.cpp', 'disolarm.cpp', 'disolarm64.cpp') |
    ForEach-Object { Join-Path $detours $_ }
$allSources = @($source) + @($detourSources)
$quotedSources = ($allSources | ForEach-Object { '"' + $_ + '"' }) -join ' '
$build = @"
@echo off
call "$vs\VC\Auxiliary\Build\vcvarsall.bat" $vcArchitecture
if errorlevel 1 exit /b 1
cl /nologo /std:c++17 /W4 /WX /MT /EHsc /I"$detours" $quotedSources /Fe:"$output" /link advapi32.lib
"@
$buildPath = Join-Path $outputDirectory "build-file-operation-shapes-$Architecture.cmd"
Set-Content -Path $buildPath -Value $build -Encoding ascii
cmd /c $buildPath
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "FILE_OPERATION_SHAPES_BINARY=$output"
if (!$CompileOnly) {
    & $output
    exit $LASTEXITCODE
}
