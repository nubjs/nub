#Requires -Version 5
# Deterministic filesystem workload for the Windows observability survey.
#
# Every artifact name carries a unique TOKEN so a mechanism's output can be
# scanned for it without ambiguity. Source names (alpha/beta/gamma) and
# destination names (*_RENAMEDEST, *_LINKDEST, …) are distinct precisely so
# "captured the rename" can be told apart from "captured only the rename
# SOURCE" -- the defect this survey exists to test.
#
# ⛔ The two ABANDON shapes at the end are the load-bearing ones. Every other
# destination is opened again afterwards (by the shell, by a later step), so its
# path leaks into the trace through an ordinary Create event and a mechanism can
# look complete while carrying no rename/link record at all. A destination that
# is never touched again can ONLY be seen by a mechanism that records the
# rename/link operation itself.
param(
    [string]$Root = 'C:\obs\work'
)

$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force -Path $Root | Out-Null
New-Item -ItemType Directory -Force -Path "$Root\sub" | Out-Null

function Step($n, $sb) {
    try { & $sb; Write-Host "  ok   $n" }
    catch { Write-Host "  FAIL $n :: $($_.Exception.Message)" }
}

# PowerShell converts a $null String argument to "", which makes the
# MemoryMappedFile overloads reject it ("Map name cannot be an empty string").
# [NullString]::Value is the only way to pass a real null.
$nullStr = [NullString]::Value

Write-Host "workload: begin ($Root)"

# 1. plain create -- the POSITIVE CONTROL. A mechanism that misses this is
#    broken, and none of its other rows mean anything.
Step 'create alpha.txt' { Set-Content -LiteralPath "$Root\alpha.txt" -Value 'hello' -NoNewline }

# 2. same-directory rename. Destination token: RENAMEDEST.
Step 'rename alpha.txt -> beta_RENAMEDEST.txt' {
    Rename-Item -LiteralPath "$Root\alpha.txt" -NewName 'beta_RENAMEDEST.txt'
}

# 3. cross-directory move (still a FileRenameInformation at the IRP layer).
Step 'move beta_RENAMEDEST.txt -> sub\gamma_MOVEDEST.txt' {
    Move-Item -LiteralPath "$Root\beta_RENAMEDEST.txt" -Destination "$Root\sub\gamma_MOVEDEST.txt"
}

# 4. HARDLINK -- a SECOND NAME for existing content.
Step 'hardlink delta_LINKDEST.txt -> sub\gamma_MOVEDEST.txt' {
    New-Item -ItemType HardLink -Path "$Root\delta_LINKDEST.txt" -Target "$Root\sub\gamma_MOVEDEST.txt" -ErrorAction Stop | Out-Null
}

# 5. symlink (admin or Developer Mode).
Step 'symlink zeta_SYMDEST.txt -> sub\gamma_MOVEDEST.txt' {
    New-Item -ItemType SymbolicLink -Path "$Root\zeta_SYMDEST.txt" -Target "$Root\sub\gamma_MOVEDEST.txt" -ErrorAction Stop | Out-Null
}

# 6. memory-mapped write to a file CREATED during the trace.
Step 'mmap-write epsilon_MMAP.bin (created in-trace)' {
    $mm = [System.IO.MemoryMappedFiles.MemoryMappedFile]::CreateFromFile(
        "$Root\epsilon_MMAP.bin", [System.IO.FileMode]::Create, $nullStr, 65536)
    $acc = $mm.CreateViewAccessor()
    for ($i = 0; $i -lt 65536; $i += 4) { $acc.Write($i, [int]0x41414141) }
    $acc.Flush(); $acc.Dispose(); $mm.Dispose()
}

# 7. memory-mapped write to a file that ALREADY EXISTED before tracing began.
#    The pure section-write case: no create, no WriteFile, only dirtied pages.
Step 'mmap-write omega_PREMMAP.bin (pre-existing)' {
    $mm = [System.IO.MemoryMappedFiles.MemoryMappedFile]::CreateFromFile(
        "$Root\omega_PREMMAP.bin", [System.IO.FileMode]::Open, $nullStr, 0,
        [System.IO.MemoryMappedFiles.MemoryMappedFileAccess]::ReadWrite)
    $acc = $mm.CreateViewAccessor()
    for ($i = 0; $i -lt 65536; $i += 4) { $acc.Write($i, [int]0x42424242) }
    $acc.Flush(); $acc.Dispose(); $mm.Dispose()
    # Force the modified pages out while the trace is still running.
    [System.GC]::Collect(); [System.GC]::WaitForPendingFinalizers()
}

# 8. ordinary write through an already-open handle, then truncate + setattr.
Step 'write/truncate/setattr theta_HANDLE.txt' {
    $fs = [System.IO.File]::Open("$Root\theta_HANDLE.txt", 'Create', 'Write')
    $b = [System.Text.Encoding]::ASCII.GetBytes('x' * 4096)
    $fs.Write($b, 0, $b.Length); $fs.Flush(); $fs.SetLength(16); $fs.Dispose()
    Set-ItemProperty -LiteralPath "$Root\theta_HANDLE.txt" -Name Attributes -Value 'ReadOnly'
    Set-ItemProperty -LiteralPath "$Root\theta_HANDLE.txt" -Name Attributes -Value 'Normal'
}

# 9. delete + rmdir.
Step 'delete delta_LINKDEST.txt' { Remove-Item -LiteralPath "$Root\delta_LINKDEST.txt" -Force }
Step 'mkdir/rmdir iota_DIRDEST' {
    New-Item -ItemType Directory -Force -Path "$Root\iota_DIRDEST" | Out-Null
    Remove-Item -LiteralPath "$Root\iota_DIRDEST" -Force
}

# 10. child process doing a rename, so process-TREE attribution is exercised.
Step 'child process renames kappa -> lambda_CHILDDEST.txt' {
    Set-Content -LiteralPath "$Root\kappa.txt" -Value 'child' -NoNewline
    $cmd = "ren `"$Root\kappa.txt`" lambda_CHILDDEST.txt"
    $p = Start-Process -FilePath "$env:SystemRoot\System32\cmd.exe" -ArgumentList '/c', $cmd -Wait -PassThru -NoNewWindow
    Write-Host "  child pid=$($p.Id) exit=$($p.ExitCode)"
}

# 11. 8.3 SHORT NAME access.
Step 'touch via 8.3 short path' {
    $short = (New-Object -ComObject Scripting.FileSystemObject).GetFolder($Root).ShortPath
    Write-Host "  shortpath=$short"
    if ($short) { Set-Content -LiteralPath "$short\mu_SHORTNAME.txt" -Value 's' -NoNewline }
}

# 12. write STORM -- exact-count fidelity under load.
Step 'storm: 500 files' {
    for ($i = 0; $i -lt 500; $i++) {
        Set-Content -LiteralPath ("$Root\sub\storm_{0:d4}.tmp" -f $i) -Value $i -NoNewline
    }
}

# ---------------------------------------------------------------- ABANDON pair
# ⛔ These run LAST and nothing reopens them. A mechanism that names these has
# genuinely recorded the rename / link operation. A mechanism that names only
# the earlier destinations was reading an incidental later Create.
# `cmd /c ren` is a bare MoveFile with no managed pre-existence check, so the
# destination path enters the trace only through the kernel's own accounting.
Step 'ABANDON rename: nu_src -> nu_ABANDONRENAME.txt (never touched again)' {
    Set-Content -LiteralPath "$Root\nu_src.txt" -Value 'r' -NoNewline
    & "$env:SystemRoot\System32\cmd.exe" /c ren "$Root\nu_src.txt" nu_ABANDONRENAME.txt
}
Step 'ABANDON hardlink: xi_ABANDONLINK.txt (never touched again)' {
    Set-Content -LiteralPath "$Root\xi_src.txt" -Value 'l' -NoNewline
    $rc = & "$env:SystemRoot\System32\cmd.exe" /c mklink /H "$Root\xi_ABANDONLINK.txt" "$Root\xi_src.txt"
    Write-Host "  mklink: $rc"
}

Write-Host 'workload: end'
