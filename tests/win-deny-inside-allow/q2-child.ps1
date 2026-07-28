# The CONFINED child for Q2/Q3: runs AS THE DEDICATED LOCAL ACCOUNT and reports, for each path in
# a tab-separated manifest, whether it could read it and -- critically -- WHY it could not.
#
# Distinguishing DENIED from NOTFOUND from DIRNOTFOUND is the whole point: "the read failed" is
# worthless evidence on its own, because it could equally mean the process never launched, the path
# was wrong, or an ancestor lacked traverse. It also prints its own identity, group membership and
# privileges so the parent can PROVE which principal actually did the reading.
#
# ASCII ONLY.
param([string]$Manifest, [string]$Out)

$lines = New-Object System.Collections.Generic.List[string]
try {
    $wi = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    $lines.Add("IDENTITY=" + $wi.Name)
    $lines.Add("SID=" + $wi.User.Value)
    $g = @()
    foreach($grp in $wi.Groups){ try { $g += $grp.Translate([System.Security.Principal.NTAccount]).Value } catch { $g += $grp.Value } }
    $lines.Add("GROUPS=" + ($g -join ' ; '))
    $lines.Add("PRIV_BEGIN")
    foreach($p in (whoami /priv)){ $lines.Add("  " + $p) }
    $lines.Add("PRIV_END")
} catch {
    $lines.Add("IDENTITY_ERROR=" + $_.Exception.Message)
}

foreach($line in (Get-Content -Path $Manifest)) {
    if ([string]::IsNullOrWhiteSpace($line)) { continue }
    $parts = $line -split "`t", 2
    $tag = $parts[0]; $path = $parts[1]
    try {
        $t = [System.IO.File]::ReadAllText($path)
        $lines.Add("READ`t$tag`tOK`tbytes=$($t.Length)")
    }
    catch [System.UnauthorizedAccessException]      { $lines.Add("READ`t$tag`tDENIED`t$($_.Exception.Message)") }
    catch [System.IO.FileNotFoundException]         { $lines.Add("READ`t$tag`tNOTFOUND`t$($_.Exception.Message)") }
    catch [System.IO.DirectoryNotFoundException]    { $lines.Add("READ`t$tag`tDIRNOTFOUND`t$($_.Exception.Message)") }
    catch { $lines.Add("READ`t$tag`tOTHER`t$($_.Exception.GetType().FullName): $($_.Exception.Message)") }
}

$lines.Add("DONE")
Set-Content -Path $Out -Value $lines -Encoding ASCII
