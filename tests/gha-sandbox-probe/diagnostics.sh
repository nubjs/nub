#!/usr/bin/env bash
# Environment diagnostics + baseline sandbox capability probe for a hosted GHA
# runner. Prints everything unconditionally (never aborts), then records:
#   userns-smoke            plain unprivileged user+net namespace
#   custom-apparmor-load     can an admin load a trivial custom AppArmor profile?
#   nub-apparmor-load        can the ACTUAL nub-bwrap-userns path-bound profile load?
#   single-level             stock apt bwrap, single sandbox
#   nested-baseline          stock apt bwrap, NESTED — no workaround (the D5 problem)
set +e
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$SELF_DIR/lib.sh"

hdr "KERNEL / OS"
uname -a
cat /etc/os-release 2>/dev/null | sed -n '1,6p'
detail "runner-image" "${ImageOS:-?} / ${ImageVersion:-?}"

hdr "USERNS + APPARMOR SYSCTLS"
for k in kernel.apparmor_restrict_unprivileged_userns \
         kernel.apparmor_restrict_unprivileged_userns_complain \
         kernel.apparmor_restrict_unprivileged_userns_force \
         kernel.unprivileged_userns_clone; do
  v="$(sysctl -n "$k" 2>/dev/null)"; rc=$?
  if [[ $rc -eq 0 ]]; then printf '%s = %s\n' "$k" "$v"; detail "sysctl:$k" "$v"
  else printf '%s = <absent>\n' "$k"; detail "sysctl:$k" "<absent>"; fi
done
printf 'max_user_namespaces = %s\n' "$(cat /proc/sys/user/max_user_namespaces 2>/dev/null)"
printf 'kernel lockdown = %s\n' "$(cat /sys/kernel/security/lockdown 2>/dev/null || echo '<absent>')"

hdr "APPARMOR STATUS"
sudo aa-status 2>&1 | head -40 || aa-status 2>&1 | head -40 || echo "aa-status unavailable"
apparmor_parser --version 2>&1 | head -1 || echo "apparmor_parser absent"
echo "--- bwrap-related profiles ---"
sudo aa-status 2>/dev/null | grep -iE 'bwrap|bubblewrap|unpriv' || echo "(none named bwrap/unpriv)"

hdr "DMESG APPARMOR TAIL"
sudo dmesg 2>/dev/null | grep -iE 'apparmor|userns|namespace' | tail -15 || echo "(dmesg unreadable or empty)"

# ---- TIER 1: plain unprivileged userns smoke -------------------------------
hdr "TIER 1: unprivileged userns smoke (unshare -Urn true)"
out="$(unshare -Urn true 2>&1)"; rc=$?
if [[ $rc -eq 0 ]]; then emit userns-smoke PASS; detail userns-smoke "unshare -Urn true rc=0"
else emit userns-smoke FAIL; detail userns-smoke "rc=$rc :: ${out:-<no output>}"; fi

# ---- TIER 2a: load a TRIVIAL custom AppArmor profile -----------------------
hdr "TIER 2a: load a trivial custom AppArmor profile"
if command -v apparmor_parser >/dev/null 2>&1; then
  tp="$(mktemp)"
  cat > "$tp" <<'EOF'
abi <abi/3.0>,
include <tunables/global>
profile nub_probe_trivial /usr/libexec/nub-probe/does-not-exist flags=(unconfined) {
  userns,
}
EOF
  out="$(sudo apparmor_parser --replace --skip-cache "$tp" 2>&1)"; rc=$?
  if [[ $rc -eq 0 ]]; then emit custom-apparmor-load PASS; detail custom-apparmor-load "trivial abi/3.0 profile loaded"
  else emit custom-apparmor-load FAIL; detail custom-apparmor-load "rc=$rc :: ${out:-<no output>}"; fi
  sudo apparmor_parser --remove "$tp" 2>/dev/null || true
  rm -f "$tp"
else
  emit custom-apparmor-load SKIP; detail custom-apparmor-load "apparmor_parser absent"
fi

# ---- TIER 2b: load the ACTUAL nub path-bound userns profile ----------------
hdr "TIER 2b: load the REAL nub-bwrap-userns.apparmor (abi/4.0)"
NUB_PROFILE="$SELF_DIR/../../crates/nub-sandbox/setup/linux-nesting/nub-bwrap-userns.apparmor"
if [[ -f "$NUB_PROFILE" ]] && command -v apparmor_parser >/dev/null 2>&1; then
  sudo install -D -m 0644 "$NUB_PROFILE" /etc/apparmor.d/nub-bwrap-userns
  out="$(sudo apparmor_parser --replace /etc/apparmor.d/nub-bwrap-userns 2>&1)"; rc=$?
  if [[ $rc -eq 0 ]]; then emit nub-apparmor-load PASS; detail nub-apparmor-load "nub-bwrap-userns (abi/4.0) loaded"
  else emit nub-apparmor-load FAIL; detail nub-apparmor-load "rc=$rc :: ${out:-<no output>}"; fi
  sudo apparmor_parser --remove /etc/apparmor.d/nub-bwrap-userns 2>/dev/null || true
  sudo rm -f /etc/apparmor.d/nub-bwrap-userns
else
  emit nub-apparmor-load SKIP; detail nub-apparmor-load "profile file or apparmor_parser missing"
fi

# ---- TIER 3: single-level stock bwrap --------------------------------------
hdr "TIER 3: single-level stock apt bwrap"
BW="$(apt_bwrap)"
detail bwrap-path "$BW"; detail bwrap-version "$("$BW" --version 2>&1 || echo '?')"
out="$(run_single "$BW")"; rc=$?
if [[ $rc -eq 0 ]]; then emit single-level PASS; detail single-level "stock bwrap single sandbox rc=0"
else emit single-level FAIL; detail single-level "rc=$rc :: ${out:-<no output>}"; fi

# ---- TIER 4: NESTED stock bwrap, NO workaround (the D5 problem) -------------
hdr "TIER 4: nested stock bwrap, no workaround"
judge_nested nested-baseline "$BW" "$BW"

hdr "SUMMARY (grep RESULT:)"
