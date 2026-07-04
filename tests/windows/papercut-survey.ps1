#Requires -Version 5.1
<#
.SYNOPSIS
    Windows papercut survey for nub  -  runs a structured pass/fail sweep of the
    CLI surface and emits a human-readable console report plus a machine-readable
    JSON results file.

.DESCRIPTION
    Each check captures stdout, stderr, and the exit code of a real nub command
    run against a small fixture.  Results are tagged with a severity candidate
    (blocker / major / minor / cosmetic) so the orchestrator can triage.

    Run this on a COLD Windows 11 ARM64 VM (snapshot-reverted).  See README.md
    for setup and how to interpret the output.

.PARAMETER NubBin
    Path to the nub binary to test.  Defaults to whatever `nub` resolves on PATH.

.PARAMETER WorkDir
    Scratch directory for fixtures.  Created fresh each run.  Defaults to
    $env:TEMP\nub-papercut-<timestamp>.

.PARAMETER OutputJson
    Path for the machine-readable results JSON.  Defaults to
    $WorkDir\results.json.

.PARAMETER Timeout
    Per-check timeout in seconds.  Default 60.
#>
param(
    [string]$NubBin    = "nub",
    [string]$WorkDir   = "",
    [string]$OutputJson = "",
    [int]$Timeout      = 60
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ── helpers ───────────────────────────────────────────────────────────────────

if ($WorkDir -eq "") {
    $ts      = (Get-Date -Format "yyyyMMdd-HHmmss")
    $WorkDir = Join-Path $env:TEMP "nub-papercut-$ts"
}
if ($OutputJson -eq "") {
    $OutputJson = Join-Path $WorkDir "results.json"
}

New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

$results  = [System.Collections.Generic.List[hashtable]]::new()
$checkNum = 0

function Invoke-Check {
    param(
        [string]$id,
        [string]$label,
        [string]$severity,       # blocker / major / minor / cosmetic
        [scriptblock]$Body,
        [string]$note = ""
    )

    $script:checkNum++
    $num = $script:checkNum
    Write-Host "`n[$num] $id  -  $label" -ForegroundColor Cyan

    $result = @{
        id       = $id
        label    = $label
        severity = $severity
        note     = $note
        pass     = $false
        stdout   = ""
        stderr   = ""
        exit_code = -1
        detail   = ""
    }

    try {
        $out = & $Body
        if ($out -is [hashtable]) {
            foreach ($k in $out.Keys) { $result[$k] = $out[$k] }
        }
    } catch {
        $result.detail = "PowerShell exception: $_"
    }

    if ($result.pass) {
        Write-Host "  PASS" -ForegroundColor Green
    } else {
        Write-Host "  FAIL  [$severity]" -ForegroundColor Red
        if ($result.detail) {
            Write-Host "  $($result.detail)" -ForegroundColor Yellow
        }
    }

    if ($result.stdout) {
        Write-Host "  stdout: $($result.stdout.Substring(0, [Math]::Min(200, $result.stdout.Length)))" -ForegroundColor DarkGray
    }
    if ($result.stderr) {
        Write-Host "  stderr: $($result.stderr.Substring(0, [Math]::Min(200, $result.stderr.Length)))" -ForegroundColor DarkGray
    }

    $script:results.Add($result)
}

# PS5.1-compatible argument quoting (no regex with backslash; use Contains).
function ConvertTo-ArgString {
    param([string[]]$argList)
    $parts = foreach ($a in $argList) {
        if ($a.Contains(' ') -or $a.Contains('"') -or $a.Contains('`t')) {
            # wrap in double-quotes; escape inner double-quotes
            '"' + ($a -replace '"', '\"') + '"'
        } else {
            $a
        }
    }
    return $parts -join " "
}

# Run a process with a timeout; return @{stdout stderr exit_code}.
# PS5.1-compatible: uses ReadToEndAsync (not OutputDataReceived events)
# and builds Arguments string (ArgumentList is .NET Core only).
function Invoke-Process {
    param(
        [string]$exe,
        [string[]]$argList = @(),
        [string]$cwd = $WorkDir,
        [int]$timeoutSec = $Timeout,
        [hashtable]$env = @{}
    )

    $psi                        = [System.Diagnostics.ProcessStartInfo]::new($exe)
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError  = $true
    $psi.UseShellExecute        = $false
    $psi.WorkingDirectory       = $cwd
    if ($argList.Count -gt 0) {
        $psi.Arguments = ConvertTo-ArgString $argList
    }
    foreach ($k in $env.Keys) { $psi.Environment[$k] = $env[$k] }

    $proc = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi

    $null = $proc.Start()
    $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
    $stderrTask = $proc.StandardError.ReadToEndAsync()

    $exited = $proc.WaitForExit($timeoutSec * 1000)
    if (-not $exited) {
        try { $proc.Kill() } catch {}
        return @{ stdout=""; stderr="TIMEOUT after ${timeoutSec}s"; exit_code=124 }
    }
    $proc.WaitForExit()

    return @{
        stdout    = $stdoutTask.Result.TrimEnd()
        stderr    = $stderrTask.Result.TrimEnd()
        exit_code = $proc.ExitCode
    }
}

# Write a UTF-8 file without BOM (PowerShell 5 default adds BOM).
function Write-Utf8 {
    param([string]$path, [string]$content)
    [System.IO.File]::WriteAllText($path, $content, [System.Text.UTF8Encoding]::new($false))
}

# ── fixture directories ────────────────────────────────────────────────────────

$fixSimple    = Join-Path $WorkDir "fix-simple"       # plain Node/TS project
$fixPkg       = Join-Path $WorkDir "fix-pkg"          # package.json with scripts
$fixNative    = Join-Path $WorkDir "fix-native"       # native dep (esbuild)
$fixWorkspace = Join-Path $WorkDir "fix-workspace"    # 2-package workspace

foreach ($d in @($fixSimple, $fixPkg, $fixNative, $fixWorkspace)) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}

# Simple JS + TS scripts
Write-Utf8 (Join-Path $fixSimple "hello.js") 'console.log("hello from js");'
Write-Utf8 (Join-Path $fixSimple "hello.ts") 'const msg: string = "hello from ts"; console.log(msg);'

# package.json with scripts that cover plain, .cmd-shim, and POSIX-ism paths
Write-Utf8 (Join-Path $fixPkg "package.json") @'
{
  "name": "fix-pkg",
  "version": "1.0.0",
  "scripts": {
    "greet":    "node -e \"console.log('greet script')\"",
    "env-test": "node -e \"console.log(process.env.NUB_TEST_VAR || 'unset')\"",
    "posix-ism": "NUB_TEST_VAR=hello node -e \"console.log(process.env.NUB_TEST_VAR)\""
  },
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
'@

# Native dep project  -  esbuild has postinstall that downloads a platform binary
Write-Utf8 (Join-Path $fixNative "package.json") @'
{
  "name": "fix-native",
  "version": "1.0.0",
  "devDependencies": {
    "esbuild": "0.21.5"
  }
}
'@

# 2-package workspace
Write-Utf8 (Join-Path $fixWorkspace "package.json") @'
{
  "name": "fix-workspace",
  "version": "1.0.0",
  "workspaces": ["packages/alpha", "packages/beta"],
  "scripts": {
    "build": "echo root-build"
  }
}
'@
$wsAlpha = Join-Path $fixWorkspace "packages\alpha"
$wsBeta  = Join-Path $fixWorkspace "packages\beta"
New-Item -ItemType Directory -Force -Path $wsAlpha | Out-Null
New-Item -ItemType Directory -Force -Path $wsBeta  | Out-Null
Write-Utf8 (Join-Path $wsAlpha "package.json") '{"name":"alpha","version":"1.0.0","scripts":{"build":"node -e \"console.log(''alpha-build'')\""} }'
Write-Utf8 (Join-Path $wsBeta  "package.json") '{"name":"beta","version":"1.0.0","scripts":{"build":"node -e \"console.log(''beta-build'')\""} }'

# ── SECTION 1: install / PATH ─────────────────────────────────────────────────

Invoke-Check -id "install-version" -label "nub --version resolves on PATH" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @("--version")
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match '^\d+\.\d+\.\d+')
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "install-nubx-path" -label "nubx resolves on PATH" -severity "blocker" -Body {
    $r = Invoke-Process "nubx" @("--version")
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match '^\d+\.\d+\.\d+')
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "install-which-nub" -label "nub binary path sanity (Get-Command)" -severity "minor" -Body {
    $cmd = Get-Command $NubBin -ErrorAction SilentlyContinue
    $path = if ($cmd) { $cmd.Source } else { "" }
    @{
        detail = if ($path) { $path } else { "nub not found on PATH" }
        pass   = [bool]$path
    }
}

Invoke-Check -id "install-bin-arch" -label "nub binary arch (ARM64 or x64)" -severity "minor" `
    -note "ARM VM natively runs arm64; x64 runs under emulation  -  note which" -Body {
    $cmd = Get-Command $NubBin -ErrorAction SilentlyContinue
    if (-not $cmd) { return @{ pass=$false; detail="nub not found" } }
    try {
        $mi = [System.Reflection.PE.PEHeaders]::new([System.IO.File]::OpenRead($cmd.Source))
        $arch = $mi.CoffHeader.Machine.ToString()
        return @{ pass=$true; detail="machine type: $arch" }
    } catch {
        # fallback: just note the binary path
        return @{ pass=$true; detail="binary: $($cmd.Source) (arch check requires PE parser)" }
    }
}

# ── SECTION 2: file runner ────────────────────────────────────────────────────

Invoke-Check -id "file-js" -label "nub hello.js (plain JS file runner)" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @((Join-Path $fixSimple "hello.js")) -cwd $fixSimple
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "hello from js")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "file-ts" -label "nub hello.ts (TypeScript just-works)" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @((Join-Path $fixSimple "hello.ts")) -cwd $fixSimple
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "hello from ts")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "file-stdin" -label "nub - (stdin execution)" -severity "major" -Body {
    $psi                        = [System.Diagnostics.ProcessStartInfo]::new($NubBin)
    $psi.Arguments              = "-"
    $psi.RedirectStandardInput  = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError  = $true
    $psi.UseShellExecute        = $false
    $psi.WorkingDirectory       = $WorkDir

    $proc = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    $proc.Start() | Out-Null
    $proc.StandardInput.WriteLine('console.log("stdin ok");')
    $proc.StandardInput.Close()

    $stdout   = $proc.StandardOutput.ReadToEnd()
    $stderr   = $proc.StandardError.ReadToEnd()
    $exited   = $proc.WaitForExit($Timeout * 1000)
    if (-not $exited) { try { $proc.Kill() } catch {} }

    @{
        stdout    = $stdout.TrimEnd()
        stderr    = $stderr.TrimEnd()
        exit_code = if ($exited) { $proc.ExitCode } else { 124 }
        pass      = ($exited -and $proc.ExitCode -eq 0 -and $stdout -match "stdin ok")
        detail    = if (-not $exited) { "TIMEOUT" } elseif ($proc.ExitCode -ne 0) { "exit $($proc.ExitCode): $stderr" } else { "" }
    }
}

# ── SECTION 3: nub run (package.json scripts) ─────────────────────────────────

Invoke-Check -id "run-greet" -label "nub run greet (plain node script)" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @("run", "greet") -cwd $fixPkg
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "greet script")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "run-posix-ism" -label "nub run posix-ism (FOO=val node -e) via default cmd.exe" `
    -severity "major" `
    -note "CMD.EXE cannot interpret 'FOO=val cmd' inline env assignment  -  expect fail under the default cmd shell; pass is if nub degrades gracefully" -Body {
    $r = Invoke-Process $NubBin @("run", "posix-ism") -cwd $fixPkg
    # On Windows with cmd.exe, 'FOO=1 node …' is not valid CMD syntax.
    # PASS if nub either: (a) routes it through sh and it works, or
    # (b) exits non-zero with a clear error (not a crash / no output).
    $crashed = ($r.exit_code -gt 128 -or ($r.exit_code -ne 0 -and $r.stderr -eq "" -and $r.stdout -eq ""))
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        # Not a blocker if cmd.exe fails the POSIX syntax  -  record the observation
        pass      = (-not $crashed)
        detail    = "exit $($r.exit_code); observation: does nub surface a clear error or silently fail?"
    }
}

Invoke-Check -id "run-script-shell" -label "nub run posix-ism --script-shell <sh> (POSIX sh via explicit shell)" `
    -severity "minor" `
    -note "Only passes if a POSIX sh.exe is findable (Git for Windows / WSL); skip otherwise. --script-shell is the explicit escape hatch for running POSIX-ism script bodies on Windows." -Body {
    # Locate a POSIX sh.exe to hand to --script-shell. The harness does the finding
    # (PATH, then the standard Git-for-Windows install dirs) since --script-shell
    # takes an explicit path rather than auto-detecting.
    $sh = $null
    $onPath = Get-Command "sh" -ErrorAction SilentlyContinue
    if ($onPath) {
        $sh = $onPath.Source
    } else {
        foreach ($p in @(
            "C:\Program Files\Git\bin\sh.exe",
            "C:\Program Files\Git\usr\bin\sh.exe",
            "C:\Program Files (x86)\Git\bin\sh.exe"
        )) {
            if (Test-Path $p) { $sh = $p; break }
        }
    }
    if (-not $sh) {
        return @{ pass=$true; detail="SKIP  -  no sh.exe found (no Git for Windows / WSL)" }
    }
    $r = Invoke-Process $NubBin @("run", "--script-shell", $sh, "posix-ism") -cwd $fixPkg
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "hello")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# .cmd bin invocation via nub run  -  install is-odd so node_modules/.bin/is-odd.cmd exists
Invoke-Check -id "run-install-fixture" -label "nub install in fix-pkg (needed for bin checks)" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @("install") -cwd $fixPkg -timeoutSec 120
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "run-cmd-bin" -label "nub exec is-odd (node_modules/.bin .cmd shim invocation)" `
    -severity "major" `
    -note ".cmd shim resolution is a known Windows pain point: nub must invoke via cmd /C" -Body {
    # is-odd ships a bin entry; check that the .cmd shim resolves and exits 0
    $r = Invoke-Process $NubBin @("exec", "is-odd", "--", "3") -cwd $fixPkg
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        # is-odd CLI prints true/false; any exit_code 0 means the .cmd shim resolved
        pass      = ($r.exit_code -eq 0)
        detail    = "exit $($r.exit_code)  -  did .cmd shim resolve through cmd /C?"
    }
}

# ── SECTION 4: nubx DLX ───────────────────────────────────────────────────────

Invoke-Check -id "nubx-cowsay" -label "nubx cowsay@latest hi (DLX fetch-and-run)" -severity "major" -Body {
    $r = Invoke-Process "nubx" @("cowsay@latest", "hi") -cwd $WorkDir -timeoutSec 120
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "hi")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# ── SECTION 5: nub install / add / remove / ci ────────────────────────────────

Invoke-Check -id "pm-native-install" -label "nub install with native dep (esbuild postinstall)" `
    -severity "major" `
    -note "Tests postinstall lifecycle on Windows; esbuild downloads its own .exe" -Body {
    $r = Invoke-Process $NubBin @("install") -cwd $fixNative -timeoutSec 180
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "pm-native-bin" -label "esbuild binary runs after install" -severity "major" -Body {
    $esbuild = Join-Path $fixNative "node_modules\.bin\esbuild.cmd"
    if (-not (Test-Path $esbuild)) {
        $esbuild = Join-Path $fixNative "node_modules\.bin\esbuild"
    }
    $r = Invoke-Process $NubBin @("exec", "esbuild", "--version") -cwd $fixNative
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match '^\d+\.\d+\.\d+')
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# add / remove round-trip on a fresh fixture
$fixAddRemove = Join-Path $WorkDir "fix-add-remove"
New-Item -ItemType Directory -Force -Path $fixAddRemove | Out-Null
Write-Utf8 (Join-Path $fixAddRemove "package.json") '{"name":"fix-add-remove","version":"1.0.0"}'

Invoke-Check -id "pm-add" -label "nub add is-number (adds dep + lockfile)" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @("add", "is-number") -cwd $fixAddRemove -timeoutSec 120
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and (Test-Path (Join-Path $fixAddRemove "node_modules\is-number")))
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "pm-ci" -label "nub ci (frozen install from existing lockfile)" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @("ci") -cwd $fixAddRemove -timeoutSec 120
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "pm-remove" -label "nub remove is-number (removes dep)" -severity "major" -Body {
    $r = Invoke-Process $NubBin @("remove", "is-number") -cwd $fixAddRemove -timeoutSec 120
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and -not (Test-Path (Join-Path $fixAddRemove "node_modules\is-number")))
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# ── SECTION 6: nub node version management ────────────────────────────────────

Invoke-Check -id "node-ls" -label "nub node ls (list cached versions)" -severity "minor" -Body {
    $r = Invoke-Process $NubBin @("node", "ls")
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        # 0 = ok; may print "(empty)" if cache is clean; non-zero is a bug
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "node-install" -label "nub node install 22 (provision from nodejs.org)" `
    -severity "major" -note "Downloads ~30 MB  -  needs network; ARM64 VM gets arm64 build natively" -Body {
    $r = Invoke-Process $NubBin @("node", "install", "22") -timeoutSec 180
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "node-pin" -label "nub node pin 22 (writes .node-version)" -severity "minor" -Body {
    $pinDir = Join-Path $WorkDir "fix-pin"
    New-Item -ItemType Directory -Force -Path $pinDir | Out-Null
    Write-Utf8 (Join-Path $pinDir "package.json") '{"name":"fix-pin","version":"1.0.0"}'

    $r = Invoke-Process $NubBin @("node", "pin", "22") -cwd $pinDir
    $pinFile = Join-Path $pinDir ".node-version"
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and (Test-Path $pinFile) -and (Get-Content $pinFile -Raw).Trim() -match "22")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "wrote: $(if (Test-Path $pinFile) { Get-Content $pinFile -Raw } else { 'missing' })" }
    }
}

Invoke-Check -id "node-uninstall" -label "nub node uninstall 22 (remove from cache)" -severity "minor" -Body {
    # Only meaningful if node-install passed; safe to run either way (may be a no-op)
    $r = Invoke-Process $NubBin @("node", "uninstall", "22")
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# ── SECTION 7: nub upgrade ────────────────────────────────────────────────────

Invoke-Check -id "upgrade-dry-run" -label "nub upgrade --dry-run (observe, no actual upgrade)" `
    -severity "minor" `
    -note "Self-owned channel upgrade is documented as unsupported on Windows; npm channel must work" -Body {
    $r = Invoke-Process $NubBin @("upgrade", "--dry-run")
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = "channel detected: $($r.stdout -replace '`n',' ')"
    }
}

# ── SECTION 8: nub watch ──────────────────────────────────────────────────────

Invoke-Check -id "watch-restart" -label "nub watch hello.js (start + touch + observe restart)" `
    -severity "major" -note "Watches for file change via Node --watch; ARM64 Windows has known NTFS watcher quirks" -Body {

    $watchFile = Join-Path $WorkDir "watch-target.js"
    Write-Utf8 $watchFile 'console.log("run-" + Date.now());'

    $psi                        = [System.Diagnostics.ProcessStartInfo]::new($NubBin)
    $psi.Arguments              = "watch " + (ConvertTo-ArgString @($watchFile))
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError  = $true
    $psi.UseShellExecute        = $false
    $psi.WorkingDirectory       = $WorkDir

    $proc    = [System.Diagnostics.Process]::new()
    $proc.StartInfo = $psi
    $null = $proc.Start()
    $stdoutTask = $proc.StandardOutput.ReadToEndAsync()
    $stderrTask = $proc.StandardError.ReadToEndAsync()

    Start-Sleep -Seconds 3  # let it start

    # Touch the file to trigger a restart
    [System.IO.File]::SetLastWriteTimeUtc($watchFile, [DateTime]::UtcNow)

    Start-Sleep -Seconds 4  # wait for restart

    # Kill the entire process tree (nub spawns Node; Kill() only kills nub, Node inherits handle)
    try { & taskkill /F /T /PID $proc.Id 2>&1 | Out-Null } catch {}
    $proc.WaitForExit(3000) | Out-Null

    # Wait for async reads with a 3s cap to avoid blocking if child kept handle open
    $readDone = [System.Threading.Tasks.Task]::WhenAll($stdoutTask, $stderrTask)
    $null = $readDone.Wait(3000)
    $allOut = ($(if ($stdoutTask.IsCompleted) { $stdoutTask.Result } else { "" }) +
               "`n[err] " +
               $(if ($stderrTask.IsCompleted) { $stderrTask.Result } else { "" })).TrimEnd()
    $runCount = ($allOut | Select-String -Pattern "run-\d+" -AllMatches).Matches.Count

    @{
        stdout    = $allOut.Substring(0, [Math]::Min(400, $allOut.Length))
        exit_code = 0
        pass      = ($runCount -ge 2)  # at least initial run + one restart
        detail    = "saw $runCount 'run-<ts>' lines; need ≥2 for restart confirmation"
    }
}

# ── SECTION 9: workspace -r / -F ──────────────────────────────────────────────

Invoke-Check -id "workspace-install" -label "nub install in workspace root" -severity "blocker" -Body {
    $r = Invoke-Process $NubBin @("install") -cwd $fixWorkspace -timeoutSec 120
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "workspace-run-recursive" -label "nub run -r build (recursive workspace run)" -severity "major" -Body {
    $r = Invoke-Process $NubBin @("run", "-r", "build") -cwd $fixWorkspace
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "alpha-build" -and $r.stdout -match "beta-build")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

Invoke-Check -id "workspace-filter" -label "nub run -F alpha build (filter selector)" -severity "major" -Body {
    $r = Invoke-Process $NubBin @("run", "--filter", "alpha", "build") -cwd $fixWorkspace
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        pass      = ($r.exit_code -eq 0 -and $r.stdout -match "alpha-build" -and $r.stdout -notmatch "beta-build")
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# ── SECTION 10: PATH shim / nub.exe detection ─────────────────────────────────

Invoke-Check -id "shim-detection" -label "nub pm shim --help (shim subcommand exists)" `
    -severity "minor" -note "Tests that the pm shim surface is reachable; does not actually install shims" -Body {
    $r = Invoke-Process $NubBin @("pm", "shim", "--help")
    @{
        stdout    = $r.stdout
        stderr    = $r.stderr
        exit_code = $r.exit_code
        # help exits 0 on clap; check something recognizable in output
        pass      = ($r.exit_code -eq 0)
        detail    = if ($r.exit_code -ne 0) { "exit $($r.exit_code): $($r.stderr)" } else { "" }
    }
}

# ── summary ───────────────────────────────────────────────────────────────────

$total  = $results.Count
$passed = ($results | Where-Object { $_.pass }).Count
$failed = $total - $passed

Write-Host "`n$('=' * 60)" -ForegroundColor White
Write-Host "RESULTS: $passed / $total passed   ($failed failed)" -ForegroundColor $(if ($failed -eq 0) {"Green"} else {"Yellow"})
Write-Host "$('=' * 60)" -ForegroundColor White

$blockers = $results | Where-Object { -not $_.pass -and $_.severity -eq "blocker" }
$majors   = $results | Where-Object { -not $_.pass -and $_.severity -eq "major" }
if ($blockers) {
    Write-Host "BLOCKERS:" -ForegroundColor Red
    foreach ($b in $blockers) { Write-Host "  [$($b.id)] $($b.label)" -ForegroundColor Red }
}
if ($majors) {
    Write-Host "MAJOR:" -ForegroundColor Yellow
    foreach ($m in $majors) { Write-Host "  [$($m.id)] $($m.label)" -ForegroundColor Yellow }
}

# Write JSON results
$json = $results | ForEach-Object {
    [PSCustomObject]@{
        id        = $_.id
        label     = $_.label
        severity  = $_.severity
        pass      = $_.pass
        exit_code = $_.exit_code
        stdout    = $_.stdout
        stderr    = $_.stderr
        detail    = $_.detail
        note      = $_.note
    }
} | ConvertTo-Json -Depth 4

[System.IO.File]::WriteAllText($OutputJson, $json, [System.Text.UTF8Encoding]::new($false))
Write-Host "`nResults written to: $OutputJson" -ForegroundColor Cyan
Write-Host "Work dir: $WorkDir" -ForegroundColor Cyan

exit $(if ($blockers) { 1 } else { 0 })
