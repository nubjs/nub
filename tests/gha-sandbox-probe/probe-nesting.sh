#!/usr/bin/env bash
# Apply ONE candidate workaround, then test whether a NESTED bwrap launch
# succeeds. Each mode runs in its OWN fresh hosted-runner job so there is zero
# cross-contamination between workarounds.
#
# Usage: probe-nesting.sh <mode>
#   sysctl-disable   sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0   (A)
#   complain-knob    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns_complain=1 (B)
#   aa-complain      neutralize the stock bwrap AppArmor profile (aa-complain / unload) (C)
#   setuid           chown root:root + chmod u+s /usr/bin/bwrap, restriction left ON (D)
#   d5-helper        raw replication of the D5 dedicated-helper + path-bound profile
#   baseline         no workaround (control; expected to FAIL where 24.04 blocks nesting)
set +e
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SELF_DIR/lib.sh"

MODE="${1:?usage: probe-nesting.sh <mode>}"
KEY="nested-$MODE"
BW="$(apt_bwrap)"
detail bwrap-path "$BW"
detail bwrap-version "$("$BW" --version 2>&1 || echo '?')"

hdr "PRE-STATE"
echo "restrict = $(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo '<absent>')"
sudo aa-status 2>/dev/null | grep -iE 'bwrap|bubblewrap' || echo "(no bwrap apparmor profile)"

hdr "APPLY WORKAROUND: $MODE"
case "$MODE" in
  baseline)
    echo "no workaround applied (control)"
    ;;

  sysctl-disable) # (A)
    out="$(sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 2>&1)"; rc=$?
    echo "$out"
    if [[ $rc -ne 0 ]]; then
      emit "$KEY" SKIP; detail "$KEY" "sysctl knob absent/unsettable: $out"; exit 0
    fi
    detail workaround-applied "restrict=$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null)"
    ;;

  complain-knob) # (B)
    out="$(sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns_complain=1 2>&1)"; rc=$?
    echo "$out"
    if [[ $rc -ne 0 ]]; then
      emit "$KEY" SKIP; detail "$KEY" "complain knob absent: $out"; exit 0
    fi
    detail workaround-applied "complain=$(sysctl -n kernel.apparmor_restrict_unprivileged_userns_complain 2>/dev/null)"
    ;;

  aa-complain) # (C) neutralize the stock bwrap profile
    # The stock 24.04 profile is usually named 'unpriv_bwrap' attached to /usr/bin/bwrap.
    applied=""
    if command -v aa-complain >/dev/null 2>&1; then
      out="$(sudo aa-complain /usr/bin/bwrap 2>&1)"; echo "$out"
      [[ $? -eq 0 ]] && applied="aa-complain /usr/bin/bwrap"
    fi
    if [[ -z "$applied" ]]; then
      # Fallback: unload every loaded profile whose name mentions bwrap.
      for prof in $(sudo aa-status 2>/dev/null | grep -iE 'bwrap' | awk '{print $1}'); do
        if [[ -f "/etc/apparmor.d/$prof" ]]; then
          sudo apparmor_parser -R "/etc/apparmor.d/$prof" 2>&1 && applied="unloaded $prof"
        fi
      done
    fi
    if [[ -z "$applied" ]]; then
      emit "$KEY" SKIP; detail "$KEY" "no bwrap apparmor profile to neutralize"; exit 0
    fi
    detail workaround-applied "$applied"
    ;;

  setuid) # (D) restriction LEFT ON, setuid-root bwrap sidesteps unprivileged userns
    echo "restrict left at: $(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo '<absent>')"
    sudo chown root:root "$BW" && sudo chmod u+s "$BW"
    ls -l "$BW"
    detail workaround-applied "setuid $BW ($(stat -c '%A %U:%G' "$BW"))"
    ;;

  d5-helper) # raw replication of the real D5 setup, minus nub's version pin
    HELPER=/usr/libexec/nub/nub-bwrap
    sudo install -D -o root -g root -m 0755 "$BW" "$HELPER"
    PROF="$SELF_DIR/../../crates/nub-sandbox/setup/linux-nesting/nub-bwrap-userns.apparmor"
    sudo install -D -m 0644 "$PROF" /etc/apparmor.d/nub-bwrap-userns
    out="$(sudo apparmor_parser --replace /etc/apparmor.d/nub-bwrap-userns 2>&1)"; rc=$?
    echo "profile load rc=$rc :: $out"
    if [[ $rc -ne 0 ]]; then
      emit "$KEY" FAIL; detail "$KEY" "nub path-bound profile FAILED to load: $out"; exit 0
    fi
    detail workaround-applied "helper=$HELPER, nub-bwrap-userns loaded"
    # Nested via the helper as OUTER, stock bwrap as INNER (nub's actual topology).
    hdr "NESTED via D5 helper (outer=$HELPER inner=$BW)"
    judge_nested "$KEY" "$HELPER" "$BW"
    exit 0
    ;;

  *) echo "unknown mode: $MODE" >&2; exit 2 ;;
esac

hdr "NESTED bwrap after workaround (outer=inner=$BW)"
judge_nested "$KEY" "$BW" "$BW"
