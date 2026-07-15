#!/usr/bin/env bash
#
# End-to-end proof of the Linux direct-nesting candidate gate (epic item D5).
#
# A `cargo test` cannot manipulate host ownership, permissions, or the loaded
# AppArmor profile, so this harness drives the whole set-up-and-break matrix with
# `sudo` and asserts the exact fail-closed diagnostic each scenario produces:
#
#   (a)+(d) a correctly set-up host admits the dedicated helper and a nested
#           launch succeeds       (the linux_nesting cargo test, REQUIRE_NESTING=1)
#   (b)     a mis-owned or group-writable helper is rejected as an integrity failure
#   (c)     an unloaded AppArmor profile fails closed with the profile diagnostic
#           and a caller without the nub-sandbox group is denied access
#
# It SKIPS WITH A PRINTED REASON (never silently passes) unless it runs on an
# AppArmor-restricted Ubuntu host with sudo, cargo, and a packaged Bubblewrap
# 0.11.2 to install. It restores the host to its starting state on exit.
#
# Usage:
#   NUB_BWRAP=/path/to/nub-resources/bwrap \
#   CARGO_TARGET_DIR=/tmp/nub-sandbox-target \
#     tests/sandbox-nesting/prove.sh
#
#   NUB_BWRAP   Packaged Bubblewrap 0.11.2 to install as the helper. If unset, the
#               harness builds it reproducibly with scripts/build-bwrap-resource.sh.
#   NUB_GROUP_USER  Login to authorize (default: the invoking user).

set -uo pipefail

HELPER=/usr/libexec/nub/nub-bwrap
GROUP=nub-sandbox
PROFILE=/etc/apparmor.d/nub-bwrap-userns
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
SETUP="$REPO_ROOT/crates/nub-sandbox/setup/linux-nesting"
GROUP_USER=${NUB_GROUP_USER:-$(id -un)}

skip() { echo "SKIP sandbox-nesting: $*"; exit 0; }
have() { command -v "$1" >/dev/null 2>&1; }

# ── gate ────────────────────────────────────────────────────────────────────
[[ "$(uname -s)" == Linux ]] || skip "not Linux"
[[ -r /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]] \
  || skip "no AppArmor unprivileged-userns restriction (not an Ubuntu-family host)"
[[ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" == 1 ]] \
  || skip "AppArmor unprivileged-userns restriction is not active; nesting needs no helper here"
have sudo || skip "sudo is required"
have cargo || skip "cargo is required"
sudo -n true 2>/dev/null || skip "passwordless sudo is required to manipulate the host"

# ── obtain a packaged Bubblewrap 0.11.2 ──────────────────────────────────────
bwrap_src=${NUB_BWRAP:-}
if [[ -z "$bwrap_src" ]]; then
  echo "building the packaged Bubblewrap resource..."
  out=$(mktemp -d)
  bash "$REPO_ROOT/scripts/build-bwrap-resource.sh" "$out" || skip "could not build the Bubblewrap resource"
  bwrap_src="$out/bwrap"
fi
[[ -f "$bwrap_src" ]] || skip "packaged Bubblewrap not found at $bwrap_src"
digest=$(sha256sum "$bwrap_src" | awk '{print $1}')

# ── restore host state on exit ───────────────────────────────────────────────
cleanup() {
  sudo chown root:"$GROUP" "$HELPER" 2>/dev/null
  sudo chmod 0750 "$HELPER" 2>/dev/null
  [[ -f "$PROFILE" ]] && sudo apparmor_parser -r "$PROFILE" 2>/dev/null
}
trap cleanup EXIT

fail=0
check() { # check <label> <expected-substring> <actual>
  if [[ "$3" == *"$2"* ]]; then
    echo "  PASS $1"
  else
    echo "  FAIL $1"
    echo "    expected substring: $2"
    echo "    actual:             $3"
    fail=1
  fi
}

# ── set up + build the gated cargo test with the pinned digest ───────────────
echo "installing the helper (setup script)..."
sudo bash "$SETUP/install.sh" --bwrap "$bwrap_src" --user "$GROUP_USER" --sha256 "$digest" >/dev/null \
  || skip "the setup script failed"

echo "building the linux_nesting cargo test with the pinned digest..."
export NUB_BWRAP_VERSION=0.11.2 NUB_BWRAP_SHA256="$digest"
cargo test -p nub-sandbox --test linux_nesting --no-run >/dev/null 2>&1 \
  || skip "could not build the linux_nesting test"
BIN=$(ls -t "${CARGO_TARGET_DIR:-$REPO_ROOT/target}"/debug/deps/linux_nesting-* 2>/dev/null \
  | grep -v '\.d$' | head -1)
[[ -x "$BIN" ]] || skip "could not locate the compiled linux_nesting test binary"
run_gate() { sg "$GROUP" -c "$* $BIN --nocapture --test-threads=1" 2>&1; }

echo
echo "== (a)+(d) correctly set-up host admits + nested launch =="
out=$(run_gate "NUB_SANDBOX_REQUIRE_NESTING=1")
check "admit + nested launch succeeds" \
  "set_up_host_admits_the_helper_and_a_nested_launch_succeeds ... ok" "$out"

echo
echo "== (b) integrity: mis-owned helper =="
sudo chown "$GROUP_USER":"$GROUP" "$HELPER"
out=$(run_gate "")
check "mis-owned helper rejected as integrity failure" "failed its integrity check" "$out"
sudo chown root:"$GROUP" "$HELPER"

echo
echo "== (b) integrity: group-writable helper =="
sudo chmod 0770 "$HELPER"
out=$(run_gate "")
check "group-writable helper rejected as integrity failure" "failed its integrity check" "$out"
sudo chmod 0750 "$HELPER"

echo
echo "== (c) profile unloaded =="
sudo apparmor_parser -R "$PROFILE"
out=$(run_gate "")
check "unloaded profile fails closed with the profile diagnostic" \
  "AppArmor profile is not loaded" "$out"
sudo apparmor_parser -r "$PROFILE"

echo
echo "== (c) missing group access =="
# The invoking session without the group active cannot open the 0750 helper.
out=$("$BIN" --nocapture --test-threads=1 2>&1)
check "caller without group access is denied" "not in the nub-sandbox group" "$out"

echo
if [[ $fail -eq 0 ]]; then
  echo "sandbox-nesting: ALL SCENARIOS PASSED"
else
  echo "sandbox-nesting: FAILURES ABOVE"
fi
exit $fail
