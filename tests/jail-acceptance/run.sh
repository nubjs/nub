#!/usr/bin/env bash
# Does a real project install successfully with the build jail ON, and how many of its
# install-script dependencies has nobody measured?
#
# ⛔⛔ WHY THIS EXISTS SEPARATELY FROM `tests/daily-driver/`. That harness installs a real project and
# tests the PACKAGE MANAGER; it references the jail nowhere, so it would pass identically with
# confinement switched off. Default-on needs the opposite subject: the jail is the thing under test and
# the PM is the vehicle. Two suites, because a failure in each means something different — a daily-driver
# failure is a PM defect, a failure here is a confinement defect.
#
# ⛔ THE SECOND NUMBER IS THE ONE THAT DECIDES THE SHIP, AND IT IS NOT THE PASS RATE. Per-install
# success is the bar, and an install succeeds only if EVERY install-script dependency works under the
# grant it gets. An uncatalogued package gets the baseline — sufficient for 96.4% of measured packages —
# so the per-install rate is roughly that rate raised to the count of uncatalogued deps: ten of them
# compounds to ~69%, nowhere near the 99.9% bar. A green run with a high uncatalogued count is NOT
# evidence that default-on is ready; it is evidence that this project got lucky.
#
# ⛔ THE JAIL IS STATED, NOT INHERITED. `install.buildJail` defaults to on, so a fixture with no
# `nub.jsonc` would test the default rather than confinement — and if the default ever flipped, every
# case here would silently start measuring an unjailed install and keep passing. Stating it is what
# makes the run falsifiable, and the same reasoning the corpus drivers' verify arms use.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
# ⛔ RESOLVED TO AN ABSOLUTE PATH, BECAUSE EVERY INSTALL RUNS AFTER A `cd`. A caller passing a relative
# path (CI naturally writes `target/debug/nub`) would pass the `-x` check here, in the repo root, and
# then fail to execute from inside each fixture directory — so the suite would report every project
# broken and none of it would be about the jail.
NUB="${NUB:-$REPO/target/fast/nub}"
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac
CATALOG="${CATALOG:-$REPO/crates/nub-sandbox/data/build-jail-catalog-v2.json}"

[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }
[ -f "$CATALOG" ] || { echo "error: catalog not found: $CATALOG" >&2; exit 2; }

# ⛔ NOT UNDER /tmp. The jail redirects the child's temp dir, so a fixture there cannot exercise a
# filesystem denial at all — the corpus drivers carry the same rule and it produced a wrong all-clear
# once already.
ROOT="$(mktemp -d "$HOME/jail-acceptance-XXXXXX")"
# `KEEP=1` leaves the tree and every install log in place. Worth a knob: the first real run of this
# harness reported an impossible zero, and the logs that would have explained it had been deleted by
# this trap before anyone could read them.
cleanup() { if [ -n "${KEEP:-}" ]; then echo "kept: $ROOT"; else rm -rf "$ROOT"; fi; }
trap cleanup EXIT

PASS=0; FAIL=0; TOTAL_UNCAT=0
results=""

# One project shape per line: `<name> <dependency-json>`. Kept as real registry packages rather than
# invented fixtures, because the population under test is what real projects actually pull in.
run_project () {
  local name="$1" deps="$2"
  local dir="$ROOT/$name"; mkdir -p "$dir"
  printf '{"name":"acceptance-%s","version":"1.0.0","dependencies":%s}\n' "$name" "$deps" > "$dir/package.json"
  printf '{"install":{"buildJail":true}}\n' > "$dir/nub.jsonc"
  # The memo off at source: nub keys a lifecycle outcome on package identity, so a warm cache would
  # REPLAY an earlier result with every precondition green — a pass that ran nothing.
  printf 'side-effects-cache=false\n' > "$dir/.npmrc"

  local log="$ROOT/$name.install.log" rc=0
  ( cd "$dir" && "$NUB" install > "$log" 2>&1 ) || rc=$?

  local uncat
  uncat="$(node "$HERE/uncatalogued.mjs" --install-log "$log" --catalog "$CATALOG" | head -1)"
  local n
  n="$(printf '%s' "$uncat" | sed -nE 's/.*UNCATALOGUED ([0-9]+).*/\1/p')"
  n="${n:-0}"
  TOTAL_UNCAT=$((TOTAL_UNCAT + n))

  if [ "$rc" -eq 0 ]; then
    PASS=$((PASS + 1)); results="${results}  ✓ ${name}  ${uncat}"$'\n'
    return
  fi

  # ⛔⛔ A FAILED INSTALL IS NOT YET A JAIL FAILURE — ASK THE JAIL-OFF ARM BEFORE NAMING THE JAIL. This
  # is the same discipline the corpus drivers' jail-off control exists for, and it was learned there
  # expensively: a package that fails identically unjailed says NOTHING about capabilities, and filing it
  # as a confinement defect sends the next reader chasing a bug that is not there. Measured right here on
  # this harness's first run: `node-sass@7.0.3` fails under Node 26 because node-gyp cannot build it, with
  # or without the jail. Without this arm that would have been a permanently red acceptance suite blaming
  # confinement for a toolchain era mismatch.
  local offdir="$ROOT/$name-jail-off"; mkdir -p "$offdir"
  cp "$dir/package.json" "$offdir/package.json"
  printf '{"install":{"buildJail":false}}\n' > "$offdir/nub.jsonc"
  printf 'side-effects-cache=false\n' > "$offdir/.npmrc"
  # A unique root name, or nub's lifecycle memo replays the jailed arm's outcome and the control agrees
  # with it for free — a false "not the jail" that looks exactly like a real one.
  node -e 'const f=process.argv[1],j=JSON.parse(require("fs").readFileSync(f,"utf8"));j.name+="-off";require("fs").writeFileSync(f,JSON.stringify(j))' "$offdir/package.json"
  # ⛔ A THROWAWAY HOME FOR THE UNJAILED ARM. Unjailed means the script writes the REAL `$HOME` — that
  # is what the arm is for — so a failing or interrupted download leaves a HALF-POPULATED cache in the
  # user's own home, and every later install of that package then fails jailed AND unjailed with an
  # error that reads like a confinement defect. Measured on `puppeteer`: the browser folder present, the
  # executable missing. A control must not be able to damage the machine it runs on.
  local offhome; offhome="$(mktemp -d "$ROOT/$name-offhome-XXXXXX")"
  local offlog="$ROOT/$name.jail-off.log" offrc=0
  ( cd "$offdir" && HOME="$offhome" NUB_CACHE_DIR="$offhome/nubcache" "$NUB" install > "$offlog" 2>&1 ) || offrc=$?

  if [ "$offrc" -ne 0 ]; then
    # Fails either way: not a confinement finding, so it does not fail this run.
    results="${results}  ~ ${name}  BROKEN-WITHOUT-JAIL-TOO (rc=${rc} jailed, rc=${offrc} unjailed) — not a jail defect  ${uncat}"$'\n'
    return
  fi
  FAIL=$((FAIL + 1))
  results="${results}  ✗ ${name}  rc=${rc} jailed, rc=0 UNJAILED — the jail IS the difference  ${uncat}"$'\n'
  echo "── ${name}: jailed install output ──"; tail -20 "$log"
}

# ⛔ SHAPES CHOSEN FOR THEIR DEPENDENCY GRAPHS, NOT THEIR POPULARITY. The count that matters is
# uncatalogued INSTALL-SCRIPT deps, so a project with none tells us nothing however widely it is used.
# These three cover the bands measured earlier in this effort: a framework starter (typically 0-1), a
# native-heavy app (the packages with real compile steps), and an older pinned tree (where the
# deprecated tail lives, and where the one uncatalogued package found so far turned up).
run_project framework-starter '{"vite":"^5.0.0","react":"^18.0.0"}'
run_project native-heavy      '{"esbuild":"^0.21.0","sharp":"^0.33.0"}'
run_project older-pinned      '{"node-sass":"7.0.3"}'

echo
echo "jail-acceptance: ${PASS} passed, ${FAIL} failed"
echo "uncatalogued install-script deps across all projects: ${TOTAL_UNCAT}"
printf '%s' "$results"

# ⛔ A FAILED INSTALL FAILS THE RUN; A HIGH UNCATALOGUED COUNT DOES NOT. The count is a measurement,
# and gating on it would either be set so loose it never fires or so tight the suite is red for weeks
# while the corpus fills — and a suite that is red by default is a suite nobody reads. It is REPORTED so
# the number that decides the ship is visible on every run.
[ "$FAIL" -eq 0 ] || exit 1
