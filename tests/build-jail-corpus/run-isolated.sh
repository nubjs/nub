#!/usr/bin/env bash
# run-isolated.sh — re-run ONE package as the only package in its shard.
#
# WHY THIS EXISTS. aube stops SCHEDULING queued lifecycle jobs once any sibling has
# failed (vendor/aube/crates/aube/src/commands/install/lifecycle.rs, the `failed`
# flag checked just above the build). Jobs already running drain; jobs still behind
# the semaphore return Ok(0) having done nothing. That is correct for production —
# do not start a build whose result is about to be discarded — and it is fatal to a
# batch measurement, because a package skipped that way is indistinguishable from a
# package the jail blocked. The harness scores both NEVER-RAN-ITS-REAL-PATH.
#
# THE BIAS IS ASYMMETRIC AND POINTS THE WRONG WAY. A tighter jail makes the first
# failure fire EARLIER, which skips MORE siblings, which manufactures MORE breaks —
# so the confined arm is penalised for scheduling, not for confinement. Measured
# 2026-07-31 on the macOS corpus: all 7 shards failed a lifecycle script in BOTH
# arms, and a DIFFERENT package failed first in each, so the A0 denominator and the
# PROD numerator were computed over two differently-truncated executions. Of the 18
# packages the batch reported as broken, 10 install cleanly under the same jail when
# run alone.
#
# So: a batch run is a SCREEN, and nothing it reports is attributable until the
# package has been re-run here. Do not put a batch break count in front of anyone
# without saying which of the two numbers it is.
#
#   ./run-isolated.sh @railway/cli                    # both arms
#   ./run-isolated.sh @railway/cli PROD               # one arm
#   ./run-isolated.sh --from-worklist inconclusive.txt   # everything the screen could not judge
#
# THE WORKLIST FORM IS THE ONE THAT MATTERS, and `aggregate.mjs` writes its input.
# Isolating the whole corpus would be correct and costs an order of magnitude more
# than isolating the rows the screen could not decide — 30 of 345 on the macOS run.
# Two stages, mechanically linked, is what makes the default path trustworthy
# without making it unaffordable:
#
#   run-shard.sh * -> aggregate.mjs -> inconclusive.txt -> run-isolated.sh --from-worklist
#
# Env is the same as run-shard.sh (NUB_BIN, NUB_EXPECT_GIT_SHA, STUDY_PATH,
# OUTROOT, and NUB_BUILD_JAIL_CATALOG when iterating on grants).
set -u -o pipefail

HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Generated one-line manifests are RUN ARTEFACTS and belong beside the run output,
# not in the checkout. Written into the tracked directory they accumulated as
# untracked `shard-iso-*.tsv` files that the row lookup below then had to skip.
GEN="$(printf '%s' "${OUTROOT:-${TMPDIR:-/tmp}/build-jail-corpus}")/manifests"
mkdir -p "$GEN"

# The row is copied VERBATIM, which is what carries the class and the NEED column.
# Retyping it by hand is how a codegen package loses its `prisma-schema` provisioning
# and reports INVALID-FIXTURE for a reason that has nothing to do with the jail.
find_row() {
  local pkg="$1" m row
  for m in "$HARNESS"/shard-*.tsv; do
    case "$m" in *"shard-iso"*) continue ;; esac
    row="$(tr -d '\r' < "$m" | awk -F'\t' -v p="$pkg" '$1==p {print; exit}')"
    [ -n "$row" ] && { printf '%s' "$row"; return 0; }
  done
  return 1
}

run_one() {
  local pkg="$1" arms="$2" slug row man
  slug="$(printf '%s' "$pkg" | tr -c 'A-Za-z0-9' '-')"
  row="$(find_row "$pkg")" || { echo "no manifest row for '$pkg' in $HARNESS/shard-*.tsv" >&2; return 2; }
  man="$GEN/shard-iso-$slug.tsv"
  printf '%s\n' "$row" > "$man"
  echo "isolated manifest: $man"
  printf '  %s\n' "$row"
  for arm in $arms; do
    ( cd "$HARNESS" && ./run-shard.sh "iso-$slug" "$man" "$arm" )
  done
}

if [ "${1:-}" = "--from-worklist" ]; then
  WORKLIST="${2:?path to the worklist aggregate.mjs wrote (inconclusive.txt)}"
  ARMS="${3:-A0 PROD}"
  FAILED=0
  while IFS= read -r pkg; do
    [ -z "$pkg" ] && continue
    echo "=== isolating $pkg"
    # A single package failing to isolate must not abandon the rest of the
    # worklist — the whole point is to come back with every row resolved.
    run_one "$pkg" "$ARMS" || FAILED=$((FAILED + 1))
  done < <(tr -d '\r' < "$WORKLIST")
  echo "worklist done; $FAILED package(s) could not be isolated"
  exit $(( FAILED > 0 ? 1 : 0 ))
fi

run_one "${1:?package name, exactly as it appears in a shard manifest}" "${2:-A0 PROD}"
