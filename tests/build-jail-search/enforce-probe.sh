#!/usr/bin/env bash
# Does the build jail actually ENFORCE what the compiler emits?
#
# Usage:  ./enforce-probe.sh /path/to/nub          # built with build-jail-catalog-override
#
# WHY THIS EXISTS AND WHY IT IS NOT A `cargo test`. A unit test proves the COMPILER EMITS A
# RULE. It does not prove the BACKEND ENFORCES ONE, and the difference is not academic:
# `write.deps` passed its unit test while being INERT in every real install, because
# `resolve_dependency_dir` clamps the resolved path to inside the project and a dependency
# under the global virtual store is not. The unit fixture was project-local, so the clamp
# never fired. Only a real jailed spawn against a real store layout can tell the two apart,
# and that needs a network install — so this is a script you run, not a gate that runs itself.
#
# Every arm here is a DIFFERENTIAL: same binary, same fixture, only the catalog differs. An
# arm that cannot show the override engaged is not an arm and is reported as a harness error
# rather than a result.

set -u
NUB="${1:-}"
[ -x "$NUB" ] || { echo "usage: $0 /path/to/nub"; exit 2; }
# ABSOLUTE, always: every arm cds into its own fixture before invoking nub, so a relative path
# silently stops resolving and every arm reports "no marker" instead of a result.
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac

# NEVER /tmp: a /tmp/package.json on some machines is found by walking up, and it silently
# becomes the project root.
ROOT="$HOME/.cache/nub-enforce-probe"
rm -rf "$ROOT"
fails=0

banner_ok () { grep -qh 'OVERRIDDEN from' "$@" 2>/dev/null && ! grep -qh 'was REJECTED' "$@" 2>/dev/null; }

# --- arm 1: a write OUTSIDE the project must be denied, with no grant in play --------------
arm_outside () {
  local d="$ROOT/outside"
  mkdir -p "$d/proj/dep" "$d/home" "$d/target"
  cat > "$d/proj/dep/package.json" <<JSON
{"name":"probe-dep","version":"1.0.0","scripts":{"postinstall":"node -e \"try{require('fs').writeFileSync('$d/target/x.txt','x');console.log('OUTSIDE-WRITE-OK')}catch(e){console.log('OUTSIDE-WRITE-DENIED '+e.code)}\""}}
JSON
  printf '{"name":"f","version":"1.0.0","dependencies":{"probe-dep":"file:./dep"}}' > "$d/proj/package.json"
  printf 'side-effects-cache=false\n' > "$d/proj/.npmrc"
  ( cd "$d/proj" && HOME="$d/home" "$NUB" install > i.log 2>&1
    HOME="$d/home" "$NUB" approve-builds --all > a.log 2>&1 )
  local got
  got=$(grep -hoE 'OUTSIDE-WRITE-(OK|DENIED [A-Z]+)' "$d/proj"/*.log | head -1)
  if [ "${got%% *}" = "OUTSIDE-WRITE-DENIED" ]; then
    echo "  PASS  a write outside the project is denied  ($got)"
  else
    echo "  FAIL  expected a denial outside the project, got: ${got:-<no marker — did the script run?>}"
    fails=$((fails + 1))
  fi
}

# --- arm 2: write.deps reaches a DECLARED dependency, and only with the grant ---------------
# The dependency resolves into the GLOBAL virtual store, which is exactly the path the v1
# project clamp discarded. Both directions are asserted: a grant that never denies proves
# nothing, and neither does a denial that no grant can lift.
arm_deps () {
  local d="$ROOT/deps"
  mkdir -p "$d/proj/a"
  cat > "$d/proj/a/package.json" <<'JSON'
{"name":"pa","version":"1.0.0","dependencies":{"is-odd":"3.0.1"},
 "scripts":{"postinstall":"node -e \"const p=require('path').dirname(require.resolve('is-odd/package.json'));try{require('fs').writeFileSync(p+'/probe.txt','x');console.log('DEPS-WRITE-OK')}catch(e){console.log('DEPS-WRITE-DENIED '+e.code)}\""}}
JSON
  printf '{"name":"f","version":"1.0.0","dependencies":{"pa":"file:./a"}}' > "$d/proj/package.json"
  printf 'side-effects-cache=false\n' > "$d/proj/.npmrc"
  printf '{"packages":{"pa":[{"write":{"deps":true},"notes":"enforcement probe: reach a declared dependency"}]}}' > "$d/grant.json"
  printf '{"packages":{}}' > "$d/nogrant.json"

  local arm expect got
  for arm in grant nogrant; do
    [ "$arm" = grant ] && expect=DEPS-WRITE-OK || expect=DEPS-WRITE-DENIED
    rm -rf "$d/home"; mkdir -p "$d/home"
    ( cd "$d/proj" && HOME="$d/home" NUB_BUILD_JAIL_CATALOG="$d/$arm.json" "$NUB" install > "i-$arm.log" 2>&1
      HOME="$d/home" NUB_BUILD_JAIL_CATALOG="$d/$arm.json" "$NUB" approve-builds --all > "a-$arm.log" 2>&1 )
    if ! banner_ok "$d/proj/i-$arm.log" "$d/proj/a-$arm.log"; then
      echo "  HARNESS  the catalog override did not engage in the '$arm' arm — result discarded"
      fails=$((fails + 1)); continue
    fi
    got=$(grep -hoE 'DEPS-WRITE-(OK|DENIED [A-Z]+)' "$d/proj/i-$arm.log" "$d/proj/a-$arm.log" | head -1)
    if [ "${got%% *}" = "$expect" ]; then
      echo "  PASS  write.deps '$arm' arm  ($got)"
    else
      echo "  FAIL  write.deps '$arm' arm: wanted $expect, got ${got:-<no marker>}"
      fails=$((fails + 1))
    fi
  done
}

echo "build-jail enforcement probe — $NUB"
arm_outside
arm_deps
rm -rf "$ROOT"
echo "failures: $fails"
exit $((fails > 0))
