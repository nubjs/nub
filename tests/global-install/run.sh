#!/usr/bin/env bash
# End-to-end guard for `nub install -g` / `nub remove -g` bin handling.
#
# The unit tests in `vendor/aube/.../commands/global.rs` cover the ownership
# LOGIC against constructed fixtures. This covers the INTEGRATION: a real
# registry install, the real content store, the real symlink shapes the linker
# produces, and the resolved on-disk layout. Three of the four defects below
# were invisible to unit tests because they only appear once a package has
# actually been installed.
#
# What it guards, and the defect each case exists for:
#
#   1. `remove -g` left every bin it linked behind in the global bin dir.
#      Ownership was decided by testing whether the bin symlink resolved under
#      the install dir, but under the isolated layout `node_modules/<alias>` is
#      a symlink into the shared content store, so it resolves OUT of the
#      install dir and the test never matched.
#   2. `add -g` over an overlapping alias set deleted the bin it had just
#      linked. Bins are linked BEFORE priors are torn down (deliberately — a
#      crash must leave a working copy), and two installs of one package at one
#      version share a content-store path, so the prior's recorded target
#      matched the live link.
#   3. `install -g` silently replaced a binary it did not own. The global bin
#      dir is shared with every other tool that installs there.
#   4. Bins landed in a pnpm-named directory (`~/.local/share/pnpm`,
#      `~/Library/pnpm`) that nothing puts on PATH, and the install reported
#      success anyway. nubjs/nub#642, reported twice.
#
# HOW THIS SCRIPT LIES TO YOU, if you let it. Every check here reads "clean"
# when the command under test never ran, so a failed install prints a screen of
# passes. Five separate vacuous passes were found while writing it:
#
#   - Run from a checkout with two lockfiles, `install -g` aborts with
#     ERR_NUB_LOCKFILE_AMBIGUOUS before doing any global work. Hence the `cd`
#     to an empty directory.
#   - A second package with a native dependency (`lolcatjs` -> `sleep`) fails
#     its node-gyp `install` script on current Node, the cleanup guard rolls
#     the install back, and the teardown path never runs. Hence `semver`.
#   - `printf > "$BIN_DIR/<name>"` FOLLOWS an existing symlink, so on a build
#     with defect 1 it writes through the link into the content store and
#     corrupts the package. Every later check then reads the planted string
#     back out and passes on ANY build. Hence the `rm -f` and the not-a-symlink
#     assertion.
#   - A relative path to the nub binary stops resolving after the `cd`. Hence
#     the absolute-path resolution up top.
#   - Any install failing unchecked. Hence the explicit aborts.
#
# So: RUN IT AGAINST A BUILD THAT PREDATES THE FIX TOO. If the control does not
# fail, the harness is broken, not the code. `git worktree add` at the
# merge-base and build there — never the shipped release, which trails main by
# unrelated commits.
#
# Requires network (installs cowsay + semver from the registry).
#
# Usage: tests/global-install/run.sh <path-to-nub>
set -uo pipefail

NUB="${1:?usage: run.sh <path-to-nub>}"
NUB=$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")
[ -x "$NUB" ] || { echo "not executable: $NUB"; exit 2; }

ROOT=$(mktemp -d)
HOME_DIR="$ROOT/home"
WORK="$ROOT/work"
mkdir -p "$HOME_DIR" "$WORK"
trap 'rm -rf "$ROOT"' EXIT
cd "$WORK" || exit 1

# PNPM_HOME is commonly exported on a developer machine and used to win over
# HOME when resolving the global root, so a run that does not clear it escapes
# the sandbox and writes into the real home.
run() { env -u PNPM_HOME -u XDG_DATA_HOME -u XDG_BIN_HOME HOME="$HOME_DIR" "$NUB" "$@"; }

fail=0
check() { if [ "$2" = "0" ]; then echo "  PASS  $1"; else echo "  FAIL  $1"; fail=1; fi; }
abort() { echo "  ABORT $1"; tail -12 "$2" | sed 's/^/        /'; exit 2; }

BIN_DIR=$(run bin -g 2>/dev/null | tail -1)
echo "global bin dir: $BIN_DIR"

echo
echo "== 1. install -g then remove -g leaves nothing behind =="
run install -g cowsay > "$ROOT/i1.log" 2>&1 || abort "install -g failed" "$ROOT/i1.log"
[ -e "$BIN_DIR/cowsay" ]; check "cowsay linked after install" $?
run remove -g cowsay >/dev/null 2>&1
# `-e` follows symlinks and so reports false for a DANGLING link; only the
# union of a link test and an existence test catches both leftover shapes.
leftover=$(find "$BIN_DIR" -maxdepth 1 \( -type l -o -type f \) 2>/dev/null || true)
[ -z "$leftover" ]; check "no bin entry left after remove -g" $?
[ -n "$leftover" ] && echo "        leftover: $leftover"

echo
echo "== 2. add -g over an overlapping alias set keeps the live bin =="
run install -g cowsay >/dev/null 2>&1
run install -g cowsay semver > "$ROOT/i2.log" 2>&1 || abort "overlapping install -g failed" "$ROOT/i2.log"
[ -e "$BIN_DIR/cowsay" ]; check "cowsay still resolves after the overlapping install" $?
[ -e "$BIN_DIR/semver" ]; check "semver linked by the new install" $?

echo
echo "== 3. a foreign binary in the bin dir survives an install =="
run remove -g cowsay >/dev/null 2>&1
run remove -g semver >/dev/null 2>&1
rm -f "$BIN_DIR/cowsay"
printf '#!/bin/sh\necho FOREIGN\n' > "$BIN_DIR/cowsay"
chmod +x "$BIN_DIR/cowsay"
[ ! -L "$BIN_DIR/cowsay" ] || { echo "  ABORT planted file is a symlink, not a real file"; exit 2; }
run install -g cowsay > "$ROOT/i3.log" 2>&1 || abort "install -g failed" "$ROOT/i3.log"
out=$("$BIN_DIR/cowsay" 2>/dev/null || true)
[ "$out" = "FOREIGN" ]; check "foreign cowsay not overwritten by install -g" $?
[ -e "$BIN_DIR/cowthink" ]; check "the non-colliding bin of the same package still links" $?

echo
echo "== 4. the layout is nub-named and PATH-conventional =="
case "$BIN_DIR" in
  */pnpm|*/Library/pnpm) echo "  FAIL  bin dir is still pnpm-named: $BIN_DIR"; fail=1 ;;
  */.local/bin)          echo "  PASS  bin dir is the conventional ~/.local/bin" ;;
  *)                     echo "  FAIL  unexpected bin dir: $BIN_DIR"; fail=1 ;;
esac
[ -d "$HOME_DIR/.local/share/nub/global" ]; check "installs live under the data namespace" $?
[ ! -e "$HOME_DIR/.local/share/pnpm" ]; check "nothing written to a pnpm-named path" $?
[ ! -e "$HOME_DIR/Library/pnpm" ]; check "nothing written to ~/Library/pnpm" $?

echo
echo "== 5. an install warns when the bin dir is not on PATH =="
# The sandboxed HOME guarantees this dir is absent from PATH, so the warning
# must fire. Reporting success for commands that cannot run is #642 itself.
run remove -g cowsay >/dev/null 2>&1
run install -g cowsay > "$ROOT/i5.log" 2>&1
grep -q "is not on PATH" "$ROOT/i5.log"; check "install warns that the bin dir is not on PATH" $?
grep -q "export PATH=" "$ROOT/i5.log"; check "the warning names the line to add" $?

echo
if [ "$fail" = "0" ]; then echo "ALL CHECKS PASSED"; else echo "FAILURES PRESENT"; fi
exit "$fail"
