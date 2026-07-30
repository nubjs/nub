# The verdict for the non-local-volume traverse probe, factored out so `selftest.ps1` can drive it
# with SYNTHETIC cells and prove it discriminates in BOTH directions.
#
# THE QUESTION. MECHANISM-FACTS §5h measured that an AppContainer child reads five components deep
# under `%USERPROFILE%` with `C:\` and `C:\Users` holding no ace at all, and closed with: "NOT
# ESTABLISHED HERE: which mechanism does the traverse skip. `windows.rs:412-423` credits
# `SeChangeNotifyPrivilege` AND `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL` on the volume device;
# both predict this observable and this probe cannot separate them." The shipping backend's own
# comment then goes further and asserts the volume-flag reading ("Traverse would only be enforced on
# the rare device LACKING the volume flag"), which is exactly the untested part.
#
# The two hypotheses diverge on a device that does NOT carry the flag. So the probe runs the SAME
# deep-read shape on four surfaces — local `C:`, a freshly formatted VHD volume, a `subst`-mapped
# drive, and an SMB path — and separately deletes `SeChangeNotifyPrivilege` from the launching token,
# which is the one differential that separates the mechanisms without needing a second volume at all.
#
# THE THREE CONTROLS PER VOLUME, and what each rules out:
#
#   <vol>-plain        UNCONFINED child, same path. Must read. Rules out "the fixture is broken" and
#                      "this volume is not there".
#   <vol>-rootgrant    Confined child, inheritable ace at the VOLUME ROOT so NO ancestor is
#                      ungranted. Must read. This is the reachability control: it separates "the
#                      volume is unreachable under an AppContainer for some other reason" (network
#                      isolation on a UNC path, a device-map issue on subst) from "traverse is
#                      enforced here". Without it, a red deepgrant cell is uninterpretable.
#   <vol>-deepgrant    Confined child, ace ONLY on the deep directory, every ancestor ungranted.
#                      THE TREATMENT. Red here WITH a green rootgrant means traverse is enforced.
#
# plus, on every arm: the per-arm DACL read-back on the decisive target (a propagation slip must not
# be mistakable for a kernel denial) and an ungranted sibling on the same volume that must be denied
# (or the grant is scoping nothing and a green read means nothing).

. (Join-Path (Join-Path (Join-Path $PSScriptRoot '..') 'win-bypass-traverse') 'verdict.ps1')

# `local-c` is deliberately first: it is the KNOWN-GOOD arm from §5h re-measured in THIS run, so a
# failure on another volume is attributable to the volume rather than to the harness.
$script:VolNames = @('local-c', 'vhd', 'subst', 'unc')
$script:VolOps = @('dacl-grants-ac-sid', 'read-deep', 'stat-deep', 'readdir-deepdir', 'require-deep',
  'read-ungranted-sibling', 'walk-up-from-deepdir', 'stat-c-root', 'read-system32-hosts')

function Invoke-VolVerdict {
  param([hashtable]$Cells, [hashtable]$Facts)
  $cells = $Cells
  if ($null -eq $Facts) { $Facts = @{} }
  function F([string]$k) { if ($Facts.ContainsKey($k)) { return $Facts[$k] } else { return $null } }

  $arms = @()
  foreach ($v in $script:VolNames) { $arms += "$v-plain"; $arms += "$v-rootgrant"; $arms += "$v-deepgrant" }
  $arms += @('cn-kept', 'cn-deleted')

  W ''
  W '== operations table =='
  foreach ($op in $script:VolOps) {
    W ("  {0,-24}" -f $op)
    foreach ($a in $arms) { W ("      {0,-22} {1}  {2}" -f $a, (Cell $a $op), (Detail $a $op)) }
  }
  W ''
  W '  launch status per arm:'
  foreach ($a in $arms) { W ("    {0,-22} {1}" -f $a, (LaunchOf $a)) }

  W ''
  W '== verdict =='

  # ── THE ANCHOR. §5h's result, re-measured in this very run. If this is red the run says nothing
  # about any other volume, because the harness itself is not reproducing the known-good case.
  Prop 'anchor-local-c-deep-read-still-works' ((Cell 'local-c-deepgrant' 'read-deep') -eq 'OK') `
    "§5h's known-good case must reproduce HERE, or nothing else in this table is attributable to a volume: $(Cell 'local-c-deepgrant' 'read-deep') $(Detail 'local-c-deepgrant' 'read-deep')"
  Prop 'anchor-local-c-gate-is-live' ((Cell 'local-c-deepgrant' 'stat-c-root') -eq 'ERR') `
    "the anchor arm must be genuinely confined: stat-c-root=$(Cell 'local-c-deepgrant' 'stat-c-root')"
  Prop 'anchor-local-c-positive-control' ((Cell 'local-c-deepgrant' 'read-system32-hosts') -eq 'OK') `
    "…and the gate must be passable, so a red column is about DACLs: $(Cell 'local-c-deepgrant' 'read-system32-hosts')"

  # ── PER VOLUME. Every property is emitted for every volume even when the volume could not be
  # created: an absent volume fails `was-actually-tested` and is visibly a GAP rather than silently
  # a pass. A volume that could not be provisioned is a hole in the answer, not a green cell.
  foreach ($v in $script:VolNames) {
    Prop "$v-was-actually-tested" ((F "$v-available") -eq $true) `
      "the volume had to exist for its rows to mean anything: available=$(F "$v-available") detail=$(F "$v-why") root=$(F "$v-root")"
    Prop "$v-plain-baseline-reads-deep" ((Cell "$v-plain" 'read-deep') -eq 'OK') `
      "UNCONFINED child on the same path: $(Cell "$v-plain" 'read-deep') $(Detail "$v-plain" 'read-deep')"
    Prop "$v-reachable-when-whole-chain-granted" ((Cell "$v-rootgrant" 'read-deep') -eq 'OK') `
      "THE REACHABILITY CONTROL — ace at the volume root, no ungranted ancestor. Red here means the volume is unreachable under an AppContainer for a reason OTHER than traverse (network isolation, device map), and the deepgrant row below is then uninterpretable: $(Cell "$v-rootgrant" 'read-deep') $(Detail "$v-rootgrant" 'read-deep')"
    Prop "$v-grant-reached-the-deep-target" ((Cell "$v-deepgrant" 'dacl-grants-ac-sid') -eq 'OK') `
      "the deep file's own DACL must name the AC sid, or a denial is a propagation slip rather than a kernel answer: $(Detail "$v-deepgrant" 'dacl-grants-ac-sid')"
    Prop "$v-ungranted-sibling-denied" ((Cell "$v-deepgrant" 'read-ungranted-sibling') -eq 'ERR') `
      "a sibling on the SAME volume that never got a grant must fail, or the grant scopes nothing: $(Cell "$v-deepgrant" 'read-ungranted-sibling')"
    # THE TREATMENT CELL. Named for the claim, so PASS = the skip holds on this volume and the
    # shipping backend's assumption survives; FAIL = traverse is ENFORCED there, which is the
    # compat finding AND the mechanism discriminator.
    Prop "$v-bypass-traverse-holds" ((Cell "$v-deepgrant" 'read-deep') -eq 'OK') `
      "ace ONLY on the deep dir, every ancestor ungranted: read=$(Cell "$v-deepgrant" 'read-deep') stat=$(Cell "$v-deepgrant" 'stat-deep') require=$(Cell "$v-deepgrant" 'require-deep') $(Detail "$v-deepgrant" 'read-deep')"
  }

  # ── THE DEVICE FLAG, read rather than assumed. `windows.rs` credits
  # `FILE_DEVICE_ALLOW_APPCONTAINER_TRAVERSAL` on the volume device object; this reports the raw
  # Characteristics bitmask per volume plus the constant grepped out of the Windows SDK header on the
  # runner, so the decode is checked against the SDK and not against anyone's memory.
  W ''
  W '  device characteristics (raw, so the decode is auditable):'
  Fact 'sdk-constant-source' (F 'sdk-const-source')
  Fact 'sdk-constant-value' (F 'sdk-const-value')
  foreach ($v in $script:VolNames) {
    Fact "volchar/$v" "$(F "$v-volchar")  traverse-flag-set=$(F "$v-traverse-flag")"
  }
  # The flag hypothesis is only DISCRIMINATING if the flag actually varies across the volumes
  # measured. If every volume carries it, a uniformly green table cannot distinguish the mechanisms
  # and must not be read as if it had.
  Prop 'device-flag-varies-across-the-volumes-measured' ((F 'traverse-flag-varies') -eq $true) `
    "PASS means at least one measured volume LACKS the flag, so the volume rows can discriminate; FAIL means they cannot and only the privilege differential below can: values=$(F 'traverse-flag-summary')"

  # ── THE PRIVILEGE DIFFERENTIAL. The clean mechanism separator, and it needs no second volume:
  # relaunch the ANCHOR arm from a `CreateRestrictedToken` with `SeChangeNotifyPrivilege` DELETED. If
  # the skip is the privilege, the deep read must now fail on the very volume where `cn-kept`
  # succeeds. If it still succeeds, the privilege is not what carries it.
  Prop 'cn-control-restricted-launch-path-works' ((Cell 'cn-kept' 'read-deep') -eq 'OK') `
    "the SAME CreateProcessAsUser+restricted-token launch WITH the privilege retained must read deep, or a failure below is about the launch mechanism rather than the privilege: $(Cell 'cn-kept' 'read-deep') launch=$(LaunchOf 'cn-kept')"
  Prop 'cn-control-kept-token-really-holds-the-privilege' ((F 'cn-kept-has-privilege') -eq $true) `
    "read back off the process that ACTUALLY ran: $(LaunchOf 'cn-kept')"
  Prop 'cn-control-deleted-token-really-lacks-the-privilege' ((F 'cn-deleted-has-privilege') -eq $false) `
    "…and the treatment token must genuinely not hold it, or the differential varied nothing: $(LaunchOf 'cn-deleted')"
  # INVERTED READING: PASS here means SeChangeNotifyPrivilege IS the mechanism.
  Prop 'deleting-changenotify-breaks-the-deep-read' ((Cell 'cn-deleted' 'read-deep') -ne 'OK') `
    "PASS means the traverse skip is the PRIVILEGE (and any token lacking it loses deep reads); FAIL means the privilege is not what carries it and the volume flag is the remaining candidate: kept=$(Cell 'cn-kept' 'read-deep') deleted=$(Cell 'cn-deleted' 'read-deep')"

  return $script:fails
}
