# Q1 -- INDEPENDENT RE-VERIFICATION of sandbox-epic.md 15r:
#   "an explicit deny ACE naming the per-run AppContainer SID is INERT against that
#    AppContainer's own child."
#
# Method, per cell: build a dir with a PROTECTED (inheritance-stripped) DACL so nothing leaks in,
# grant <allowSid> (OI)(CI)(RX) on the dir, seed public.txt + secret.txt, then stamp an explicit
# DENY for <denySid> on secret.txt ONLY. The AppContainer'd child then reads:
#
#   ARM A (positive control, sibling)   public.txt  -- MUST be 0. If not, the grant/traverse never
#                                                     worked and nothing else in the cell means
#                                                     anything.
#   ARM B (the measurement)             secret.txt  -- 0 => deny INERT ; 5 => deny BINDS
#   ARM C (positive control, same file) secret.txt after the deny ACE is REMOVED -- MUST be 0.
#                                                     This is the differential: the ONLY variable
#                                                     between B and C is the deny ACE itself. It is
#                                                     what makes "0 in B" mean "the deny did
#                                                     nothing" rather than "the file was readable
#                                                     for some unrelated reason".
#
# Plus a standalone TRUE-NEGATIVE control (a dir the AC was never granted) proving the harness can
# observe an ACCESS_DENIED at all -- without it, "everything returned 0" could just mean the child
# is not actually confined.
#
# probe-child.exe read <path> exit contract: 0 = read OK, 5 = ACCESS_DENIED, 9 = other error.
# ASCII ONLY.

$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'
function Section($s){ Write-Host "`n========== $s ==========" }
function Note($s){ Write-Host "  $s" }
. "$PSScriptRoot\probe-common.ps1"

$fail = New-Object System.Collections.Generic.List[string]
$verdicts = New-Object System.Collections.Generic.List[string]

$id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = (New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole([System.Security.Principal.WindowsBuiltinRole]::Administrator)
$me = "$($env:USERDOMAIN)\$($env:USERNAME)"
Write-Host "Q1 running as: $($id.Name)  elevated=$isAdmin"
Write-Host "OS: $([System.Environment]::OSVersion.VersionString)"
Write-Host "Build: $((Get-CimInstance Win32_OperatingSystem).Caption) $((Get-CimInstance Win32_OperatingSystem).Version) (UBR $((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR))"

$AAP  = 'S-1-15-2-1'   # ALL APPLICATION PACKAGES
$ARAP = 'S-1-15-2-2'   # ALL RESTRICTED APPLICATION PACKAGES (the LPAC-visible group)

$stage = Join-Path $env:TEMP ("nubq1-" + [guid]::NewGuid().ToString('N').Substring(0,12))
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Write-Host "stage: $stage"

# Host the child exe where BOTH an ordinary AC child and an LPAC child can load it: AAP for the
# ordinary case, ARAP + the per-run AC SID for the LPAC case (LPAC ignores AAP entirely).
$bin = Join-Path $stage 'bin'
$child = Build-ProbeChild $bin
& icacls $stage /grant "*${AAP}:(OI)(CI)(RX)"  | Out-Null
& icacls $stage /grant "*${ARAP}:(OI)(CI)(RX)" | Out-Null
Write-Host "probe child: $child"

$acName = 'NubQ1Probe_' + ([guid]::NewGuid().ToString('N').Substring(0,12))
$acSidPtr = [IntPtr]::Zero
$hr = [AC]::CreateAppContainerProfile($acName,$acName,'nub Q1 deny-ACE probe',[IntPtr]::Zero,0,[ref]$acSidPtr)
if ($hr -ne 0) { throw "CreateAppContainerProfile failed hr=0x$("{0:X8}" -f $hr)" }
$acSidStr = [AC]::SidToString($acSidPtr)
Write-Host "per-run AppContainer SID: $acSidStr"

function Launch-Read($path, $lpac){
    try { return [AC]::LaunchEx($acSidPtr, $null, $lpac, "`"$child`" read `"$path`"", $bin) }
    catch { Note "LAUNCH FAILED (not a deny -- inconclusive): $($_.Exception.Message)"; return 255 }
}

try {
    # ---- prove the child really lands in an AppContainer -------------------------------------
    Section 'Child token check (is the confinement real?)'
    $diag = Join-Path $stage 'diag'; New-Item -ItemType Directory -Path $diag -Force | Out-Null
    & icacls $diag /grant "*${acSidStr}:(OI)(CI)(M)" | Out-Null
    $dump = Join-Path $diag 'token.txt'
    try { [void][AC]::LaunchEx($acSidPtr,$null,$false,"`"$child`" whoami `"$dump`"",$bin) } catch { Note "whoami launch: $($_.Exception.Message)" }
    $inAC = $false
    if (Test-Path $dump) { $dt = Get-Content -Raw $dump; Write-Host $dt; if($dt -match 'TokenIsAppContainer=1'){ $inAC = $true } }
    Note "TokenIsAppContainer=$inAC"
    if(-not $inAC){ $fail.Add('child is not in an AppContainer -- every Q1 cell is invalid') }

    # ---- TRUE-NEGATIVE control: a dir the AC SID was never granted ----------------------------
    Section 'TRUE-NEGATIVE control -- can this harness observe an ACCESS_DENIED at all?'
    $vault = Join-Path $stage 'vault'; New-Item -ItemType Directory -Path $vault -Force | Out-Null
    & icacls $vault /inheritance:r /grant:r "${me}:(OI)(CI)(F)" | Out-Null
    $vsec = Join-Path $vault 'never-granted.txt'; Set-Content $vsec 'NEVER' -NoNewline
    Note "vault ACL:`n$((& icacls $vsec | Out-String).Trim())"
    $tn = Launch-Read $vsec $false
    Note "read never-granted file = $tn  (MUST be 5)"
    if($tn -ne 5){ $fail.Add("TRUE-NEGATIVE control broke: ungranted read gave $tn, expected 5 -- the child is not confined, so no Q1 cell can be trusted") }

    # ---- the deny cells -----------------------------------------------------------------------
    function Test-DenyCell($tag, $allowSid, $allowLabel, $denySid, $denyLabel, $lpac){
        Section "CELL $tag -- allow=$allowLabel deny=$denyLabel lpac=$lpac"
        $r = Join-Path $stage $tag; New-Item -ItemType Directory -Path $r -Force | Out-Null
        & icacls $r /inheritance:r /grant:r "${me}:(OI)(CI)(F)" | Out-Null
        $ga = (& icacls $r /grant "*${allowSid}:(OI)(CI)(RX)" 2>&1 | Out-String); $gaRc = $LASTEXITCODE
        Note "grant rc=$gaRc : $($ga.Trim())"
        $pub = Join-Path $r 'public.txt'; Set-Content $pub 'PUBLIC' -NoNewline
        $sec = Join-Path $r 'secret.txt'; Set-Content $sec 'SECRET' -NoNewline

        $do = (& icacls $sec /deny "*${denySid}:(R)" 2>&1 | Out-String); $doRc = $LASTEXITCODE
        Note "deny  rc=$doRc : $($do.Trim())"
        Note "--- secret.txt icacls WITH deny ---`n$((& icacls $sec | Out-String).Trim())"
        Note "--- secret.txt SDDL   WITH deny (ACE ORDER is left-to-right; D: = DACL) ---"
        Note "$((Get-Acl $sec).Sddl)"

        $a = Launch-Read $pub $lpac      # ARM A  positive control (sibling)
        $b = Launch-Read $sec $lpac      # ARM B  the measurement

        # ARM C: remove ONLY the deny ACE, change nothing else, re-read the SAME file.
        $rm = (& icacls $sec /remove:d "*${denySid}" 2>&1 | Out-String); $rmRc = $LASTEXITCODE
        Note "remove-deny rc=$rmRc : $($rm.Trim())"
        Note "--- secret.txt icacls WITHOUT deny ---`n$((& icacls $sec | Out-String).Trim())"
        $c = Launch-Read $sec $lpac      # ARM C  positive control (same file, deny gone)

        Note "ARM A public(with deny in place)=$a  ARM B secret(deny)=$b  ARM C secret(deny removed)=$c"

        $verdict = ''
        if($a -ne 0){
            $verdict = "INVALID: ARM A positive control failed (public read=$a, expected 0) -- the allow grant or launch never worked"
            $fail.Add("$tag ARM A control failed ($a)")
        } elseif($c -ne 0){
            $verdict = "INVALID: ARM C positive control failed (secret read after deny removal=$c, expected 0) -- the file was unreadable for a reason other than the deny"
            $fail.Add("$tag ARM C control failed ($c)")
        } elseif($b -eq 0){
            $verdict = "DENY INERT -- controls both green (A=0, C=0) and the child read the denied file anyway (B=0)"
        } elseif($b -eq 5){
            $verdict = "DENY BINDS -- controls both green and the denied read was ACCESS_DENIED (B=5)"
        } else {
            $verdict = "INDETERMINATE: B=$b (neither 0 nor 5)"
            $fail.Add("$tag ARM B indeterminate ($b)")
        }
        Write-Host "VERDICT $tag : $verdict"
        $verdicts.Add("$tag (allow=$allowLabel deny=$denyLabel lpac=$lpac) A=$a B=$b C=$c : $verdict")
    }

    Test-DenyCell 'c1_allowAC_denyAC'   $acSidStr 'per-run AC SID' $acSidStr 'per-run AC SID' $false
    Test-DenyCell 'c2_allowAAP_denyAC'  $AAP      'AAP'            $acSidStr 'per-run AC SID' $false
    Test-DenyCell 'c3_allowAAP_denyAAP' $AAP      'AAP'            $AAP      'AAP'            $false
    Test-DenyCell 'c4_allowAC_denyAAP'  $acSidStr 'per-run AC SID' $AAP      'AAP'            $false

    # LPAC cells. An LPAC child ignores AAP grants entirely, so the dir must grant the AC SID (or
    # ARAP) for ARM A to pass. If ARM A fails here it is an LPAC launch/loader problem, and the cell
    # is reported INVALID rather than being read as a deny.
    Test-DenyCell 'c5_LPAC_allowAC_denyAC'   $acSidStr 'per-run AC SID' $acSidStr 'per-run AC SID' $true
    Test-DenyCell 'c6_LPAC_allowARAP_denyAC' $ARAP     'ARAP'           $acSidStr 'per-run AC SID' $true
}
finally {
    [void][AC]::DeleteAppContainerProfile($acName)
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}

Section 'Q1 SUMMARY'
foreach($v in $verdicts){ Write-Host "  $v" }
Write-Host ""
if($fail.Count -gt 0){
    Write-Host "Q1 HARNESS: $($fail.Count) control/validity problem(s):"
    foreach($f in $fail){ Write-Host "   - $f" }
    exit 1
}
Write-Host "Q1 HARNESS OK -- every cell's positive controls passed, so every ARM B result is meaningful."
exit 0
