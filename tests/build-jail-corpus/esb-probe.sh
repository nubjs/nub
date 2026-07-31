#!/usr/bin/env bash
# esb-probe.sh — settle esbuild across its two install mechanisms, one version per shard.
#
# WHY ONE VERSION PER SHARD: the break this probe re-examines was a window holding
# `esbuild@0.11.23` + `esbuild@0.28.1` together, because `approve-builds` takes a bare
# NAME and aube approves every pending version under it. A shard with exactly one
# version can only produce one window with one job, so the two versions are separated
# by construction rather than by parsing them apart afterwards.
#
# THE VERDICT IS THE EXIT CODE, plus one artifact that only the script can produce.
# esbuild ships `bin/esbuild` as a `#!/usr/bin/env node` TEXT shim in the tarball; the
# postinstall replaces it with the native executable (0.28.x hardlinks the resolved
# optionalDependency over it, 0.11.x renames a downloaded temp file onto it). So a
# Mach-O at that path is proof the script ran AND completed, and it is not replayable:
# the fixture sets `side-effects-cache=false` and every arm gets a fresh HOME and store.
set -u -o pipefail
HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSIONS="${VERSIONS:?set VERSIONS}"
export OUTROOT="${OUTROOT:-/tmp/esb-probe}"
export PER_PKG=1
RESULTS="$OUTROOT/results.tsv"
mkdir -p "$OUTROOT"
printf 'version\tarm\tarm_effect\tinstall_rc\tapprove_rc\twin_jobs\twin_rc\tbin_type\tbin_bytes\n' > "$RESULTS"

for v in $VERSIONS; do
  for arm in A0 PROD; do
    S="esb-$v"
    echo "### $v $arm" >&2
    "$HARNESS/run-shard.sh" "$S" "$HARNESS/shard-esb-$v.tsv" "$arm" >/dev/null 2>&1
    OUT="$OUTROOT/$S-$arm"
    LOG="$OUT/run.log"
    ae=$(grep -oE 'arm_effect=[^ ]+' "$LOG" 2>/dev/null | tail -1 | cut -d= -f2)
    irc=$(grep -oE '^install rc=[0-9]+' "$LOG" 2>/dev/null | tail -1 | cut -d= -f2)
    arc=$(grep -oE '^approve rc=[0-9]+' "$LOG" 2>/dev/null | tail -1 | cut -d= -f2)
    # windows.json is the runner's own record of what each window actually ran.
    wj=$(node -e '
      try{const r=require(process.argv[1]);
        process.stdout.write(r.map(w=>w.jobs.join("+")||w.name).join(";")+"\t"+r.map(w=>w.rc).join(";"));
      }catch(e){process.stdout.write("-\t-")}' "$OUT/windows.json" 2>/dev/null || printf -- "-\t-")
    # Resolve through the store symlink; `file` never executes the target.
    BIN="$OUT/proj/node_modules/esbuild/bin/esbuild"
    if [ -e "$BIN" ]; then
      bt=$(file -b "$BIN" | cut -c1-28)
      bb=$(wc -c < "$BIN" | tr -d ' ')
    else
      bt="ABSENT"; bb=0
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$v" "$arm" "${ae:--}" "${irc:--}" "${arc:--}" "$wj" "$bt" "$bb" >> "$RESULTS"
    # Reclaim immediately: the host is at 97% and each arm leaves a store behind.
    rm -rf "$OUT/proj/node_modules" "$OUT/cache" "$OUT/home" "$OUT"/*.ndjson
  done
done
echo "=== $RESULTS"
cat "$RESULTS"
