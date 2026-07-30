#!/usr/bin/env bash
# Follow-ups to probes.sh: the ABI margin the artifact actually consumes, egress posture, and the
# warm cost of the no-overlay fallback (its first run is dominated by cold page cache, not by copy).
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WORLD="${WORLD:?}"; RUN="${RUN:-/tmp/vw-run}"; JAIL="$HERE/jail"
J=("$JAIL" --root "$WORLD" --bind "$RUN/work:/work" --uid 1000 --cwd /work)
runq() { echo "\$ $1"; bash -c "$1" 2>&1; echo "[rc=$?]"; }

echo "################ X1  ABI margin: what the compiled artifact actually requires ################"
BS=$(find "$RUN/work/project/real/node_modules" -name better_sqlite3.node | head -1)
echo "artifact: $BS"
runq "objdump -T '$BS' | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tr '\n' ' '"
runq "objdump -T '$BS' | grep -o 'GLIBCXX_[0-9.]*' | sort -Vu | tail -1"
runq "strings $WORLD/lib/x86_64-linux-gnu/libc.so.6 | grep -m1 'GNU C Library'"
runq "ldd --version | head -1"
runq "ldd '$BS'"

echo
echo "################ X2  egress posture — CLONE_NEWNET vs host netns, one variable ################"
runq "${J[*]} --netns -- /bin/sh -c 'ip -o addr 2>/dev/null | head -3; getent hosts registry.npmjs.org >/dev/null && echo DNS_OK || echo DNS_FAIL'"
runq "${J[*]} -- /bin/sh -c 'getent hosts registry.npmjs.org >/dev/null && echo DNS_OK || echo DNS_FAIL'"

echo
echo "################ X3  no-overlay fallback, WARM (the 32s first rep was cold page cache) ################"
for i in 1 2 3; do "${J[@]}" --no-overlay --timings -- /bin/true 2>&1 >/dev/null | grep TOTAL; done
echo "--- overlay path again for side-by-side ---"
for i in 1 2 3; do "${J[@]}" --timings -- /bin/true 2>&1 >/dev/null | grep TOTAL; done

echo
echo "################ X4  world size ################"
runq "du -sh $WORLD; du -sh $WORLD/opt/node; du -sh $WORLD/usr"
