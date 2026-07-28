# Q2 -- can the PRIVILEGED Windows route (a DEDICATED LOCAL ACCOUNT, not an AppContainer) express
#        DENY-INSIDE-ALLOW?  And Q3 -- does that deny survive files created AFTER setup?
#
# A separate local principal is an ordinary NT security principal, so the ordinary NT access check
# applies: deny ACEs sort ahead of allow ACEs and a matching deny halts the check. Q1 shows the
# AppContainer principal does NOT behave that way. This measures the account principal directly.
#
# LAYOUT (all under one stage dir on C:\, so the confined account is never asked to traverse a
# protected user profile):
#
#   stage\                      PROTECTED DACL; Administrators+SYSTEM F; <acct> (OI)(CI)(RX)
#     bin\      q2-child.ps1    inherits RX          -- the confined child
#     out\                      <acct> (M)           -- child writes results here
#     granted\                  inherits RX          -- the "allow" region
#        allowed.txt            inherited allow, no deny        ARM A  positive control
#        .env                   inherited allow + explicit DENY ARM B  measurement (inherited-allow case)
#        .env-exp               EXPLICIT allow + explicit DENY  ARM B' measurement (explicit-allow case)
#     ungranted\                PROTECTED, admins only          TRUE-NEGATIVE control
#        secret.txt
#     q3\                       inherits RX
#        keep.txt               control, expect OK
#        .env.local             present at setup; matched by a WILDCARD deny -> expect DENIED
#        .env.late              CREATED AFTER SETUP by an external admin -> the Q3 question
#
# PHASE 1 reads everything with the denies in place.
# PHASE 2 removes ONLY the deny ACEs from .env / .env-exp and re-reads the SAME files (ARM C), and
#         reads .env.late. ARM C is the differential that makes a PHASE-1 result mean something.
#
# ASCII ONLY. The account password is generated at runtime and never printed.

$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'
function Section($s){ Write-Host "`n========== $s ==========" }
function Note($s){ Write-Host "  $s" }

$fail = New-Object System.Collections.Generic.List[string]
$verdicts = New-Object System.Collections.Generic.List[string]

$id = [System.Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = (New-Object System.Security.Principal.WindowsPrincipal($id)).IsInRole([System.Security.Principal.WindowsBuiltinRole]::Administrator)
Write-Host "Q2 running as: $($id.Name)  elevated=$isAdmin"
Write-Host "OS: $([System.Environment]::OSVersion.VersionString)"
Write-Host "Build: $((Get-CimInstance Win32_OperatingSystem).Caption) $((Get-CimInstance Win32_OperatingSystem).Version) (UBR $((Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').UBR))"
if(-not $isAdmin){ throw "Q2 requires admin (the dedicated-account route is the explicitly privileged one)" }

$suffix   = [guid]::NewGuid().ToString('N').Substring(0,8)
$acct     = "nubq2$suffix"
$acctFull = "$env:COMPUTERNAME\$acct"
$pw       = "Nq2!" + ([guid]::NewGuid().ToString('N').Substring(0,18)) + "aZ9#"
$secure   = ConvertTo-SecureString $pw -AsPlainText -Force
$stage    = "C:\nubq2-$suffix"
$taskName = "nubq2probe_$suffix"

function Show-Acl($path){ Note "--- icacls $path ---`n$((& icacls $path 2>&1 | Out-String).Trim())"; Note "--- SDDL ---  $((Get-Acl $path).Sddl)" }

try {
    Section 'Create the dedicated local account'
    New-LocalUser -Name $acct -Password $secure -AccountNeverExpires -PasswordNeverExpires -UserMayNotChangePassword -Description 'nub Q2 deny-inside-allow probe' | Out-Null
    Add-LocalGroupMember -Group 'Users' -Member $acct
    $lu = Get-LocalUser -Name $acct
    Note "created $acctFull  SID=$($lu.SID.Value)"
    $acctSid = $lu.SID.Value
    Note "group membership: $(((Get-LocalGroupMember -Group 'Users' | Where-Object { $_.Name -like "*\$acct" }) | ForEach-Object { $_.Name }) -join ',')"

    Section 'Build the stage tree and its ACLs'
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    & icacls $stage /inheritance:r /grant:r "Administrators:(OI)(CI)(F)" "SYSTEM:(OI)(CI)(F)" | Out-Null
    & icacls $stage /grant "${acctFull}:(OI)(CI)(RX)" | Out-Null
    Show-Acl $stage

    Note "--- ancestor traverse evidence: the account must be able to walk C:\ down to the stage ---"
    Show-Acl 'C:\'

    $bin = Join-Path $stage 'bin'; New-Item -ItemType Directory -Path $bin -Force | Out-Null
    Copy-Item (Join-Path $PSScriptRoot 'q2-child.ps1') $bin
    $childPs1 = Join-Path $bin 'q2-child.ps1'

    $out = Join-Path $stage 'out'; New-Item -ItemType Directory -Path $out -Force | Out-Null
    & icacls $out /grant "${acctFull}:(OI)(CI)(M)" | Out-Null

    $granted = Join-Path $stage 'granted'; New-Item -ItemType Directory -Path $granted -Force | Out-Null
    $fAllowed = Join-Path $granted 'allowed.txt'; Set-Content $fAllowed 'PUBLIC-SIBLING' -NoNewline
    $fEnv     = Join-Path $granted '.env';        Set-Content $fEnv 'SECRET_TOKEN=sk-inherited' -NoNewline
    $fEnvExp  = Join-Path $granted '.env-exp';    Set-Content $fEnvExp 'SECRET_TOKEN=sk-explicit' -NoNewline

    $ungranted = Join-Path $stage 'ungranted'; New-Item -ItemType Directory -Path $ungranted -Force | Out-Null
    & icacls $ungranted /inheritance:r /grant:r "Administrators:(OI)(CI)(F)" "SYSTEM:(OI)(CI)(F)" | Out-Null
    $fNever = Join-Path $ungranted 'secret.txt'; Set-Content $fNever 'NEVER-GRANTED' -NoNewline

    $q3 = Join-Path $stage 'q3'; New-Item -ItemType Directory -Path $q3 -Force | Out-Null
    $fKeep     = Join-Path $q3 'keep.txt';   Set-Content $fKeep 'KEEP' -NoNewline
    $fEnvLocal = Join-Path $q3 '.env.local'; Set-Content $fEnvLocal 'SECRET_TOKEN=sk-existing' -NoNewline
    $fEnvLate  = Join-Path $q3 '.env.late'   # deliberately NOT created yet

    Section 'Stamp the DENY ACEs'
    # .env      : allow is INHERITED from stage, deny is EXPLICIT on the file.
    # .env-exp  : an EXPLICIT allow is placed alongside the explicit deny, so the deny competes with
    #             an explicit allow for the same principal on the same object -- i.e. the test is the
    #             canonical ACE ORDERING rule, not merely explicit-beats-inherited.
    #             ORDER MATTERS HERE: `icacls /deny` documents that it removes the same permissions
    #             from any explicit grant, so granting first and denying second leaves NO explicit
    #             allow behind (run 1 did exactly that and the cell silently degenerated into a copy
    #             of the .env cell). Deny first, then grant.
    $d1 = (& icacls $fEnv    /deny  "${acctFull}:(R)" 2>&1 | Out-String); Note "deny  .env     rc=$LASTEXITCODE : $($d1.Trim())"
    $d2 = (& icacls $fEnvExp /deny  "${acctFull}:(R)" 2>&1 | Out-String); Note "deny  .env-exp rc=$LASTEXITCODE : $($d2.Trim())"
    $d2b= (& icacls $fEnvExp /grant "${acctFull}:(R)" 2>&1 | Out-String); Note "grant .env-exp rc=$LASTEXITCODE : $($d2b.Trim())"
    $expAcl = (& icacls $fEnvExp 2>&1 | Out-String)
    $hasBoth = ($expAcl -match '\(DENY\)') -and ($expAcl -match "(?m)^\s*\S*${acct}:\(R\)")
    Note ".env-exp carries BOTH an explicit deny and an explicit allow: $hasBoth"
    if(-not $hasBoth){ Note "WARNING: the explicit-allow-vs-explicit-deny cell did not materialize; treat it as a duplicate of the inherited-allow cell" }

    # Q3: the FLOOR shape. NT DACLs are per-object, so a ".env*" floor can only be applied by
    # ENUMERATING matches at setup time. icacls' wildcard is command-time glob expansion, not a
    # stored pattern -- this call proves what it does and does not cover.
    Section 'Q3 -- apply the ".env*" floor as a WILDCARD deny and record exactly what it matched'
    Note "files present in q3 at floor-application time: $(((Get-ChildItem -Force $q3).Name) -join ', ')"
    $d3 = (& icacls (Join-Path $q3 '.env*') /deny "${acctFull}:(R)" 2>&1 | Out-String)
    Note "wildcard deny rc=$LASTEXITCODE :`n$($d3.Trim())"

    foreach($f in @($fAllowed,$fEnv,$fEnvExp,$fNever,$fKeep,$fEnvLocal)){ Show-Acl $f }

    # ---- runner ------------------------------------------------------------------------------
    function Invoke-AsAccount($phase, $manifestLines){
        $manifest = Join-Path $stage "manifest-$phase.txt"
        Set-Content -Path $manifest -Value $manifestLines -Encoding ASCII
        & icacls $manifest /grant "${acctFull}:(R)" | Out-Null
        $outFile = Join-Path $out "result-$phase.txt"
        Remove-Item $outFile -Force -ErrorAction SilentlyContinue

        $arg = "-NoProfile -ExecutionPolicy Bypass -File `"$childPs1`" `"$manifest`" `"$outFile`""
        $mech = ''
        try {
            $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $arg -WorkingDirectory 'C:\'
            Register-ScheduledTask -TaskName $taskName -Action $action -User $acctFull -Password $pw -RunLevel Limited -Force | Out-Null
            Start-ScheduledTask -TaskName $taskName
            for($i=0;$i -lt 120;$i++){ if((Test-Path $outFile) -and ((Get-Content -Raw $outFile) -match 'DONE')){ break }; Start-Sleep -Milliseconds 500 }
            $info = Get-ScheduledTaskInfo -TaskName $taskName
            Note "scheduled-task LastTaskResult=0x$('{0:X}' -f $info.LastTaskResult)"
            $mech = 'scheduled-task'
        } catch { Note "scheduled-task route failed: $($_.Exception.Message)" }

        if(-not ((Test-Path $outFile) -and ((Get-Content -Raw $outFile) -match 'DONE'))){
            Note "falling back to CreateProcessWithLogonW (Start-Process -Credential)"
            try {
                $cred = New-Object System.Management.Automation.PSCredential($acctFull,$secure)
                Start-Process -FilePath 'powershell.exe' -ArgumentList $arg -Credential $cred -WorkingDirectory 'C:\' -Wait
                $mech = 'CreateProcessWithLogonW'
            } catch { Note "Start-Process -Credential failed: $($_.Exception.Message)" }
        }

        if(-not (Test-Path $outFile)){ throw "PHASE $phase produced no output -- the confined process never ran; results are INCONCLUSIVE, not denials" }
        $text = Get-Content -Raw $outFile
        Write-Host "--- PHASE $phase raw child output (mechanism: $mech) ---"
        Write-Host $text
        if($text -notmatch 'DONE'){ throw "PHASE $phase output truncated -- child did not finish" }

        $res = @{}
        foreach($l in ($text -split "`r?`n")){
            if($l.StartsWith("READ`t")){ $p = $l -split "`t"; $res[$p[1]] = $p[2] }
            if($l -match '^IDENTITY='){ $res['__identity'] = $l.Substring(9) }
        }
        return $res
    }

    # ---- PHASE 1: denies in place ------------------------------------------------------------
    Section 'PHASE 1 -- read with the deny ACEs IN PLACE'
    $m1 = @(
        "A_sibling`t$fAllowed",
        "B_env_inherited_allow`t$fEnv",
        "B2_env_explicit_allow`t$fEnvExp",
        "TN_never_granted`t$fNever",
        "Q3_keep`t$fKeep",
        "Q3_env_local_wildcard_denied`t$fEnvLocal",
        "Q3_env_late_not_yet_created`t$fEnvLate"
    )
    $p1 = Invoke-AsAccount 'p1' $m1

    Note "child identity: $($p1['__identity'])  (MUST be $acctFull)"
    if($p1['__identity'] -ne $acctFull){ $fail.Add("child ran as '$($p1['__identity'])', not '$acctFull' -- every Q2 result is invalid") }
    if($p1['A_sibling'] -ne 'OK'){ $fail.Add("ARM A positive control failed: sibling read = $($p1['A_sibling'])") }
    if($p1['TN_never_granted'] -ne 'DENIED'){ $fail.Add("TRUE-NEGATIVE control failed: never-granted read = $($p1['TN_never_granted']) (expected DENIED) -- the account is not actually confined") }
    if($p1['Q3_keep'] -ne 'OK'){ $fail.Add("q3 control failed: keep.txt = $($p1['Q3_keep'])") }

    # ---- PHASE 2: remove ONLY the deny ACEs; create the late file -----------------------------
    Section 'PHASE 2 -- remove ONLY the deny ACEs (ARM C) and create the post-setup file (Q3)'
    & icacls $fEnv    /remove:d "$acctFull" | Out-Null; Note "removed deny from .env      rc=$LASTEXITCODE"
    & icacls $fEnvExp /remove:d "$acctFull" | Out-Null; Note "removed deny from .env-exp  rc=$LASTEXITCODE"
    Show-Acl $fEnv
    Show-Acl $fEnvExp

    # Created by the ADMIN, after the floor was applied, so its ACL is purely inherited -- no
    # creator-owner confound. This is a file that simply did not exist when the deny was stamped.
    Set-Content $fEnvLate 'SECRET_TOKEN=sk-created-after-setup' -NoNewline
    Note "created AFTER the floor was applied: $fEnvLate"
    Show-Acl $fEnvLate

    $m2 = @(
        "A_sibling`t$fAllowed",
        "C_env_deny_removed`t$fEnv",
        "C2_envexp_deny_removed`t$fEnvExp",
        "TN_never_granted`t$fNever",
        "Q3_env_local_wildcard_denied`t$fEnvLocal",
        "Q3_env_late_created_after_setup`t$fEnvLate"
    )
    $p2 = Invoke-AsAccount 'p2' $m2

    if($p2['A_sibling'] -ne 'OK'){ $fail.Add("PHASE 2 ARM A control failed: $($p2['A_sibling'])") }
    if($p2['C_env_deny_removed'] -ne 'OK'){ $fail.Add("ARM C positive control failed: .env after deny removal = $($p2['C_env_deny_removed']) (expected OK) -- the file was unreadable for a reason other than the deny") }
    if($p2['C2_envexp_deny_removed'] -ne 'OK'){ $fail.Add("ARM C' positive control failed: .env-exp after deny removal = $($p2['C2_envexp_deny_removed'])") }

    # ---- verdicts -----------------------------------------------------------------------------
    Section 'Q2 / Q3 VERDICTS'
    function Verdict($tag,$armB,$armC,$label){
        $v = ''
        if($armC -ne 'OK'){ $v = "INVALID: ARM C control = $armC" }
        elseif($armB -eq 'DENIED'){ $v = "DENY BINDS -- read with deny = DENIED, and the SAME file read OK once only the deny ACE was removed" }
        elseif($armB -eq 'OK'){ $v = "DENY INERT -- read with deny = OK" }
        else { $v = "INDETERMINATE: ARM B = $armB" }
        Write-Host "VERDICT $tag ($label): B=$armB C=$armC : $v"
        $verdicts.Add("$tag ($label) B=$armB C=$armC : $v")
    }
    Verdict 'Q2-inherited-allow' $p1['B_env_inherited_allow'] $p2['C_env_deny_removed']    'explicit deny vs INHERITED allow'
    Verdict 'Q2-explicit-allow'  $p1['B2_env_explicit_allow'] $p2['C2_envexp_deny_removed'] 'explicit deny vs EXPLICIT allow on the same file'

    $q3Existing = $p1['Q3_env_local_wildcard_denied']
    $q3LateP1   = $p1['Q3_env_late_not_yet_created']
    $q3LateP2   = $p2['Q3_env_late_created_after_setup']
    Write-Host "VERDICT Q3-wildcard-floor: .env.local (existed at floor time) = $q3Existing ; .env.late (created after) = $q3LateP2 (phase1, before creation: $q3LateP1)"
    $verdicts.Add("Q3 wildcard floor: existing-at-setup .env.local=$q3Existing ; created-after-setup .env.late=$q3LateP2 (p1 pre-creation=$q3LateP1)")
    if($q3Existing -eq 'DENIED' -and $q3LateP2 -eq 'OK'){
        Write-Host "  => The '.env*' floor is a SETUP-TIME ENUMERATION, not a stored pattern: a matching file created after setup inherits the ALLOW and is readable."
        $verdicts.Add("Q3 => floor is setup-time enumeration; post-setup .env files ESCAPE it (the Windows analogue of Landlock's fixed ruleset)")
    }
}
finally {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
    Remove-LocalUser -Name $acct -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}

Section 'Q2 SUMMARY'
foreach($v in $verdicts){ Write-Host "  $v" }
Write-Host ""
if($fail.Count -gt 0){
    Write-Host "Q2 HARNESS: $($fail.Count) control/validity problem(s):"
    foreach($f in $fail){ Write-Host "   - $f" }
    exit 1
}
Write-Host "Q2 HARNESS OK -- identity confirmed, positive and true-negative controls all passed."
exit 0
