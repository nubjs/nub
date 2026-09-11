#!/usr/bin/env bash
# npm-incumbent corpus: real projects whose package manager is npm, pinned by commit in
# corpus.tsv, frozen-installed under nub on a cold store from their own package-lock.json,
# then checked against that lockfile (check-tree.mjs). The verdict per project is
# "installs and satisfies its lockfile", which is the contract `npm ci` gives them today.
#
#   PASS         install exit 0, lifecycle scripts included, and the tree checks clean
#   FAIL         nub fails where `npm ci` succeeds — a nub defect
#   XFAIL        listed in expected-failures.txt (a known, tracked red); passing is XPASS-STALE
#   XFAIL-ENV    `npm ci` fails on this runner too, so the red is the environment or the pin
#
# Usage: run.sh <path-to-nub> [owner/repo ...]      (no repos = the whole corpus)
# Env:   CONTROL=on-failure|always|never  run `npm ci` on a second, pristine checkout of the
#                                         same commit with a cold cache. on-failure (default)
#                                         classifies a red; always also records the wall-clock
#                                         of both installs.
#        WORK=<dir>   clone and install here (default: a fresh temp dir, removed on success)
#        KEEP=1       keep the work dir
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NUB_ARG="${1:?usage: run.sh <path-to-nub> [owner/repo ...]}"
shift
NUB="$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }
CONTROL="${CONTROL:-on-failure}"
case "$CONTROL" in on-failure|always|never) ;; *) echo "error: CONTROL must be on-failure, always or never" >&2; exit 2 ;; esac
KEEP="${KEEP:-0}"
CREATED_WORK=0
if [ -z "${WORK:-}" ]; then
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-npm-corpus.XXXXXX")"
  CREATED_WORK=1
fi
cleanup() {
  local code=$?
  if [ "$CREATED_WORK" -eq 1 ] && [ "$KEEP" = "0" ] && [ "$code" -eq 0 ]; then rm -rf "$WORK"
  elif [ "$code" -ne 0 ]; then echo "(work dir preserved for inspection at $WORK)"; fi
}
trap cleanup EXIT
TIMEOUT=()
command -v timeout >/dev/null 2>&1 && TIMEOUT=(timeout 1500)

# Selection: the arguments, or every non-comment line of corpus.tsv. Names are matched
# exactly; one that matches no line is an error rather than a silently empty run.
selected=("$@")
entries=()
matched=()
while IFS=$'\t' read -r repo commit node _notes; do
  if [ -z "$repo" ] || [ "${repo:0:1}" = "#" ]; then continue; fi
  if [ "${#selected[@]}" -gt 0 ]; then
    hit=0; for s in "${selected[@]}"; do [ "$s" = "$repo" ] && hit=1; done
    [ "$hit" -eq 1 ] || continue
  fi
  entries+=("$repo"$'\t'"$commit"$'\t'"$node")
  matched+=("$repo")
done < "$HERE/corpus.tsv"
for s in "${selected[@]}"; do
  hit=0; for m in "${matched[@]}"; do [ "$s" = "$m" ] && hit=1; done
  [ "$hit" -eq 1 ] || { echo "error: not in corpus.tsv: $s" >&2; exit 2; }
done
[ "${#entries[@]}" -gt 0 ] || { echo "error: nothing selected from corpus.tsv" >&2; exit 2; }

expected_reason() { # expected_reason <repo> → prints the reason, exits 1 if unlisted
  grep -v '^#' "$HERE/expected-failures.txt" | awk -v r="$1" '$1 == r { $1 = ""; sub(/^ /, ""); print; found = 1 } END { exit !found }'
}
elapsed() { echo $(( $(date +%s) - $1 )); }

echo "== npm corpus: ${#entries[@]} project(s), nub $("$NUB" --version 2>/dev/null | head -1), node $(node --version), control=$CONTROL =="
fails=0; stale=0; envreds=0; passes=0; xfails=0
RESULTS=()
for entry in "${entries[@]}"; do
  IFS=$'\t' read -r repo commit node <<< "$entry"
  name="${repo//\//__}"
  dir="$WORK/$name"
  rm -rf "$dir"; mkdir -p "$dir"
  xdg="$WORK/$name.home"
  export XDG_DATA_HOME="$xdg/data" XDG_CACHE_HOME="$xdg/cache" npm_config_cache="$xdg/npm-cache" CI=1
  echo
  echo "-- $repo @ ${commit:0:12} (their CI runs node $node)"
  if ! ( cd "$dir" && git init -q && git remote add origin "https://github.com/$repo.git" \
         && git fetch -q --depth 1 origin "$commit" && git checkout -q FETCH_HEAD ) > "$WORK/$name.clone.log" 2>&1; then
    echo "  FAIL  $repo — clone at $commit failed:"; tail -5 "$WORK/$name.clone.log" | sed 's/^/        /'
    fails=$((fails + 1)); RESULTS+=("$repo|FAIL|clone"); continue
  fi
  if [ ! -f "$dir/package-lock.json" ]; then
    echo "  FAIL  $repo — no package-lock.json at $commit"; fails=$((fails + 1)); RESULTS+=("$repo|FAIL|no lockfile"); continue
  fi

  nub_log="$WORK/$name.nub.log"
  t0=$(date +%s)
  ( cd "$dir" && "${TIMEOUT[@]}" "$NUB" install --frozen-lockfile ) > "$nub_log" 2>&1; rc=$?
  nub_s=$(elapsed "$t0")
  stage="install"
  if [ "$rc" -eq 0 ]; then
    stage="check-tree"
    node "$HERE/check-tree.mjs" "$dir" >> "$nub_log" 2>&1; rc=$?
  fi
  summary="$(grep -oE 'check-tree: .*' "$nub_log" | tail -1)"

  npm_note=""
  if [ "$CONTROL" = "always" ] || { [ "$CONTROL" = "on-failure" ] && [ "$rc" -ne 0 ]; }; then
    if command -v npm >/dev/null 2>&1; then
      # A second checkout of the pinned commit: nub's lifecycle scripts may have written into
      # the first one, and a residue that fails npm would read as an environment red.
      npm_log="$WORK/$name.npm.log"
      npm_dir="$dir.npm"
      rm -rf "$npm_dir"
      if git -C "$dir" worktree add -q --detach "$npm_dir" HEAD > "$npm_log" 2>&1; then
        t1=$(date +%s)
        ( cd "$npm_dir" && "${TIMEOUT[@]}" npm ci ) >> "$npm_log" 2>&1; npm_rc=$?
        npm_s=$(elapsed "$t1")
        if [ "$npm_rc" -eq 0 ]; then npm_note="npm ci ${npm_s}s"; else npm_note="npm ci FAILED (exit $npm_rc, ${npm_s}s)"; fi
      else
        # No control checkout means no control: the nub verdict stands on its own.
        npm_rc=-1; npm_note="control checkout failed, no control"
      fi
    else
      npm_rc=-1; npm_note="npm not on PATH, no control"
    fi
  else
    npm_rc=-1
  fi

  reason="$(expected_reason "$repo")" && listed=1 || listed=0
  if [ "$rc" -eq 0 ]; then
    if [ "$listed" -eq 1 ]; then
      echo "  XPASS-STALE $repo — now passes; remove its line from expected-failures.txt: $reason"
      stale=$((stale + 1)); RESULTS+=("$repo|XPASS-STALE|nub ${nub_s}s${npm_note:+, $npm_note}")
    else
      echo "  PASS  $repo — nub ${nub_s}s${npm_note:+, $npm_note}; $summary"
      passes=$((passes + 1)); RESULTS+=("$repo|PASS|nub ${nub_s}s${npm_note:+, $npm_note}")
    fi
  elif [ "$listed" -eq 1 ]; then
    echo "  XFAIL $repo — expected: $reason"
    xfails=$((xfails + 1)); RESULTS+=("$repo|XFAIL|$reason")
  elif [ "$npm_rc" -gt 0 ]; then
    echo "  XFAIL-ENV $repo — nub failed at $stage (exit $rc) and $npm_note here too; not counted. nub tail:"
    tail -8 "$nub_log" | sed 's/^/        /'; echo "        npm tail:"; tail -5 "$npm_log" | sed 's/^/        /'
    envreds=$((envreds + 1)); RESULTS+=("$repo|XFAIL-ENV|$stage, $npm_note")
  else
    echo "  FAIL  $repo — $stage failed (exit $rc, nub ${nub_s}s${npm_note:+, $npm_note}); tail:"
    tail -25 "$nub_log" | sed 's/^/        /'
    fails=$((fails + 1)); RESULTS+=("$repo|FAIL|$stage${npm_note:+, $npm_note}")
  fi
  [ "$KEEP" = "1" ] || rm -rf "$dir/node_modules" "$dir.npm" "$xdg"
done

echo
echo "== summary =="
for r in "${RESULTS[@]}"; do IFS='|' read -r repo verdict note <<< "$r"; printf "  %-12s %-36s %s\n" "$verdict" "$repo" "$note"; done
echo
if [ "$fails" -gt 0 ] || [ "$stale" -gt 0 ]; then
  echo "RESULT: FAIL ($fails nub failure(s), $stale stale expected-failure entry/entries; $passes passed, $xfails expected red, $envreds environment red)"
  exit 1
fi
echo "RESULT: OK ($passes passed, $xfails expected red, $envreds environment red)"
