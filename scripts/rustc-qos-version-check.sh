#!/bin/sh
# rustc-qos-version-check — refuse a change to scripts/rustc-qos.sh that leaves
# its `# rustc-qos-version: N` stamp where it was.
#
# build-status compares the INSTALLED ~/.cargo/rustc-qos.sh against the tree's
# copy by that stamp to report a stale governor. Nothing else bumps it, so the
# first behavior change that forgets would make every stale host read as
# current — the exact failure the stamp exists to catch. One bump per pull
# request: the pre-push hook runs it against the merge-base with trunk, CI
# against the pull request's base.
#
#   scripts/rustc-qos-version-check.sh <base-rev> [<head-rev>]
set -eu
base=$1
head=${2:-HEAD}
f=scripts/rustc-qos.sh
# 0 untouched, 1 changed, anything else is git failing (a bad rev) — which must
# not read as "untouched".
# shellcheck disable=SC2015  # `exit` never returns, so the `||` arm is the changed case only
git diff --quiet "$base" "$head" -- "$f" && exit 0 || [ $? -eq 1 ]
stamp() { git show "$1:$f" 2>/dev/null | sed -n 's/^# rustc-qos-version: \([0-9]*\).*/\1/p' | head -1; }
old=$(stamp "$base"); new=$(stamp "$head")
if [ -z "$new" ]; then
  echo "rustc-qos-version-check: $f has no '# rustc-qos-version: N' line" >&2; exit 1
fi
if [ "${old:-0}" -ge "$new" ] 2>/dev/null; then
  echo "rustc-qos-version-check: $f changed but its stamp is still v$new (base v${old:-none}) — bump '# rustc-qos-version:' so build-status can tell a stale install from a current one" >&2
  exit 1
fi
exit 0
