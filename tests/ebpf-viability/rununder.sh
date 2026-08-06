#!/bin/bash
# rununder.sh [probe args...] -- <cmd> [args...]
# Starts a gated shell, attaches fileprobe to that tgid (so the probe is live
# BEFORE the first event), releases it, then drains and reports.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE"

# The PROBE needs root; the TRACED COMMAND must not get it, or $HOME moves and
# the workload runs in a different environment than the one being characterised.
# Set SUDO=sudo to elevate only the probe.
SUDO=${SUDO:-}

PROBE_ARGS=()
while [ $# -gt 0 ]; do
	[ "$1" = "--" ] && { shift; break; }
	PROBE_ARGS+=("$1"); shift
done
[ $# -gt 0 ] || { echo "rununder.sh: no command"; exit 2; }

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkfifo "$D/go"

bash -c 'echo $$ > '"$D"'/pid; read -r x < '"$D"'/go; exec "$@"' _ "$@" 2> "$D/cmd.err" &
SHELLPID=$!
for i in $(seq 1 200); do [ -s "$D/pid" ] && break; sleep 0.05; done
TGID=$(cat "$D/pid")

$SUDO ./fileprobe --pid "$TGID" "${PROBE_ARGS[@]}" > "$D/probe.out" 2> "$D/probe.err" &
PROBE=$!
for i in $(seq 1 400); do grep -q READY "$D/probe.err" 2>/dev/null && break; sleep 0.05; done
if ! grep -q READY "$D/probe.err"; then
	echo "PROBE NEVER READY"; cat "$D/probe.err"
	kill $PROBE $SHELLPID 2>/dev/null; exit 1
fi

echo go > "$D/go"
wait $SHELLPID
sleep 1
$SUDO kill -TERM $PROBE
wait $PROBE
cat "$D/cmd.err"
cat "$D/probe.out"
