#Requires -Version 5.1
<#
.SYNOPSIS
    Black-box release gate for the Windows embedded-runtime compile cache.

.DESCRIPTION
    Exercises a real release nub.exe, nub-launcher.exe and staged nub-native DLL.
    In particular, compile's full-tree repair must fail closed while a live nub
    process is holding the runtime directory, then repair exactly after that
    process exits. The final two probes ensure unsafe cache candidates are
    rejected rather than written through.

    "Holding" means a directory handle, not a loaded DLL: a mapped nub-native.node
    was measured NOT to block the repair rename on Windows. See the holder comment
    below for the measurement and what does block it.
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$Nub,

    [Parameter(Mandatory = $true)]
    [string]$Launcher
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function ConvertTo-Arguments {
    param([string[]]$Values)
    # CI paths do not contain quotes; still quote every argument so spaces in the
    # checkout / runner temp directory cannot change the child command grammar.
    return (($Values | ForEach-Object { '"' + ($_ -replace '"', '\\"') + '"' }) -join ' ')
}

function Invoke-Native {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        [hashtable]$Environment = @{}
    )

    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $FilePath
    $psi.Arguments = ConvertTo-Arguments $Arguments
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    foreach ($key in $Environment.Keys) {
        $psi.EnvironmentVariables[$key] = [string]$Environment[$key]
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    if (-not $process.Start()) { throw "failed to start $FilePath" }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    $process.WaitForExit()
    return @{
        ExitCode = $process.ExitCode
        Stdout = $stdoutTask.Result
        Stderr = $stderrTask.Result
    }
}

function Assert-Exit {
    param([hashtable]$Result, [int]$Expected, [string]$What)
    if ($Result.ExitCode -ne $Expected) {
        throw "$What exited $($Result.ExitCode), expected $Expected.`nstdout:`n$($Result.Stdout)`nstderr:`n$($Result.Stderr)"
    }
}

function Get-TreeFingerprint {
    param([string]$Path)
    $entries = Get-ChildItem -LiteralPath $Path -File -Recurse | Sort-Object FullName | ForEach-Object {
        $relative = $_.FullName.Substring($Path.Length).TrimStart('\', '/') -replace '\\', '/'
        "$relative`t$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash)"
    }
    $text = [string]::Join("`n", [string[]]$entries)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($text)
    return ([System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') }) -join ''
}

function Assert-NoRuntimeDebris {
    param([string]$NubRoot)
    $debris = @(Get-ChildItem -LiteralPath $NubRoot -Force -ErrorAction SilentlyContinue | Where-Object {
        $_.Name -match '^\.runtime-.*\.tmp$' -or $_.Name -match '^\.stale\.runtime-'
    })
    if ($debris.Count -ne 0) {
        throw "runtime repair left stage/stale debris: $($debris.FullName -join ', ')"
    }
}

function Stop-ProcessTree {
    param([System.Diagnostics.Process]$Process)
    if ($null -eq $Process -or $Process.HasExited) { return }
    & "$env:SystemRoot\System32\taskkill.exe" /PID $Process.Id /T /F | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "taskkill failed for holder process $($Process.Id): exit $LASTEXITCODE" }
    $Process.WaitForExit(10000) | Out-Null
    if (-not $Process.HasExited) { throw "holder process $($Process.Id) survived taskkill" }
}

function Assert-NoRuntimeWriteThrough {
    param([string]$Root, [string]$What)
    # The property under test is that the RUNTIME CACHE refuses an unsafe candidate
    # -- ensure_safe_base returns None and extraction falls through. Assert that by
    # the absence of an extracted `runtime-*` tree, NOT by the absence of the `nub`
    # directory: nub's node-discovery / node-env-flags / transpile / v8-compile-cache
    # caches create that directory unconditionally through a plain create_dir_all,
    # with no DACL validation of their own, so it exists either way and testing for
    # it can never pass. Measured on windows-latest against a real release build --
    # under a rejected Everyone-Modify ancestor the `nub` dir held exactly
    # transpile/, v8-compile-cache/, node-discovery.json and node-env-flags.json,
    # runtime-* count 0, while the runtime itself landed in the fallback root.
    #
    # That those other caches write into an unsafe directory at all is a real but
    # SEPARATE gap, tracked as a follow-up; it is not what this gate covers.
    $nubDir = Join-Path $Root 'nub'
    if (-not (Test-Path -LiteralPath $nubDir)) { return }
    $runtimes = @(Get-ChildItem -LiteralPath $nubDir -Directory -Filter 'runtime-*' -Force -ErrorAction SilentlyContinue)
    if ($runtimes.Count -ne 0) {
        throw "runtime wrote through $What`: $($runtimes.FullName -join ', ')"
    }
}

function Invoke-CacheFallbackProbe {
    param(
        [string]$Candidate,
        [string]$Fallback,
        [string]$Work,
        [string]$Fixture
    )
    if (Test-Path -LiteralPath $Fallback) {
        Remove-Item -LiteralPath $Fallback -Recurse -Force
    }
    $env = @{
        XDG_CACHE_HOME = $Candidate
        TEMP = $Fallback
        TMP = $Fallback
        __NUB_LAUNCHER_TEMPLATE = $Launcher
    }
    $result = Invoke-Native -FilePath $Nub -Arguments @($Fixture) -WorkingDirectory $Work -Environment $env
    Assert-Exit $result 0 "cache fallback probe for $Candidate"
    if (($result.Stdout + $result.Stderr) -notmatch 'cache-security-ok') {
        throw "cache fallback probe for $Candidate did not execute the fixture:`nstdout:`n$($result.Stdout)`nstderr:`n$($result.Stderr)"
    }
    $fallbackNub = Join-Path $Fallback 'nub'
    $fallbackRuntimes = @(Get-ChildItem -LiteralPath $fallbackNub -Directory -Filter 'runtime-*' -ErrorAction SilentlyContinue)
    if ($fallbackRuntimes.Count -ne 1) {
        throw "cache fallback probe for $Candidate did not materialize exactly one runtime below $fallbackNub"
    }
    $fallbackAddon = Join-Path $fallbackRuntimes[0].FullName 'addons\nub-native.node'
    if (-not (Test-Path -LiteralPath $fallbackAddon -PathType Leaf)) {
        throw "cache fallback probe for $Candidate did not use the embedded runtime: $fallbackAddon is missing"
    }
}

if (-not (Test-Path -LiteralPath $Nub -PathType Leaf)) { throw "nub.exe not found: $Nub" }
if (-not (Test-Path -LiteralPath $Launcher -PathType Leaf)) { throw "nub-launcher.exe not found: $Launcher" }

$root = Join-Path $env:RUNNER_TEMP ("nub-compile-cache-release-" + [Guid]::NewGuid().ToString('N'))
$holder = $null
try {
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $work = Join-Path $root 'work'
    $cache = Join-Path $root 'cache'
    New-Item -ItemType Directory -Force -Path $work | Out-Null

    $app = Join-Path $work 'app.ts'
    [System.IO.File]::WriteAllText($app, @'
const message: string = "compile-cache-release-ok";
console.log(message);
'@, [System.Text.UTF8Encoding]::new($false))
    # `enum` is NON-ERASABLE, so running this file REQUIRES nub's transpiler and
    # therefore dlopen's nub-native.node. Measured: the previous body was plain
    # JavaScript under a .ts extension, so the holder never mapped the addon at all
    # -- its module list came back empty -- and the gate's "while its addon was
    # loaded" premise was vacuous a second way, independently of the rename
    # semantics below.
    $holderApp = Join-Path $work 'holder.ts'
    [System.IO.File]::WriteAllText($holderApp, @'
enum HolderState { Ready = 1 }
console.log("compile-cache-holder-ready", HolderState.Ready);
setInterval(() => {}, 1000);
'@, [System.Text.UTF8Encoding]::new($false))

    # Cold run both materializes the release runtime and loads the native addon.
    # The template override and --smol target make compile entirely local. An
    # unusable release endpoint turns an accidental template/Node fetch into a
    # deterministic failure instead of silently weakening this offline gate.
    $runtimeEnv = @{
        XDG_CACHE_HOME = $cache
        __NUB_LAUNCHER_TEMPLATE = $Launcher
        NUB_RELEASE_BASE_URL = 'http://127.0.0.1:9'
    }
    $cold = Invoke-Native -FilePath $Nub -Arguments @($app) -WorkingDirectory $work -Environment $runtimeEnv
    Assert-Exit $cold 0 'cold release runtime launch'
    if (($cold.Stdout + $cold.Stderr) -notmatch 'compile-cache-release-ok') { throw "cold runtime launch did not execute fixture" }

    $nubRoot = Join-Path $cache 'nub'
    $runtimeDirs = @(Get-ChildItem -LiteralPath $nubRoot -Directory -Filter 'runtime-*')
    if ($runtimeDirs.Count -ne 1) { throw "expected exactly one extracted runtime, got $($runtimeDirs.Count)" }
    $runtime = $runtimeDirs[0].FullName
    $workerBlob = Join-Path $runtime 'worker-blob-url.cjs'
    $addon = Join-Path $runtime 'addons\nub-native.node'
    if (-not (Test-Path -LiteralPath $workerBlob -PathType Leaf)) { throw "missing transitive compile support: $workerBlob" }
    if (-not (Test-Path -LiteralPath $addon -PathType Leaf)) { throw "missing extracted staged native addon: $addon" }
    Assert-NoRuntimeDebris $nubRoot

    # Keep a live nub process using this runtime tree while repair is attempted.
    #
    # Its WORKING DIRECTORY is the runtime dir, and that -- not the dlopen'd addon --
    # is what makes the repair rename fail. Measured on windows-latest: a DLL
    # confirmed mapped into a live process (verified through its module list) does
    # NOT block renaming the DLL itself, nor its ancestor directory; both returned
    # RENAME_OK. Windows blocks DELETING a mapped image, not renaming it. A process
    # whose current directory is the target does hold a directory handle opened
    # without FILE_SHARE_DELETE, and `std::fs::rename` -- the exact call
    # `swap_extract` makes -- then fails with ERROR_SHARING_VIOLATION (os error 32),
    # one of the two codes `classify_target_rename_failure` maps to `InUse`.
    # 5/5 runs blocked with a holder and 5/5 succeeded once it exited.
    #
    # The holder now genuinely loads the addon too (see its non-erasable fixture
    # above), so the realistic shape is what the gate always claimed; the cwd is
    # what makes the obstruction real rather than assumed.
    $holderOut = Join-Path $root 'holder.stdout'
    $holderErr = Join-Path $root 'holder.stderr'
    $oldCache = $env:XDG_CACHE_HOME
    $oldTemplate = $env:__NUB_LAUNCHER_TEMPLATE
    try {
        $env:XDG_CACHE_HOME = $cache
        $env:__NUB_LAUNCHER_TEMPLATE = $Launcher
        $holder = Start-Process -FilePath $Nub -ArgumentList (ConvertTo-Arguments @($holderApp)) -WorkingDirectory $runtime -PassThru -RedirectStandardOutput $holderOut -RedirectStandardError $holderErr
    } finally {
        $env:XDG_CACHE_HOME = $oldCache
        $env:__NUB_LAUNCHER_TEMPLATE = $oldTemplate
    }
    $ready = $false
    for ($i = 0; $i -lt 100; $i++) {
        if ($holder.HasExited) { throw "holder exited early ($($holder.ExitCode)): $(Get-Content -Raw -LiteralPath $holderErr -ErrorAction SilentlyContinue)" }
        if ((Get-Content -Raw -LiteralPath $holderOut -ErrorAction SilentlyContinue) -match 'compile-cache-holder-ready') {
            $ready = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { throw "holder did not report readiness" }

    $cleanFingerprint = Get-TreeFingerprint $runtime
    $cleanWorkerHash = (Get-FileHash -LiteralPath $workerBlob -Algorithm SHA256).Hash
    [System.IO.File]::WriteAllText($workerBlob, '// intentionally corrupt transitive compile support', [System.Text.UTF8Encoding]::new($false))
    $corruptFingerprint = Get-TreeFingerprint $runtime
    if ($corruptFingerprint -eq $cleanFingerprint) { throw 'corrupting worker-blob-url.cjs did not change the runtime tree fingerprint' }

    $artifact = Join-Path $work 'blocked-artifact.exe'
    $nodeVersion = (& node --version).Trim().TrimStart('v')
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($nodeVersion)) { throw 'node --version failed; the offline --smol compile needs PATH Node' }
    $compile = Invoke-Native -FilePath $Nub -Arguments @('compile', '--smol', '--target', $nodeVersion, '--out', $artifact, $app) -WorkingDirectory $work -Environment $runtimeEnv
    if ($compile.ExitCode -eq 0) { throw "compile unexpectedly repaired a runtime while its addon was loaded:`n$($compile.Stdout)`n$($compile.Stderr)" }
    $failedText = $compile.Stdout + $compile.Stderr
    foreach ($expected in @(
        'did not match the embedded runtime; re-extracting',
        'embedded runtime cache is in use by another nub/node process; close it and retry'
    )) {
        if ($failedText -notmatch $expected) { throw "blocked repair did not report '$expected':`n$failedText" }
    }
    if ((Get-TreeFingerprint $runtime) -ne $corruptFingerprint) { throw 'blocked repair changed the canonical corrupt runtime tree' }
    if ((Get-FileHash -LiteralPath $workerBlob -Algorithm SHA256).Hash -eq $cleanWorkerHash) { throw 'blocked repair unexpectedly restored worker-blob-url.cjs' }
    if (Test-Path -LiteralPath $artifact) { throw "blocked compile left an artifact at $artifact" }
    Assert-NoRuntimeDebris $nubRoot

    Stop-ProcessTree $holder
    $holder = $null

    $healed = Invoke-Native -FilePath $Nub -Arguments @('compile', '--smol', '--target', $nodeVersion, '--out', $artifact, $app) -WorkingDirectory $work -Environment $runtimeEnv
    Assert-Exit $healed 0 'compile retry after holder exit'
    if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) { throw "successful compile did not create $artifact" }
    if ((Get-FileHash -LiteralPath $workerBlob -Algorithm SHA256).Hash -ne $cleanWorkerHash) { throw 'self-heal did not restore worker-blob-url.cjs exactly' }
    if ((Get-TreeFingerprint $runtime) -ne $cleanFingerprint) { throw 'self-heal did not restore the complete runtime tree exactly' }
    Assert-NoRuntimeDebris $nubRoot
    $runArtifact = Invoke-Native -FilePath $artifact -Arguments @() -WorkingDirectory $work
    Assert-Exit $runArtifact 0 'healed compiled artifact'
    if (($runArtifact.Stdout + $runArtifact.Stderr) -notmatch 'compile-cache-release-ok') { throw 'healed compiled artifact did not execute the app' }

    $securityFixture = Join-Path $work 'cache-security.ts'
    [System.IO.File]::WriteAllText($securityFixture, 'const marker: string = "cache-security-ok"; console.log(marker);', [System.Text.UTF8Encoding]::new($false))

    $everyoneModify = Join-Path $root 'everyone-modify-ancestor'
    $aclFallback = Join-Path $root 'safe-acl-fallback'
    New-Item -ItemType Directory -Force -Path $everyoneModify | Out-Null
    & icacls.exe $everyoneModify /grant '*S-1-1-0:(OI)(CI)M' | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "icacls failed to grant Everyone Modify: exit $LASTEXITCODE" }
    Invoke-CacheFallbackProbe -Candidate $everyoneModify -Fallback $aclFallback -Work $work -Fixture $securityFixture
    Assert-NoRuntimeWriteThrough $everyoneModify 'an Everyone-Modify cache ancestor'

    $junctionTarget = Join-Path $root 'junction-target'
    $junction = Join-Path $root 'cache-junction'
    $junctionFallback = Join-Path $root 'safe-junction-fallback'
    New-Item -ItemType Directory -Force -Path $junctionTarget | Out-Null
    & cmd.exe /d /c "mklink /J `"$junction`" `"$junctionTarget`"" | Out-Host
    if ($LASTEXITCODE -ne 0) { throw "mklink /J failed: exit $LASTEXITCODE" }
    if (-not ((Get-Item -LiteralPath $junction -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'cache-junction is not a reparse point' }
    Invoke-CacheFallbackProbe -Candidate $junction -Fallback $junctionFallback -Work $work -Fixture $securityFixture
    Assert-NoRuntimeWriteThrough $junctionTarget 'a junction/reparse cache root'

    Write-Host 'Windows release compile-cache gate passed: blocked repair while the tree was held, exact self-heal, and unsafe-cache rejection.'
} finally {
    if ($null -ne $holder) {
        try { Stop-ProcessTree $holder } catch { Write-Warning "holder cleanup failed: $_" }
    }
    if (Test-Path -LiteralPath (Join-Path $root 'cache-junction')) {
        & cmd.exe /d /c "rmdir `"$(Join-Path $root 'cache-junction')`"" | Out-Null
    }
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue
    }
}
