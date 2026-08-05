#!/usr/bin/env bash
# Does a COMPILED artifact still make every augmentation guarantee `nub` makes?
#
# The mission this guards is that nub is erasable: a compiled program runs on
# plain Node with nub's runtime machinery gone, and behaves identically anyway,
# because the work moved to build time. Every fixture here is therefore run
# THREE ways and the three are compared:
#
#   plain node   — the baseline. What the program would do with no nub at all.
#   nub <file>   — the reference. What the author saw while developing.
#   the artifact — must equal the REFERENCE, not the baseline.
#
# The plain-node column is not decoration: it is the positive control. A row
# where all three agree proves nothing, because the augmentation was not
# load-bearing on that Node version — the fixture would pass with nub deleted.
# Rows where plain differs from nub are the ones carrying the evidence, and the
# summary counts them, so a run that silently stops discriminating is visible.
#
# Usage:  NUB=/path/to/nub tests/compile-augmentation/run.sh [node-version]
# The launcher template is needed for a dev build that has no embedded one:
#   __NUB_LAUNCHER_TEMPLATE=crates/nub-launcher/target/release/nub-launcher
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NUB="${NUB:?set NUB to the nub binary under test}"
NODE_VERSION="${1:-26.5.0}"
WORK="${WORK:-${TMPDIR:-/tmp}/nub-compile-augmentation}"

rm -rf "$WORK"; mkdir -p "$WORK"; cd "$WORK" || exit 1
printf '%s\n' "$NODE_VERSION" > .node-version
npm init -y >/dev/null 2>&1

# The Node the artifact will embed, so the plain-node column is the SAME build
# the other two columns run — otherwise a difference could be a version gap
# rather than an augmentation gap.
PLAIN_NODE="$HOME/.cache/nub/node/$NODE_VERSION/bin/node"
[ -x "$PLAIN_NODE" ] || PLAIN_NODE="$(command -v node)"

pass=0; fail=0; discriminating=0; vacuous=0
printf '%-22s %-24s %-24s %s\n' FIXTURE 'nub (reference)' 'artifact' VERDICT
printf '%s\n' "$(printf '%.0s-' {1..92})"

for fixture in "$HERE"/fixtures/*.mjs; do
  name="$(basename "$fixture" .mjs)"
  cp "$fixture" ./app.mjs
  # Anything the fixture needs beside it (a data file, a worker entry).
  [ -d "$HERE/fixtures/$name.d" ] && cp -R "$HERE/fixtures/$name.d/." ./

  plain="$("$PLAIN_NODE" app.mjs 2>&1 | tail -1)"
  ref="$("$NUB" app.mjs 2>&1 | tail -1)"

  if "$NUB" compile app.mjs --out ./bin >./build.log 2>&1; then
    rm -rf ./cache
    got="$(cd "${TMPDIR:-/tmp}" && XDG_CACHE_HOME="$WORK/cache" "$WORK/bin" 2>&1 | tail -1)"
  else
    got="<build failed: $(grep -m1 -iE 'error' ./build.log | cut -c1-60)>"
  fi
  rm -f ./bin

  # A few augmentations are SUPPOSED to disappear when compiled — .env loading
  # above all, because a baked program's configuration must not depend on the
  # directory it happens to be started in. Those carry a `.differs` file holding
  # the reason, and the assertion inverts: matching the reference would be the
  # bug. Keeping the reason beside the fixture is what stops the list of
  # exceptions growing quietly.
  reason=""
  [ -f "$HERE/fixtures/$name.differs" ] && reason="$(head -1 "$HERE/fixtures/$name.differs")"

  if [ -n "$reason" ]; then
    if [ "$got" != "$ref" ]; then
      verdict="ok (differs on purpose: $reason)"; pass=$((pass + 1))
    else
      verdict="FAIL — expected to differ: $reason"; fail=$((fail + 1))
    fi
  elif [ "$got" = "$ref" ]; then
    verdict=ok; pass=$((pass + 1))
  else
    verdict='FAIL'; fail=$((fail + 1))
  fi
  # Did nub change anything here at all? If not, the row cannot fail for the
  # right reason and is reported so the coverage claim stays honest.
  if [ "$plain" != "$ref" ]; then
    discriminating=$((discriminating + 1))
  else
    vacuous=$((vacuous + 1)); verdict="$verdict (vacuous on $NODE_VERSION)"
  fi
  printf '%-22s %-24s %-24s %s\n' "$name" "${ref:0:24}" "${got:0:24}" "$verdict"
done

echo
echo "node $NODE_VERSION: $pass ok, $fail failed; $discriminating fixtures where nub changed the answer, $vacuous where it did not"
[ "$fail" = 0 ]
