$ErrorActionPreference = 'Stop'
$tokens = $null; $errors = $null
$source = Join-Path $PSScriptRoot 'run-full-network-standard-user.ps1'
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$outer = Get-Content $source -Raw
if (!$outer.Contains('[Parameter(Mandatory=$true)][string]$RelayBinary')) { throw 'Expected a required standalone relay binary' }
if (!$outer.Contains('[string]$FileOperationShapesBinary') -or !$outer.Contains('[string]$FileOperationShapesManifest')) { throw 'Expected paired optional file-operation-shapes artifacts' }
$inner = @($ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.StringConstantExpressionAst] -and
    $node.StringConstantType -eq 'SingleQuotedHereString'
}, $true))
if ($inner.Count -ne 1) { throw 'Expected one generated ordinary-user script' }
$ast = [Management.Automation.Language.Parser]::ParseInput($inner[0].Value, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
if (!$inner[0].Value.Contains("`$env:NUB_WINDOWS_RELAY_FIXTURE = Join-Path `$owned 'windows_relay_fixture.exe'")) { throw 'Expected the ordinary-user relay executable binding' }
if (!$inner[0].Value.Contains('STANDARD_USER_FULL_NETWORK_OPERATION_SHAPES=absent;no-auxiliary-claim')) { throw 'Expected an explicit no-auxiliary claim' }
$function = @($ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Test-Arguments'
}, $true))
if ($function.Count -ne 1) { throw 'Expected the production argument builder' }
Invoke-Expression $function[0].Extent.Text
$selector = @($ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Select-RunSet'
}, $true))
if ($selector.Count -ne 1) { throw 'Expected the production run selector' }
Invoke-Expression $selector[0].Extent.Text
$operationShapes = @($ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Run-OperationShapes'
}, $true))
if ($operationShapes.Count -ne 1) { throw 'Expected the bounded operation-shapes diagnostic runner' }
$cases = @(
    @('', $false, $false, '--nocapture|--test-threads=1'),
    @('group', $false, $false, 'group|--nocapture|--test-threads=1'),
    @('single', $true, $false, '--exact|single|--nocapture|--test-threads=1'),
    @('single', $true, $true, '--ignored|--exact|single|--nocapture|--test-threads=1'),
    @('backend::windows_file_broker::tests::', $false, $false, 'backend::windows_file_broker::tests::|--nocapture|--test-threads=1'),
    @('backend::windows_file_broker::tests::file_broker_native_open_create_metadata_with_raw_control', $true, $true, '--ignored|--exact|backend::windows_file_broker::tests::file_broker_native_open_create_metadata_with_raw_control|--nocapture|--test-threads=1'),
    @('backend::windows_file_broker::tests::file_broker_native_loader_with_raw_control', $true, $true, '--ignored|--exact|backend::windows_file_broker::tests::file_broker_native_loader_with_raw_control|--nocapture|--test-threads=1'),
    @('backend::windows_file_broker::tests::file_broker_kills_job_before_joining_blocked_worker', $true, $true, '--ignored|--exact|backend::windows_file_broker::tests::file_broker_kills_job_before_joining_blocked_worker|--nocapture|--test-threads=1')
)
foreach ($case in $cases) {
    $actual = Test-Arguments $case[0] $case[1] $case[2]
    if (($actual -join '|') -ne $case[3]) { throw "Arguments differed: $($actual -join '|')" }
}
Write-Host 'FULL_NETWORK_ARGUMENT_CASES=8'
Write-Host 'FULL_NETWORK_RELAY_INSTRUMENT=standalone-executable'
if ($null -ne (Select-RunSet 'full')) { throw 'Full mode must retain the full baseline selection' }
$diagnostic = @(Select-RunSet 'owner-pipe-diagnostic')
if ($diagnostic.Count -ne 1 -or $diagnostic[0].label -ne 'owner-pipe-diagnostic-owner-drop' -or !$diagnostic[0].exact -or !$diagnostic[0].ignored) { throw 'Owner-pipe diagnostic must select only the exact ignored owner-drop test' }
$diagnosticArguments = Test-Arguments $diagnostic[0].filter ([bool]$diagnostic[0].exact) ([bool]$diagnostic[0].ignored)
if (($diagnosticArguments -join '|') -ne '--ignored|--exact|native_adapter_drop_reaps_pending_listener_and_closes_port|--nocapture|--test-threads=1') { throw 'Owner-pipe diagnostic arguments differed' }
Write-Host 'FULL_NETWORK_OWNER_PIPE_SELECTOR=exact-owner-drop'
$repair = @(Select-RunSet 'native-repair-diagnostic' $true)
if ($repair.Count -ne 8 -or $repair[0].filter -ne 'backend::windows::tests::explicit_broad_write_roots_are_granted' -or $repair[1].filter -ne 'native_adapter_full_network_has_peer_oracles_and_retained_policy_separation') { throw 'Native repair diagnostic must select the root test and native peer driver' }
$repairIgnored = @($repair | Where-Object { $_.ignored })
if ($repairIgnored.Count -ne 4 -or $repairIgnored[1].filter -ne 'backend::windows_file_broker::tests::file_broker_native_open_create_metadata_with_raw_control' -or $repairIgnored[2].filter -ne 'backend::windows_file_broker::tests::file_broker_native_loader_with_raw_control' -or $repairIgnored[3].filter -ne 'backend::windows_file_broker::tests::file_broker_kills_job_before_joining_blocked_worker') { throw 'Native repair diagnostic must select the three ignored file-broker acceptance tests' }
foreach ($run in $repair) {
    $arguments = Test-Arguments $run.filter ([bool]$run.exact) ([bool]$run.ignored)
    if ($run.exact -and $arguments -notcontains '--exact') { throw "Native repair exact arguments omitted --exact: $($run.label)" }
    if ($run.exact) {
        $parts = $run.filter -split '::'
        $source = if ($parts.Length -eq 1) { 'tests/windows_native_full_network.rs' } else { "src/backend/$($parts[1]).rs" }
        $text = Get-Content -Raw (Join-Path $PSScriptRoot "../../crates/nub-sandbox/$source")
        if ($text -notmatch ("\bfn\s+" + [regex]::Escape($parts[-1]) + "\s*\(")) { throw "Native repair filter has no source declaration: $($run.filter)" }
    }
}
Write-Host 'FULL_NETWORK_NATIVE_REPAIR_SELECTOR=root-peer-socket-file'
Write-Host 'FULL_NETWORK_OPERATION_SHAPES=separate-manifest-private-diagnostic'
