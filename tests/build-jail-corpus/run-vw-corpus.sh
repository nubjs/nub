#!/bin/bash
# Drive a manifest through both arms with bounded parallelism, HOST first so the
# A0 validity gate (README: "a jail-arm verdict is admissible only if the same
# package reached its class effect with the jail off") can be applied per package.
set -u
HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="${1:?manifest tsv}"; JOBS="${JOBS:-4}"; RESULTS="${RESULTS:-/tmp/vwc/results.tsv}"
mkdir -p "$(dirname "$RESULTS")"; : > "$RESULTS"

one() {
  IFS=$'\t' read -r PKG VER CLASS NEED <<< "$1"
  [ -z "${PKG:-}" ] && return
  # A full disk makes every snapshot abort, and an aborted snapshot is reported as
  # INADMISSIBLE -- indistinguishable, downstream, from a package that genuinely
  # never reached its class effect. Refuse to produce rows instead.
  AVAIL=$(df -Pk /tmp | awk 'NR==2{print $4}')
  if [ "${AVAIL:-0}" -lt 3000000 ]; then
    echo "ABORT: <3 GB free on /tmp; refusing to run $PKG (results would be bogus)" >&2
    return 1
  fi
  H=$("$HARNESS/run-vw-pkg.sh" "$PKG" "$VER" "$CLASS" "${NEED:-}" HOST 2>/dev/null | tail -1)
  HV=$(printf '%s' "$H" | cut -f5)
  case "$HV" in DID-WORK-AND-SUCCEEDED) A0=true ;; *) A0=false ;; esac
  W=$(A0_CLASS_EFFECT=$A0 "$HARNESS/run-vw-pkg.sh" "$PKG" "$VER" "$CLASS" "${NEED:-}" WORLD 2>/dev/null | tail -1)
  printf '%s\n%s\n' "$H" "$W" >> "$RESULTS"
  printf '%s\n%s\n' "$H" "$W"
}
export -f one; export HARNESS

grep -v '^#' "$MANIFEST" | grep -v '^[[:space:]]*$' | \
  xargs -d '\n' -P "$JOBS" -I{} bash -c 'one "$@"' _ {}
echo "results -> $RESULTS"
