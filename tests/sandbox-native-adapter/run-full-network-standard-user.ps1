param(
    [Parameter(Mandatory=$true)][string]$FixtureBinary,
    [Parameter(Mandatory=$true)][string]$LibraryBinary,
    [Parameter(Mandatory=$true)][string]$TmpBinary,
    [Parameter(Mandatory=$true)][string]$NetworkBinary,
    [Parameter(Mandatory=$true)][string]$RelayBinary,
    [Parameter(Mandatory=$true)][string]$BinaryManifest,
    [string]$FileBrokerFixtureBinary,
    [string]$FileOperationShapesBinary,
    [string]$FileOperationShapesManifest,
    [ValidateSet('full', 'owner-pipe-diagnostic', 'native-repair-diagnostic')][string]$Mode = 'full',
    [Parameter(Mandatory=$true)][string]$ReportDirectory
)

# Runs only staged, hash-recorded artifacts under a new ordinary local account.
# It intentionally does not alter firewall, DNS, or machine-wide network policy.
$ErrorActionPreference = 'Stop'
foreach ($path in @($FixtureBinary, $LibraryBinary, $TmpBinary, $NetworkBinary, $RelayBinary)) {
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required artifact is missing: $path" }
}
if (!(Test-Path -LiteralPath $BinaryManifest -PathType Leaf)) { throw "Binary manifest is missing: $BinaryManifest" }
if ($FileBrokerFixtureBinary -and !(Test-Path -LiteralPath $FileBrokerFixtureBinary -PathType Leaf)) { throw "File-broker fixture is missing: $FileBrokerFixtureBinary" }
if ([bool]$FileOperationShapesBinary -ne [bool]$FileOperationShapesManifest) { throw 'File-operation-shapes binary and manifest must be supplied together' }
foreach ($path in @($FileOperationShapesBinary, $FileOperationShapesManifest)) {
    if ($path -and !(Test-Path -LiteralPath $path -PathType Leaf)) { throw "File-operation-shapes artifact is missing: $path" }
}
New-Item -ItemType Directory -Force $ReportDirectory | Out-Null
$ReportDirectory = (Resolve-Path $ReportDirectory).Path
$name = 'sbx' + [guid]::NewGuid().ToString('N').Substring(0, 10)
$stage = Join-Path $env:PUBLIC $name
$password = ConvertTo-SecureString ([guid]::NewGuid().ToString('N') + '!aA1') -AsPlainText -Force
$user = $null; $process = $null; $exitCode = 1

try {
    $user = New-LocalUser -Name $name -Password $password -AccountNeverExpires -PasswordNeverExpires
    Add-LocalGroupMember -Group (Get-LocalGroup -SID 'S-1-5-32-545') -Member $name
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item $FixtureBinary (Join-Path $stage 'native-full-network.exe')
    Copy-Item $LibraryBinary (Join-Path $stage 'nub_sandbox_lib.exe')
    Copy-Item $TmpBinary (Join-Path $stage 'windows_tmp_policy.exe')
    Copy-Item $NetworkBinary (Join-Path $stage 'windows_native_full_network.exe')
    Copy-Item $RelayBinary (Join-Path $stage 'windows_relay_fixture.exe')
    if ($FileBrokerFixtureBinary) { Copy-Item $FileBrokerFixtureBinary (Join-Path $stage 'file-broker-fixture.dll') }
    if ($FileOperationShapesBinary) {
        Copy-Item $FileOperationShapesBinary (Join-Path $stage 'file-operation-shapes.exe')
        Copy-Item $FileOperationShapesManifest (Join-Path $stage 'file-operation-shapes-sha256.json')
    }
    Copy-Item $BinaryManifest (Join-Path $stage 'binary-sha256.json')
@'
param([string]$Stage, [ValidateSet('full', 'owner-pipe-diagnostic', 'native-repair-diagnostic')][string]$Mode)
$ErrorActionPreference = 'Stop'
$profileRoot = [Environment]::GetFolderPath('UserProfile')
if (!$profileRoot -or $profileRoot -like '*systemprofile*') { throw 'A normal user profile is required' }
$env:USERPROFILE = $profileRoot; $env:HOME = $profileRoot
$env:APPDATA = Join-Path $profileRoot 'AppData\Roaming'; $env:LOCALAPPDATA = Join-Path $profileRoot 'AppData\Local'
$env:TEMP = Join-Path $env:LOCALAPPDATA 'Temp'; $env:TMP = $env:TEMP
New-Item -ItemType Directory -Force $env:TEMP | Out-Null
$owned = Join-Path $profileRoot 'sandbox-native-full-network-gate'
New-Item -ItemType Directory -Force $owned | Out-Null
foreach ($file in @('native-full-network.exe', 'nub_sandbox_lib.exe', 'windows_tmp_policy.exe', 'windows_native_full_network.exe', 'windows_relay_fixture.exe')) {
    Copy-Item (Join-Path $Stage $file) (Join-Path $owned $file) -Force
}
$fileBrokerFixture = Join-Path $Stage 'file-broker-fixture.dll'
if (Test-Path -LiteralPath $fileBrokerFixture -PathType Leaf) {
    Copy-Item $fileBrokerFixture (Join-Path $owned 'file-broker-fixture.dll') -Force
    $fileBrokerFixture = Join-Path $owned 'file-broker-fixture.dll'
} else {
    $fileBrokerFixture = $null
}
$operationShapes = Join-Path $Stage 'file-operation-shapes.exe'
$operationShapesManifest = Join-Path $Stage 'file-operation-shapes-sha256.json'
if (Test-Path -LiteralPath $operationShapes -PathType Leaf) {
    if (!(Test-Path -LiteralPath $operationShapesManifest -PathType Leaf)) { throw 'File-operation-shapes manifest is missing from staged artifacts' }
    Copy-Item $operationShapes (Join-Path $owned 'file-operation-shapes.exe') -Force
    $operationShapes = Join-Path $owned 'file-operation-shapes.exe'
} else {
    if (Test-Path -LiteralPath $operationShapesManifest -PathType Leaf) { throw 'File-operation-shapes manifest was staged without its binary' }
    $operationShapes = $null
}
function Sha256([string]$Path) {
    $sha = [Security.Cryptography.SHA256]::Create(); $stream = [IO.File]::OpenRead($Path)
    try { return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() }
    finally { $stream.Dispose(); $sha.Dispose() }
}
$expected = @{}
@((Get-Content (Join-Path $Stage 'binary-sha256.json') -Raw | ConvertFrom-Json)) | ForEach-Object { $expected[[IO.Path]::GetFileName($_.Path)] = $_.Hash.ToLowerInvariant() }
foreach ($file in @('native-full-network.exe', 'nub_sandbox_lib.exe', 'windows_tmp_policy.exe', 'windows_native_full_network.exe', 'windows_relay_fixture.exe')) {
    $actual = Sha256 (Join-Path $owned $file)
    if (!$expected.ContainsKey($file) -or $actual -ne $expected[$file]) { throw "Staged binary hash mismatch: $file" }
    Write-Host "STANDARD_USER_FULL_NETWORK_SHA256=${file}:$actual"
}
if ($fileBrokerFixture) {
    $actual = Sha256 $fileBrokerFixture
    if (!$expected.ContainsKey('file-broker-fixture.dll') -or $actual -ne $expected['file-broker-fixture.dll']) { throw "Staged binary hash mismatch: file-broker-fixture.dll" }
    Write-Host "STANDARD_USER_FULL_NETWORK_SHA256=file-broker-fixture.dll:$actual"
}
if ($operationShapes) {
    $operationExpected = @{}
    @((Get-Content $operationShapesManifest -Raw | ConvertFrom-Json)) | ForEach-Object { $operationExpected[[IO.Path]::GetFileName($_.Path)] = $_.Hash.ToLowerInvariant() }
    $actual = Sha256 $operationShapes
    if (!$operationExpected.ContainsKey('file-operation-shapes.exe') -or $actual -ne $operationExpected['file-operation-shapes.exe']) { throw 'Staged file-operation-shapes hash mismatch' }
    Write-Host "STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES_SHA256=$actual"
} else {
    Write-Host 'STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES=absent;no-auxiliary-claim'
}
Set-Location $owned
whoami /all
$env:NUB_WINDOWS_NATIVE_FULL_NETWORK_FIXTURE = Join-Path $owned 'native-full-network.exe'
$env:NUB_WINDOWS_RELAY_FIXTURE = Join-Path $owned 'windows_relay_fixture.exe'
if ($fileBrokerFixture) { $env:NUB_FILE_BROKER_TEST_DLL = $fileBrokerFixture }
$env:NUB_JAIL_DUMP_POLICY = '1'
Write-Host "STANDARD_USER_FULL_NETWORK_PROFILE=$profileRoot"
Write-Host "STANDARD_USER_FULL_NETWORK_FIXTURE=$env:NUB_WINDOWS_NATIVE_FULL_NETWORK_FIXTURE"
Write-Host "STANDARD_USER_FULL_NETWORK_OWNED=$owned"
Write-Host "STANDARD_USER_FULL_NETWORK_MODE=$Mode"
$script:failed = $false
function Test-Arguments([string]$filter, [bool]$exact, [bool]$ignored) {
    [string[]]$arguments = @()
    if ($ignored) { $arguments += '--ignored' }
    if ($exact) { $arguments += '--exact' }
    if ($filter) { $arguments += $filter }
    $arguments += '--nocapture'
    $arguments += '--test-threads=1'
    return $arguments
}
function Run-Filtered([string]$file, [string]$label, [string[]]$arguments, [string]$summary, [bool]$nativeAdapter = $false, [bool]$dnsOptIn = $false, [string[]]$requiredMarkers = @()) {
    $out = Join-Path $owned "$label.stdout.log"; $err = Join-Path $owned "$label.stderr.log"
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = Join-Path $owned $file; $info.WorkingDirectory = $owned; $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    # Windows PowerShell 5 lacks ProcessStartInfo.ArgumentList; these fixed harness arguments are safe.
    $info.Arguments = [string]::Join(' ', $arguments)
    if ($nativeAdapter) { $info.Environment['NUB_NATIVE_EMBEDDED_ADAPTER'] = '1' }
    if ($dnsOptIn) { $info.Environment['NUB_WINDOWS_NATIVE_FULL_NETWORK_DNS_OPT_IN'] = '1' }
    $proc = New-Object Diagnostics.Process; $proc.StartInfo = $info
    try {
        if (!$proc.Start()) { throw "Could not start ${label}" }
        $outTask = $proc.StandardOutput.ReadToEndAsync(); $errTask = $proc.StandardError.ReadToEndAsync()
        $timedOut = !$proc.WaitForExit(300000)
        if ($timedOut) {
            taskkill /PID $proc.Id /T /F | Out-Null
            if (!$proc.WaitForExit(30000)) { throw "${label} did not exit after timeout termination" }
        }
        if (![Threading.Tasks.Task]::WaitAll(@($outTask, $errTask), 30000)) { throw "${label} output drain timed out" }
        [IO.File]::WriteAllText($out, $outTask.Result); [IO.File]::WriteAllText($err, $errTask.Result)
        $text = $outTask.Result + $errTask.Result
        Write-Host "STANDARD_USER_FULL_NETWORK_FILTER_EXIT=${label}:$($proc.ExitCode)"
        Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDOUT_BEGIN"; Write-Host $outTask.Result; Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDOUT_END"
        Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDERR_BEGIN"; Write-Host $errTask.Result; Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDERR_END"
        $missingMarkers = @($requiredMarkers | Where-Object { !$text.Contains($_) })
        if ($timedOut -or $proc.ExitCode -ne 0 -or !$text.Contains($summary) -or $text -notmatch 'running [1-9][0-9]* test(s)?' -or $missingMarkers.Count -ne 0) { $script:failed = $true; Write-Host "STANDARD_USER_FULL_NETWORK_FILTER_FAILURE=${label}:timeout=$timedOut;required-summary-markers-or-exit;missing=$($missingMarkers -join ',')" }
    } catch {
        $script:failed = $true
        [IO.File]::WriteAllText($err, $_.Exception.ToString())
        Write-Host "STANDARD_USER_FULL_NETWORK_FILTER_FAILURE=${label}:$($_.Exception.Message)"
    } finally { if ($proc) { $proc.Dispose() } }
}
function Run-OperationShapes([string]$file) {
    $label = 'file-operation-shapes'; $out = Join-Path $owned "$label.stdout.log"; $err = Join-Path $owned "$label.stderr.log"
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $file; $info.WorkingDirectory = $owned; $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    $proc = New-Object Diagnostics.Process; $proc.StartInfo = $info
    try {
        if (!$proc.Start()) { throw 'Could not start file-operation-shapes diagnostic' }
        $outTask = $proc.StandardOutput.ReadToEndAsync(); $errTask = $proc.StandardError.ReadToEndAsync()
        $timedOut = !$proc.WaitForExit(120000)
        if ($timedOut) {
            taskkill /PID $proc.Id /T /F | Out-Null
            if (!$proc.WaitForExit(30000)) { throw 'file-operation-shapes did not exit after timeout termination' }
        }
        if (![Threading.Tasks.Task]::WaitAll(@($outTask, $errTask), 30000)) { throw 'file-operation-shapes output drain timed out' }
        [IO.File]::WriteAllText($out, $outTask.Result); [IO.File]::WriteAllText($err, $errTask.Result)
        $text = $outTask.Result + $errTask.Result
        $resultCount = ([regex]::Matches($text, '(?m)^OPERATION_RESULT ')).Count
        $cleanup = if ($text -match '(?m)^CLEANUP_RESULT result=pass') { 'observed-pass' } else { 'not-observed-pass' }
        Write-Host "STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES_EXIT=$($proc.ExitCode);timeout=$timedOut"
        Write-Host "STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES_RESULTS=$resultCount;cleanup=$cleanup"
        Write-Host 'STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES_OBSERVATION=private-diagnostic;zero-nt-events-is-not-native-coverage'
        Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDOUT_BEGIN"; Write-Host $outTask.Result; Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDOUT_END"
        Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDERR_BEGIN"; Write-Host $errTask.Result; Write-Host "STANDARD_USER_FULL_NETWORK_${label}_STDERR_END"
    } catch {
        [IO.File]::WriteAllText($err, $_.Exception.ToString())
        Write-Host "STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES_INSTRUMENT_ERROR=$($_.Exception.Message)"
    } finally { if ($proc) { $proc.Dispose() } }
}
$one = 'test result: ok. 1 passed; 0 failed; 0 ignored;'
function Select-RunSet([ValidateSet('full', 'owner-pipe-diagnostic', 'native-repair-diagnostic')][string]$Mode, [bool]$FileBrokerCoreKnown = $false) {
    if ($Mode -eq 'owner-pipe-diagnostic') {
        return ,@{ file='windows_native_full_network.exe'; label='owner-pipe-diagnostic-owner-drop'; filter='native_adapter_drop_reaps_pending_listener_and_closes_port'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true }
    }
    if ($Mode -eq 'native-repair-diagnostic') {
        $runs = @(
            @{ file='nub_sandbox_lib.exe'; label='native-repair-root-grant'; filter='backend::windows::tests::explicit_broad_write_roots_are_granted'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; exact=$true },
            @{ file='windows_native_full_network.exe'; label='native-repair-peer-driver'; filter='native_adapter_full_network_has_peer_oracles_and_retained_policy_separation'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true },
            @{ file='nub_sandbox_lib.exe'; label='native-repair-socket-foreign-client'; filter='backend::windows_native_compat::tests::socket_broker_rejects_a_live_client_outside_its_job_and_cancels_idle_workers'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; exact=$true },
            @{ file='nub_sandbox_lib.exe'; label='native-repair-socket-framing'; filter='backend::windows_native_compat::tests::socket_broker_rejects_wrong_frame_lengths_and_drains_cancelled_read'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; exact=$true }
        )
        if ($FileBrokerCoreKnown) {
            $runs += @(
                @{ file='nub_sandbox_lib.exe'; label='native-repair-file-broker-core'; filter='backend::windows_file_broker::tests::'; summary='test result: ok. 5 passed; 0 failed; 5 ignored;' },
                @{ file='nub_sandbox_lib.exe'; label='native-repair-file-broker-open'; filter='backend::windows_file_broker::tests::file_broker_native_open_create_metadata_with_raw_control'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true; nativeAdapter=$true; requiredMarkers=@('FILE_BROKER_NATIVE_CHILD_OK') },
                @{ file='nub_sandbox_lib.exe'; label='native-repair-file-broker-loader'; filter='backend::windows_file_broker::tests::file_broker_native_loader_with_raw_control'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true; nativeAdapter=$true; requiredMarkers=@('FILE_BROKER_NATIVE_LOADER_OK', 'FILE_BROKER_NATIVE_CHILD_OK') },
                @{ file='nub_sandbox_lib.exe'; label='native-repair-file-broker-kill-before-join'; filter='backend::windows_file_broker::tests::file_broker_kills_job_before_joining_blocked_worker'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true; requiredMarkers=@('FILE_BROKER_BLOCKED_WORKER_ENTERED', 'FILE_BROKER_WORKER_OBSERVED_JOB_EXIT=1', 'FILE_BROKER_KILL_BEFORE_JOIN_OK') },
                @{ file='nub_sandbox_lib.exe'; label='native-repair-file-broker-namespace'; filter='backend::windows_file_broker::tests::file_broker_native_namespace_with_raw_control'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true; nativeAdapter=$true; requiredMarkers=@('FILE_BROKER_NATIVE_NAMESPACE_OK') },
                @{ file='nub_sandbox_lib.exe'; label='native-repair-file-broker-cancel-namespace'; filter='backend::windows_file_broker::tests::file_broker_cancels_namespace_before_mutation'; summary='test result: ok. 1 passed; 0 failed; 0 ignored;'; ignored=$true; exact=$true; requiredMarkers=@('FILE_BROKER_BLOCKED_WORKER_ENTERED', 'FILE_BROKER_WORKER_OBSERVED_JOB_EXIT=1', 'FILE_BROKER_KILL_BEFORE_JOIN_OK') }
            )
        }
        return $runs
    }
    return $null
}
$selectedRuns = Select-RunSet $Mode ([bool]$fileBrokerFixture)
if ($null -ne $selectedRuns) {
    if ($Mode -eq 'owner-pipe-diagnostic') { Write-Host 'STANDARD_USER_FULL_NETWORK_OWNER_PIPE_DIAGNOSTIC_SCOPE=owner-drop;ordinary-user-identity;artifact-binding;cleanup;skipped=baseline,dns,file-broker,lifecycle' }
    if ($Mode -eq 'native-repair-diagnostic') { Write-Host "STANDARD_USER_FULL_NETWORK_NATIVE_REPAIR_DIAGNOSTIC_SCOPE=root-grant,peer-driver,socket-foreign,socket-framing,file-broker=$([bool]$fileBrokerFixture);skipped=baseline,dns,lifecycle" }
    $runs = @($selectedRuns)
} else {
$runs = @(
    @{ file='windows_tmp_policy.exe'; label='tmp-policy'; filter=''; summary='test result: ok. 6 passed; 0 failed; 0 ignored;' },
    @{ file='nub_sandbox_lib.exe'; label='windows-cleanup'; filter='backend::windows::windows_cleanup_tests'; summary='test result: ok. 16 passed; 0 failed; 1 ignored;' },
    @{ file='nub_sandbox_lib.exe'; label='windows-native-child'; filter='backend::windows::native_child_tests'; summary='test result: ok. 20 passed; 0 failed; 0 ignored;' },
    @{ file='nub_sandbox_lib.exe'; label='windows-registry'; filter='backend::windows::windows_registry::tests'; summary='test result: ok. 23 passed; 0 failed; 0 ignored;' },
    @{ file='nub_sandbox_lib.exe'; label='native-full-network-authority'; filter='backend::windows::tests::native_socket_authority_requires_unrestricted_network_without_rules_or_brokers'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='native-full-network-preparation'; filter='backend::windows::tests::native_full_network_preparation_keeps_requested_internet_capability'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='native-full-network-identity'; filter='backend::windows::windows_registry::tests::native_full_network_does_not_share_a_deny_network_identity'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='socket-protocol'; filter='backend::windows_native_compat::tests::socket_protocol_rejects_raw_privileged_unknown_and_malformed_requests'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='socket-foreign-client'; filter='backend::windows_native_compat::tests::socket_broker_rejects_a_live_client_outside_its_job_and_cancels_idle_workers'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='socket-framing'; filter='backend::windows_native_compat::tests::socket_broker_rejects_wrong_frame_lengths_and_drains_cancelled_read'; summary=$one; exact=$true },
    @{ file='windows_native_full_network.exe'; label='owner-drop'; filter='native_adapter_drop_reaps_pending_listener_and_closes_port'; summary=$one; exact=$true; ignored=$true },
    @{ file='windows_native_full_network.exe'; label='broker-owner-drop'; filter='native_adapter_drop_reaps_pending_broker_session_and_closes_port'; summary=$one; exact=$true; ignored=$true },
    @{ file='nub_sandbox_lib.exe'; label='full-disk-admission'; filter='backend::windows::tests::apply_windows_full_disk_rejects_restricted_network'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='public-full-disk-admission'; filter='backend::tests::public_preparation_rejects_unrestricted_filesystem_with_restricted_network'; summary=$one; exact=$true },
    @{ file='nub_sandbox_lib.exe'; label='embedded-native-adapter'; filter='backend::windows_native_adapter_probe::embedded_native_adapter_primitives_with_raw_and_plain_controls'; summary=$one; nativeAdapter=$true; exact=$true },
    @{ file='windows_native_full_network.exe'; label='native-full-network-peer-driver'; filter='native_adapter_full_network_has_peer_oracles_and_retained_policy_separation'; summary=$one; ignored=$true; exact=$true },
    @{ file='windows_native_full_network.exe'; label='native-full-network-dns-opt-in'; filter='native_adapter_full_network_dns_opt_in'; summary=$one; dnsOptIn=$true; ignored=$true; exact=$true }
)
if ($fileBrokerFixture) {
    $runs += @(
        @{ file='nub_sandbox_lib.exe'; label='file-broker-core'; filter='backend::windows_file_broker::tests::'; summary='test result: ok. 5 passed; 0 failed; 5 ignored;' },
        @{ file='nub_sandbox_lib.exe'; label='file-broker-native-open'; filter='backend::windows_file_broker::tests::file_broker_native_open_create_metadata_with_raw_control'; summary=$one; ignored=$true; exact=$true; nativeAdapter=$true; requiredMarkers=@('FILE_BROKER_NATIVE_CHILD_OK') },
        @{ file='nub_sandbox_lib.exe'; label='file-broker-native-loader'; filter='backend::windows_file_broker::tests::file_broker_native_loader_with_raw_control'; summary=$one; ignored=$true; exact=$true; nativeAdapter=$true; requiredMarkers=@('FILE_BROKER_NATIVE_LOADER_OK', 'FILE_BROKER_NATIVE_CHILD_OK') },
        @{ file='nub_sandbox_lib.exe'; label='file-broker-kill-before-join'; filter='backend::windows_file_broker::tests::file_broker_kills_job_before_joining_blocked_worker'; summary=$one; ignored=$true; exact=$true; requiredMarkers=@('FILE_BROKER_BLOCKED_WORKER_ENTERED', 'FILE_BROKER_WORKER_OBSERVED_JOB_EXIT=1', 'FILE_BROKER_KILL_BEFORE_JOIN_OK') },
        @{ file='nub_sandbox_lib.exe'; label='file-broker-native-namespace'; filter='backend::windows_file_broker::tests::file_broker_native_namespace_with_raw_control'; summary=$one; ignored=$true; exact=$true; nativeAdapter=$true; requiredMarkers=@('FILE_BROKER_NATIVE_NAMESPACE_OK') },
        @{ file='nub_sandbox_lib.exe'; label='file-broker-cancel-namespace'; filter='backend::windows_file_broker::tests::file_broker_cancels_namespace_before_mutation'; summary=$one; ignored=$true; exact=$true; requiredMarkers=@('FILE_BROKER_BLOCKED_WORKER_ENTERED', 'FILE_BROKER_WORKER_OBSERVED_JOB_EXIT=1', 'FILE_BROKER_KILL_BEFORE_JOIN_OK') }
    )
}
}
foreach ($run in $runs) {
    $arguments = Test-Arguments $run.filter ([bool]$run.exact) ([bool]$run.ignored)
    Run-Filtered $run.file $run.label $arguments $run.summary ([bool]$run.nativeAdapter) ([bool]$run.dnsOptIn) ([string[]]$run.requiredMarkers)
}
if ($operationShapes) { Run-OperationShapes $operationShapes }
if ($script:failed) {
    if ($Mode -eq 'owner-pipe-diagnostic') { Write-Host 'STANDARD_USER_FULL_NETWORK_OWNER_PIPE_DIAGNOSTIC=failed' } elseif ($Mode -eq 'native-repair-diagnostic') { Write-Host 'STANDARD_USER_FULL_NETWORK_NATIVE_REPAIR_DIAGNOSTIC=failed' } else { Write-Host 'STANDARD_USER_FULL_NETWORK_GATE=failed' }
    exit 1
}
if ($Mode -eq 'owner-pipe-diagnostic') { Write-Host 'STANDARD_USER_FULL_NETWORK_OWNER_PIPE_DIAGNOSTIC=ok' } elseif ($Mode -eq 'native-repair-diagnostic') { Write-Host 'STANDARD_USER_FULL_NETWORK_NATIVE_REPAIR_DIAGNOSTIC=ok' } else { Write-Host 'STANDARD_USER_FULL_NETWORK_GATE=ok' }
'@ | Set-Content -Encoding ascii (Join-Path $stage 'run-full-network.ps1')
    Copy-Item (Join-Path $stage 'run-full-network.ps1') (Join-Path $ReportDirectory 'standard-user-run.ps1')
    icacls $stage /grant "${name}:(OI)(CI)RX" /T | Tee-Object "$ReportDirectory/stage-acl.log"
    if ($LASTEXITCODE -ne 0) { throw 'Granting staged artifact access failed' }
    $credential = New-Object System.Management.Automation.PSCredential("$env:COMPUTERNAME\$name", $password)
    Start-Service seclogon
    $process = Start-Process powershell.exe -Credential $credential -LoadUserProfile -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "$stage\run-full-network.ps1", $stage, $Mode) -WorkingDirectory $stage -PassThru -RedirectStandardOutput "$ReportDirectory/standard-user.log" -RedirectStandardError "$ReportDirectory/standard-user-error.log"
    if (!$process.WaitForExit(1200000)) { taskkill /PID $process.Id /T /F | Out-Null; throw 'Full-network gate exceeded its 20-minute deadline' }
    $process.WaitForExit(); $exitCode = $process.ExitCode
    Get-Content "$ReportDirectory/standard-user.log" -ErrorAction Continue
    Get-Content "$ReportDirectory/standard-user-error.log" -ErrorAction Continue
    $ownedMarker = Select-String -Path "$ReportDirectory/standard-user.log" -Pattern '^STANDARD_USER_FULL_NETWORK_OWNED=(.+)$' | Select-Object -Last 1
    if ($ownedMarker) {
        $owned = $ownedMarker.Matches[0].Groups[1].Value
        try { Get-ChildItem -LiteralPath $owned -Filter '*.log' -ErrorAction Stop | Copy-Item -Destination $ReportDirectory -Force }
        catch { $exitCode = 1; Write-Warning "Could not retain ordinary-user logs: $($_.Exception.Message)" }
    } else { $exitCode = 1; Write-Warning 'Ordinary-user owned-path marker is missing' }
    $successPattern = if ($Mode -eq 'owner-pipe-diagnostic') { '^STANDARD_USER_FULL_NETWORK_OWNER_PIPE_DIAGNOSTIC=ok$' } elseif ($Mode -eq 'native-repair-diagnostic') { '^STANDARD_USER_FULL_NETWORK_NATIVE_REPAIR_DIAGNOSTIC=ok$' } else { '^STANDARD_USER_FULL_NETWORK_GATE=ok$' }
    if ($exitCode -ne 0 -or !(Select-String -Path "$ReportDirectory/standard-user.log" -Pattern $successPattern)) { $exitCode = 1 }
} finally {
    $cleanup = @{ user = $name; stage = $stage; errors = @() }
    try { if ($process -and !$process.HasExited) { taskkill /PID $process.Id /T /F | Out-Null; if (!$process.WaitForExit(30000)) { throw 'standard-user process did not exit after taskkill' } } } catch { $cleanup.errors += "process: $($_.Exception.Message)"; $exitCode = 1 }
    foreach ($log in @('standard-user.log', 'standard-user-error.log')) { $path = Join-Path $ReportDirectory $log; if (Test-Path $path) { Get-Content $path -ErrorAction Continue } }
    if ($user) { $sid = $user.SID.Value; $cleanup.sid = $sid; try { Get-CimInstance Win32_UserProfile -Filter "SID='$sid'" | Remove-CimInstance } catch { $cleanup.errors += "profile: $($_.Exception.Message)"; $exitCode = 1 }; try { Remove-LocalUser -Name $name } catch { $cleanup.errors += "account: $($_.Exception.Message)"; $exitCode = 1 } }
    try { if (Test-Path $stage) { Remove-Item -Recurse -Force $stage } } catch { $cleanup.errors += "stage: $($_.Exception.Message)"; $exitCode = 1 }
    $cleanup | ConvertTo-Json -Depth 4 | Set-Content -Encoding utf8 "$ReportDirectory/standard-user-cleanup.json"
}
exit $exitCode
