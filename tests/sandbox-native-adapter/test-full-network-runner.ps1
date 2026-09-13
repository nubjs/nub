$ErrorActionPreference = 'Stop'
$tokens = $null; $errors = $null
$source = Join-Path $PSScriptRoot 'run-full-network-standard-user.ps1'
$ast = [Management.Automation.Language.Parser]::ParseFile($source, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
$outer = Get-Content $source -Raw
if (!$outer.Contains('[Parameter(Mandatory=$true)][string]$RelayBinary')) { throw 'Expected a required standalone relay binary' }
$inner = @($ast.FindAll({ param($node)
    $node -is [Management.Automation.Language.StringConstantExpressionAst] -and
    $node.StringConstantType -eq 'SingleQuotedHereString'
}, $true))
if ($inner.Count -ne 1) { throw 'Expected one generated ordinary-user script' }
$ast = [Management.Automation.Language.Parser]::ParseInput($inner[0].Value, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
if (!$inner[0].Value.Contains("`$env:NUB_WINDOWS_RELAY_FIXTURE = Join-Path `$owned 'windows_relay_fixture.exe'")) { throw 'Expected the ordinary-user relay executable binding' }
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
