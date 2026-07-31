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

case "$(uname -s)" in
  Darwin) PLATFORM=macos ;;
  Linux) PLATFORM=linux ;;
  MINGW*|MSYS*|CYGWIN*) PLATFORM=windows ;;
  *) PLATFORM=other ;;
esac

# WINDOWS PATH DUALITY. Every path this script hands to bash must be MSYS-form
# (`/c/…`), and every path it hands to node.exe or nub.exe must be Windows-form
# (`C:\…`) — a native binary receiving `/c/Users/x` reads it as a rooted path on
# the current drive and either creates the wrong tree or reports ENOENT. MSYS's
# implicit argv translation covers a bare argument but NOT the `name=value` form
# the snapshot roots use, so convert explicitly rather than relying on it.
wp() { if [ "$PLATFORM" = windows ]; then cygpath -w "$1"; else printf '%s' "$1"; fi; }
# And the reverse, on the way IN. `runner.temp` is a native `D:\a\_temp`; used as a
# bash path prefix its backslashes are not separators, so `mkdir -p` would create
# one literal directory named `D:\a\_temp` under the CURRENT directory — inside the
# checkout, which is the one place the fixture must never live.
up() { if [ "$PLATFORM" = windows ]; then cygpath -u "$1"; else printf '%s' "$1"; fi; }
NUB="${NUB_BIN:?set NUB_BIN}"; EXPECT_SHA="${NUB_EXPECT_GIT_SHA:?set NUB_EXPECT_GIT_SHA}"
LEVER="${LEVER:-}"
# THE FIXTURE PROJECT MUST NOT LIVE INSIDE A REPOSITORY. Lockfile discovery walks
# UP from the project root, so a fixture nested under a checkout inherits that
# checkout's own lockfiles and the install aborts before a single script runs
# (`ERR_NUB_LOCKFILE_AMBIGUOUS: multiple lockfiles found`). Defaulting the output
# root to a temp directory keeps the fixture clear of any enclosing repository;
# override OUTROOT only with a path that is likewise outside one.
OUT="$(up "${OUTROOT:-${TMPDIR:-/tmp}/build-jail-corpus}")/$SHARD-$ARM${LEVER:+-$LEVER}"
rm -rf "$OUT"; mkdir -p "$OUT"

RUNID="$(date +%s)$RANDOM"; NONCE="NubProbe${RUNID}"
PROJ="$OUT/proj"; H="$OUT/home"; CACHE="$OUT/cache"; TMPD="$OUT/tmp"
mkdir -p "$PROJ" "$H" "$CACHE" "$TMPD"
# Outside every observed root. Logs written INSIDE the project once manufactured
# ACTED=true for every package, turning each NEVER-RAN into DID-WORK-AND-FAILED.
LOG="$OUT/run.log"

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
# `tr -d '\r'` because a CRLF checkout puts the carriage return on each row's LAST field.
# Left in, it rides into the provisioning capability names and — via the verdict script —
# into the class name, where it silently defeated the class lookup.
while IFS=$'\t' read -r PKG VER CLASS NEED; do
  [ -z "${PKG:-}" ] && continue; case "$PKG" in \#*) continue ;; esac
  DEPS="$DEPS,\"$PKG\":\"$VER\""; PKGS+=("$PKG")
  [ -n "${NEED:-}" ] && NEEDS="$NEEDS,$NEED"
done < <(tr -d '\r' < "$MANIFEST")
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
#
# `side-effects-cache=false` is what makes a GRANT ITERATION mean anything: the cache
# replays a package's previous postinstall RESULT when only the catalog changed, so the
# next arm scores the PREVIOUS arm's grants. The per-run private HOME is the other half
# and is NOT a substitute — `side_effects_cache_root` derives the cache from the STORE,
# not from HOME, so any later move to a shared store to save bandwidth would silently
# reintroduce the replay. Disabling it in the fixture makes that structural.
{ echo "minimumReleaseAge=0"; echo "minimumReleaseAgeStrict=false"
  echo "side-effects-cache=false"
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
    # A capability delivered as a package.json FIELD cannot be withheld by
    # deleting a path, so the default `rm -f` below silently suppressed nothing
    # and the self-test it was meant to drive passed vacuously.
    msw-manifest) node -e '
      const f=process.argv[1], j=JSON.parse(require("fs").readFileSync(f,"utf8"));
      delete j.msw; require("fs").writeFileSync(f, JSON.stringify(j,null,2));' "$PROJ/package.json" ;;
    *) rm -f "$PROJ/$SUPPRESS" ;;
  esac
  echo "SUPPRESSED capability: $SUPPRESS" >> "$LOG"
fi
echo "provisioned:$PROVISIONED suppressed:${SUPPRESS:-none}" >> "$LOG"

# A snapshot that truncates is FATAL: the delta would be taken over a partial tree
# and every package past the cut would report NEVER-RAN. Abort rather than report.
#
# THERE IS NO `nm` ROOT. `<proj>/node_modules` is walked by the `proj` root already,
# so a separate root for it recorded every file twice under two different keys —
# doubling every count a predicate reads, and defeating attribution, since the
# project virtual-store regex is anchored at `^node_modules/.store/` and cannot
# match `nm:.store/…`. `delta.mjs` folds any `nm` record from an archived run back
# onto its real path, so re-scoring older output still works.
snap() {
  SNAP_MAX_ENTRIES="${SNAP_MAX_ENTRIES:-4000000}" \
    node "$(wp "$HARNESS/lib/snapshot.mjs")" "$(wp "$1")" \
    "proj=$(wp "$PROJ")" "home=$(wp "$H")" \
    "store=$(wp "$CACHE")" "tmp=$(wp "$TMPD")" 2>> "$LOG"
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

# THE NODE ON STUDY_PATH MUST ACTUALLY WORK, and it is not necessarily the Node
# the surrounding environment provisioned. `env -i` discards the ambient PATH, so
# a hardcoded STUDY_PATH silently substitutes whatever `node` happens to sit in
# those directories. On a CI runner that resolved a Homebrew Node which aborts on
# `--version` (SIGABRT), so the engine download for the codegen fixture died and
# the codegen denominator failed for a reason that looked like a package problem.
# Prefer the caller's own `node`, and refuse to start on a broken one rather than
# letting it surface later as an unexplained lifecycle-script failure.
# WINDOWS ENV FLOOR. `env -i` is the whole point of this harness — it is what makes
# one run's ambient state unable to leak into another — but on Windows a genuinely
# empty environment is not a clean room, it is a broken one: a native process's
# startup resolves system DLLs and the winsock provider catalogue relative to
# %SystemRoot%, so a child without it fails to START rather than failing to reach
# the network, and a launch failure is not a confinement result. The floor is the
# smallest set that lets a native process boot; every path-shaped member is
# redirected into this run's private tree, so isolation is preserved rather than
# traded away. Names are read case-insensitively because Git Bash preserves the
# Windows spelling (`SystemRoot`) while a workflow may export the upper-cased one.
WIN_ENV=()
if [ "$PLATFORM" = windows ]; then
  WIN_H="$(wp "$H")"; WIN_T="$(wp "$TMPD")"
  mkdir -p "$H/AppData/Roaming" "$H/AppData/Local"
  WIN_ENV=(
    "SystemRoot=${SystemRoot:-${SYSTEMROOT:-C:\\Windows}}"
    "windir=${windir:-${WINDIR:-C:\\Windows}}"
    "COMSPEC=${COMSPEC:-${ComSpec:-C:\\Windows\\system32\\cmd.exe}}"
    "PATHEXT=${PATHEXT:-.COM;.EXE;.BAT;.CMD}"
    "OS=${OS:-Windows_NT}"
    "NUMBER_OF_PROCESSORS=${NUMBER_OF_PROCESSORS:-2}"
    "PROCESSOR_ARCHITECTURE=${PROCESSOR_ARCHITECTURE:-AMD64}"
    "SystemDrive=${SystemDrive:-${SYSTEMDRIVE:-C:}}"
    "ProgramData=${ProgramData:-${PROGRAMDATA:-C:\\ProgramData}}"
    "ProgramFiles=${ProgramFiles:-${PROGRAMFILES:-C:\\Program Files}}"
    "USERPROFILE=$WIN_H" "APPDATA=$WIN_H\\AppData\\Roaming"
    "LOCALAPPDATA=$WIN_H\\AppData\\Local"
    "TEMP=$WIN_T" "TMP=$WIN_T"
  )
fi

NODE_DIR="$(cd "$(dirname "$(command -v node)")" && pwd)"
case ":$STUDY_PATH:" in
  *":$NODE_DIR:"*) ;;
  *) STUDY_PATH="$NODE_DIR:$STUDY_PATH" ;;
esac
# `$BASH`, NEVER a bare `bash`. This probe deliberately runs on a SCRUBBED PATH, and the
# scrubbed PATH is exactly where the wrong shell lives: `C:\Windows\system32` contains
# `bash.exe`, which is the WSL LAUNCHER. The probe therefore asked WSL for a Node and got
# `Windows Subsystem for Linux has no installed distributions`, which it reported as an
# unusable Node and aborted on — while the real Node was fine and every other part of the
# Windows run was healthy. Spelling the interpreter as the absolute path of the shell
# already running removes the lookup, and removes it on every platform rather than
# special-casing the one where it happened to bite.
JAILED_NODE="$(env -i PATH="$STUDY_PATH" ${WIN_ENV[@]+"${WIN_ENV[@]}"} "$BASH" -c 'command -v node' || true)"
JAILED_NODE_V="$(env -i PATH="$STUDY_PATH" ${WIN_ENV[@]+"${WIN_ENV[@]}"} "$BASH" -c 'node --version' 2>&1 || true)"
echo "study_path_node=$JAILED_NODE ($JAILED_NODE_V)" >> "$LOG"
case "$JAILED_NODE_V" in
  v*) ;;
  *) echo "FATAL: node on STUDY_PATH is unusable: '$JAILED_NODE' -> '$JAILED_NODE_V'" | tee -a "$LOG" >&2
     exit 7 ;;
esac
# `env -i` is what makes a run hermetic, so the dev-only catalog override must be
# forwarded EXPLICITLY — otherwise a grant iteration silently measures the COMPILED
# catalog and every "the grant changed nothing" verdict is an artefact of the harness.
# Forwarded only when set; the banner nub then prints into the log is the proof of which
# catalog the run actually read, and `catalog_override=` below records it per run.
CAT_ENV=()
[ -n "${NUB_BUILD_JAIL_CATALOG:-}" ] && CAT_ENV=("NUB_BUILD_JAIL_CATALOG=$(wp "$NUB_BUILD_JAIL_CATALOG")")
echo "catalog_override=${NUB_BUILD_JAIL_CATALOG:-none}" >> "$LOG"

runnub() {
  ( cd "$PROJ" && env -i \
      PATH="$STUDY_PATH" HOME="$H" TMPDIR="$TMPD" \
      ${WIN_ENV[@]+"${WIN_ENV[@]}"} \
      ${CAT_ENV[@]+"${CAT_ENV[@]}"} \
      NUB_CACHE_DIR="$(wp "$CACHE")" npm_config_cache="$(wp "$CACHE/npm")" \
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
# An override that failed to load falls back to the COMPILED catalog with one stderr line
# and otherwise runs normally — so a grant iteration would report "the grant did nothing"
# while never having read the grant. Require the banner whenever an override was asked for.
if [ -n "${NUB_BUILD_JAIL_CATALOG:-}" ]; then
  OVR_COUNT=$(grep -c "build-jail catalog OVERRIDDEN from" "$LOG" || true)
  [ "$OVR_COUNT" -gt 0 ] || ARM_EFFECT="FAILED-catalog-override-not-loaded"
fi
# ── node-gyp IDENTITY — three facts, because one of them alone is ambiguous ────
# A bare-PATH `node-gyp` on this host resolves a stale global 3.8.0, so an
# unverified native-build verdict is worth nothing. The first cut grepped only the
# `gyp info using node-gyp@X` banner and reported `none-observed`, which conflates
# "node-gyp never ran" (the NORMAL case — these packages resolve a prebuild or
# node-gyp-build and never compile) with "we do not know which one ran". Those are
# different facts and only the second invalidates a verdict, so record all three.
GYP_ON_PATH=$(env -i PATH="$STUDY_PATH" ${WIN_ENV[@]+"${WIN_ENV[@]}"} "$BASH" -c 'command -v node-gyp' 2>/dev/null || true)
GYP_ON_PATH_VER="-"
[ -n "$GYP_ON_PATH" ] && GYP_ON_PATH_VER=$(env -i PATH="$STUDY_PATH" ${WIN_ENV[@]+"${WIN_ENV[@]}"} "$GYP_ON_PATH" --version 2>/dev/null | head -1)
GYP_RAN=$(grep -oE "gyp info using node-gyp@[0-9.]+" "$LOG" | sed 's/.*node-gyp@//' | sort -u | tr '\n' ',')
GYP_RESOLVED=$(grep -oE "node-gyp@[0-9]+\.[0-9]+\.[0-9]+" "$LOG" | sed 's/node-gyp@//' | sort -u | tr '\n' ',')
GYP_ID="ran=${GYP_RAN:-DID-NOT-RUN} resolved=${GYP_RESOLVED:--} on_path=${GYP_ON_PATH:--}($GYP_ON_PATH_VER)"
{ echo "=== ARM EFFECT ==="; echo "arm=$ARM optout_warnings=$WARN_COUNT arm_effect=$ARM_EFFECT"
  echo "node_gyp_identity=$GYP_ID"; } >> "$LOG"

node "$(wp "$HARNESS/lib/delta.mjs")" "$(wp "$OUT/pre.ndjson")" "$(wp "$OUT/post.ndjson")" \
  "$(wp "$OUT/delta.json")" 2>> "$LOG"

# ── ATTRIBUTION CONTROL — zero attributed cells against a populated node_modules ──
# Attribution is a PARSE of the store path, so it fails silently and totally when the
# path shape is not the one the regexes expect: on Windows every rel path arrived with
# backslashes, nothing matched, `installed_cells` came back empty, and all 338 packages
# reported NOT-INSTALLED while being installed perfectly well. The run stayed green and
# self-consistent — the arm effect was still `confirmed`, because that assertion only
# proves nub RAN, never that the measurement PARSED. A wipeout must be inadmissible
# rather than a catastrophic-looking result, which is the same rule the arm effect
# already applies one level up.
ATTRIBUTED=$(node -e 'try{const d=require(process.argv[1]);process.stdout.write(String((d.installed_cells||[]).length))}catch(e){process.stdout.write("0")}' "$(wp "$OUT/delta.json")")
if [ "$INSTALLED_ANY" -gt 0 ] && [ "${ATTRIBUTED:-0}" -eq 0 ]; then
  ARM_EFFECT="FAILED-attribution-wipeout(installed=$INSTALLED_ANY,cells=0)"
fi
echo "attributed_cells=$ATTRIBUTED installed_top_level=$INSTALLED_ANY" >> "$LOG"
NONCE="$NONCE" SHARD="$SHARD" ARM="$ARM" RC_SCRIPT="$RC_SCRIPT" RC_INSTALL="$RC_INSTALL" \
  PROJ="$(wp "$PROJ")" STOREROOT="$(wp "$H")" \
  STORE_BASES="$(wp "$CACHE/store")
$(wp "$PROJ/node_modules/.store")
$(wp "$H/.cache/nub/pm/store")" \
  node "$(wp "$HARNESS/lib/shard-verdict.mjs")" "$(wp "$OUT/delta.json")" "$(wp "$MANIFEST")" \
    "$(wp "$LOG")" "$(wp "$OUT/verdicts.json")"
# The verdict step can now REFUSE (an unresolvable class). Its rc was previously
# discarded, so a refusal would have left the last run's verdicts.json in place and the
# arm would still have reported. Fold it into the arm effect instead.
RC_VERDICT=$?
[ "$RC_VERDICT" -eq 0 ] || ARM_EFFECT="FAILED-verdict-refused(rc=$RC_VERDICT)"

ARM_EFFECT="$ARM_EFFECT" WARN_COUNT="$WARN_COUNT" GYP_ID="$GYP_ID" PLATFORM="$PLATFORM" \
  LEVER="${LEVER:-none}" NONCE="$NONCE" \
  node "$(wp "$HARNESS/lib/errsig.mjs")" "$(wp "$LOG")" "$(wp "$MANIFEST")" \
    "$(wp "$OUT/verdicts.json")" "$(wp "$OUT/report.json")"
echo "OUT=$OUT  arm_effect=$ARM_EFFECT  node_gyp=${GYP_ID:-none}"

# A run that produced no admissible data must FAIL its caller. Reporting success
# on a run that installed nothing is how a green CI job comes to mean nothing.
if [ "$ARM_EFFECT" != "confirmed" ]; then
  echo "FATAL: arm effect not confirmed ($ARM_EFFECT) — this run is INADMISSIBLE" >&2
  exit 8
fi
