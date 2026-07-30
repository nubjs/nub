# The verdict for the MSI-Node-DACL probe, factored out for ONE reason: so `selftest.ps1` can drive
# it with SYNTHETIC cells and prove it discriminates in BOTH directions before a real run depends
# on it. Same discipline (and same `W`/`Fact`/`Prop`/`Cell` primitives) as
# `tests/win-bypass-traverse/verdict.ps1`, which this dot-sources.
#
# THE QUESTION: `nodejs/node#63590` says the Node MSI's `SetInstallDirPermission` component writes a
# PROTECTED, explicit DACL on `C:\Program Files\nodejs` (WiX `<Permission>` -> the MSI
# `LockPermissions` table, which REPLACES the descriptor rather than merging), granting only Users /
# Authenticated Users / Administrators / SYSTEM. If that is true on a real install then no
# `S-1-15-2-*` sid holds any right there, and an AppContainer child cannot EXECUTE `node.exe` at
# all — which would make every other property of the Windows build jail moot for a user whose Node
# came from the MSI.
#
# WHY THE ARMS ARE SHAPED THIS WAY. Two failures have to be told apart and they need different
# probes: (a) `CreateProcessW` refusing to START `C:\Program Files\nodejs\node.exe`, which is
# measured with `node -e` so nothing but `node.exe` and the System32 DLLs is read; and (b) an
# already-running confined child being unable to READ that tree (`node_modules\npm`), which is
# measured with a DIFFERENT interpreter — a copy under the user's own store, the shape nub itself
# provisions — so the read cells exist even when (a) fails.

. (Join-Path (Join-Path (Join-Path $PSScriptRoot '..') 'win-bypass-traverse') 'verdict.ps1')

$script:MsiArms = @('plain-progfiles', 'ac-exec-progfiles', 'ac-exec-progfiles-repaired',
  'ac-exec-userstore', 'ac-exec-toolcache', 'ac-read-progfiles',
  'plain-progfiles-npm', 'ac-exec-progfiles-npm')
$script:MsiOps = @('exec-inline', 'entry-ran', 'cwd-at-entry', 'read-progfiles-node-exe',
  'stat-progfiles-node-exe', 'readdir-progfiles-dir', 'read-npm-package-json', 'readdir-npm-tree',
  'walk-from-progfiles', 'stat-c-root', 'read-system32-hosts')

function Invoke-MsiVerdict {
  param([hashtable]$Cells, [hashtable]$Facts)
  $cells = $Cells
  if ($null -eq $Facts) { $Facts = @{} }
  function F([string]$k) { if ($Facts.ContainsKey($k)) { return $Facts[$k] } else { return $null } }

  W ''
  W '== operations table =='
  W ("  {0,-26} {1}" -f 'op', (($script:MsiArms | ForEach-Object { "{0,-28}" -f $_ }) -join ''))
  foreach ($op in $script:MsiOps) {
    W ("  {0,-26} {1}" -f $op, (($script:MsiArms | ForEach-Object { "{0,-28}" -f (Cell $_ $op) }) -join ''))
  }
  W ''
  W '  launch status per arm:'
  foreach ($a in $script:MsiArms) { W ("    {0,-28} {1}" -f $a, (LaunchOf $a)) }

  W ''
  W '== verdict =='

  # ── CONTROLS. A red one here means the harness or the host is the story and nothing below is
  # attributable to the DACL.
  Prop 'msi-baseline-plain-execs-progfiles-node' ((Cell 'plain-progfiles' 'exec-inline') -eq 'OK') `
    "an UNCONFINED child must run C:\Program Files\nodejs\node.exe -e: $(Cell 'plain-progfiles' 'exec-inline') launch=$(LaunchOf 'plain-progfiles')"
  # `ac-exec-userstore` and `ac-read-progfiles` are the two arms that reach the operations table, so
  # they are where confinement is proven. `C:\` denied proves the LowBox gate is live; System32
  # granted proves it is PASSABLE, so a column of ERRs is about DACLs and not a dead child.
  $gateWhy = @()
  $gateLive = $true
  foreach ($a in @('ac-exec-userstore', 'ac-read-progfiles')) {
    $c = Cell $a 'stat-c-root'
    $gateWhy += "$a=$c"
    if ($c -ne 'ERR') { $gateLive = $false }
  }
  Prop 'msi-appcontainer-gate-is-live' $gateLive `
    "every table-reaching AppContainer arm must be DENIED on C:\ ($($gateWhy -join ' ')) — else the token is not confined"
  Prop 'msi-appcontainer-positive-control' `
    (((Cell 'ac-read-progfiles' 'read-system32-hosts') -eq 'OK') -and
     ((Cell 'ac-exec-userstore' 'read-system32-hosts') -eq 'OK')) `
    "System32 carries an ALL APPLICATION PACKAGES ace, so a confined child must still read it: read-arm=$(Cell 'ac-read-progfiles' 'read-system32-hosts') userstore-arm=$(Cell 'ac-exec-userstore' 'read-system32-hosts')"

  # ── THE CONTROL THAT MAKES THE DACL ROWS ATTRIBUTABLE TO THE MSI. If msiexec did not run, those
  # rows describe whatever the runner IMAGE left on that directory — plausibly identical, wrongly
  # attributed. Reported ahead of the DACL properties for that reason.
  Prop 'msi-install-actually-succeeded' ((F 'msi-install-succeeded') -eq $true) `
    "a real MSI must have installed, or every DACL row below is about the image and not the installer: $(F 'msi-install-why')"

  # ── THE DACL ITSELF, read off a real MSI install. `#63590` predicts all three: no `S-1-15-2-1`,
  # no `S-1-15-2-2`, and a PROTECTED descriptor (so nothing propagates in from `C:\Program Files`,
  # which does carry the ace). Reported as three separate properties because each has a different
  # consequence, and a single merged one would hide which prediction actually held.
  Prop 'msi-install-dir-lacks-all-application-packages' ((F 'msi-has-aap') -eq $false) `
    "C:\Program Files\nodejs after a real MSI install must not grant S-1-15-2-1 (this is #63590's claim): has-aap=$(F 'msi-has-aap')"
  Prop 'msi-install-dir-lacks-all-restricted-application-packages' ((F 'msi-has-arap') -eq $false) `
    "…nor S-1-15-2-2: has-arap=$(F 'msi-has-arap')"
  Prop 'msi-install-dir-dacl-is-protected' ((F 'msi-protected') -eq $true) `
    "the MSI LockPermissions descriptor is D:PAI, so the ace C:\Program Files DOES carry cannot propagate in: protected=$(F 'msi-protected')"
  # The comparison that makes the above a DEFECT rather than a Windows-wide fact: a sibling under
  # the same parent must HAVE the ace. Without this, "nodejs lacks it" could just mean Program Files
  # subdirectories never have it.
  Prop 'msi-control-sibling-under-program-files-has-the-ace' ((F 'sibling-has-aap') -eq $true) `
    "a sibling C:\Program Files\* directory must carry S-1-15-2-1, or 'nodejs lacks it' is not a defect: sibling=$(F 'sibling-name') has-aap=$(F 'sibling-has-aap')"

  # ── THE DECISIVE CELL. Can a confined child EXECUTE the MSI-installed node at all?
  # READ THE DIRECTION: this property PASSing means the AppContainer route is BROKEN for a user
  # whose Node came from the MSI. It is named for the claim it measures, not for the outcome anyone
  # wanted — the same inverted convention `mainonly-leaves-dependency-resolution-broken` uses.
  Prop 'msi-node-cannot-be-executed-by-an-appcontainer' ((Cell 'ac-exec-progfiles' 'exec-inline') -ne 'OK') `
    "PASS here means the jail CANNOT run the user's MSI node: exec=$(Cell 'ac-exec-progfiles' 'exec-inline') launch=$(LaunchOf 'ac-exec-progfiles')"
  # ONE VARIABLE, and it is what attributes the refusal to the DACL rather than to anything else
  # about `C:\Program Files`: add the ace (which needs WRITE_DAC, i.e. elevation) and re-launch the
  # identical command line.
  Prop 'msi-exec-is-restored-by-adding-the-ace' ((Cell 'ac-exec-progfiles-repaired' 'exec-inline') -eq 'OK') `
    "the SAME command line with the AC sid granted RX must run — that pairing is what makes the denial a DACL fact: exec=$(Cell 'ac-exec-progfiles-repaired' 'exec-inline') launch=$(LaunchOf 'ac-exec-progfiles-repaired')"
  Prop 'msi-tree-cannot-be-read-by-an-appcontainer' `
    (((Cell 'ac-read-progfiles' 'read-progfiles-node-exe') -ne 'OK') -and
     ((Cell 'ac-read-progfiles' 'read-npm-package-json') -ne 'OK')) `
    "PASS means the npm tree beside node.exe is unreadable too: node.exe=$(Cell 'ac-read-progfiles' 'read-progfiles-node-exe') npm/package.json=$(Cell 'ac-read-progfiles' 'read-npm-package-json') readdir=$(Cell 'ac-read-progfiles' 'readdir-npm-tree')"

  # ── THE PRACTICAL CONSEQUENCE, which is where the exec/read dissociation actually bites. Starting
  # `node.exe` is a PARENT-context image open, but the bundled npm is an ordinary file READ by the
  # confined child. So the question that matters for a lifecycle script is not whether node starts —
  # it is whether the npm sitting beside it still runs. One variable between these two arms.
  Prop 'npm-baseline-plain-runs-bundled-npm' ((Cell 'plain-progfiles-npm' 'entry-ran') -eq 'OK') `
    "an UNCONFINED child must run the bundled npm-cli.js, or the arm below measures a broken fixture: $(Cell 'plain-progfiles-npm' 'entry-ran') $(Detail 'plain-progfiles-npm' 'entry-ran')"
  # INVERTED READING: PASS means the bundled npm is unusable inside the jail.
  Prop 'bundled-npm-cannot-run-inside-the-jail' ((Cell 'ac-exec-progfiles-npm' 'entry-ran') -ne 'OK') `
    "PASS means node STARTS but the npm beside it cannot be read, so a lifecycle script reaching for the ambient npm dies: confined=$(Cell 'ac-exec-progfiles-npm' 'entry-ran') $(Detail 'ac-exec-progfiles-npm' 'entry-ran') launch=$(LaunchOf 'ac-exec-progfiles-npm')"

  # ── THE CONTRAST THAT BOUNDS THE FINDING. nub provisions its own Node under
  # `%USERPROFILE%\.cache\nub\node\<version>\`, a path the user owns and nub can ACE. If that shape
  # execs cleanly, the defect is scoped to an AMBIENT/MSI node rather than to the route.
  Prop 'nub-provisioned-node-shape-execs-under-appcontainer' `
    (((Lines 'ac-exec-userstore') -gt 0) -and ((Cell 'ac-exec-userstore' 'cwd-at-entry') -ne 'MISSING-OP')) `
    "a node.exe under the user's own store with an ACE must run confined: lines=$(Lines 'ac-exec-userstore') launch=$(LaunchOf 'ac-exec-userstore')"

  # ── HOW THE CI CONFIGURATION DIFFERS, which is the reason this was never caught. `actions/setup-node`
  # UNZIPS a tarball into `C:\hostedtoolcache\…`, inheriting that tree's DACL, instead of running the
  # MSI's LockPermissions component. INVERTED READING AGAIN: PASS means the two disagree, i.e. a
  # CI-provisioned Node would have hidden the defect.
  Prop 'ci-provisioned-node-and-msi-node-disagree-on-exec' `
    (((Cell 'ac-exec-toolcache' 'exec-inline') -eq 'OK') -and ((Cell 'ac-exec-progfiles' 'exec-inline') -ne 'OK')) `
    "PASS means a CI-provisioned Node is executable where the MSI one is not, so CI cannot see this: toolcache=$(Cell 'ac-exec-toolcache' 'exec-inline') msi=$(Cell 'ac-exec-progfiles' 'exec-inline') toolcache-has-aap=$(F 'toolcache-has-aap')"

  # ── CAN NUB REPAIR IT UNPRIVILEGED? Measured under an impersonated admin-stripped token, with the
  # control that makes a denial attributable: the SAME impersonated write on a directory in the
  # user's own profile must SUCCEED. Without that control, a failure could just mean impersonation
  # never took effect.
  Prop 'unprivileged-repair-control-own-profile-write-succeeds' ((F 'deelev-own-profile-write') -eq 'OK') `
    "under the admin-stripped impersonation, an ace write inside %USERPROFILE% must succeed or the arm is inconclusive: $(F 'deelev-own-profile-write') admin-under-impersonation=$(F 'deelev-admin')"
  # Read off the DELETE variant only. The `disable` variant is reported beside it because it produced
  # a false positive in run 1 and the counter-example is worth keeping on screen.
  Fact 'deelev-disable-variant-artifact' (F 'deelev-disable-artifact')
  Fact 'deelev-delete-variant-privileges' (F 'deelev-delete-privs')
  Prop 'unprivileged-repair-of-program-files-is-refused' ((F 'deelev-progfiles-write') -ne 'OK') `
    "PASS means nub cannot fix this on the user's behalf without elevation. Measured on the privilege-DELETED token, not the merely-disabled one: $(F 'deelev-progfiles-write') owner=$(F 'msi-owner')"

  return $script:fails
}
