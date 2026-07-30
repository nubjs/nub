# The verdict for the JAIL-BIN probe, factored out so `jailbin-selftest.ps1` can drive it with
# SYNTHETIC cells and prove it discriminates in BOTH directions before a runner minute is spent.
# Same `W`/`Fact`/`Prop`/`Cell` primitives as its two siblings (dot-sourced from
# `../win-bypass-traverse/verdict.ps1`).
#
# THE QUESTION. MECHANISM-FACTS §5j measured a dissociation on a stock MSI-installed Node: an
# AppContainer child CAN exec `C:\Program Files\nodejs\node.exe` (the image is opened in the CALLER's
# context) but CANNOT read one byte of that tree afterwards, so the bundled `npm` dies
# `Cannot find module '…\npm\bin\npm-cli.js'`. That tree cannot be ACE'd unprivileged — it is
# protected, owner SYSTEM, and carries no `S-1-15-2-*` at all.
#
# THE PROPOSED MECHANISM. A nub-OWNED directory (under nub's own cache, so ACE-able with no
# privilege) holding the executables a jailed lifecycle script may invoke, with the child's `PATH`
# REWRITTEN to point only there. Anything resolving `node` / `npm` / `node-gyp` finds nub's copy,
# inside the grant.
#
# WHAT EACH GROUP OF PROPERTIES DECIDES, and why the split matters:
#
#   controls          The unconfined baseline must succeed on the same fixture and the same command
#                     line; every confined arm must be DENIED on `C:\` (gate live) and GRANTED on
#                     System32 (gate passable, so a column of failures is about DACLs and not a dead
#                     child); and `cmd.exe` must run confined at all, because every cell here is
#                     resolved BY cmd from the sanitized `PATH` and a dead shell would read as a
#                     missing tool.
#
#   attribution       The ambient-MSI arm must still FAIL exactly the way §5j measured. Without that
#                     row in THIS run, a green jail-bin arm could be the runner's image rather than
#                     the mechanism.
#
#   read-vs-pipe      Stock `npm.cmd` resolves its CLI through `FOR /F ('CALL node npm-prefix.js')`,
#                     i.e. a PIPED sub-process — and §5h measured a piped spawn under an
#                     AppContainer hanging indefinitely. So "npm works" and "the npm tree is
#                     readable" are TWO questions, and conflating them would attribute a pipe hang
#                     to a permission problem. `…-npm-direct` runs `node npm-cli.js` with no shell
#                     pipe anywhere, which is the read question alone.
#
#   does-node-need-copying
#                     The hybrid arm is the one the design hinges on: the AMBIENT MSI `node.exe` as
#                     the interpreter (un-ACE-able, and §5j says it execs anyway) with ONLY the npm
#                     tree copied into the nub-owned dir. If that runs, the copy payload is the
#                     ~12 MiB npm tree and NOT the ~88 MiB `node.exe`.
#
#   stable-sid        The grant is `S-1-15-2-1` (ALL APPLICATION PACKAGES) written ONCE at populate
#                     time, not a per-run package sid written per launch. If a zero-capability LowBox
#                     token reads through it, the per-launch ACE cost on a 2,374-entry tree is zero.
#                     The arm withholds the per-run sid and reads the DACL back to prove it.
#
#   absent-tool       A tool nub did not put in the dir now fails as a RESOLUTION error rather than a
#                     permission error. The property records which, because the two produce very
#                     different field reports.
#
#   hardlink          Populating by hard link instead of copying would be cheaper. A link shares the
#                     MFT record's security descriptor, so an ACE on the link is an ACE on the
#                     original — measured directly, in both the user-owned and the Program Files
#                     direction.

. (Join-Path (Join-Path (Join-Path $PSScriptRoot '..') 'win-bypass-traverse') 'verdict.ps1')

$script:JbArms = @(
  'plain-jailbin', 'plain-ambient',
  'ac-cmd-floor', 'ac-ambient', 'ac-ambient-on-path', 'ac-jailbin-ungranted',
  'ac-jailbin-node', 'ac-jailbin-npm-direct', 'ac-hybrid-npm-direct', 'ac-aap-only',
  'ac-jailbin-npm-stock', 'ac-jailbin-npm-nubshim', 'ac-absent-python', 'ac-lifecycle',
  'ac-ungranted')
$script:JbOps = @(
  'cmd-runs', 'node-version', 'npm-version', 'npm-direct', 'python-version',
  'lc-start', 'lc-node-e', 'lifecycle',
  'interp-execpath', 'path-seen', 'read-ungranted', 'stat-c-root', 'read-system32-hosts')

function Invoke-JailBinVerdict {
  param([hashtable]$Cells, [hashtable]$Facts)
  $cells = $Cells
  if ($null -eq $Facts) { $Facts = @{} }
  function F([string]$k) { if ($Facts.ContainsKey($k)) { return $Facts[$k] } else { return $null } }

  W ''
  W '== operations table =='
  W ("  {0,-18} {1}" -f 'op', (($script:JbArms | ForEach-Object { "{0,-24}" -f $_ }) -join ''))
  foreach ($op in $script:JbOps) {
    W ("  {0,-18} {1}" -f $op, (($script:JbArms | ForEach-Object { "{0,-24}" -f (Cell $_ $op) }) -join ''))
  }
  W ''
  W '  launch status per arm:'
  foreach ($a in $script:JbArms) { W ("    {0,-24} {1}" -f $a, (LaunchOf $a)) }

  W ''
  W '== verdict =='

  # ── CONTROLS ────────────────────────────────────────────────────────────────────────────────────
  Prop 'jb-control-plain-jailbin-npm-runs' ((Cell 'plain-jailbin' 'npm-version') -eq 'OK') `
    "UNCONFINED, sanitized PATH = jail-bin only: the fixture itself must work, or every confined row measures a broken fixture: $(Cell 'plain-jailbin' 'npm-version') $(Detail 'plain-jailbin' 'npm-version')"
  Prop 'jb-control-plain-ambient-npm-runs' ((Cell 'plain-ambient' 'npm-version') -eq 'OK') `
    "UNCONFINED, PATH = the MSI install dir: §5j's 11.16.0 row, re-measured HERE so the confined ambient arm below is a differential inside this run: $(Cell 'plain-ambient' 'npm-version') $(Detail 'plain-ambient' 'npm-version')"
  # Every cell in this probe is resolved BY cmd.exe out of the sanitized PATH. If cmd cannot run
  # confined at all, a missing op is a dead shell and not a missing tool.
  Prop 'jb-control-cmd-runs-under-appcontainer' ((Cell 'ac-cmd-floor' 'cmd-runs') -eq 'OK') `
    "a confined cmd.exe /c echo must produce its line, else every PATH-resolved cell below is unreadable: $(Cell 'ac-cmd-floor' 'cmd-runs') launch=$(LaunchOf 'ac-cmd-floor')"
  $gateArms = @('ac-jailbin-node', 'ac-jailbin-npm-direct', 'ac-hybrid-npm-direct')
  $gateWhy = @(); $gateLive = $true
  foreach ($a in $gateArms) {
    $c = Cell $a 'stat-c-root'; $gateWhy += "$a=$c"
    if ($c -ne 'ERR') { $gateLive = $false }
  }
  Prop 'jb-control-appcontainer-gate-is-live' $gateLive `
    "every arm that reaches the table must be DENIED on C:\ ($($gateWhy -join ' ')) — else the token is not confined and a PASS means nothing"
  $posWhy = @(); $posOk = $true
  foreach ($a in $gateArms) {
    $c = Cell $a 'read-system32-hosts'; $posWhy += "$a=$c"
    if ($c -ne 'OK') { $posOk = $false }
  }
  Prop 'jb-control-appcontainer-gate-is-passable' $posOk `
    "System32 carries an ALL APPLICATION PACKAGES ace, so a confined child must still read it ($($posWhy -join ' ')) — this is what makes a column of ERRs a DACL statement rather than a dead child"
  Prop 'jb-control-child-saw-the-sanitized-path' `
    (((Cell 'ac-jailbin-node' 'path-seen') -eq 'OK') -and ((Cell 'ac-jailbin-npm-direct' 'path-seen') -eq 'OK')) `
    "the child must report a PATH containing the jail-bin dir and NOT the MSI install dir, or the environment block never took effect and 'resolved from jail-bin' is an assumption: node-arm=$(Detail 'ac-jailbin-node' 'path-seen') direct-arm=$(Detail 'ac-jailbin-npm-direct' 'path-seen')"
  Prop 'jb-control-ungranted-canary-exists' ((Cell 'plain-jailbin' 'read-ungranted') -eq 'OK') `
    "the UNCONFINED arm must READ the ungranted-sibling canary, or the denial below is a missing file rather than the grant: $(Cell 'plain-jailbin' 'read-ungranted') $(Detail 'plain-jailbin' 'read-ungranted')"
  Prop 'jb-control-grant-still-scopes' ((Cell 'ac-ungranted' 'read-ungranted') -eq 'ERR') `
    "an UNGRANTED sibling dir under the same fixture root must stay denied, or the grant is not doing the work: $(Cell 'ac-ungranted' 'read-ungranted') $(Detail 'ac-ungranted' 'read-ungranted')"

  # ── ATTRIBUTION: §5j's failure must still be present in THIS run ────────────────────────────────
  # EVERY inverted property below is gated on the cell being a REAL 'ERR', never merely 'not OK'.
  # `-ne 'OK'` would accept MISSING-ARM / MISSING-OP, so a treatment arm that never executed the
  # operation would report as a clean negative — which is exactly the bug §5k's run 1 shipped
  # ("a predicate that accepted MISSING-OP and reported this as a clean positive").
  # INVERTED READING: PASS means the ambient MSI npm is still broken, which is the premise the
  # mechanism exists to fix. A FAIL here does not mean jail-bin is unnecessary — it means this run
  # cannot attribute a jail-bin PASS to jail-bin.
  Prop 'jb-attribution-ambient-msi-npm-still-fails-confined' ((Cell 'ac-ambient' 'npm-version') -eq 'ERR') `
    "PASS = §5j reproduces in this run (confined + PATH = the MSI dir cannot run the bundled npm): $(Cell 'ac-ambient' 'npm-version') $(Detail 'ac-ambient' 'npm-version') launch=$(LaunchOf 'ac-ambient')"
  # THE ONE-VARIABLE ATTRIBUTION FOR THE MECHANISM. Identical command line and identical PATH to the
  # decisive arm below; the ONLY difference is that jail-bin carries no appcontainer ace. If this
  # PASSES too, the grant is not what makes the decisive arm work and the whole reading is wrong.
  # INVERTED READING: PASS here means the ungranted case is correctly DENIED.
  Prop 'jb-attribution-ungranted-jailbin-is-denied' ((Cell 'ac-jailbin-ungranted' 'npm-direct') -eq 'ERR') `
    "PASS = with the ace REVOKED the same command line fails, so a pass below is the grant's doing: $(Cell 'ac-jailbin-ungranted' 'npm-direct') $(Detail 'ac-jailbin-ungranted' 'npm-direct') launch=$(LaunchOf 'ac-jailbin-ungranted')"
  # BOTH halves: the revoke on the parent AND its propagation to the deep target. Checking only the
  # parent would miss an inherited ace surviving on the children, which would silently grant the arm.
  Prop 'jb-control-ungranted-arm-really-had-no-ace' `
    (((F 'ungranted-arm-aap-ace') -eq 'none') -and ((F 'ungranted-arm-deep-ace') -eq 'none | perrun=none')) `
    "the arm above is only a differential if the revoke actually landed AND propagated down — read back from the descriptor, both levels: jailbin-aap=$(F 'ungranted-arm-aap-ace') deep-file=$(F 'ungranted-arm-deep-ace')"

  # A SECOND, SHARPER FORM OF §5j, measured in run 30516752917 and not visible to §5j itself: with the
  # MSI install dir on the sanitized PATH, a confined child cannot even RESOLVE `node` there. A PATH
  # search is a directory PROBE and the un-ACE'd tree is not probeable, so the failure is cmd's
  # `'node' is not recognized` rather than Node's `Cannot find module`. §5j named node.exe by absolute
  # path and so never exercised it. INVERTED READING: PASS means the ambient dir on PATH is useless,
  # which is why the hybrid arm must name the interpreter absolutely.
  Prop 'jb-ambient-install-dir-is-not-even-path-resolvable' `
    (((Cell 'ac-ambient-on-path' 'npm-version') -eq 'ERR') -and
     ((F 'ambient-on-path-shape') -eq 'node-not-resolvable')) `
    "PASS = a confined child cannot resolve node OUT of the un-ACE'd MSI dir at all: shape=$(F 'ambient-on-path-shape') cell=$(Cell 'ac-ambient-on-path' 'npm-version')"

  # ── THE DECISIVE CELLS ──────────────────────────────────────────────────────────────────────────
  Prop 'jb-node-resolves-and-runs-from-jailbin' ((Cell 'ac-jailbin-node' 'node-version') -eq 'OK') `
    "confined, PATH = jail-bin only: cmd must resolve node there and it must print a version: $(Cell 'ac-jailbin-node' 'node-version') $(Detail 'ac-jailbin-node' 'node-version')"
  # THE READ QUESTION, isolated from the pipe question. No shell pipe anywhere in this command line.
  Prop 'jb-npm-tree-is-readable-from-jailbin' ((Cell 'ac-jailbin-npm-direct' 'npm-direct') -eq 'OK') `
    "confined node <jailbin>\node_modules\npm\bin\npm-cli.js --version — the cell §5j measured FAILING on the MSI tree: $(Cell 'ac-jailbin-npm-direct' 'npm-direct') $(Detail 'ac-jailbin-npm-direct' 'npm-direct')"
  # ⭐ The cell the whole design hinges on: ambient un-ACE-able interpreter, nub-owned npm tree.
  Prop 'jb-ambient-node-plus-owned-npm-tree-works' ((Cell 'ac-hybrid-npm-direct' 'npm-direct') -eq 'OK') `
    "PASS = the ~88 MiB node.exe does NOT need copying; only the ~12 MiB npm tree does. The interpreter is the AMBIENT MSI node.exe (no ace, execs per §5j) and only the npm tree is nub-owned: $(Cell 'ac-hybrid-npm-direct' 'npm-direct') $(Detail 'ac-hybrid-npm-direct' 'npm-direct')"
  # WHICH INTERPRETER ACTUALLY RAN. Without this the hybrid arm proves nothing: cmd could have
  # resolved jail-bin's own copy and the row would read identically. The two arms must DISAGREE.
  Prop 'jb-control-hybrid-really-ran-the-ambient-interpreter' `
    (((Detail 'ac-hybrid-npm-direct' 'interp-execpath') -like '*Program Files*') -and
     ((Detail 'ac-jailbin-npm-direct' 'interp-execpath') -notlike '*Program Files*')) `
    "the hybrid arm's process.execPath must be the MSI binary and the jail-bin arm's must not, or 'PATH order was the only variable' is an assumption: hybrid=$(Detail 'ac-hybrid-npm-direct' 'interp-execpath') jailbin=$(Detail 'ac-jailbin-npm-direct' 'interp-execpath')"
  # The zero-per-launch-cost design: a STABLE trustee, so the tree grant is written once at populate.
  Prop 'jb-stable-app-package-sid-grant-suffices' ((Cell 'ac-aap-only' 'npm-direct') -eq 'OK') `
    "PASS = an S-1-15-2-1 ace written ONCE at populate time is enough for a zero-capability LowBox token, so no per-launch ACE pass over 2,374 entries is needed: $(Cell 'ac-aap-only' 'npm-direct') $(Detail 'ac-aap-only' 'npm-direct')"
  Prop 'jb-control-aap-only-arm-really-withheld-the-per-run-sid' ((F 'aap-only-perrun-ace') -eq 'none') `
    "the arm above is only meaningful if the per-run package sid is ABSENT from the jail-bin DACL — read back rather than assumed: per-run-ace=$(F 'aap-only-perrun-ace') aap-ace=$(F 'aap-only-aap-ace')"

  # ── THE POPULATE ORDER, which is where the per-launch cost actually goes ────────────────────────
  # Grant the EMPTY dir, then copy into it, so all 2,374 npm entries pick the ace up AT CREATION
  # (§5e/§5h: the ace is `OICI`, flags=0x3). If that holds, the recursive pass every prior Windows
  # note measured at 0.15-0.28 ms/entry is not on the launch path at all — it is not on any path.
  Prop 'jb-inheritance-at-creation-covered-the-deep-file' `
    ((F 'inherited-ace-on-deep-npm-cli') -like 'ReadAndExecute*') `
    "the five-levels-down npm-cli.js must carry the ace with NO second grant pass, because it was created inside an already-granted dir: $(F 'inherited-ace-on-deep-npm-cli')"

  # ── STOCK npm.cmd vs a nub-authored shim: the pipe question ─────────────────────────────────────
  # Reported as facts-with-a-property rather than a pass/fail on the route, because either outcome is
  # actionable: if the stock shim hangs, nub must author its own; if it works, nub may reuse it.
  Fact 'stock-npm-cmd-confined' "$(Cell 'ac-jailbin-npm-stock' 'npm-version') launch=$(LaunchOf 'ac-jailbin-npm-stock')"
  Fact 'nub-authored-npm-shim-confined' "$(Cell 'ac-jailbin-npm-nubshim' 'npm-version') launch=$(LaunchOf 'ac-jailbin-npm-nubshim')"
  Prop 'jb-a-shim-without-the-prefix-probe-works' ((Cell 'ac-jailbin-npm-nubshim' 'npm-version') -eq 'OK') `
    "the shippable shape — a one-line .cmd that skips stock npm.cmd's FOR /F piped prefix probe: $(Cell 'ac-jailbin-npm-nubshim' 'npm-version') $(Detail 'ac-jailbin-npm-nubshim' 'npm-version')"

  # ── ABSENT TOOL: characterise the new failure shape ─────────────────────────────────────────────
  Prop 'jb-absent-tool-fails-as-a-resolution-error' `
    (((Cell 'ac-absent-python' 'python-version') -eq 'ERR') -and ((F 'absent-tool-shape') -eq 'not-recognized')) `
    "a tool nub did not put in jail-bin must fail as cmd's is not recognized (a RESOLUTION error), not as an access denial — that is what nub has to map to a useful message: shape=$(F 'absent-tool-shape') detail=$(Detail 'ac-absent-python' 'python-version')"

  # ── LIFECYCLE end to end ────────────────────────────────────────────────────────────────────────
  Prop 'jb-real-lifecycle-script-runs' ((Cell 'ac-lifecycle' 'lifecycle') -eq 'OK') `
    "a real .cmd lifecycle script (node -e + a node_modules/.bin-shaped call + npm) under the sanitized PATH: $(Cell 'ac-lifecycle' 'lifecycle') $(Detail 'ac-lifecycle' 'lifecycle')"

  # ── HARDLINK HAZARD ─────────────────────────────────────────────────────────────────────────────
  Prop 'jb-control-hardlink-was-actually-created' ((F 'hardlink-create') -eq 'OK') `
    "the user-owned hardlink must exist, or the leak cell below measured nothing: $(F 'hardlink-create')"
  # INVERTED READING: PASS means the leak is REAL, i.e. hard-linking is disqualified as a populate
  # strategy. Named for the claim, not the hoped-for outcome.
  Prop 'jb-hardlink-ace-leaks-to-the-original-path' ((F 'hardlink-leak') -eq $true) `
    "PASS = an ace written on the LINK is visible on the ORIGINAL path (one MFT record, one descriptor), so populating jail-bin by hard link would grant the confined child read on wherever the file also lives: original-dacl-after=$(F 'hardlink-original-ace') link-dacl-after=$(F 'hardlink-link-ace')"
  Fact 'hardlink-to-program-files-node' "$(F 'hardlink-pf-create') / ace-write=$(F 'hardlink-pf-ace-write') / ace-on-original=$(F 'hardlink-pf-original-ace')"

  # ── UNPRIVILEGED / ZERO-SETUP ───────────────────────────────────────────────────────────────────
  Prop 'jb-populate-and-grant-need-no-admin' ((F 'deelev-jailbin-grant') -eq 'OK') `
    "under an impersonated admin-STRIPPED token (privileges deleted, admin deny-only) the jail-bin ace write must SUCCEED — that is the zero-setup claim: write=$(F 'deelev-jailbin-grant') landed=$(F 'deelev-jailbin-landed') admin-under-impersonation=$(F 'deelev-admin') privs=$(F 'deelev-privs')"
  Prop 'jb-control-deelevated-principal-really-lacks-admin' ((F 'deelev-admin') -eq $false) `
    "the arm above proves nothing if the impersonated principal still holds admin: admin=$(F 'deelev-admin') privs=$(F 'deelev-privs')"

  # ── RESIDUE ─────────────────────────────────────────────────────────────────────────────────────
  Prop 'jb-probe-left-no-appcontainer-ace-on-program-files' ((F 'pf-residue') -eq 'none') `
    "no S-1-15-* ace may remain on C:\Program Files\nodejs: $(F 'pf-residue')"
}
