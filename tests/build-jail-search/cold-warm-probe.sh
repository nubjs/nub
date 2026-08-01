#!/usr/bin/env bash
# Does placement decide the SAME way on a cold store as on a warm one?
#
# Placement is seeded from `declares_install_script`, which reads the package's own
# package.json out of the content-addressable store. If that decision is made before the
# package has been fetched, a COLD store has no manifest to read, the predicate returns
# false, and the package is NOT placed project-local — while a WARM store places it
# correctly. Every measurement taken so far has been on a warm store, so this failure mode
# would be invisible to all of them.
#
# This is the shape that forced bun to seed on trusted-ness instead of `hasInstallScript`:
# their flag was absent from the lockfile, so packages flipped placement between the cold
# and warm install. Different cause, identical symptom.
#
# PASS = both arms place the package project-local (a real directory, not a symlink out).
set -u
NUB="${1:?usage: cold-warm-probe.sh <nub-binary> [pkg@version]}"
SPEC="${2:-simple-git-hooks@2.13.1}"
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac
PKG="${SPEC%@*}"

probe() {  # $1 = arm label, $2 = HOME (fresh => cold, reused => warm)
  local arm="$1" home="$2"
  local dir="$HOME/.cache/nub/coldwarm/$arm"
  rm -rf "$dir"; mkdir -p "$dir"; cd "$dir" || return 1
  printf '{"name":"cw","version":"1.0.0","dependencies":{"%s":"%s"},"simple-git-hooks":{"pre-commit":"echo x"}}\n' \
    "$PKG" "${SPEC##*@}" > package.json
  printf 'side-effects-cache=false\n' > .npmrc
  git init -q . 2>/dev/null
  env -u CI HOME="$home" "$NUB" install > install.log 2>&1
  local rc=$?
  # A placed package is a REAL DIRECTORY under .store; an unplaced one is a symlink into
  # the global store. That distinction IS the thing under test.
  local entry
  entry=$(ls -d node_modules/.store/"$PKG"@* 2>/dev/null | head -1)
  local kind="ABSENT"
  if [ -n "$entry" ]; then
    if [ -L "$entry" ]; then kind="symlink (NOT placed)"; else kind="real dir (placed)"; fi
  fi
  printf '  %-6s rc=%s  store entry: %s\n' "$arm" "$rc" "$kind"
  [ "$kind" = "real dir (placed)" ]
}

echo "cold/warm placement probe — $SPEC"
COLD_HOME="$HOME/.cache/nub/coldwarm-home-cold"
WARM_HOME="$HOME/.cache/nub/coldwarm-home-warm"
rm -rf "$COLD_HOME" "$WARM_HOME"; mkdir -p "$COLD_HOME" "$WARM_HOME"

# COLD: a store that has never seen this package.
probe cold "$COLD_HOME"; cold=$?
# WARM: same store, second install — the manifest is now in the CAS.
probe warm2 "$COLD_HOME"; warm=$?

echo
if [ "$cold" -eq 0 ] && [ "$warm" -eq 0 ]; then
  echo "PASS — placement agrees on a cold and a warm store."
  exit 0
fi
echo "FAIL — placement DIFFERS between a cold and a warm store."
echo "  A first-ever install on a fresh machine would not place this package, so its"
echo "  lifecycle script cannot find the project and silently does nothing."
exit 1
