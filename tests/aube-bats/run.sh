#!/usr/bin/env bash
# nub-adapted run of aube's own bats e2e suite (see README.md beside this).
#
# aube's bats tests spawn the `aube` binary against an offline Verdaccio
# registry. This harness runs a CURATED subset of those suites with nub's PM
# surface standing in for `aube`: a scratch mirror reproduces the layout
# common_setup.bash expects (PROJECT_ROOT/test, PROJECT_ROOT/fixtures,
# PROJECT_ROOT/target/debug), and PROJECT_ROOT/target/debug/aube is a shim
# that exec's nub — so the harness's own PATH prepend resolves straight to
# nub, no PATH surgery from outside.
#
# Tests that assert aube-branded behavior nub deliberately toggles off are
# skipped via skips.txt (suite|exact test name|reason) — the reason prints in
# the bats output. Keep that list curated and commented; it is the living
# statement of "where nub's PM surface intentionally diverges from aube's".
#
# Usage: tests/aube-bats/run.sh <path-to-nub> [suite.bats ...]
# (default suites: the install/add/remove/update/ci/lockfile family)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
AUBE_TEST_DIR="$REPO_ROOT/vendor/aube/test"
SKIPLIST="$HERE/skips.txt"
GAPLIST="$HERE/known-gaps.txt"

NUB_ARG=${1:?usage: run.sh <path-to-nub> [suite.bats ...]}
NUB="$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")"
[ -x "$NUB" ] || { echo "nub binary not executable: $NUB" >&2; exit 1; }
shift

SUITES=("$@")
if [ ${#SUITES[@]} -eq 0 ]; then
  SUITES=(install.bats ci.bats add.bats remove.bats update.bats lockfile_settings.bats lockfile_dir.bats)
fi

SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT

# Mirror the PROJECT_ROOT shape common_setup.bash derives from the test dir
# ($BATS_TEST_DIRNAME/..): fixtures/ for _setup_basic_fixture & co.,
# target/debug/ for its PATH prepend.
mkdir -p "$SCRATCH/test" "$SCRATCH/target/debug"
ln -s "$AUBE_TEST_DIR/test_helper" "$SCRATCH/test/test_helper"
ln -s "$AUBE_TEST_DIR/registry" "$SCRATCH/test/registry"
ln -s "$AUBE_TEST_DIR/setup_suite.bash" "$SCRATCH/test/setup_suite.bash"
ln -s "$REPO_ROOT/vendor/aube/fixtures" "$SCRATCH/fixtures"

# The aube -> nub shim. Two jobs:
#  1. exec nub (absolute path baked in — no env var; NUB_* is banned and
#     nothing else is guaranteed to survive the harness's env isolation);
#  2. translate the harness's AUBE_* knobs to their npm_config_* spellings.
#     nub deadens the engine's AUBE env family on purpose (engine_preflight
#     enables only NPM + EXTERNAL), but the same settings are registered with
#     npm_config_* env sources in aube-settings/settings.toml, so the supply
#     chain gates stay pointed away from the public APIs during bats.
#     (AUBE_TEST_REGISTRY needs no translation: common_setup writes it into
#     the per-test .npmrc, which nub's engine reads. AUBE_NO_UPDATE_CHECK
#     needs none either: nub's dispatch never calls aube's update notifier.)
cat > "$SCRATCH/target/debug/aube" <<EOF
#!/usr/bin/env bash
[ -n "\${AUBE_ADVISORY_CHECK:-}" ] && export npm_config_advisory_check="\$AUBE_ADVISORY_CHECK"
[ -n "\${AUBE_LOW_DOWNLOAD_THRESHOLD:-}" ] && export npm_config_low_download_threshold="\$AUBE_LOW_DOWNLOAD_THRESHOLD"
exec "$NUB" "\$@"
EOF
chmod +x "$SCRATCH/target/debug/aube"

# Stage each curated suite with the two lists applied: insert a bats
# `skip "<prefix>: <reason>"` as the first statement of each named @test
# block. skips.txt = permanent intended divergences; known-gaps.txt = real
# gaps in-flight work must close (kept separate so neither blurs the other).
stage_suite() {
  local suite="$1"
  awk -v suite="$suite" -v skipfile="$SKIPLIST" -v gapfile="$GAPLIST" '
    BEGIN {
      while ((getline line < skipfile) > 0) {
        if (line ~ /^[[:space:]]*(#|$)/) continue
        split(line, a, "|")
        if (a[1] == suite) reasons[a[2]] = "nub-divergence: " a[3]
      }
      close(skipfile)
      while ((getline line < gapfile) > 0) {
        if (line ~ /^[[:space:]]*(#|$)/) continue
        split(line, a, "|")
        if (a[1] == suite) reasons[a[2]] = "KNOWN-GAP: " a[3]
      }
      close(gapfile)
    }
    {
      print
      if ($0 ~ /^@test ".*" \{$/) {
        name = substr($0, 8, length($0) - 10)
        if (name in reasons) {
          printf "\tskip \"%s\"\n", reasons[name]
          used[name] = 1
        }
      }
    }
    END {
      for (n in reasons) if (!(n in used)) {
        printf "list entry matched no test in %s: %s\n", suite, n > "/dev/stderr"
        exit 1
      }
    }
  ' "$AUBE_TEST_DIR/$suite" > "$SCRATCH/test/$suite"
}

staged=()
for suite in "${SUITES[@]}"; do
  [ -f "$AUBE_TEST_DIR/$suite" ] || { echo "no such suite: $suite" >&2; exit 1; }
  stage_suite "$suite"
  staged+=("$SCRATCH/test/$suite")
done

# Hermetic color state: common_setup exports NO_COLOR=1 but doesn't defend
# against an inherited FORCE_COLOR (dev shells set it; node then warns
# "'NO_COLOR' env is ignored" and colorizes, breaking exact-output asserts).
unset FORCE_COLOR CLICOLOR_FORCE

echo "nub-adapted aube bats: ${SUITES[*]}" >&2
"$AUBE_TEST_DIR/bats/bin/bats" "${staged[@]}"
