# Fixture for the unique-restricting-sid probe. Extends setup.ps1 with the two
# things that arm needs and the low-IL arm did not:
#   - real secret files under %USERPROFILE% (.ssh/id_rsa, .npmrc), because the
#     verdict is about whether those become UNREADABLE
#   - an UNGRANTED sibling directory under the same parent as the project, so a
#     GRANTED project read can be distinguished from "the mechanism is off"
$ErrorActionPreference = 'Stop'
$root = Join-Path $env:USERPROFILE 'jail'
$proj = Join-Path $root 'project'
$sib  = Join-Path $root 'sibling'
$nested = Join-Path $proj 'node_modules\nested-pkg\a\b\c'

foreach ($d in @($proj, $sib, $nested, (Join-Path $proj 'node_modules\leftpad-stub'),
                 (Join-Path $env:USERPROFILE '.ssh'))) {
  New-Item -ItemType Directory -Force -Path $d | Out-Null
}

Set-Content -Path (Join-Path $proj 'package.json') -Value '{"name":"jail-fixture","version":"1.0.0"}'
Set-Content -Path (Join-Path $proj 'node_modules\leftpad-stub\package.json') -Value '{"name":"leftpad-stub","version":"1.0.0","main":"index.js"}'
Set-Content -Path (Join-Path $proj 'node_modules\leftpad-stub\index.js') -Value 'module.exports = "leftpad-stub-loaded";'
Set-Content -Path (Join-Path $proj 'node_modules\nested-pkg\package.json') -Value '{"name":"nested-pkg","version":"1.0.0"}'
Set-Content -Path (Join-Path $nested 'probe.js') -Value 'module.exports = require("leftpad-stub");'

Set-Content -Path (Join-Path $sib 'sibling.txt') -Value 'SIBLING-MUST-STAY-DENIED'
Set-Content -Path (Join-Path $env:USERPROFILE '.ssh\id_rsa') -Value 'FAKE-PRIVATE-KEY-MUST-STAY-DENIED'
Set-Content -Path (Join-Path $env:USERPROFILE '.npmrc') -Value '//registry.npmjs.org/:_authToken=FAKE-TOKEN-MUST-STAY-DENIED'

Write-Output "proj=$proj"
Write-Output "sibling=$sib"
Write-Output "profile=$env:USERPROFILE"
