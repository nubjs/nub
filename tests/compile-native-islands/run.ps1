# PowerShell launcher for the same neutral JavaScript driver as run.sh.
$ErrorActionPreference = 'Stop'
$nub = $env:NUB_NATIVE_ISLAND_NUB
if (-not $nub) {
  for ($i = 0; $i -lt $args.Count; $i++) {
    if ($args[$i] -eq '--nub') {
      if (($i + 1) -ge $args.Count) { throw '--nub needs a value' }
      $nub = $args[$i + 1]
      break
    }
  }
}
if (-not $nub) { throw 'usage: run.ps1 --nub <candidate> --launcher <matching-launcher> --target <node-version> [--platform <triple>] [--mode prebuilt]' }
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
& $nub (Join-Path $here 'driver.mjs') @args
exit $LASTEXITCODE
