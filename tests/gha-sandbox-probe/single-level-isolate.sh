#!/usr/bin/env bash
# Isolate WHY single-level bwrap behaves as it does on this image: separate the
# userns-creation gate from the loopback/network setup, and show the loaded
# AppArmor profile state AFTER bwrap is installed. Optional arg: a workaround to
# apply first (sysctl-disable | none).
set +e
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SELF_DIR/lib.sh"
WA="${1:-none}"

if [[ "$WA" == "sysctl-disable" ]]; then
  hdr "APPLY sysctl-disable"
  sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
fi

BW="$(apt_bwrap)"
hdr "STATE AFTER bwrap INSTALL"
"$BW" --version
echo "restrict = $(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null || echo '<absent>')"
echo "--- loaded apparmor profiles mentioning bwrap/bubblewrap ---"
sudo aa-status 2>/dev/null | grep -iE 'bwrap|bubblewrap' || echo "(NONE — no bwrap apparmor profile is loaded)"
echo "--- dpkg files that look like an apparmor profile for bwrap ---"
dpkg -L bubblewrap 2>/dev/null | grep -iE 'apparmor' || echo "(bubblewrap package ships no apparmor profile)"
ls -l /etc/apparmor.d/ 2>/dev/null | grep -iE 'bwrap|bubblewrap' || echo "(no /etc/apparmor.d entry for bwrap)"

run_variant() {
  local key="$1"; shift
  local out rc
  out="$("$BW" "$@" 2>&1)"; rc=$?
  if [[ $rc -eq 0 ]]; then emit "$key" PASS; detail "$key" "rc=0"
  else emit "$key" FAIL; detail "$key" "rc=$rc :: ${out:-<no output>}"; fi
}

hdr "VARIANT A: userns only, NO net (nub's exact bwrap_available check)"
run_variant sl-A-userns-nonet --die-with-parent --unshare-user --ro-bind / / -- /bin/true

hdr "VARIANT B: userns + net, bwrap auto-loopback"
run_variant sl-B-userns-net --die-with-parent --unshare-user --unshare-net --ro-bind / / -- /bin/true

hdr "VARIANT C: userns + net + proc/dev (my original diagnostics args)"
run_variant sl-C-userns-net-full --die-with-parent --unshare-user --unshare-net --unshare-pid --ro-bind / / --proc /proc --dev /dev -- /bin/true

hdr "VARIANT D: raw unshare userns only (no net)"
out="$(unshare -U -r true 2>&1)"; rc=$?
if [[ $rc -eq 0 ]]; then emit sl-D-raw-unshare-U PASS; detail sl-D-raw-unshare-U "rc=0"
else emit sl-D-raw-unshare-U FAIL; detail sl-D-raw-unshare-U "rc=$rc :: ${out:-<no output>}"; fi

hdr "DMESG apparmor DENIED tail (post-run)"
sudo dmesg 2>/dev/null | grep -iE 'DENIED|userns|apparmor' | tail -12 || echo "(none)"
