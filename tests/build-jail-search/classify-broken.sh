#!/usr/bin/env bash
# Why does a package fail at the WIDEST grant?
#
# `BROKEN-EVEN-WITH-EVERYTHING` currently conflates three different things, and only one of
# them is about the build jail:
#
#   NUB-BUG        fails under nub with the jail OFF   -> nothing to do with confinement
#   ENV            fails under npm too                 -> the package is broken on this
#                                                         Node/platform, not our problem
#   JAIL           passes jail-off, fails jail-on      -> a REAL grant gap, the only case
#                                                         the catalog can fix
#
# Without this split, a harness failure reads as "no grant helps" and a real gap is
# indistinguishable from a package that does not build on Node 26 at all.
#
# Usage: classify-broken.sh <nub> <pkg@version>...
set -u
NUB="${1:?usage: classify-broken.sh <nub> <pkg@ver>...}"; shift
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac

for spec in "$@"; do
  pkg="${spec%@*}"; ver="${spec##*@}"
  dir="$HOME/.cache/nub/classify/${pkg//\//+}"
  err=""

  run() {  # $1 = label, $2 = extra manifest json, $3 = command...
    local d="$dir/$1"; rm -rf "$d"; mkdir -p "$d/home"; cd "$d" || return 1
    printf '{"name":"c","version":"1.0.0","dependencies":{"%s":"%s"}%s}\n' "$pkg" "$ver" "$2" > package.json
    printf 'side-effects-cache=false\n' > .npmrc
    git init -q . 2>/dev/null
    shift 2
    env -u CI HOME="$d/home" "$@" > out.log 2>&1
    local rc=$?
    # First real error line, for the report.
    err=$(grep -oiE "(ERR_[A-Z_]+|error[: ][^\"]{0,80}|not supported|EBADENGINE|gyp ERR![^\"]{0,60})" out.log | head -1)
    return $rc
  }

  # 1. nub, jail OFF — isolates nub-vs-jail.
  run jailoff ',"install":{"buildJail":false}' "$NUB" install; off=$?
  # 2. nub, jail ON at its default grant.
  run jailon '' "$NUB" install; on=$?
  # 3. npm — is the package simply broken here?
  run npm '' npm install --silent --no-audit --no-fund; npmrc=$?

  if   [ $off -ne 0 ] && [ $npmrc -ne 0 ]; then verdict="ENV (npm fails too)"
  elif [ $off -ne 0 ]; then                     verdict="NUB-BUG (jail off still fails)"
  elif [ $on  -ne 0 ]; then                     verdict="JAIL (real grant gap)"
  else                                          verdict="PASSES (harness artifact?)"
  fi
  printf '  %-26s off=%-3s on=%-3s npm=%-3s  %-28s %s\n' "$spec" "$off" "$on" "$npmrc" "$verdict" "${err:0:70}"
done
