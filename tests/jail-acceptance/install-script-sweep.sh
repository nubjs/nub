#!/usr/bin/env bash
# The population the jail actually applies to: packages that HAVE an install script.
#
# ⛔⛔ WHY THIS EXISTS ALONGSIDE `ecosystem-sweep.sh`. That sweep installs real framework trees, which is
# the right shape for "does a user's project install" — but it turned out that only 6 of 17 such trees run
# ANY lifecycle script, so it produces 6 rows of evidence about confinement and 7 rows of nothing. The
# reason is a real and welcome fact: much of the ecosystem has moved from install scripts to platform
# `optionalDependencies`. `sharp`, `better-sqlite3`, `playwright` and even `electron@43` have no scripts at
# all now. Measuring the jail against trees that have nothing to confine cannot support a bar.
#
# So this sweep's population is the packages where confinement APPLIES, checked against the registry rather
# than assumed: 20 of 30 popular native packages still carry an install/postinstall.
#
# ⛔ APPROVAL AND GRANT ARE DIFFERENT AXES, AND CONFLATING THEM IS HOW THIS GOES VACUOUS. nub's default is
# to SKIP an unapproved build script — so measuring at "pure defaults" measures a skipped script, i.e. the
# jail never runs. This sweep therefore APPROVES the build (`allowBuilds`, which is what a user does via
# `nub approve-builds`) while leaving the GRANT entirely alone: no catalog override, no `no-jail`, no
# `dependenciesMeta`. Approval is the user's decision; the grant is what is under test.
#
# ⛔ COLD EVERY TIME. A warm download cache hides the failure that matters — `electron` was once measured as
# needing no network because the measuring machine already had its zip.#
# ⛔⛔ AND IT NOW RUNS A CONTROL, WHICH IT DID NOT BEFORE. Every earlier run of this sweep classified a
# failure by grepping the log for a permission- or network-shaped string — a PROXY that cannot tell a
# refused syscall from a package that stopped building years ago. The committed macOS results carry an
# `npm-differential` column drawing that line and clearing 12 of 15 failures as upstream rot, but it is
# HAND-WRITTEN: the string appears in no script here, and the Windows results have no equivalent, so
# `58 rc=0 of 87` on Windows has never been adjudicated. Three arms replace the proxy — see the block
# above the loop for the lattice. The headline consequence: only JAIL-CAUSED indicts the jail, and it is
# the only verdict that fails the gate.
#
# Usage: NUB=/path/to/nub ./install-script-sweep.sh [--only <pkg>] [--keep-logs <dir>] [--population <tsv>]
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
NUB="${NUB:-$ROOT/target/fast/nub}"
ONLY=""; KEEP=""
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY="$2"; shift 2 ;;
    --keep-logs) KEEP="$2"; shift 2 ;;
    --population) POPULATION="$2"; shift 2 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
[ -x "$NUB" ] || { echo "no nub binary at $NUB" >&2; exit 2; }

# ⛔⛔ STAGE THE BUSYBOX SIDECAR, OR THIS HARNESS MEASURES THE WRONG SHELL ENTIRELY.
# On Windows nub runs dependency lifecycle scripts under a bundled busybox `sh` that
# release.yml lays beside `nub.exe`. A `cargo build` target dir has none, and
# `apply_lifecycle_script_shell` (pm_engine/mod.rs) DOWNGRADES the missing sidecar to a
# `tracing::warn!` and leaves the engine on `cmd.exe` — deliberately, so read-only verbs
# like `nub list` still work, and therefore INVISIBLY at default log level.
#
# ⛔ THE COST OF NOT DOING THIS IS A FALSE `JAIL-CAUSED` VERDICT, MEASURED. Under cmd.exe a
# postinstall of `chmod u+x ffmpeg` resolves `chmod` to git-bash's MSYS2 build, which dies
# in `msys-2.0.dll` init under the AppContainer token (exit -1073741502 / 0xC0000142) — a
# failure a shipped nub can never hit, because under busybox `sh` `chmod` is a builtin
# APPLET and no MSYS2 process is ever spawned. `@ffmpeg-installer/linux-x64` was scored
# JAIL-CAUSED on that basis and flips to OK with the sidecar staged. Any package whose
# postinstall shells out to coreutils was mis-scored the same way.
#
# Six workflows already stage it (release, ci, sandbox-conformance, the two busybox probes,
# build-jail-corpus-agent); this harness was the one that missed it. Copy it beside the
# binary rather than setting `__NUB_BUSYBOX_EXE`, so the real `current_exe()` sidecar
# resolution is exercised exactly as the win32 package does it.
case "$(uname -s 2>/dev/null || echo unknown)" in
  MINGW*|MSYS*|CYGWIN*)
    _bb_dst="$(dirname "$NUB")/busybox.exe"
    if [ ! -f "$_bb_dst" ]; then
      _bb_src="$ROOT/vendor/busybox-w32/busybox64.exe"
      # ⛔ FAIL LOUDLY. Continuing without it is the exact silent degradation this block
      # exists to end — it would score cmd.exe failures as jail failures.
      [ -f "$_bb_src" ] || { echo "no busybox at $_bb_src; the sweep would measure cmd.exe, not nub's lifecycle shell" >&2; exit 2; }
      cp "$_bb_src" "$_bb_dst" || { echo "could not stage busybox at $_bb_dst" >&2; exit 2; }
    fi
    echo "busybox sidecar: $_bb_dst" >&2
    ;;
esac
OUT="${OUT:-/tmp/install-script-sweep.tsv}"
: > "$OUT"
[ -n "$KEEP" ] && mkdir -p "$KEEP"

# ⛔⛔ THE POPULATION IS DISCOVERED, NOT HARDCODED, AND THAT IS LOAD-BEARING. The set of packages the jail
# actually confines is shrinking fast: `sharp`, `better-sqlite3`, `playwright` and `electron@43` have all
# dropped their install scripts in favour of platform `optionalDependencies` (measured: 37 of 123 popular
# candidates no longer carry one). A hardcoded list rots in the direction that makes this sweep LOOK fine
# while measuring nothing — a package that drops its script keeps "passing" as a row where the jail was
# never exercised. `discover-install-scripts.mjs` asks the registry instead, and its output is checked in
# under `results/` so a run is reproducible against the population it actually used.
#
# Offline or registry-down: pass `--population <file>` to reuse a checked-in list.
POPULATION="${POPULATION:-}"
if [ -z "$POPULATION" ]; then
  # ⛔⛔ `mktemp -t iss-pop` IS BSD SYNTAX AND GNU REFUSES IT. GNU reads `-t`'s argument as a
  # TEMPLATE and requires it to end in XXXXXX, so on Linux this printed
  # `mktemp: too few X's in template 'iss-pop'`, substituted an EMPTY path, and the discovery below
  # wrote its output nowhere. PKGS then came out empty and the whole sweep exited 0 having measured
  # nothing. Measured on a Linux builder 2026-09-02, and it is why no Linux results file has ever
  # existed for this sweep. An explicit template is the form both accept.
  POPULATION="$(mktemp "${TMPDIR:-/tmp}/iss-pop.XXXXXX")" || {
    echo "could not create a population file" >&2; exit 2; }
  node "$HERE/discover-install-scripts.mjs" --out "$POPULATION" || {
    echo "could not discover the population; pass --population <file>" >&2; exit 2; }
fi
PKGS="$(awk -F'\t' '{print $1}' "$POPULATION" | tr '\n' ' ')"
# ⛔⛔ AN EMPTY POPULATION IS A HARD FAILURE, NOT AN EMPTY SWEEP — and this guard matters more than the
# fix above it. The bug there was one line of shell; its DAMAGE was that every count downstream read 0,
# every verdict bucket read 0, and the gate passed, so a sweep that confined nothing reported success.
# Whatever the reason the population is empty, that must never again be spelled as a pass.
[ -n "$(printf '%s' "$PKGS" | tr -d ' ')" ] || {
  echo "population is EMPTY ($POPULATION) — refusing to report a sweep that measured nothing" >&2
  exit 2; }

# ⛔⛔ RECONCILE THE DISCOVERED POPULATION AGAINST THE LAST COMMITTED ONE, BECAUSE A SHRINK IS SILENT.
# Discovery is deliberate (the header says why a hardcoded list rots), but it makes the population a
# MOVING TARGET, and nothing reported when it moved. Measured 2026-09-04: three same-commit sweeps
# produced 179/178/179 rows against a committed 180, and the macOS run dropped a DIFFERENT package
# than the other two — a per-platform difference a pinned file cannot produce. `prisma` was absent
# from all three despite carrying a `preinstall` at both the latest and the in-use version, and it
# measures OK when swept explicitly. So the coverage those sweeps claimed was smaller than it read,
# and the only reason anyone noticed was a hand comparison months later.
#
# This does NOT fail the run: real drift is expected and legitimate. It makes the delta impossible to
# miss, so a shrink is a fact in the log rather than an absence nobody counts.
POP_DELTA=""
BASELINE_POP="$HERE/results/install-script-population.tsv"
if [ -f "$BASELINE_POP" ]; then
  _now="$(awk -F'\t' '{print $1}' "$POPULATION" | sort -u)"
  _was="$(awk -F'\t' '{print $1}' "$BASELINE_POP" | sort -u)"
  _gone="$(comm -23 <(printf '%s\n' "$_was") <(printf '%s\n' "$_now") | tr '\n' ' ')"
  _new="$(comm -13 <(printf '%s\n' "$_was") <(printf '%s\n' "$_now") | tr '\n' ' ')"
  if [ -n "$(printf '%s' "$_gone$_new" | tr -d ' ')" ]; then
    POP_DELTA="population differs from $BASELINE_POP — DROPPED: ${_gone:-none}| ADDED: ${_new:-none}"
    echo "⛔ $POP_DELTA" >&2
  fi
fi

ran_total=0; ok=0; noscript=0; jailcaused=0; nubcaused=0; upstream=0
# ⛔⛔ THREE ARMS, BECAUSE TWO CANNOT TELL THE ONLY THING THIS SWEEP EXISTS TO SAY. Until now every
# arm here ran nub with the jail ON, and a failure was classified by grepping the log for a
# permission- or network-shaped string. That is a PROXY for a control, and it cannot separate the
# three ways a row fails: the jail refused something, nub's package manager is broken, or the
# package simply does not build any more. The committed macOS results carry an `npm-differential`
# column that draws exactly this distinction and clears 12 of 15 failures as upstream rot — but that
# column is hand-written, it appears in no script in this repository, and the Windows results have
# nothing equivalent. So the Windows numbers have never been adjudicated at all.
#
#   arm A  nub, jail ON            the measurement
#   arm B  nub, jail OFF           A fails and B passes  => the JAIL did it. The only verdict that indicts it.
#   arm C  npm                     both fail and C passes => nub's PM did it, not the jail.
#                                  all three fail        => the package is broken for everyone.
#
# B and C run only when A did not pass, so an all-green sweep costs exactly what it did before.
first_err() {
  grep -viE '^\s*(WARN|npm warn|warning|gyp WARN)' "$1" 2>/dev/null \
    | grep -oiE 'nub build sandbox: blocked[^.]*|could not confine[^.]*|could not open '"'"'[^'"'"']*'"'"'[^:]*: Permission denied|EPERM[^,]*|EACCES[^,]*|ENOTFOUND [a-z0-9.-]*|node-pre-gyp ERR![^,]*|fatal error: [^ ]*|Cannot find module [^ ]*|× lifecycle script [a-z]* failed for [^ ]*|npm error code [A-Z0-9]*' \
    | head -1
}

# One fixture writer for all three arms so they cannot drift apart. `allowBuilds` is nub's field and
# npm ignores an unknown key, which is what lets the SAME package.json drive the control.
write_fixture() {
  _dir="$1"; _pkg="$2"; _ver="$3"; _jail="$4"
  mkdir -p "$_dir"
  node -e '
    const [out, pkg, ver] = process.argv.slice(1);
    require("fs").writeFileSync(out, JSON.stringify({
      name: "iss", version: "1.0.0",
      dependencies: { [pkg]: ver },
      allowBuilds: { [pkg]: true },
    }));
  ' "$_dir/package.json" "$_pkg" "$_ver"
  # ⛔ TURNING THE AGE GATE OFF IS LOAD-BEARING, NOT TIDINESS — AND THE SPELLING IS THE WHOLE
  # TRICK. The setting has TWO surfaces: `nub.jsonc` takes `install.minimumReleaseAge` (camelCase,
  # and it REJECTS a bare integer), while the CLI and `.npmrc` take `minimum-release-age` as bare
  # MINUTES. This file writes an `.npmrc`, so the camelCase spelling it used to write alone was the
  # WRONG surface and nothing read it — an unknown npmrc key is silently ignored, so the line looked
  # present and did nothing. The kebab form is pinned live by `install_engine.rs`, which writes
  # `minimum-release-age=10069920` and asserts exit 21 with `ERR_NUB_NO_MATURE_MATCHING_VERSION`.
  # Both spellings are written now: the extra one is inert wherever it is not the surface, so this
  # cannot regress either way. nub's age gate refuses any version
  # published inside the last 1440 minutes (`ERR_NUB_NO_MATURE_MATCHING_VERSION`, exit 21), and the
  # population pins actively-released packages at `latest` — `workerd` publishes daily. Measured
  # 2026-09-02: SEVEN packages (`workerd`, `nx`, `@anthropic-ai/claude-code`, `@openrouter/sdk`,
  # `@posthog/cli`, `opencode-ai`, `koffi`) came back NO-SCRIPT-RAN at rc=21 having measured
  # NOTHING, which reads as a benign row and is not one. The same seven on Windows AND Linux, and
  # NONE on macOS, whose sweep ran outside the gate window — so the count is a property of when a
  # sweep runs, not of the platform, and re-counting beats trusting this list. The gate is a supply-chain
  # default worth having; it is simply not what this sweep measures, and leaving it on silently
  # deletes packages from the population.
  printf 'side-effects-cache=false\nminimum-release-age=0\nminimumReleaseAge=0\n' > "$_dir/.npmrc"
  # ⛔ NODE, NOT PYTHON, AND THAT IS WHAT MAKES THIS RUNNABLE ON WINDOWS. `nub-win3` has no python3
  # and no `py`, so the heredoc that used to be here wrote no fixture at all and every Windows row read
  # NO-SCRIPT-RAN — 87 rows summarising as "0 jail-suspect failures" over installs that never happened.
  # node is present wherever nub is, by construction. The sibling cold-network sweep was ported for this
  # exact reason; this file was missed.
  # ⛔ `install.buildJail: false` in `nub.jsonc` is the ONLY opt-out: it is global and there is no env
  # override (`crates/nub-cli/src/pm_engine/build_jail.rs:1552`). Arm B is therefore spelled as a
  # FILE, and an arm that tried to disable the jail with a variable would silently measure arm A.
  [ "$_jail" = nojail ] && printf '{ "install": { "buildJail": false } }\n' > "$_dir/nub.jsonc"
  return 0
}

# Every arm gets its own fresh home under the same scrub, so a difference between arms is the
# installer and nothing else.
# ⛔⛔ A FRESH `HOME` IS NOT A FRESH STORE ON WINDOWS, AND THAT INVALIDATED A WHOLE SWEEP.
# `nub_data_dir()` resolves `$XDG_DATA_HOME/nub`, THEN `%LOCALAPPDATA%\nub` on Windows, then
# `<home>/.local/share/nub`. This loop set only `HOME`, which the Windows branch never consults — so
# every row shared the machine's real `%LOCALAPPDATA%\nub\pm\store`. Measured: 13 of 87 rows failed
# with `Cannot find module 'C:\Users\nub\AppData\Local\nub\pm\store\simple-get@…\once\once.js'`,
# i.e. one row's store state breaking another's resolution, reported as package failures.
# `XDG_DATA_HOME` alone would fix it, but naming all three keeps the row isolated whichever branch a
# platform takes.
install_arm() {
  _h="$1"; _p="$2"; _l="$3"; shift 3
  ( cd "$_p" && env -u ELECTRON_CACHE -u ELECTRON_MIRROR -u PLAYWRIGHT_BROWSERS_PATH \
      -u PUPPETEER_CACHE_DIR -u npm_config_cache -u CYPRESS_INSTALL_BINARY \
      NUB_JAIL_DUMP_POLICY=1 HOME="$_h" NUB_CACHE_DIR="$_h/.cache/nub" \
      XDG_DATA_HOME="$_h/xdg" USERPROFILE="$_h" LOCALAPPDATA="$_h/AppData/Local" \
      timeout 1200 "$@" > "$_l" 2>&1 )
}

for pkg in $PKGS; do
  [ -n "$ONLY" ] && [ "$ONLY" != "$pkg" ] && continue
  home="$(mktemp -d "$HOME/iss-XXXXXX")"
  proj="$home/project"
  mkdir -p "$proj"
  # APPROVED so the script runs; the GRANT is whatever nub ships.
  # The exact version the population file names, so a re-run measures the same thing rather than whatever
  # `latest` has become since.
  ver="$(awk -F'\t' -v p="$pkg" '$1==p{print $2}' "$POPULATION" | head -1)"
  [ -n "$ver" ] || ver="latest"
  write_fixture "$proj" "$pkg" "$ver" jail
  log="$home/install.log"
  # ⛔ TIME THE JAILED ARM. This sweep is a CORRECTNESS gate and was BLIND to overhead: a catalog
  # `userHome` write grant costs a full-profile ACE walk on Windows — measured 86 s of grant+revoke
  # for ONE confined spawn — and against the 1200 s per-arm timeout that reads as a comfortable OK.
  # Recording seconds per package puts an outlier like that in the results file instead of leaving
  # it visible only to whoever thinks to profile by hand.
  #
  # ⛔ `SECONDS`, NEVER `date +%s%3N`. BSD `date` has no `%N`, so `%3N` yields a literal `3N` on
  # macOS and every later arithmetic expansion silently produces garbage — this sweep runs on all
  # three platforms. Second resolution is what this is for: finding multi-second outliers, not
  # micro-benchmarking. Sub-second packages correctly read 0.
  _jail_t0=$SECONDS
  install_arm "$home" "$proj" "$log" "$NUB" install
  rc=$?
  jail_s=$(( SECONDS - _jail_t0 ))

  # ⛔⛔ COUNT THE JAIL'S OWN PER-SPAWN DUMP, NOT A WARNING. The first version counted
  # `running build scripts`, which is emitted only on the defaultTrust path — so with an explicit
  # `allowBuilds` approval (which is what this sweep uses) it never appears, and every row reported
  # NO-SCRIPT-RAN while the script had in fact run and been confined.
  #
  # ⛔⛔ AND THE DUMP IS MULTI-LINE PER SPAWN, SO A BARE `grep -c JAILDUMP` COUNTS LINES, NOT SPAWNS.
  # Its length tracks the grant’s rule count, which differs per package: measured on macOS, ONE confined
  # spawn produced 36 lines for `core-js@3.46.0` and 54 for `bufferutil@4.0.9`. Exactly one line per spawn
  # carries `pkg=`, so that is what a spawn count greps for — confirmed by a two-package fixture, which
  # yields 2 `pkg=` lines against 108 total. The old count still answered the only question the verdict
  # asks (did the jail engage at all), but it read as a script tally and was not one.
  ran=$(grep -c 'JAILDUMP pkg=' "$log" 2>/dev/null || true)
  jail_lines=$(grep -ciE 'nub build sandbox: blocked|could not confine|sandbox could not be applied|WARN_NUB_JAIL_NET_DENIED' "$log" 2>/dev/null || true)
  # ⛔⛔ A DENIAL DOES NOT HAVE TO NAME THE JAIL, AND THIS CLASS READ AS FAILED-OTHER FOR A WHOLE SWEEP.
  # `node-libcurl`'s preinstall shells out to `git clone`, and git inside the AppContainer reported
  # `fatal: could not open '/dev/null' for reading and writing: Permission denied` — the NUL device is
  # granted on macOS and Linux (see `macos_seatbelt_base.sbpl` and `linux_landlock.rs`) but not on
  # Windows. No jail-branded string, no network error, so both detectors above scored it clean and the
  # row landed in FAILED-OTHER alongside genuine upstream compile errors.
  #
  # A permission/denied shape inside a CONFINED run is therefore its own gating signal. It can be a
  # package's own bug, so this does not assert the jail is at fault — it refuses to let the row pass
  # unexamined, which is the whole job of this column.
  deny_lines=$(grep -ciE "EPERM|EACCES|permission denied|access is denied|operation not permitted" "$log" 2>/dev/null || true)
  # A denied egress on macOS surfaces as a DNS failure with no jail-branded line — measured. So a network
  # error is its own gating bucket rather than being filed as unrelated.
  net_err=$(grep -cE '\b(ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ETIMEDOUT)\b' "$log" 2>/dev/null || true)

  # Arm A only ever answers PASS or NOT-PASS here. WHICH kind of failure it is, is not something a
  # log grep can decide — that is what arms B and C are for. The detector columns survive as
  # diagnostics, but they no longer set the verdict.
  if [ "$ran" = 0 ]; then
    verdict="NO-SCRIPT-RAN"; noscript=$((noscript+1))
  elif [ "$rc" = 0 ] && [ "$jail_lines" = 0 ] && [ "$net_err" = 0 ]; then
    verdict="OK"; ok=$((ok+1)); ran_total=$((ran_total+1))
  else
    verdict="SUSPECT"; ran_total=$((ran_total+1))
  fi
  err=$(first_err "$log")

  # ── arms B and C ────────────────────────────────────────────────────────────────────────────────
  # Only for a row arm A did not clear, so an all-green sweep costs what it always did. Note arm A
  # can be SUSPECT at rc=0: a script that swallows its own failure still shows a jail or network
  # line, and that is exactly the shape an exit code cannot see.
  nojail_rc="—"; npm_rc="—"; ctl_err="—"
  if [ "$verdict" = SUSPECT ]; then
    bhome="$(mktemp -d "$HOME/issb-XXXXXX")"; bproj="$bhome/project"; blog="$bhome/install.log"
    write_fixture "$bproj" "$pkg" "$ver" nojail
    install_arm "$bhome" "$bproj" "$blog" "$NUB" install
    nojail_rc=$?
    if [ "$nojail_rc" = 0 ]; then
      # Same nub, same package, same cold home — the jail is the only variable that moved.
      verdict="JAIL-CAUSED"; jailcaused=$((jailcaused+1)); ctl_err="nojail-arm PASSED"
    else
      chome="$(mktemp -d "$HOME/issc-XXXXXX")"; cproj="$chome/project"; clog="$chome/install.log"
      # No `nub.jsonc` for arm C: npm would ignore it, and leaving it out keeps the control's fixture
      # the plain one.
      #
      # ⛔⛔ NO `--legacy-peer-deps`, AND THE REASON IS THE OPPOSITE OF THE ONE THAT PUT IT HERE. It was
      # added so a spurious control failure could not excuse nub for free. But it makes npm SKIP peer
      # dependencies, so the control resolves a strictly SMALLER TREE than nub — and a failure that
      # originates in a peer-installed subtree then reads as NUB-CAUSED, which is the control excusing
      # nub's accuser instead. Measured 2026-09-02 on `n8n-nodes-evolution-api@1.0.4`, whose peer
      # `n8n-workflow` reaches `@n8n/expression-runtime` -> `isolated-vm@6.2.0`, which does not compile
      # against Node 26: with the flag npm installed 28 top-level packages, no `isolated-vm` anywhere,
      # and "passed"; without it npm resolved the same subtree nub does and failed identically. The row
      # is UPSTREAM, and the flag alone made it NUB-CAUSED.
      #
      # A real ERESOLVE conflict now fails the control, which scores the row UPSTREAM. That is the safe
      # direction: it is the verdict that indicts nobody. This flag cannot affect JAIL-CAUSED either way
      # — arms A and B decide that, and arm C runs only once both have already failed.
      write_fixture "$cproj" "$pkg" "$ver" jail
      install_arm "$chome" "$cproj" "$clog" npm install --no-audit --no-fund
      npm_rc=$?
      ctl_err=$(first_err "$clog")
      if [ "$npm_rc" = 0 ]; then
        verdict="NUB-CAUSED"; nubcaused=$((nubcaused+1))
      else
        verdict="UPSTREAM"; upstream=$((upstream+1))
      fi
      [ -n "$KEEP" ] && cp "$clog" "$KEEP/$(echo "$pkg" | tr '/' '_').npm.log" 2>/dev/null
      rm -rf "$chome"
    fi
    [ -n "$KEEP" ] && cp "$blog" "$KEEP/$(echo "$pkg" | tr '/' '_').nojail.log" 2>/dev/null
    rm -rf "$bhome"
  fi

  # `jail-s` is APPENDED, never inserted: the two free-text error fields stay last-but-one and
  # last-but-two so every existing positional reader of fields 1-11 keeps working unchanged.
  printf '%s\t%s\trc=%s\tconfined-spawns=%s\tjail-lines=%s\tnet-err=%s\tdeny=%s\tnojail-rc=%s\tnpm-rc=%s\t%s\t%s\tjail-s=%s\n' \
    "$pkg" "$verdict" "$rc" "$ran" "$jail_lines" "$net_err" "${deny_lines:-0}" \
    "$nojail_rc" "$npm_rc" "${err:-—}" "${ctl_err:-—}" "${jail_s:-0}" >> "$OUT"
  echo "  $pkg -> $verdict (rc=$rc, ran=$ran, ${jail_s:-0}s)"
  [ -n "$KEEP" ] && cp "$log" "$KEEP/$(echo "$pkg" | tr '/' '_').log" 2>/dev/null
  rm -rf "$home"
done

echo
echo "── install-script packages, jail DEFAULT-ON, approved builds, cold ──"
[ -z "$POP_DELTA" ] || echo "⛔ POPULATION DRIFT: $POP_DELTA"
awk -F'\t' '{print $2}' "$OUT" | sort | uniq -c | sort -rn
total=$(wc -l < "$OUT" | tr -d ' ')
echo "packages whose script actually ran: $ran_total / $total"
echo "  OK          $ok"
echo "  JAIL-CAUSED $jailcaused   <- nub failed jailed, PASSED unjailed. The only count that indicts the jail."
echo "  NUB-CAUSED  $nubcaused   <- failed both ways, npm succeeded. nub's PM, not the jail."
echo "  UPSTREAM    $upstream   <- nub and npm both failed. The package is broken for everyone."
# ⛔ THE GATE MOVED FROM A PROXY TO THE DIFFERENTIAL, AND IT IS DELIBERATELY LOOSER IN ONE DIRECTION.
# It used to fail on JAIL-SUSPECT, i.e. on any failure whose LOG looked permission- or network-shaped
# — which fires on ecosystem rot that has nothing to do with confinement. It now fails only when a
# package installs with the jail off and not with it on. UPSTREAM and NUB-CAUSED rows are real
# findings and are printed, but they are not this gate's business.
[ "$jailcaused" = 0 ] || exit 1
