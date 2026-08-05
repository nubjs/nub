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
#
# COMPILE_FLAGS adds flags to every fixture's build, which is how the second
# shape gets covered: `COMPILE_FLAGS=--smol` ships no Node blob and finds one at
# run time, so it computes the injected flag set against a version the build
# never saw. That is the reason the flag policy is what it is, and it is a whole
# code path the default shape never touches.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NUB="${NUB:?set NUB to the nub binary under test}"
NODE_VERSION="${1:-26.5.0}"
WORK="${WORK:-${TMPDIR:-/tmp}/nub-compile-augmentation}"
read -r -a EXTRA_COMPILE_FLAGS <<< "${COMPILE_FLAGS:-}"

rm -rf "$WORK"; mkdir -p "$WORK"; cd "$WORK" || exit 1
printf '%s\n' "$NODE_VERSION" > .node-version
npm init -y >/dev/null 2>&1
# The transform fixtures import real packages — the JSX runtime and the metadata
# shim — because a transform is only proven by the thing it emits actually
# resolving. Installed once here rather than vendored into the repo.
npm i --silent --no-audit --no-fund react reflect-metadata >/dev/null 2>&1 \
  || { echo "could not install the fixture dependencies (offline?)" >&2; exit 2; }

# The Node the artifact will embed, so the plain-node column is the SAME build
# the other two columns run — otherwise a difference could be a version gap
# rather than an augmentation gap.
PLAIN_NODE="$HOME/.cache/nub/node/$NODE_VERSION/bin/node"
[ -x "$PLAIN_NODE" ] || PLAIN_NODE="$(command -v node)"

pass=0; fail=0; discriminating=0; vacuous=0
printf '%-22s %-24s %-24s %s\n' FIXTURE 'nub (reference)' 'artifact' VERDICT
printf '%s\n' "$(printf '%.0s-' {1..92})"

# The entry EXTENSION is part of what is under test — a `.ts` entry exercises the
# transform path a `.mjs` one never reaches — so fixtures keep their own and the
# glob must not quietly narrow to one. An earlier `*.mjs` here skipped the whole
# TypeScript fixture and still reported every row green.
shopt -s nullglob
fixtures=("$HERE"/fixtures/*.mjs "$HERE"/fixtures/*.ts "$HERE"/fixtures/*.tsx "$HERE"/fixtures/*.cjs)
shopt -u nullglob
if [ "${#fixtures[@]}" -eq 0 ]; then
  echo "no fixtures matched — the harness would report green having tested nothing" >&2
  exit 2
fi

for fixture in "${fixtures[@]}"; do
  base="$(basename "$fixture")"
  name="${base%.*}"
  ext="${base##*.}"
  entry="app.$ext"
  cp "$fixture" "./$entry"
  # Anything the fixture needs beside it (a data file, a tsconfig, a worker entry).
  [ -d "$HERE/fixtures/$name.d" ] && cp -R "$HERE/fixtures/$name.d/." ./

  # Last line AND exit status. A compiled CLI's exit code is contract — a binary
  # that returns 0 where the program returned 1 breaks every script wrapping it,
  # and the launcher spawns the user's Node as a child, so forwarding it is a
  # thing that can silently go wrong. Comparing only stdout also let a fixture
  # print the right line and then die: the status is what closes that.
  #
  # Captured on its own line, never through the pipe — `$(cmd | tail -1)` would
  # report tail's status and always be 0.
  plain_out="$("$PLAIN_NODE" "$entry" 2>&1)"; plain_rc=$?
  plain="$(printf '%s' "$plain_out" | tail -1) rc=$plain_rc"
  ref_out="$("$NUB" "$entry" 2>&1)"; ref_rc=$?
  ref="$(printf '%s' "$ref_out" | tail -1) rc=$ref_rc"

  # A fixture may need compile flags of its own — source maps are off by default,
  # so the one that checks stack traces cannot be tested without asking for them.
  # Kept beside the fixture for the same reason `.differs` is: the alternative is
  # a table in here that drifts from the fixtures it describes.
  compile_flags=()
  if [ -f "$HERE/fixtures/$name.flags" ]; then
    while read -r flag; do [ -n "$flag" ] && compile_flags+=("$flag"); done < "$HERE/fixtures/$name.flags"
  fi

  if "$NUB" compile "$entry" ${compile_flags[@]+"${compile_flags[@]}"} \
       ${EXTRA_COMPILE_FLAGS[@]+"${EXTRA_COMPILE_FLAGS[@]}"} --out ./bin >./build.log 2>&1; then
    rm -rf ./cache
    got_out="$(cd "${TMPDIR:-/tmp}" && XDG_CACHE_HOME="$WORK/cache" "$WORK/bin" 2>&1)"; got_rc=$?
    got="$(printf '%s' "$got_out" | tail -1) rc=$got_rc"
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
shape="${COMPILE_FLAGS:-}"
echo "node $NODE_VERSION${shape:+ ($shape)}: $pass ok, $fail failed; $discriminating fixtures where nub changed the answer, $vacuous where it did not"
[ "$fail" = 0 ]
