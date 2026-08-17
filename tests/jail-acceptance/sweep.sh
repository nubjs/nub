#!/usr/bin/env bash
# A BROAD sweep: install real framework and application dependency sets with the jail ON, and report
# every dependency whose build script did not succeed.
#
# ⛔ WHY THIS IS SEPARATE FROM `run.sh`. That one is the CI gate — three projects, fast, must stay green.
# This is the exploratory sweep: many real dependency sets, minutes to run, and its job is to FIND
# something rather than to stay green. Keeping them apart means the gate never gets slow and the sweep
# never gets trimmed to keep the gate fast.
#
# ⛔ THE PROJECT LIST IS CHOSEN TO MAXIMISE INSTALL-SCRIPT COVERAGE, NOT POPULARITY. The jail confines
# lifecycle scripts and nothing else, so a project whose dependencies have no install scripts exercises
# nothing however widely it is used — measured: a Next.js app has ZERO. Hence two bands: real framework
# app dependency sets (what an application actually pulls in), and the native/toolchain packages that
# genuinely compile or download at install time, which is where confinement is load-bearing.
#
# ⛔ A FAILURE IS NOT A JAIL FAILURE UNTIL THE JAIL-OFF ARM SAYS SO. Same discipline as the corpus
# drivers': a package that fails identically unjailed says nothing about capabilities. Without this arm
# an era mismatch (node-gyp against a modern Node) reads as a confinement defect.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
NUB="${NUB:-$REPO/target/fast/nub}"
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac
CATALOG="${CATALOG:-$REPO/crates/nub-sandbox/data/build-jail-catalog-v2.json}"
# The real-home dirs nub grants ON PURPOSE (curated `home_paths`), derived from curated.rs rather
# than copied — a stale list would report intended behaviour as a confinement failure, which is
# exactly the misreading that cost an entire investigation here.
. "$HERE/curated-home-paths.sh"
[ -x "$NUB" ] || { echo "error: nub not executable: $NUB" >&2; exit 2; }

ROOT="$(mktemp -d "$HOME/jail-sweep-XXXXXX")"
echo "sweep root: $ROOT"

PASS=0; JAILFAIL=0; BOTHFAIL=0; UNCAT_TOTAL=0; ESCAPES=0
summary=""

# `SWEEP_ONLY="angular puppeteer"` runs just those projects. Worth a knob: iterating on the escape
# detector needs a two-project run, and the alternative — copying this script elsewhere and trimming it
# — silently breaks the `. "$HERE/curated-home-paths.sh"` above, leaving the exclusion array EMPTY so
# every project reports an escape. That produced one wrong control run before this existed.
one () { # $1=name  $2=deps-json
  local name="$1" deps="$2"
  if [ -n "${SWEEP_ONLY:-}" ]; then
    case " $SWEEP_ONLY " in *" $name "*) ;; *) return ;; esac
  fi
  local dir="$ROOT/$name"; mkdir -p "$dir"
  printf '{"name":"sweep-%s","version":"1.0.0","dependencies":%s}\n' "$name" "$deps" > "$dir/package.json"
  printf '{"install":{"buildJail":true}}\n' > "$dir/nub.jsonc"
  # nub memoises a lifecycle outcome on package identity, so without this a second project sharing a
  # dependency REPLAYS the first one's result and the cell measures nothing.
  printf 'side-effects-cache=false\n' > "$dir/.npmrc"

  # ⛔⛔ THE JAILED ARM GETS ITS OWN HOME SO POLLUTION IS OBSERVABLE. This suite originally asserted only
  # that the install SUCCEEDED, and that is exactly why it reported 21 clean projects while a jailed
  # `puppeteer` was writing two multi-hundred-megabyte archives into the user's real `~/.cache`. An
  # install that escapes confinement still exits 0 — success and containment are independent properties,
  # and a suite that measures one while claiming the other is worse than no suite.
  local jhome; jhome="$(mktemp -d "$ROOT/$name-home-XXXXXX")"
  local log="$ROOT/$name.log" rc=0
  ( cd "$dir" && HOME="$jhome" NUB_CACHE_DIR="$jhome/nubcache" "$NUB" install > "$log" 2>&1 ) || rc=$?

  # Anything the jailed run left in its HOME that nub does not own itself is escaped state: the jail
  # redirects a script's home, so a dependency's lifecycle script should leave NOTHING here.
  #
  # ⛔ FILES ONLY, AND EXCLUDE EVERY `nub` SUBTREE — the first version of this check did neither and
  # flagged a project with ZERO install-script dependencies, which is how a false positive would have
  # discredited the real finding. nub legitimately creates three of its own trees (`.cache/nub`,
  # `.local/share/nub`, `Library/Caches/nub`) plus their bare container directories, and counting empty
  # containers as escaped state fires on every project. Measured discriminator: a zero-script project's
  # home holds ONLY paths under a `nub` directory, while a jailed `puppeteer` adds `.cache/puppeteer`.
  local escaped
  # ⛔ NO `| head` HERE. `find | head` SIGPIPEs the producer, and under `set -o pipefail` that
  # became exit 141 with ZERO output — the whole suite dying before its first line, which reads
  # like a crash rather than a result. Measured: this exact shape killed the gate silently. The
  # truncation happens in the shell after the pipeline completes instead.
  escaped="$(cd "$jhome" 2>/dev/null && find . -type f "${CURATED_FIND_ARGS[@]}" 2>/dev/null | tr '\n' ' ')"
  escaped="$(printf '%s' "$escaped" | cut -d' ' -f1-6)"
  if [ -n "$escaped" ]; then
    ESCAPES=$((ESCAPES + 1))
    summary="${summary}  ⛔ ${name}  ESCAPED CONFINEMENT — a jailed script wrote outside the jail: ${escaped}"$'\n'
    echo "── ${name}: files a jailed install left in its own HOME (none should exist) ──"
    (cd "$jhome" && find . -type f "${CURATED_FIND_ARGS[@]}" -exec ls -la {} \; 2>/dev/null \
      | awk '{printf "    %s bytes  %s\n", $5, $NF}' | sort -rn | head -5)
  fi

  local counts uncat
  counts="$(node "$HERE/uncatalogued.mjs" --install-log "$log" --catalog "$CATALOG" 2>/dev/null | head -1)"
  uncat="$(printf '%s' "$counts" | sed -nE 's/.*UNCATALOGUED ([0-9]+).*/\1/p')"; uncat="${uncat:-0}"
  UNCAT_TOTAL=$((UNCAT_TOTAL + uncat))

  # ⛔ ONE VERDICT LINE PER PROJECT. An escaping install still exits 0, so without this a project that
  # escaped printed twice — once as ⛔ and again as ✓ — and a reader scanning for green ticks would take
  # the second line as the answer. The escape already recorded its own line above.
  if [ "$rc" -eq 0 ]; then
    if [ -z "$escaped" ]; then
      PASS=$((PASS + 1)); summary="${summary}  ✓ ${name}  ${counts}"$'\n'
    fi
    return
  fi

  # Classify: jail-off arm, unique root name so the memo cannot replay the jailed result.
  local off="$ROOT/$name-off"; mkdir -p "$off"
  printf '{"name":"sweepoff-%s","version":"1.0.0","dependencies":%s}\n' "$name" "$deps" > "$off/package.json"
  printf '{"install":{"buildJail":false}}\n' > "$off/nub.jsonc"
  printf 'side-effects-cache=false\n' > "$off/.npmrc"
  # ⛔⛔ THE JAIL-OFF ARM GETS A THROWAWAY HOME, AND OMITTING IT POISONED A LATER RUN. Unjailed means
  # the script writes the REAL `$HOME` — that is the entire point of the arm — so an interrupted or
  # failing download leaves a HALF-POPULATED cache in the user's own home. Measured here: a partial
  # `~/.cache/puppeteer` (the browser folder present, the executable missing) made every later puppeteer
  # install fail, jailed AND unjailed, with an error that reads like a confinement defect and is not one.
  # The sweep must not be able to damage the machine it runs on, and it must not manufacture its own
  # next failure.
  local offhome; offhome="$(mktemp -d "$ROOT/$name-offhome-XXXXXX")"
  local offrc=0
  ( cd "$off" && HOME="$offhome" NUB_CACHE_DIR="$offhome/nubcache" "$NUB" install > "$ROOT/$name.off.log" 2>&1 ) || offrc=$?

  if [ "$offrc" -ne 0 ]; then
    BOTHFAIL=$((BOTHFAIL + 1))
    summary="${summary}  ~ ${name}  fails BOTH ways (jailed rc=${rc}, unjailed rc=${offrc}) — not the jail  ${counts}"$'\n'
  else
    JAILFAIL=$((JAILFAIL + 1))
    summary="${summary}  ✗ ${name}  jailed rc=${rc}, UNJAILED rc=0 — THE JAIL IS THE DIFFERENCE  ${counts}"$'\n'
    echo "── ${name}: the jail is the difference; jailed output tail ──"
    grep -iE 'error|failed|denied|EACCES|EPERM|ETIMEDOUT|gyp' "$log" | tail -12
  fi
}

# ── Band 1: real framework application dependency sets ────────────────────────────────────────────
one next        '{"next":"15.5.9","react":"^19.0.0","react-dom":"^19.0.0"}'
one nuxt        '{"nuxt":"^3.15.0"}'
one angular     '{"@angular/cli":"^19.0.0","@angular/core":"^19.0.0"}'
one svelte      '{"svelte":"^5.0.0","@sveltejs/kit":"^2.0.0","vite":"^6.0.0"}'
one astro       '{"astro":"^5.0.0"}'
one remix       '{"@remix-run/dev":"^2.15.0","@remix-run/node":"^2.15.0"}'
one vite-react  '{"vite":"^6.0.0","react":"^19.0.0","@vitejs/plugin-react":"^4.3.0"}'
one nest        '{"@nestjs/core":"^10.4.0","@nestjs/common":"^10.4.0","reflect-metadata":"^0.2.0"}'

# ── Band 2: the packages that actually compile or download at install time ─────────────────────────
one electron    '{"electron":"^33.0.0"}'
one playwright  '{"playwright":"^1.49.0"}'
one puppeteer   '{"puppeteer":"^23.0.0"}'
one cypress     '{"cypress":"^13.17.0"}'
one sharp       '{"sharp":"^0.33.0"}'
one prisma      '{"prisma":"^6.0.0","@prisma/client":"^6.0.0"}'
one swc         '{"@swc/core":"^1.10.0"}'
one esbuild-bin '{"esbuild":"^0.24.0"}'
one sqlite3     '{"sqlite3":"^5.1.7"}'
one bcrypt      '{"bcrypt":"^5.1.1"}'
one node-pty    '{"node-pty":"^1.0.0"}'
one sass-embed  '{"sass-embedded":"^1.83.0"}'
one storybook   '{"storybook":"^8.4.0"}'
one imagemin    '{"gifsicle":"^7.0.1","optipng-bin":"^9.0.0","jpegtran-bin":"^7.0.0"}'

echo
echo "════ SWEEP RESULT ════"
echo "  clean installs:                 $PASS"
echo "  THE JAIL IS THE DIFFERENCE:     $JAILFAIL"
echo "  fail both ways (not the jail):  $BOTHFAIL"
echo "  ⛔ ESCAPED CONFINEMENT:          $ESCAPES"
echo "  uncatalogued install-script deps across all projects: $UNCAT_TOTAL"
echo
printf '%s' "$summary"
echo "logs: $ROOT"
# ⛔ AN ESCAPE FAILS THE RUN. A package that installs fine while writing outside the jail is the
# failure this suite exists to catch; treating it as a warning would repeat the mistake that let
# it through the first time.
# ⛔ AN ESCAPE FAILS THE GATE, and it stayed a gate after one understood false positive tried to
# talk it into a report. Promotion's destinations are excluded at the source (see
# `curated-home-paths.sh`); anything left really is a write nobody designed.
[ "$JAILFAIL" -eq 0 ] && [ "$ESCAPES" -eq 0 ] || exit 1
