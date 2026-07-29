#!/usr/bin/env bash
# run-shard.sh — one shard, one arm, one pass.
#
# BASH, NEVER ZSH: zsh does not word-split unquoted expansions, and a three-root
# argument list once collapsed into one bogus root, making every snapshot empty.
#
# ARMS AVAILABLE WITHOUT PATCHING THE BINARY (verified in source at the tip):
#   A0    jail OFF   — per-package `dependenciesMeta.<name>.sandbox: false`. There is
#                      no global off-switch; main.rs installs the jail unconditionally.
#   PROD  jail ON, production default. What this MEANS differs by platform, and that
#         difference is what supplies two arms for free:
#           macOS   net = the curated $downloads allowlist, enforced through the proxy
#                   (Seatbelt pins egress to exactly the proxy port)  => the A3 cell
#           Linux   net = BINARY DENY. linux.rs passes per_host=false to build_seccomp
#                   unconditionally on the Landlock path, so $downloads is inert and
#                   socket() is refused outright                      => the A2 cell
#
# `NUB_CORPUS_NO_JAIL` and `NUB_CORPUS_NET_ARM` HAVE ZERO READERS on this line. A prior
# harness selected its arms with them and measured jail-on against jail-on. Hence the
# arm-effect assertion below, which fails the run rather than reporting a number.
set -u -o pipefail

SHARD="${1:?shard name}"; MANIFEST="${2:?manifest tsv}"; ARM="${3:?A0|PROD}"
HARNESS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NUB="${NUB_BIN:?set NUB_BIN}"; EXPECT_SHA="${NUB_EXPECT_GIT_SHA:?set NUB_EXPECT_GIT_SHA}"
LEVER="${LEVER:-}"
# THE FIXTURE PROJECT MUST NOT LIVE INSIDE A REPOSITORY. Lockfile discovery walks
# UP from the project root, so a fixture nested under a checkout inherits that
# checkout's own lockfiles and the install aborts before a single script runs
# (`ERR_NUB_LOCKFILE_AMBIGUOUS: multiple lockfiles found`). Defaulting the output
# root to a temp directory keeps the fixture clear of any enclosing repository;
# override OUTROOT only with a path that is likewise outside one.
OUT="${OUTROOT:-${TMPDIR:-/tmp}/build-jail-corpus}/$SHARD-$ARM${LEVER:+-$LEVER}"
rm -rf "$OUT"; mkdir -p "$OUT"

RUNID="$(date +%s)$RANDOM"; NONCE="NubProbe${RUNID}"
PROJ="$OUT/proj"; H="$OUT/home"; CACHE="$OUT/cache"; TMPD="$OUT/tmp"
mkdir -p "$PROJ" "$H" "$CACHE" "$TMPD"
# Outside every observed root. Logs written INSIDE the project once manufactured
# ACTED=true for every package, turning each NEVER-RAN into DID-WORK-AND-FAILED.
LOG="$OUT/run.log"

case "$(uname -s)" in Darwin) PLATFORM=macos ;; Linux) PLATFORM=linux ;; *) PLATFORM=other ;; esac
# A per-run private HOME beats wiping the side-effects cache between arms: the cache
# can grow large and was measured replaying one arm's side effect into another. A
# fresh HOME makes that structurally impossible rather than procedurally avoided.
{ echo "=== PROVENANCE ==="
  echo "shard=$SHARD arm=$ARM lever=${LEVER:-none} platform=$PLATFORM nonce=$NONCE"
  echo "nub_bin=$NUB"
  echo "nub_bin_sha256=$(shasum -a 256 "$NUB" 2>/dev/null | awk '{print $1}' || sha256sum "$NUB" | awk '{print $1}')"
  echo "expect_git_sha=$EXPECT_SHA"
  echo "nub_version=$("$NUB" --version 2>&1 | head -1)"
  echo "host=$(uname -srm)"
  echo "node=$(node --version 2>&1)"
  echo "date=$(date -u +%FT%TZ)"; } > "$LOG"

# ── manifest -> dependency set + the project-file capability UNION ─────────────
DEPS=""; NEEDS=""; PKGS=()
while IFS=$'\t' read -r PKG VER CLASS NEED; do
  [ -z "${PKG:-}" ] && continue; case "$PKG" in \#*) continue ;; esac
  DEPS="$DEPS,\"$PKG\":\"$VER\""; PKGS+=("$PKG")
  [ -n "${NEED:-}" ] && NEEDS="$NEEDS,$NEED"
done < "$MANIFEST"
DEPS="${DEPS#,}"

DEPS_META=""
if [ "$ARM" = "A0" ]; then
  M=""
  for p in "${PKGS[@]}"; do M="$M,\"$p\":{\"sandbox\":false}"; done
  DEPS_META=", \"dependenciesMeta\": { ${M#,} }"
fi

cat > "$PROJ/package.json" <<EOF
{ "name": "corpus-$SHARD", "version": "1.0.0", "private": true,
  "dependencies": { $DEPS }$DEPS_META }
EOF

# minimumReleaseAge blocks ~70 packages at RESOLUTION; the strict flag must go too.
{ echo "minimumReleaseAge=0"; echo "minimumReleaseAgeStrict=false"
  [ "$LEVER" = "OPTIONAL_OFF" ] && echo "optional=false"; } > "$PROJ/.npmrc"

provision() {
  case "$1" in
    git-repo)
      git -C "$PROJ" init -q 2>/dev/null || true
      git -C "$PROJ" config user.email probe@example.com
      git -C "$PROJ" config user.name probe
      git -C "$PROJ" commit -qm probe --allow-empty 2>/dev/null || true ;;
    prisma-schema)
      mkdir -p "$PROJ/prisma"
      # The nonce rides IN the generator input, so generated output that contains it
      # cannot have come from a cache — no cache could supply a name invented this run.
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
      node -e '
        const f=process.argv[1], j=JSON.parse(require("fs").readFileSync(f,"utf8"));
        j.msw={workerDirectory:["public"]};
        require("fs").writeFileSync(f, JSON.stringify(j,null,2));' "$PROJ/package.json" ;;
    nx-json) printf '{ "affected": { "defaultBase": "main" } }\n' > "$PROJ/nx.json" ;;
    vue-dep)
      node -e '
        const f=process.argv[1], j=JSON.parse(require("fs").readFileSync(f,"utf8"));
        j.dependencies.vue="3.5.13";
        require("fs").writeFileSync(f, JSON.stringify(j,null,2));' "$PROJ/package.json" ;;
  esac
}
PROVISIONED=""
for n in $(printf '%s' "${NEEDS#,}" | tr ',' ' '); do
  case " $PROVISIONED " in *" $n "*) continue ;; esac
  [ -n "$n" ] && { provision "$n"; PROVISIONED="$PROVISIONED $n"; }
done
# SUPPRESS withholds ONE capability, which is how a false pass is induced deliberately
# inside an otherwise-correct shard. It is the harness's own self-test.
if [ -n "${SUPPRESS:-}" ]; then
  case "$SUPPRESS" in
    prisma-schema) rm -rf "$PROJ/prisma" ;;
    git-repo) rm -rf "$PROJ/.git" ;;
    *) rm -f "$PROJ/$SUPPRESS" ;;
  esac
  echo "SUPPRESSED capability: $SUPPRESS" >> "$LOG"
fi
echo "provisioned:$PROVISIONED suppressed:${SUPPRESS:-none}" >> "$LOG"

# A snapshot that truncates is FATAL: the delta would be taken over a partial tree
# and every package past the cut would report NEVER-RAN. Abort rather than report.
snap() {
  SNAP_MAX_ENTRIES="${SNAP_MAX_ENTRIES:-4000000}" \
    node "$HARNESS/lib/snapshot.mjs" "$1" \
    "proj=$PROJ" "nm=$PROJ/node_modules" "home=$H" "store=$CACHE" "tmp=$TMPD" 2>> "$LOG"
  local rc=$?
  if [ $rc -ne 0 ]; then
    echo "FATAL: snapshot $1 failed rc=$rc — run is INADMISSIBLE" | tee -a "$LOG"
    exit 9
  fi
}

# ABSOLUTE paths only. The jail scrubs the env, so a var-based path degrades to a
# relative one and a "successful write" silently lands in the cwd.
FORCE=""
[ "$LEVER" = "npm_config_build_from_source=true" ] && FORCE="npm_config_build_from_source=true"
# `timeout` is GNU coreutils and is ABSENT from a stock macOS. A Mac with Homebrew
# coreutils has it, so a local run succeeds while a clean runner returns 127
# ("command not found") for every invocation — which reads as "nothing installed"
# rather than "the harness is broken". Resolve a real one or run without it.
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
[ -n "$TIMEOUT_BIN" ] || echo "note: no timeout(1) available; running unbounded" >> "$LOG"
runnub() {
  ( cd "$PROJ" && env -i \
      PATH="$STUDY_PATH" HOME="$H" TMPDIR="$TMPD" \
      NUB_CACHE_DIR="$CACHE" npm_config_cache="$CACHE/npm" \
      ${FORCE:+$FORCE} ${TIMEOUT_BIN:+"$TIMEOUT_BIN" 3000} "$NUB" "$@" ) >> "$LOG" 2>&1
  return $?
}

echo "--- phase 1: install (resolve+fetch+link) — OUTSIDE the window" >> "$LOG"
runnub install; RC_INSTALL=$?
echo "install rc=$RC_INSTALL" >> "$LOG"
snap "$OUT/pre.ndjson"

echo "--- phase 2: approve-builds  <<<< THE LIFECYCLE-SCRIPT WINDOW >>>>" >> "$LOG"
# NEVER dangerouslyAllowAllBuilds: it runs scripts DURING install, collapsing the two
# phases and destroying the measurement window the whole method rests on.
runnub approve-builds --all; RC_SCRIPT=$?
echo "approve rc=$RC_SCRIPT" >> "$LOG"
snap "$OUT/post.ndjson"

# ── ARM-EFFECT ASSERTION — the control that makes every number admissible ──────
WARN_COUNT=$(grep -c "running without the build sandbox" "$LOG" || true)
ARM_EFFECT=unknown
# THE PROD ASSERTION IS VACUOUSLY SATISFIABLE ON ITS OWN, so it carries a
# precondition. "No opt-out warning was printed" is true when the jail is
# correctly enforcing AND when nub never ran at all — a run where the binary was
# not found produced zero warnings and self-reported `confirmed` while installing
# nothing. A positive control that also passes when the experiment did not happen
# is not a control, so require evidence the install actually did work first.
INSTALLED_ANY=$(ls "$PROJ/node_modules" 2>/dev/null | grep -c . || true)
if [ "$RC_INSTALL" -ne 0 ] || [ "$INSTALLED_ANY" -eq 0 ]; then
  ARM_EFFECT="FAILED-install-did-not-run(rc=$RC_INSTALL,installed=$INSTALLED_ANY)"
elif [ "$ARM" = "A0" ]; then
  [ "$WARN_COUNT" -gt 0 ] && ARM_EFFECT=confirmed || ARM_EFFECT=FAILED-no-optout-warning
else
  [ "$WARN_COUNT" -eq 0 ] && ARM_EFFECT=confirmed || ARM_EFFECT=FAILED-optout-leaked
fi
# ── node-gyp IDENTITY — three facts, because one of them alone is ambiguous ────
# A bare-PATH `node-gyp` on this host resolves a stale global 3.8.0, so an
# unverified native-build verdict is worth nothing. The first cut grepped only the
# `gyp info using node-gyp@X` banner and reported `none-observed`, which conflates
# "node-gyp never ran" (the NORMAL case — these packages resolve a prebuild or
# node-gyp-build and never compile) with "we do not know which one ran". Those are
# different facts and only the second invalidates a verdict, so record all three.
GYP_ON_PATH=$(env -i PATH="$STUDY_PATH" bash -c 'command -v node-gyp' 2>/dev/null || true)
GYP_ON_PATH_VER="-"
[ -n "$GYP_ON_PATH" ] && GYP_ON_PATH_VER=$(env -i PATH="$STUDY_PATH" "$GYP_ON_PATH" --version 2>/dev/null | head -1)
GYP_RAN=$(grep -oE "gyp info using node-gyp@[0-9.]+" "$LOG" | sed 's/.*node-gyp@//' | sort -u | tr '\n' ',')
GYP_RESOLVED=$(grep -oE "node-gyp@[0-9]+\.[0-9]+\.[0-9]+" "$LOG" | sed 's/node-gyp@//' | sort -u | tr '\n' ',')
GYP_ID="ran=${GYP_RAN:-DID-NOT-RUN} resolved=${GYP_RESOLVED:--} on_path=${GYP_ON_PATH:--}($GYP_ON_PATH_VER)"
{ echo "=== ARM EFFECT ==="; echo "arm=$ARM optout_warnings=$WARN_COUNT arm_effect=$ARM_EFFECT"
  echo "node_gyp_identity=$GYP_ID"; } >> "$LOG"

node "$HARNESS/lib/delta.mjs" "$OUT/pre.ndjson" "$OUT/post.ndjson" "$OUT/delta.json" 2>> "$LOG"
NONCE="$NONCE" SHARD="$SHARD" ARM="$ARM" RC_SCRIPT="$RC_SCRIPT" RC_INSTALL="$RC_INSTALL" \
  PROJ="$PROJ" STOREROOT="$H" \
  node "$HARNESS/lib/shard-verdict.mjs" "$OUT/delta.json" "$MANIFEST" "$LOG" "$OUT/verdicts.json"

ARM_EFFECT="$ARM_EFFECT" WARN_COUNT="$WARN_COUNT" GYP_ID="$GYP_ID" PLATFORM="$PLATFORM" \
  LEVER="${LEVER:-none}" NONCE="$NONCE" \
  node "$HARNESS/lib/errsig.mjs" "$LOG" "$MANIFEST" "$OUT/verdicts.json" "$OUT/report.json"
echo "OUT=$OUT  arm_effect=$ARM_EFFECT  node_gyp=${GYP_ID:-none}"

# A run that produced no admissible data must FAIL its caller. Reporting success
# on a run that installed nothing is how a green CI job comes to mean nothing.
if [ "$ARM_EFFECT" != "confirmed" ]; then
  echo "FATAL: arm effect not confirmed ($ARM_EFFECT) — this run is INADMISSIBLE" >&2
  exit 8
fi
