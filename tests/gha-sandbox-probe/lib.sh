# Shared helpers for the GHA hosted-runner sandbox feasibility probe.
# Sourced by diagnostics.sh and probe-nesting.sh. NOT executable on its own.
#
# Each check prints a machine-greppable line so the workflow summary can harvest
# results regardless of interleaved tool noise:
#   RESULT:<key>=PASS|FAIL|SKIP
#   DETAIL:<key>=<free text, exact denial / value>
#
# NOTHING here aborts on error — every tier is best-effort and self-reports.

emit() { printf 'RESULT:%s=%s\n' "$1" "$2"; }
detail() { printf 'DETAIL:%s=%s\n' "$1" "$*"; }
hdr() { printf '\n========== %s ==========\n' "$*"; }

# A representative bwrap invocation: bwrap creating an unprivileged user+net
# namespace and running /bin/true inside a read-only view of /. This mirrors the
# core of what nub's single-level sandbox does.
BWRAP_ARGS=(--die-with-parent --unshare-user --unshare-net --unshare-pid --ro-bind / / --proc /proc --dev /dev -- /bin/true)

# Run one bwrap binary single-level; echo combined stderr, return its exit code.
run_single() {
  local bw="$1"; shift
  "$bw" "${BWRAP_ARGS[@]}" 2>&1
}

# NESTED launch: an OUTER bwrap creating a userns, and INSIDE it an INNER bwrap
# creating a SECOND userns. This is the exact capability D5 exists to unlock —
# on stock Ubuntu 24.04 the inner userns is denied by the AppArmor child profile.
# $1 = outer bwrap path, $2 = inner bwrap path.
run_nested() {
  local outer="$1" inner="$2"
  # Outer binds / read-only and keeps the inner bwrap + /bin/true reachable, then
  # the inner unshares its own user+net ns. Inner uses the SAME representative args.
  "$outer" --die-with-parent --unshare-user --ro-bind / / --proc /proc --dev /dev -- \
    "$inner" --die-with-parent --unshare-user --unshare-net --ro-bind / / --proc /proc --dev /dev -- \
    /bin/true 2>&1
}

# Judge + record a nested attempt under key $1, outer=$2 inner=$3.
judge_nested() {
  local key="$1" outer="$2" inner="$3" out rc
  out="$(run_nested "$outer" "$inner")"; rc=$?
  if [[ $rc -eq 0 ]]; then
    emit "$key" PASS
    detail "$key" "nested launch succeeded (outer=$outer inner=$inner)"
  else
    emit "$key" FAIL
    detail "$key" "rc=$rc :: ${out:-<no output>}"
  fi
  return 0
}

apt_bwrap() {
  if ! command -v bwrap >/dev/null 2>&1; then
    sudo apt-get update -qq >/dev/null 2>&1 || true
    sudo apt-get install -y -qq bubblewrap >/dev/null 2>&1 || true
  fi
  command -v bwrap 2>/dev/null || echo /usr/bin/bwrap
}
