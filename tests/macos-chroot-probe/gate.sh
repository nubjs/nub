#!/bin/bash
# Is the AMFI chroot gate ARMED on this runner, and can ad-hoc re-signing defeat it?
#
# Run 1 measured macos-latest as SIP-DISABLED, which makes it useless for this question:
# with the gate unarmed every cell passes and the differential goes flat. This script's FIRST
# job is therefore to classify the test bed; the gate result is only meaningful when
# `csrutil status` reports enabled. Swept across runner labels to find any SIP-on image.

set -u
export LC_ALL=C
W=/tmp/gate; mkdir -p "$W"
run() { echo "\$ $*"; "$@"; local rc=$?; echo "[exit $rc]"; return $rc; }
sub() { printf '\n----- %s -----\n' "$*"; }

sub "test bed"
run sw_vers
run uname -a
SIP="$(csrutil status 2>&1)"; echo "csrutil status: $SIP"
run csrutil authenticated-root status
run nvram boot-args
sysctl -a 2>/dev/null | grep -E 'amfi\.(launch_constraints_enforced|allow_only_platform_code|unsigned_code_policy)|vm\.cs_(process_enforcement|library_validation|system_enforcement)'

VALID=no
case "$SIP" in *enabled*) VALID=yes ;; esac
echo "SIP_ENABLED=$VALID"

sub "gate: hardened-runtime binary inside chroot — the exact thing AMFI kills"
# A locally-compiled binary carries no Apple identity, so launch constraints cannot apply.
# Varying ONLY --options runtime isolates CS_RUNTIME. If hello_hard runs inside chroot, the
# gate is NOT armed on this machine and every other cell below is uninformative.
printf '#include <stdio.h>\nint main(){puts("ran");return 0;}\n' > "$W/h.c"
clang -o "$W/h_plain" "$W/h.c"
cp "$W/h_plain" "$W/h_hard"; codesign -f -s - --options runtime "$W/h_hard" 2>&1
run sudo chroot / "$W/h_hard"; HARD=$?
echo "GATE_ARMED=$([ $HARD -eq 0 ] && echo no || echo yes)"

sub "step 1 — untouched Apple platform binary in chroot"
run sudo chroot / /bin/echo hello; S1=$?

sub "step 2 — THE BALLGAME: ad-hoc re-signed Apple binary in chroot"
cp /bin/echo "$W/echo_plain"
cp /bin/echo "$W/echo_adhoc"; codesign -f -s - "$W/echo_adhoc" 2>&1
run "$W/echo_plain" x;              A=$?   # launch-constraint wall, outside chroot
run "$W/echo_adhoc" x;              B=$?
run sudo chroot / "$W/echo_plain" x;  C=$?
run sudo chroot / "$W/echo_adhoc" x;  D=$?

sub "AMFI log"
sudo log show --last 2m --style compact --predicate \
  'eventMessage CONTAINS "chroot" OR eventMessage CONTAINS "not allowed"' 2>&1 \
  | grep -iE 'chroot|amfi' | grep -v 'log run noninteractively' | tail -20

sub "SUMMARY"
cat <<EOF
macOS            : $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m)
kernel           : $(uname -v | sed 's/.*root://;s/ .*//')
SIP enabled      : $VALID
gate armed       : $([ $HARD -eq 0 ] && echo "NO  (hardened binary ran in chroot => results below are uninformative)" || echo "YES (hardened binary killed in chroot)")
step1 apple/chroot            rc=$S1
step2 [plain |outside]        rc=$A
step2 [adhoc |outside]        rc=$B
step2 [plain |INSIDE ]        rc=$C
step2 [adhoc |INSIDE ] BALL   rc=$D
EOF
exit 0
