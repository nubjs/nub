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
#   ./run-isolated.sh @railway/cli            # both arms
#   ./run-isolated.sh @railway/cli PROD       # one arm
#
# Env is the same as run-shard.sh (NUB_BIN, NUB_EXPECT_GIT_SHA, STUDY_PATH,
# OUTROOT, and NUB_BUILD_JAIL_CATALOG when iterating on grants).
set -u -o pipefail

PKG="${1:?package name, exactly as it appears in a shard manifest}"
ARMS="${2:-A0 PROD}"
HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SLUG="$(printf '%s' "$PKG" | tr -c 'A-Za-z0-9' '-')"

# The row is copied VERBATIM, which is what carries the class and the NEED column.
# Retyping it by hand is how a codegen package loses its `prisma-schema` provisioning
# and reports INVALID-FIXTURE for a reason that has nothing to do with the jail.
ROW=""
for m in "$HARNESS"/shard-*.tsv; do
  case "$m" in *"shard-iso"*) continue ;; esac
  ROW="$(tr -d '\r' < "$m" | awk -F'\t' -v p="$PKG" '$1==p {print; exit}')"
  [ -n "$ROW" ] && break
done
[ -n "$ROW" ] || { echo "no manifest row for '$PKG' in $HARNESS/shard-*.tsv" >&2; exit 2; }

MAN="$HARNESS/shard-iso-$SLUG.tsv"
printf '%s\n' "$ROW" > "$MAN"
echo "isolated manifest: $MAN"
printf '  %s\n' "$ROW"

for arm in $ARMS; do
  ( cd "$HARNESS" && ./run-shard.sh "iso-$SLUG" "$(basename "$MAN")" "$arm" )
done
