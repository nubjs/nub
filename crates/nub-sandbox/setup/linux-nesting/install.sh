#!/usr/bin/env bash
#
# One-time administrator setup for Nub's Linux direct-nesting sandbox.
#
# Nub NEVER runs this script itself. Direct sandbox composition (a sandboxed
# process launching a stricter sandbox inside itself) needs nested unprivileged
# user namespaces, which Ubuntu's default AppArmor policy blocks. This script
# opts a dedicated, root-owned copy of the packaged Bubblewrap back into that
# capability for one exact path, and authorizes one group to run it.
#
# Read README.md (the Security tradeoff section) before running this. In short:
# members of the nub-sandbox group can execute a helper that creates unprivileged
# user namespaces — the same kernel attack surface the distribution restricts by
# default. The global restriction is left ENABLED for every other executable;
# this script never disables it.
#
# Usage:
#   sudo ./install.sh --bwrap <path-to-packaged-bwrap> [--user <login>] [--sha256 <hex>]
#
#   --bwrap <path>   The unmodified packaged Bubblewrap resource that ships with
#                    Nub (bin/nub-resources/bwrap in an installed Nub). Required.
#   --user <login>   The login to add to the nub-sandbox group. Defaults to
#                    $SUDO_USER (the human who invoked sudo).
#   --sha256 <hex>   Expected digest of the packaged Bubblewrap (nub's
#                    NUB_BWRAP_SHA256). When given, the source is hard-verified
#                    against it before install; otherwise the digest is printed
#                    for the admin to compare.
#
# The script is idempotent: re-running it re-verifies ownership, permissions,
# group membership, and the loaded profile, repairing only what drifted.

set -euo pipefail

HELPER_PATH=/usr/libexec/nub/nub-bwrap
HELPER_GROUP=nub-sandbox
PROFILE_NAME=nub-bwrap-userns
PROFILE_DEST="/etc/apparmor.d/${PROFILE_NAME}"
SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PROFILE_SRC="${SCRIPT_DIR}/nub-bwrap-userns.apparmor"

bwrap_src=""
target_user="${SUDO_USER:-}"
expected_sha256=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bwrap)  bwrap_src="${2:-}"; shift 2 ;;
    --user)   target_user="${2:-}"; shift 2 ;;
    --sha256) expected_sha256="${2:-}"; shift 2 ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

fail() { echo "error: $*" >&2; exit 1; }

[[ $(id -u) -eq 0 ]] || fail "run this script as root (sudo)."
[[ -n "$bwrap_src" ]] || fail "pass --bwrap <path-to-packaged-bwrap>."
[[ -f "$bwrap_src" ]] || fail "packaged Bubblewrap not found: $bwrap_src"
[[ -f "$PROFILE_SRC" ]] || fail "AppArmor profile not found next to this script: $PROFILE_SRC"

# Verify the source really is upstream Bubblewrap 0.11.2 before installing it.
src_version=$("$bwrap_src" --version 2>/dev/null || true)
[[ "$src_version" == "bubblewrap 0.11.2" ]] \
  || fail "expected the packaged 'bubblewrap 0.11.2', got: ${src_version:-<none>}"

# Pin the exact bytes. nub verifies the installed helper against the digest it was
# built with, so a same-version-string but different-bytes build would install here
# and then fail nub's runtime integrity gate with a cryptic error. Fail loudly now.
# Pass --sha256 <hex> (nub's NUB_BWRAP_SHA256) to hard-verify; otherwise the digest
# is printed for the admin to compare.
src_sha256=$(sha256sum "$bwrap_src" | awk '{print $1}')
if [[ -n "$expected_sha256" ]]; then
  [[ "$src_sha256" == "$expected_sha256" ]] \
    || fail "packaged Bubblewrap digest ${src_sha256} does not match --sha256 ${expected_sha256}"
fi

command -v apparmor_parser >/dev/null 2>&1 \
  || fail "apparmor_parser is not available; this host does not run AppArmor."

# 1. Group.
if getent group "$HELPER_GROUP" >/dev/null; then
  echo "group ${HELPER_GROUP} already exists"
else
  groupadd --system "$HELPER_GROUP"
  echo "created group ${HELPER_GROUP}"
fi

# 2. Membership (a FRESH LOGIN is required for it to take effect).
if [[ -n "$target_user" ]]; then
  if id -nG "$target_user" 2>/dev/null | tr ' ' '\n' | grep -qx "$HELPER_GROUP"; then
    echo "user ${target_user} already in ${HELPER_GROUP}"
  else
    gpasswd --add "$target_user" "$HELPER_GROUP" >/dev/null
    echo "added ${target_user} to ${HELPER_GROUP} (log out and back in for it to apply)"
  fi
else
  echo "no --user given; add members later with: gpasswd --add <login> ${HELPER_GROUP}"
fi

# 3. Helper: root-owned, group-executable, NOT setuid. install(1) is atomic.
install -D -o root -g "$HELPER_GROUP" -m 0750 "$bwrap_src" "$HELPER_PATH"
echo "installed helper at ${HELPER_PATH} (root:${HELPER_GROUP} 0750)"
echo "helper sha256: ${src_sha256}"

# 4. Profile: path-bound userns grant for the helper only. Leaves the global
#    apparmor_restrict_unprivileged_userns control untouched.
install -D -o root -g root -m 0644 "$PROFILE_SRC" "$PROFILE_DEST"
apparmor_parser --replace "$PROFILE_DEST"
echo "loaded AppArmor profile ${PROFILE_NAME}"

restrict=$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo "?")
echo
echo "done. The global unprivileged-user-namespace restriction is left as-is"
echo "(apparmor_restrict_unprivileged_userns=${restrict}); only ${HELPER_PATH}"
echo "is opted back in, and only for ${HELPER_GROUP} members."
if [[ -n "$target_user" ]]; then
  echo "If ${target_user} was just added to the group, start a fresh login now."
fi
