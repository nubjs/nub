#!/usr/bin/env bash
# Linux/macOS leg: gcc and clang, int128 candidate vs the loop control.
set -u; cd "$(dirname "$0")"; uname -m
for cxx in g++ clang++; do command -v "$cxx" > /dev/null || continue; "$cxx" --version | head -1
  for v in LOOP INT128; do
    if "$cxx" -O2 -std=c++17 -DMULMOD_$v mulmod-probe.cc -o "probe-$cxx-$v" > "build-$cxx-$v.log" 2>&1; then
      "./probe-$cxx-$v" 2000 | grep -E 'candidate|speedup' | tr '\n' ' '; echo " [$cxx $v]"
    else echo "$cxx $v: BUILD FAIL"; head -3 "build-$cxx-$v.log"; fi
  done
done
