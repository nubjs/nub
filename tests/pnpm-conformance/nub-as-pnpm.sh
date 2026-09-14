#!/bin/sh
# Installed by run.sh at `<pnpm clone>/target/debug/pnpm`, the path the suite's
# `Command::cargo_bin("pnpm")` resolves. It runs nub under its own name: nub's
# argv0 `pnpm` is the package-manager shim, which hands off to a real pnpm, so
# the command under test must reach nub as `nub`. The paths are baked at swap
# time because tests rebuild the child environment.
#
# Each call appends one tab-separated line to the log: the nextest test name,
# the project identity nub will pick for the cwd, the cwd, and the arguments.
# The identity walk is a cheap copy of nub_core::pm::identity::identity_of_dir
# (nearest directory with any marker wins; a manifest pin is matched textually),
# kept in sh so the shim adds no process spawn before nub starts.
identity=nub
dir=$PWD
while :; do
  pin=
  if [ -f "$dir/package.json" ]; then
    if grep -Eq '"packageManager"[[:space:]]*:[[:space:]]*"nub@|"name"[[:space:]]*:[[:space:]]*"nub"' "$dir/package.json" 2>/dev/null \
      && grep -Eq '"(packageManager|devEngines)"' "$dir/package.json" 2>/dev/null; then
      pin=nub
    elif grep -Eq '"packageManager"[[:space:]]*:[[:space:]]*"pnpm@|"devEngines"' "$dir/package.json" 2>/dev/null \
      && grep -Eq '"packageManager"[[:space:]]*:[[:space:]]*"pnpm@|"name"[[:space:]]*:[[:space:]]*"pnpm"' "$dir/package.json" 2>/dev/null; then
      pin=pnpm
    elif grep -Eq '"packageManager"[[:space:]]*:' "$dir/package.json" 2>/dev/null; then
      pin=other
    fi
  fi
  if [ "$pin" = nub ]; then identity=nub; break; fi
  if [ -e "$dir/pnpm-lock.yaml" ] || [ -e "$dir/pnpm-workspace.yaml" ]; then identity=pnpm; break; fi
  if [ "$pin" = pnpm ]; then identity=pnpm; break; fi
  if [ "$pin" = other ] || [ -e "$dir/nub.lock" ]; then identity=nub; break; fi
  [ "$dir" = / ] && break
  dir=$(dirname "$dir")
done
printf '%s\t%s\t%s\t%s\n' "${NEXTEST_TEST_NAME:--}" "$identity" "$PWD" "$*" >> '__SHIM_LOG__' 2>/dev/null
exec '__NUB_BIN__' "$@"
