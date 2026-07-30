# Fixture for the Windows restricted-token + low-IL build-jail probe.
# Everything lives under the invoking user's own profile, because that is where
# an unprivileged owner actually holds WRITE_OWNER (via the profile's Full
# Control DACL) and can therefore relabel.
$ErrorActionPreference = 'Stop'
$root = Join-Path $env:USERPROFILE 'jail'
$proj = Join-Path $root 'project'
$nested = Join-Path $proj 'node_modules\nested-pkg\a\b\c'
$store = Join-Path $env:LOCALAPPDATA 'nub\store'
$cache = Join-Path $env:LOCALAPPDATA 'nub\cache'

foreach ($d in @($proj, $nested, (Join-Path $proj 'node_modules\leftpad-stub'), $store, $cache)) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
}

Set-Content -Path (Join-Path $proj 'package.json') -Value '{"name":"jail-fixture","version":"1.0.0"}'
Set-Content -Path (Join-Path $proj 'node_modules\leftpad-stub\package.json') -Value '{"name":"leftpad-stub","version":"1.0.0","main":"index.js"}'
Set-Content -Path (Join-Path $proj 'node_modules\leftpad-stub\index.js') -Value 'module.exports = "leftpad-stub-loaded";'
Set-Content -Path (Join-Path $proj 'node_modules\nested-pkg\package.json') -Value '{"name":"nested-pkg","version":"1.0.0"}'
Set-Content -Path (Join-Path $nested 'probe.js') -Value 'module.exports = require("leftpad-stub");'

# The recon target: broad reads are sanctioned, so this is expected to be
# READABLE under the jail. It exists so "reads work" is measured, not assumed.
Set-Content -Path (Join-Path $env:USERPROFILE 'secret-probe.txt') -Value 'SECRET-VISIBLE-TO-JAIL'

Write-Output "proj=$proj"
Write-Output "store=$store"
Write-Output "cache=$cache"
Write-Output "temp=$env:TEMP"
Write-Output "lowtemp-exists=$(Test-Path (Join-Path $env:LOCALAPPDATA 'Temp\Low'))"
