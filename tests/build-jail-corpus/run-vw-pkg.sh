#!/bin/bash
# run-vw-pkg.sh — ONE package, ONE arm, against the rerooted "virgin world" prototype.
#
# This is the corpus harness's method (README.md) pointed at a different mechanism.
# The verdict logic is UNCHANGED: lib/snapshot.mjs, lib/delta.mjs and lib/verdict.mjs
# are used verbatim, and classes.json supplies the same per-class work predicates.
# Only the two ARMS differ, because the prototype has no per-package switch:
#
#   HOST   npm install on the machine, nothing confining it     (the validity baseline)
#   WORLD  the identical install inside jail --root <rootfs>    (the design under test)
#
# Per-PACKAGE rather than per-shard, which the shard harness could not do because a
# lifecycle log is not framed per package. One package per install makes `pkg:` a real
# per-package root and makes the process exit code attributable, so `attribute.mjs`'s
# store-cell parse is not needed here.
#
# Everything the run observes lives under one directory that is the jail's single bind
# mount, so HOME and TMPDIR are inside the world AND readable afterwards from the host.
# A tmpfs-upper HOME would be discarded with the namespace and every home-cache write
# would read as NEVER-RAN.
set -u -o pipefail

PKG="${1:?package}"; VER="${2:?version}"; CLASS="${3:?class}"; NEED="${4-}"; ARM="${5:?HOST|WORLD}"
HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORLD="${VW_WORLD:?set VW_WORLD to the prepared rootfs}"
JAIL="${VW_JAIL:?set VW_JAIL to the jail binary}"
SAFE="$(printf '%s' "$PKG" | tr '/@' '__')"
OUT="${OUTROOT:-/tmp/vwc}/$SAFE-$ARM"
rm -rf "$OUT"; mkdir -p "$OUT"

RUNID="$(date +%s)$RANDOM"; NONCE="NubProbe${RUNID}"
PROJ="$OUT/proj"; H="$OUT/home"; TMPD="$OUT/tmp"
mkdir -p "$PROJ" "$H" "$TMPD"
LOG="$OUT/run.log"          # outside every observed root, per the harness README

# One package per install, with ONE exception the shard harness documents: the 6.x
# client's codegen postinstall shells out to the `prisma` CLI, so installed alone it
# gets no generator and exits 0 having done nothing. Splitting the shard split that
# pair, and the A0 baseline then failed for a fixture reason rather than a jail one.
PEER=""
case "$NEED" in *prisma-schema*)
  [ "$PKG" = "prisma" ]          && PEER=', "@prisma/client": "6.19.3"'
  [ "$PKG" = "@prisma/client" ]  && PEER=', "prisma": "6.19.3"' ;;
esac
cat > "$PROJ/package.json" <<EOF
{ "name": "vwc-$SAFE", "version": "1.0.0", "private": true,
  "dependencies": { "$PKG": "$VER"$PEER } }
EOF
printf 'minimumReleaseAge=0\nminimumReleaseAgeStrict=false\naudit=false\nfund=false\n' > "$PROJ/.npmrc"

# Fixture provisioning, copied from run-shard.sh so both harnesses reach the same
# real code path. Done on the HOST side: the project is bind-mounted, so a .git dir
# or a schema created here is present inside the world too.
provision() {
  case "$1" in
    git-repo)
      git -C "$PROJ" init -q 2>/dev/null || true
      git -C "$PROJ" config user.email probe@example.com
      git -C "$PROJ" config user.name probe
      git -C "$PROJ" commit -qm probe --allow-empty 2>/dev/null || true ;;
    prisma-schema)
      mkdir -p "$PROJ/prisma"
      cat > "$PROJ/prisma/schema.prisma" <<EOS
generator client {
  provider = "prisma-client-js"
}
datasource db {
  provider = "sqlite"
  url      = "file:./dev.db"
}
model $NONCE {
  id Int @id @default(autoincrement())
}
EOS
      ;;
    lefthook-config) printf 'pre-commit:\n  commands:\n    probe:\n      run: echo %s\n' "$NONCE" > "$PROJ/lefthook.yml" ;;
    msw-manifest)
      mkdir -p "$PROJ/public"
      node -e 'const f=process.argv[1],j=JSON.parse(require("fs").readFileSync(f,"utf8"));j.msw={workerDirectory:["public"]};require("fs").writeFileSync(f,JSON.stringify(j,null,2));' "$PROJ/package.json" ;;
    nx-json) printf '{ "affected": { "defaultBase": "main" } }\n' > "$PROJ/nx.json" ;;
    vue-dep)
      node -e 'const f=process.argv[1],j=JSON.parse(require("fs").readFileSync(f,"utf8"));j.dependencies.vue="3.5.13";require("fs").writeFileSync(f,JSON.stringify(j,null,2));' "$PROJ/package.json" ;;
  esac
}
for n in $(printf '%s' "$NEED" | tr ',' ' '); do [ -n "$n" ] && provision "$n"; done

# Per classes.json: the gyp class must be forced to COMPILE (a prebuild download is a
# bail-out of the path under test) and the prebuilt class must be forced to DOWNLOAD.
# Identical in both arms, so the compile/download choice is never the variable.
case "$CLASS" in
  native-build-gyp)      FORCE_BFS=true ;;
  native-build-prebuilt) FORCE_BFS=false ;;
  *)                     FORCE_BFS= ;;
esac

snap() {
  SNAP_MAX_ENTRIES="${SNAP_MAX_ENTRIES:-4000000}" \
    node "$HARNESS/lib/snapshot.mjs" "$1" \
      "proj=$PROJ" "pkg=$PROJ/node_modules/$PKG" "nm=$PROJ/node_modules" \
      "home=$H" "tmp=$TMPD" 2>> "$LOG"
  local rc=$?
  [ $rc -eq 0 ] || { echo "FATAL: snapshot $1 rc=$rc — INADMISSIBLE" | tee -a "$LOG"; exit 9; }
}

{ echo "=== PROVENANCE ==="
  echo "pkg=$PKG@$VER class=$CLASS need=${NEED:-none} arm=$ARM nonce=$NONCE"
  echo "world=$WORLD jail=$JAIL"
  echo "host_node=$(node --version) host_npm=$(npm --version)"
  echo "date=$(date -u +%FT%TZ)"; } >> "$LOG"

snap "$OUT/pre.ndjson"
T0=$(date +%s)
# A hung install must not stall the corpus; 600 s is ~40x the slowest pilot install.
TO="$(command -v timeout || true)"
if [ "$ARM" = "HOST" ]; then
  # env -i so the two arms see the same variable set; the jail scrubs env to a fixed
  # eight, and an inherited *_SKIP_DOWNLOAD or npm_config_* would be a silent bail-out.
  env -i HOME="$H" TMPDIR="$TMPD" PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin \
      LANG=C.UTF-8 SHELL=/bin/sh USER="$(id -un)" \
      ${FORCE_BFS:+npm_config_build_from_source=$FORCE_BFS} \
      ${TO:+$TO 600} sh -c "cd '$PROJ' && npm install --no-audit --no-fund" >> "$LOG" 2>&1
  RC=$?
else
  ${TO:+$TO 600} "$JAIL" --root "$WORLD" --bind "$OUT:/work" --uid 1000 --cwd /work/proj \
      --env HOME=/work/home --env TMPDIR=/work/tmp \
      ${FORCE_BFS:+--env npm_config_build_from_source=$FORCE_BFS} \
      -- /bin/sh -c "cd /work/proj && npm install --no-audit --no-fund" >> "$LOG" 2>&1
  RC=$?
fi
WALL=$(( $(date +%s) - T0 ))
snap "$OUT/post.ndjson"

node "$HARNESS/lib/delta.mjs" "$OUT/pre.ndjson" "$OUT/post.ndjson" "$OUT/delta.json" >> "$LOG" 2>&1

# `env`, not an assignment prefix: a ${VAR:+...} word is not parsed as an assignment,
# so an unset A0_CLASS_EFFECT ends the prefix and the next assignment is run as a
# command. That failed the verdict step for every package while the run still
# reported a wall time, which is exactly the shape of a silently empty result set.
env RUN_RC=$RC RUN_NONCE="$NONCE" \
  ${A0_CLASS_EFFECT:+A0_CLASS_EFFECT=$A0_CLASS_EFFECT} \
  ROOTS_JSON="{\"proj\":\"$PROJ\",\"pkg\":\"$PROJ/node_modules/$PKG\",\"nm\":\"$PROJ/node_modules\",\"home\":\"$H\",\"tmp\":\"$TMPD\"}" \
  node "$HARNESS/lib/verdict.mjs" "$OUT/delta.json" "$LOG" "$CLASS" "$HARNESS/lib/classes.json" "$OUT/verdict.json" > /dev/null 2>> "$LOG"
[ -s "$OUT/verdict.json" ] || { echo "FATAL: verdict step produced nothing for $PKG/$ARM" | tee -a "$LOG"; exit 8; }

node -e '
  const fs=require("fs"), [v,pkg,ver,cls,arm,rc,wall]=process.argv.slice(1);
  const o=JSON.parse(fs.readFileSync(v,"utf8"));
  o.pkg=pkg; o.version=ver; o.arm=arm; o.rc=Number(rc); o.wall_s=Number(wall);
  fs.writeFileSync(v, JSON.stringify(o));
  console.log([pkg,ver,cls,arm,o.verdict,"rc="+rc,wall+"s"].join("\t"));
' "$OUT/verdict.json" "$PKG" "$VER" "$CLASS" "$ARM" "$RC" "$WALL"

# RECLAIM. Each arm materialises a full node_modules plus two whole-tree snapshots;
# 352 packages x 2 arms filled a 29 GB disk mid-run, and once the filesystem was full
# every subsequent snapshot aborted with "INADMISSIBLE" -- a harness failure that
# reads like a corpus result. Everything the aggregation needs (verdict.json,
# delta.json, run.log) is small and is kept; the bulk is dropped the moment the
# verdict has been computed, never before, because content_nonce reads real files.
rm -rf "$PROJ" "$H" "$TMPD" "$OUT/pre.ndjson" "$OUT/post.ndjson"
