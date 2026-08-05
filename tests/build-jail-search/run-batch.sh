#!/usr/bin/env bash
# Run the grant search over a worklist. One JSON record per line to stdout, and one
# `results/runs/<pkg>@<version>.json` per package written by search.mjs itself.
#
# Usage:  ./run-batch.sh <nub> [--force] <pkg@ver> [pkg@ver ...]
#         ./run-batch.sh <nub> [--force] --file <worklist.txt>
#
# RESUMABLE BY DEFAULT. search.mjs skips a package whose run file already exists, so a
# re-run costs only what has not been measured — and a batch killed halfway can simply be
# started again. `--force` re-measures everything, which is what a harness or jail change
# calls for, since a run file records the answer AND the harness hash that produced it.
set -u
NUB="${1:-}"; shift || true
[ -x "$NUB" ] || { echo "usage: $0 <nub> [--force] <pkg@ver>... | --file <list>"; exit 2; }
case "$NUB" in /*) ;; *) NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")" ;; esac

# ⛔ SNAPSHOT THE BINARY, and run the batch against the COPY.
#
# A batch takes hours. Any cargo command on the same profile during that window rewrites
# `target/fast/nub` with WHATEVER FEATURES that command specified — and a binary without
# `build-jail-catalog-override` refuses every override, so every package measured after the
# rebuild records a control failure. MEASURED: a `cargo test --profile fast` run concurrently
# with a batch silently invalidated six packages, which then read as broken-at-the-widest-grant
# while installing fine jailed, unjailed and under npm.
#
# Copying is the fix that does not depend on remembering. The batch reads a file nothing else
# writes, so a rebuild mid-run cannot reach it.
SNAP="${TMPDIR:-/tmp}/nub-batch-$$"
# ⛔ KEEP THE `.exe`, AND HAND NODE A NATIVE PATH. Two separate Windows requirements:
#
#   1. Windows decides executability from the SUFFIX, so a snapshot without `.exe` cannot be
#      spawned at all — ENOENT on a file that is plainly there.
#   2. This is a Git Bash path (`/tmp/nub-batch-1830`), and Windows node hands it to CreateProcess
#      verbatim. search.mjs converts a `/c/...` spelling, but `/tmp` has no drive letter to convert,
#      so that guard does not fire here. `cygpath -w` is the only thing that knows the mapping.
#
# MEASURED on a windows-latest runner: the fixture canary refused in under a second, with the job
# still GREEN because the probe step is continue-on-error — a green wrapper over a failed step.
case "$(uname -s 2>/dev/null)" in
  MINGW*|MSYS*|CYGWIN*) SNAP="${SNAP}.exe" ;;
esac
cp "$NUB" "$SNAP" && chmod +x "$SNAP"
trap 'rm -f "$SNAP"' EXIT
# The path handed to node, native where that differs from the shell's own spelling.
SNAP_NATIVE="$SNAP"
if command -v cygpath >/dev/null 2>&1; then SNAP_NATIVE="$(cygpath -w "$SNAP")"; fi
# PORTABLE HASH. macOS has `shasum`; Git Bash on Windows has neither it nor `sha256sum` by
# default, and the failure is SILENT -- `$(shasum ...)` expands to empty, so the banner prints
# "batch binary:   (snapshot of ...)" and the anti-clobber identity check has nothing to compare.
# MEASURED on a windows-latest runner: `./run-batch.sh: line 31: shasum: command not found`.
_hash() {
  if command -v shasum >/dev/null 2>&1; then shasum -a256 "$1" | cut -c1-16
  elif command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -c1-16
  elif command -v certutil >/dev/null 2>&1; then certutil -hashfile "$1" SHA256 2>/dev/null | sed -n 2p | tr -d " \r" | cut -c1-16
  else echo "NO-HASH-TOOL"; fi
}
_snaphash="$(_hash "$SNAP")"
[ -n "$_snaphash" ] && [ "$_snaphash" != "NO-HASH-TOOL" ] || {
  echo "REFUSING TO RUN: no SHA-256 tool (shasum/sha256sum/certutil) on PATH." >&2
  echo "  The binary-identity check would be inert, and a mid-run rebuild could silently" >&2
  echo "  invalidate every measurement after it. Install one, or run in a shell that has one." >&2
  exit 2
}
echo "batch binary: $_snaphash  (snapshot of $NUB)" >&2

# ⛔ SWEEP LEAKED FIXTURE ROOTS BEFORE STARTING. search.mjs roots each package at
# `~/.cache/nub-search-XXXX` (mkdtempSync) and removes it on NORMAL exit only — so every SIGKILL
# strands one, and stop-fix-restart is the normal loop when triaging. Each root holds a full
# installed dependency tree, so they are hundreds of MB apiece.
#
# MEASURED: 31 orphans totalling 18.9 GB on a disk already at 98%, and twice more on a fresh corpus
# box in a single afternoon of resharding. A sweep that only runs at cleanup time cannot help,
# because the failure mode IS the run that never reached cleanup.
#
# `-mmin +30` is what makes this safe to run while OTHER shards are live: a working shard writes
# into its root continuously, so it can never be 30 minutes cold. Do not lower it — concurrent
# shards are the normal case and deleting a live root destroys that package's measurement.
_swept=0
for _d in "$HOME"/.cache/nub-search-*; do
  [ -d "$_d" ] || continue
  if [ -z "$(find "$_d" -maxdepth 0 -mmin -30 2>/dev/null)" ]; then
    rm -rf "$_d" && _swept=$((_swept + 1))
  fi
done
[ "$_swept" -gt 0 ] && echo "swept $_swept leaked fixture root(s) untouched for >30min" >&2

here="$(cd "$(dirname "$0")" && pwd)"

# ⛔ WHERE RECORDS LAND IS A CONTRACT WITH THE CALLER, NOT AN IMPLEMENTATION DETAIL.
#
# This wrote records only to `$here/results/runs` — i.e. INSIDE the harness directory. That is fine
# for a local sweep, and it is what produced the 2,443-record legacy corpus. It is wrong for the
# corpus repo, whose runner collects, verifies and COMMITS from `records/` at the repo root: the
# measure step wrote to `harness/results/runs/` while every later step read `records/`, so
# collect-verdicts found 0, claim-slice completed 0 rows, and the slice committed nothing.
#
# MEASURED end-to-end on a 3-package local slice: 3 records written, all MINIMUM, and
# `collect-verdicts --runs records` reported "collected 0 verdict(s) from 0 record file(s)".
#
# This sat BEHIND the missing-`timeout` defect and produces the identical symptom — a green slice
# that measured nothing — so fixing the first one alone would have changed nothing observable.
# Expect a chain of faults, each hidden by the one in front of it.
#
# Overridable rather than moved: the default keeps every existing local invocation byte-identical.
RUNS_ROOT="${NUB_CORPUS_RUNS:-$here/results/runs}"
mkdir -p "$RUNS_ROOT"

# ⛔ `timeout` IS NOT ON macOS. It is GNU coreutils; the BSD userland does not ship it and GitHub's
# macOS image does not add it. MEASURED on the first live macOS corpus slice: the fixture canary hit
# `timeout: command not found`, this script refused to run, and the JOB STILL WENT GREEN because the
# caller wraps it in `|| true` — 100 queue rows claimed, zero measured, a commit that looked like
# progress. Resolve it once here instead of assuming a Linux userland.
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT=timeout
elif command -v gtimeout >/dev/null 2>&1; then   # what Homebrew's coreutils installs
  TIMEOUT=gtimeout
else
  TIMEOUT="$here/portable-timeout.sh"            # Perl shim; exits 124 like GNU, verified against it
fi

# And PROVE the override engages before spending hours on it, rather than discovering per-cell.
#
# ⛔ THE PROBE CATALOG COMES FROM `catalogFor`, NEVER FROM A LITERAL HERE. This check used to write
# `{"packages":{}}` — an EMPTY map, which parses under every packages shape there has ever been. So
# when the catalog shape changed under it, the check passed happily while every SYNTHESIZED cell
# catalog was rejected, and a 100-package batch produced 100 HARNESS-ERRORs before anyone looked.
# An empty map is ADJACENT to the question; the artifact the cells actually use IS the question.
_probe="${TMPDIR:-/tmp}/nub-ovcheck-$$"; rm -rf "$_probe"; mkdir -p "$_probe/home"
printf '{"name":"ovcheck","version":"1.0.0"}\n' > "$_probe/package.json"
if ! node "$here/search.mjs" --emit-sample-catalog > "$_probe/c.json"; then
  echo "REFUSING TO RUN: search.mjs could not emit a sample catalog." >&2
  rm -rf "$_probe"; exit 2
fi
_ovlog="$_probe/ov.log"
( cd "$_probe" && NUB_BUILD_JAIL_CATALOG="$_probe/c.json" HOME="$_probe/home" \
    "$SNAP" install > "$_ovlog" 2>&1 )
if ! grep -q "build-jail catalog OVERRIDDEN from" "$_ovlog"; then
  echo "REFUSING TO RUN: the override did not engage with this binary + catalog." >&2
  if grep -q "was REJECTED" "$_ovlog"; then
    echo "  The catalog was REJECTED — the harness emits a shape the parser does not accept:" >&2
    grep -o "was REJECTED ([^)]*)" "$_ovlog" | head -1 | sed 's/^/    /' >&2
  else
    echo "  Rebuild: scripts/rust-build.sh build -p nub-cli --profile fast \\" >&2
    echo "             --features nub-cli/build-jail-catalog-override" >&2
  fi
  rm -rf "$_probe"; exit 2
fi
rm -rf "$_probe"
# The NATIVE spelling from here on: everything below hands this to `node`, which on Windows cannot
# spawn the shell's `/tmp/...` form. Bash-side uses above (`cp`, the hash, the override probe) keep
# using $SNAP, since those are the shell's own.
NUB="$SNAP_NATIVE"

# ⛔ FIXTURE CANARY — prove the fixture still installs a REAL TREE before spending hours on it.
#
# The override check above proves the binary and the catalog SHAPE are sound. It says nothing about
# whether the fixture still causes a real install, and a fixture that quietly stops scripts doing
# work makes the whole ecosystem look free — every package measures as needing nothing, every
# verdict is MINIMUM, and coverage is 100%. Nothing fails.
#
# MEASURED, twice. A hand-written package-lock.json with an empty `packages` map took puppeteer's
# control from 9,629 installed files to 32, and all eight packages in that run came back needing
# NOTHING — including cypress and puppeteer, which cannot work without downloading a binary.
# Separately, an invalid `$home/...` baseline entry made the jail fail to COMPILE, so no lifecycle
# script spawned at all, with the same "everything is free" result.
#
# So the canary asserts the CONTROL'S SHAPE, not a verdict: a package whose answer we know must
# still install a large tree and still be materialized. `is-odd` would not catch this — it needs
# nothing legitimately. Skip with NUB_PROBE_SKIP_CANARY=1 when deliberately testing the fixture.

# PER-PACKAGE WALL-CLOCK BUDGET, overridable so a SLOW LANE can exist without touching the fleet.
#
# ⛔ THE DEFAULT IS UNCHANGED AT 2400s AND MUST STAY THAT WAY. Raising it globally is not safe: the
# job cap is 350 min, and a slice only fits because most packages finish in a fraction of the budget.
#
# Why the knob: 47 of 79 HARNESS-* records are HARNESS-TIMEOUT, and they cluster on exactly the
# packages a user is most likely to install — @aws-amplify/cli x8, appium-uiautomator2-driver x6,
# postman-code-generators x5, purescript x5, hugo-extended x4, plus gatsby and netlify-cli. Those
# are not measurements; they are absences, so the catalog silently omits its most important entries.
#
# ⛔ IT IS A BUDGET, NOT A LIVELOCK — the distinction that decides whether a bigger number is a fix
# or a cover-up. These packages walk the full 55-cell ladder and every cell is a COMPLETE install of
# a large tree, so ~55 x ~40s lands on the cap by arithmetic. A livelock would fail at every value;
# this fails at one value and the work genuinely takes that long.
#
# Raising the budget for a targeted re-measure changes NO measurement semantics — same ladder, same
# order, same predicate — so records produced under a longer budget stay comparable with the fleet's.
PKG_BUDGET="${NUB_CORPUS_PKG_BUDGET:-2400}"
case "$PKG_BUDGET" in ''|*[!0-9]*) echo "NUB_CORPUS_PKG_BUDGET must be an integer number of seconds, got '$PKG_BUDGET'" >&2; exit 2 ;; esac

# ⛔ TOOLCHAIN CENSUS — WHERE THE RUNNER'S TOOLS ACTUALLY LIVE.
#
# A package that shells out to a toolchain binary the jail does not grant READ on cannot exec it
# (exec of a binary requires read on the binary; `(allow process-exec)` is necessary and not
# sufficient), fails — often SILENTLY, exit 0 having done nothing — and walks the ladder to
# `write:"disk"`, which is no confinement at all. Diagnosing that needs one fact the cell logs do
# not carry: the ABSOLUTE PATH of the tool on THIS runner.
#
# Measured cost of not having it: appium-uiautomator2-driver logs only `Could not find JAVA`, and
# `PATH` appears in 0 of its 56 cell logs. Three hypotheses were raised and refuted on a dev Mac —
# missing read, a needed write, and libuv's PATH-lookup spawn — none of which reproduced, because
# the dev box's JDK sits in an already-granted prefix and the runner's does not. This line is what
# would have answered it in one run instead.
#
# ⛔ EMITTED ONCE PER BATCH, BEFORE THE CANARY, AND IT MUST STAY OUTSIDE EVERY MEASUREMENT. Running
# it inside a cell would touch files and perturb `seen`, i.e. corrupt the very digest the walk
# compares against the control. It only reads env and resolves names; it executes nothing.
{
  echo "toolchain census (runner environment, not a measurement):"
  echo "  PATH=${PATH}"
  for _v in JAVA_HOME ANDROID_HOME ANDROID_SDK_ROOT AGENT_TOOLSDIRECTORY npm_config_prefix NPM_CONFIG_PREFIX; do
    eval "_val=\${$_v-}"
    [ -n "${_val}" ] && echo "  ${_v}=${_val}"
  done
  for _t in java git python3 make cc brew pkg-config node npm; do
    echo "  which ${_t}: $(command -v "${_t}" 2>/dev/null || echo '(not found)')"
  done
} >&2

if [ "${NUB_PROBE_SKIP_CANARY:-0}" != "1" ]; then
  _can="${TMPDIR:-/tmp}/nub-canary-$$"; rm -rf "$_can"
  _canjson="$_can/results/runs/$(node -p 'process.platform+"-"+process.arch')/puppeteer/25.4.0/results.json"
  echo "fixture canary: installing puppeteer@25.4.0 …" >&2
  # `--control-only`: the canary asks whether the FIXTURE still installs a real tree, which the
  # control alone answers. Running the full 54-state walk here was ~50x the work and timed out at
  # 900s on a windows-latest runner, blocking every Windows shard behind a check that never
  # finished — and `timeout` killed it before it could write a reason.
  if ! "$TIMEOUT" 900 node "$here/search.mjs" puppeteer@25.4.0 --nub "$NUB" --force --control-only \
        --runs "$_can/results/runs" > /dev/null 2>"$_can.err"; then
    # ⛔ PRINT THE REASON, do not point at a file. On a CI runner that path is never uploaded and
    # the workspace is destroyed with the job, so "see $_can.err" is a dead end — MEASURED on a
    # windows-latest run where the canary refused in under a second and the only evidence died
    # with the runner. Worse, the probe step is `continue-on-error`, so the JOB stayed green over
    # a step that had already failed.
    echo "REFUSING TO RUN: the fixture canary could not complete." >&2
    echo "  --- first error from the canary ---" >&2
    grep -m5 -E '^[A-Za-z]*(Error|Exception)[:( ]|^error[: ]|REFUSING|ENOENT|EPERM|EINVAL' \
      "$_can.err" 2>/dev/null | sed 's/^/  /' >&2 || true
    echo "  --- first 20 lines ---" >&2
    head -20 "$_can.err" 2>/dev/null | sed 's/^/  /' >&2
    exit 2
  fi
  _files=$(node -e 'const f=process.argv[1];try{const d=require(f);console.log(d.control?.fileCount??0)}catch{console.log(0)}' "$_canjson")
  _mat=$(node -e 'const f=process.argv[1];try{const d=require(f);console.log(String(d.control?.materialized))}catch{console.log("false")}' "$_canjson")
  if [ "$_files" -lt 5000 ] || [ "$_mat" != "true" ]; then
    echo "REFUSING TO RUN: THE FIXTURE IS NOT PRODUCING A REAL INSTALL." >&2
    echo "  puppeteer@25.4.0 control: ${_files} files (expected >5000), materialized=${_mat} (expected true)" >&2
    echo "  A fixture in this state makes EVERY package measure as needing nothing." >&2
    echo "  Suspect: a change to makeFixture, or an invalid entry in baseline.json." >&2
    # ⛔ WHERE DID THE FILES GO? The count walks the FIXTURE ROOT and does not traverse symlinks or
    # junctions, so it only sees what physically lives under that root. If nub's store resolves
    # somewhere the redirected HOME does not cover — which is the live question on Windows, where
    # the cache root is %LOCALAPPDATA% rather than $HOME/.cache — the tree is real and the count is
    # still small. Printing both locations turns "the fixture is broken" into a decidable question
    # instead of a guess.
    echo "  --- where the bytes actually are ---" >&2
    node -e '
      const fs = require("node:fs"), os = require("node:os"), path = require("node:path");
      const json = process.argv[1];
      let rec = null; try { rec = JSON.parse(fs.readFileSync(json, "utf8")); } catch {}
      console.error(`  platform: ${process.platform}  homedir: ${os.homedir()}`);
      for (const k of ["LOCALAPPDATA", "APPDATA", "XDG_CACHE_HOME", "NUB_CACHE_DIR"]) {
        if (process.env[k]) console.error(`  env ${k}=${process.env[k]}`);
      }
      const guess = [
        path.join(os.homedir(), ".cache", "nub"),
        process.env.LOCALAPPDATA && path.join(process.env.LOCALAPPDATA, "nub"),
      ].filter(Boolean);
      for (const g of guess) {
        let n = 0; (function w(d, depth) {
          if (depth > 6) return;
          let e; try { e = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
          for (const x of e) { if (x.isDirectory()) w(path.join(d, x.name), depth + 1); else n++; }
        })(g, 0);
        console.error(`  ${fs.existsSync(g) ? "EXISTS" : "absent"}  ${g}  (${n} files, depth<=6)`);
      }
      if (rec?.control) console.error(`  recorded control fileCount=${rec.control.fileCount} materialized=${rec.control.materialized}`);
    ' "$_canjson" 2>&1 >/dev/null || true
    rm -rf "$_can" "$_can.err"; exit 2
  fi
  echo "fixture canary OK: ${_files} files, materialized=${_mat}" >&2
  rm -rf "$_can" "$_can.err"
fi

FORCE=""
[ "${1:-}" = "--force" ] && { FORCE="--force"; shift; }

# ⛔ RE-MEASURE ONLY THE VERDICTS A JAIL-OFF CHANGE INVALIDATED — the harness HASH is too blunt for
# this. `--stale-harness` keys on a sha of search.mjs+states.mjs, so ANY edit invalidates every
# record, a comment reword included. MEASURED right after the jail-off fix: all 794 macOS records
# sat on stale revisions, so `--stale-harness` would have discarded hundreds of perfectly good
# MINIMUM measurements to re-check a switch those records never touch.
#
# The precise scope is by VERDICT CLASS, not by instrument revision. Only two verdicts are derived
# from the jail-off cell:
#   BROKEN-WITHOUT-JAIL-TOO      — "fails with the jail off too", read straight off that cell
#   BROKEN-EVEN-WITH-EVERYTHING  — reached only by RULING OUT the jail-off cell
# A MINIMUM never runs it, so a MINIMUM is unaffected by any jail-off bug, at any revision.
#
# Use after changing anything about how confinement is turned off.
if [ "${1:-}" = "--stale-jailoff" ]; then
  shift
  node -e '
    const fs = require("node:fs"), path = require("node:path");
    const runs = process.argv[1];
    const out = [];
    (function walk(d) { let e; try { e = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
      for (const x of e) { const f = path.join(d, x.name);
        if (x.isDirectory()) walk(f); else if (x.name === "results.json") out.push(f); } })(runs);
    const DERIVED = new Set(["BROKEN-WITHOUT-JAIL-TOO", "BROKEN-EVEN-WITH-EVERYTHING"]);
    let purged = 0, kept = 0;
    for (const f of out) {
      let r; try { r = JSON.parse(fs.readFileSync(f, "utf8")); } catch { continue; }
      if (!DERIVED.has(r.verdict)) { kept++; continue; }
      fs.rmSync(path.dirname(f), { recursive: true, force: true });
      purged++;
    }
    console.error(`purged ${purged} jail-off-derived record(s); kept ${kept} unaffected`);
  ' "$RUNS_ROOT" >&2
fi

# ⛔ RE-MEASURE ONLY WHAT A HARNESS CHANGE INVALIDATED. The collator refuses to ship a catalog whose
# records span several harness revisions — correctly, since a record means something different under
# a changed instrument. But the two existing options are both wrong for that: a plain resume SKIPS
# the stale records (they are valid measurements), and `--force` re-runs the whole corpus.
#
# MEASURED: 358 records across 3 revisions (305 / 47 / 6-unknown) after one afternoon of fixes. That
# is 53 to redo, not 358.
#
# `--stale-harness` lists exactly the mismatching records, deletes them so the resume check re-runs
# them naturally, and leaves everything measured under the current revision alone.
if [ "${1:-}" = "--stale-harness" ]; then
  shift
  _cur="$(node -e '
    const { createHash } = require("node:crypto"), fs = require("node:fs"), path = require("node:path");
    const here = process.argv[1], h = createHash("sha256");
    for (const f of ["search.mjs", "states.mjs"]) h.update(fs.readFileSync(path.join(here, f)));
    console.log(h.digest("hex").slice(0, 16));
  ' "$here")"
  echo "current harness revision: $_cur" >&2
  _n="$(node -e '
    const fs = require("node:fs"), path = require("node:path");
    const [root, cur] = process.argv.slice(1);
    let n = 0;
    (function walk(d) {
      let ents; try { ents = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
      for (const e of ents) {
        const f = path.join(d, e.name);
        if (e.isDirectory()) { walk(f); continue; }
        if (e.name !== "results.json") continue;
        let r; try { r = JSON.parse(fs.readFileSync(f, "utf8")); } catch { continue; }
        // An UNKNOWN revision counts as stale: a record that cannot name its instrument cannot be
        // trusted to have been taken under this one.
        if ((r.provenance?.harnessSha256 ?? null) !== cur) {
          fs.rmSync(path.dirname(f), { recursive: true, force: true });
          n++;
        }
      }
    })(root, cur);
    console.log(n);
  ' "$RUNS_ROOT" "$_cur")"
  echo "purged $_n record(s) measured under an older harness — they will be re-run below" >&2
fi

if [ "${1:-}" = "--file" ]; then
  [ -r "${2:-}" ] || { echo "cannot read worklist ${2:-}"; exit 2; }
  set -- $(grep -vE '^\s*(#|$)' "$2")
fi

# ⛔ A FAILED RUN MUST BE AS LEGIBLE AS A SUCCESSFUL ONE, AND MUST LAND ON DISK.
#
# This loop used to send stderr to /dev/null and emit a bare
# `{"verdict":"HARNESS-TIMEOUT-OR-CRASH"}` to stdout. MEASURED consequence: 54 of 100 packages in
# one sweep failed, the reason for every one was discarded, nothing was written under results/,
# and the surviving 46 read as a completed sweep. A biased sample -- the heavy native builds are
# exactly the ones that fail -- was reported as the corpus.
#
# Three things this now guarantees:
#   1. stderr is KEPT, per package, next to that package's other logs.
#   2. TIMEOUT and CRASH are distinguished (`timeout` exits 124), because they need opposite fixes.
#   3. The failure is written as a results.json, so it is visible to the collator and the watcher
#      instead of living only in a stdout stream a restart loses.
ATTEMPTED=0; RECORDED=0; FAILED=0; SKIPPED_PAST_DEADLINE=0
PLAT="$(node -p 'process.platform + "-" + process.arch')"

# ⛔ STOP BEFORE THE JOB IS KILLED, SO THE WORK ALREADY DONE CAN BE COMMITTED.
#
# A CI job that hits its wall-clock cap is TERMINATED — no commit, no queue update, rows left
# claimed, chain dead. Everything measured up to that instant is lost with the runner. The failure is
# silent in the sense that matters: the run just stops, and the slice looks like it never happened.
#
# MEASURED, and this is not a hypothetical: the one Windows run that completed measured 15 packages
# in 198 minutes -- 13.2 min/package. A 100-package slice therefore needs ~1,320 minutes of measuring
# against a 350-minute job cap, so EVERY Windows slice would die at the cap having measured ~20 and
# committed none of them. Windows could never finish a slice at all.
#
# A deadline fixes it for every platform at once, and better than tuning a per-OS slice size would:
# the batch stops STARTING packages once the remaining budget cannot hold another one, returns
# normally, and the caller commits what exists. Unmeasured rows simply stay pending for the next
# slice, which is exactly the queue's designed behaviour.
#
# Deliberately measured per package rather than assumed: `_pkg_budget` tracks the slowest package
# seen so far, because the cost varies by an order of magnitude between a pure-JS postinstall and a
# native build, and stopping on the AVERAGE would still let one heavy package overrun the cap.
DEADLINE="${NUB_CORPUS_DEADLINE:-0}"          # epoch seconds; 0 disables
_pkg_budget=0
_now() { date +%s; }
for spec in "$@"; do
  if [ "$DEADLINE" -gt 0 ]; then
    _left=$(( DEADLINE - $(_now) ))
    if [ "$_left" -le "$_pkg_budget" ]; then
      SKIPPED_PAST_DEADLINE=$((SKIPPED_PAST_DEADLINE + 1))
      continue
    fi
  fi
  _started=$(_now)
  ATTEMPTED=$((ATTEMPTED + 1))
  pkg="${spec%@*}"; ver="${spec##*@}"
  d="$RUNS_ROOT/$PLAT/$(printf '%s' "$pkg" | tr '/' '+')/$ver"
  mkdir -p "$d"
  # ⛔ PASS --runs EXPLICITLY. search.mjs computes its OWN output path and defaults it to
  # `<harness dir>/results/runs`; without this flag, RUNS_ROOT above governs only the directories
  # this script creates and the record still lands beside the harness code. MEASURED: with
  # NUB_CORPUS_RUNS set, `records under records/runs: 0` and `records under harness/: 10`.
  # Wall clock for THIS spec. On the failure path search.mjs is killed before it can stamp its own
  # duration, and a timeout's elapsed time is exactly the cap it hit -- which is what separates
  # "raise the budget" from "this spec is hopeless at any budget".
  _spec_t0=$(date +%s)
  if "$TIMEOUT" "$PKG_BUDGET" node "$here/search.mjs" "$spec" --nub "$NUB" --runs "$RUNS_ROOT" $FORCE 2>"$d/harness-stderr.log"; then
    RECORDED=$((RECORDED + 1))
    [ -s "$d/harness-stderr.log" ] || rm -f "$d/harness-stderr.log"
  else
    rc=$?
    FAILED=$((FAILED + 1))
    if [ "$rc" -eq 124 ]; then verdict="HARNESS-TIMEOUT"; why="exceeded the 2400s per-package cap"
    else verdict="HARNESS-CRASH"; why="search.mjs exited $rc — see harness-stderr.log"; fi
    # THE FIRST REAL ERROR LINE, NOT THE TAIL. A node stack trace ENDS with the version banner, so
    # tailing a crash log reports "Node.js v26.5.0" and hides the message that says what broke.
    # Same trap that cost five wrong diagnoses on one package: read the first error, not the last.
    tail="$(grep -m1 -E '^[A-Za-z]*(Error|Exception)[:( ]|^error[: ]' "$d/harness-stderr.log" 2>/dev/null || true)"
    [ -n "$tail" ] || tail="$(head -c 400 "$d/harness-stderr.log" 2>/dev/null | tr -d '\000')"
    # ⛔⛔ STAMP PROVENANCE ON THE FAILURE RECORD TOO — WITHOUT IT A RE-MEASURE IS UNFALSIFIABLE.
    # `search.mjs` builds provenance only after a walk completes, and a timeout kills it long before
    # that, so every HARNESS-TIMEOUT/CRASH record used to land with `provenance` ABSENT ENTIRELY
    # (measured: `keys=0` on all 7 probe specs). The consequence is not cosmetic: a spec that WAS
    # re-measured and timed out AGAIN wrote a record byte-indistinguishable from one never touched.
    # So the timeout-recovery probe could only ever detect a RECOVERY — "still HARNESS-TIMEOUT" was
    # no evidence at all, and I misread a 1-of-4 result as 1-of-4 when it may have been 1-of-1.
    # That matters because the decision it feeds is whether to re-dispatch ~170 timed-out specs, the
    # largest single runner commitment left; authorising it on an unreadable negative would burn
    # hours to learn nothing.
    #
    # `at` alone settles it — a fresh timestamp proves the spec ran. `nubGitSha` comes from the same
    # NUB_GIT_SHA the workflow already exports to this step, so a failure record can be sha-cut
    # exactly like a measurement, which is what every "is this stale?" query in the corpus keys on.
    # Deliberately NOT the full provenance block: no sha256 of the binary (a needless hash per
    # failure) and nothing that implies a measurement happened. This attests WHEN AND WITH WHAT the
    # instrument ran, never what it found.
    # ⛔ `$NUB`/`$PLAT` ARE SHELL LOCALS, NOT EXPORTED — they must ride argv. Reading them as
    # `process.env.*` yields null silently and the record looks well-formed while attesting nothing.
    # `NUB_GIT_SHA` is the exception: the workflow exports it to this step, so env is correct there.
    node -e '
      const [f,pkg,ver,verdict,why,rc,tail,nubPath,plat,dur] = process.argv.slice(1);
      require("fs").writeFileSync(f, JSON.stringify(
        { pkg, version: ver, verdict, why, exitCode: Number(rc), stderrTail: tail,
          provenance: { at: new Date().toISOString(), nubGitSha: process.env.NUB_GIT_SHA || null,
                        nubPath: nubPath || null, platform: plat || null,
                        durationMs: Number(dur) * 1000 } },
        null, 2));
    ' "$d/results.json" "$pkg" "$ver" "$verdict" "$why" "$rc" "$tail" "$NUB" "$PLAT" "$(( $(date +%s) - _spec_t0 ))"
    echo "{\"pkg\":\"$pkg\",\"version\":\"$ver\",\"verdict\":\"$verdict\",\"exitCode\":$rc}"
    echo "  ✗ $spec — $verdict ($why)" >&2
    tail -3 "$d/harness-stderr.log" 2>/dev/null | sed 's/^/      /' >&2
  fi
  # ⛔ PUBLISH THIS RECORD NOW, not at the end of the slice. A slice runs for hours and used to commit
  # once at the end, so a runner that died at minute 115 lost 100 measurements and nothing was visible
  # until it finished. Publishing per package caps the loss at one and makes progress observable while
  # the run is still going.
  #
  # Deliberately NOT allowed to fail the batch: `|| true` plus a hook that always exits 0. A record
  # that does not publish is not lost — it stays on disk, the end-of-slice commit sweeps it up, and
  # the CI artifact carries it regardless. Killing a two-hour measurement because a push was rejected
  # would trade the thing being protected for the protection.
  if [ -n "${NUB_CORPUS_ON_RECORD:-}" ] && [ -f "$d/results.json" ]; then
    "$NUB_CORPUS_ON_RECORD" "$d" || true
  fi

  # Track the SLOWEST package seen, not the average: cost varies by an order of magnitude between a
  # pure-JS postinstall and a native build, so stopping on the mean would still let one heavy package
  # start late and overrun the cap.
  _elapsed=$(( $(_now) - _started ))
  [ "$_elapsed" -gt "$_pkg_budget" ] && _pkg_budget="$_elapsed"
  true
done

# ⛔ RE-VERIFY EVERY NUB-DEFECT VERDICT SERIALLY, ONCE THE BATCH HAS DRAINED.
#
# `BROKEN-EVEN-WITH-EVERYTHING` is the harness accusing NUB of a defect — the single most
# trust-destroying thing it can emit, and the one output nobody should have to re-derive by hand.
# It must never rest on a measurement taken while N shards were hammering the box.
#
# MEASURED: `@pdftron/pdfnet-node@7.1.1` was recorded BROKEN-EVEN-WITH-EVERYTHING, and the same
# package at the same widest grant on the same box installs rc=0 once the box is quiet. Its sweep
# failure was `npm ERR! rimraf: missing path` + `Callback called more than once` — npm 6 racing on
# its own _cacache. Old-Node pins are the exposed set, because publish-date pinning hands a 2020
# package npm 6.
#
# The double control and the confirmation retry did NOT catch it: both attempts ran inside the same
# busy window, so they agreed with each other and were both wrong. Only re-measuring AFTER the batch
# drains changes the condition that caused it — which is why this loop lives here, at the end,
# rather than as another retry inside the cell.
if [ "$RECORDED" -gt 0 ]; then
  _defects="$(node -e '
    const fs = require("fs"), path = require("path");
    const root = process.argv[1];
    const out = [];
    (function walk(d) {
      let ents; try { ents = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
      for (const e of ents) {
        const f = path.join(d, e.name);
        if (e.isDirectory()) walk(f);
        else if (e.name === "results.json") {
          try {
            const r = JSON.parse(fs.readFileSync(f, "utf8"));
            if (r.verdict === "BROKEN-EVEN-WITH-EVERYTHING") out.push(`${r.pkg}@${r.version}`);
          } catch {}
        }
      }
    })(root);
    console.log(out.join(" "));
  ' "$RUNS_ROOT/$PLAT" 2>/dev/null)"
  if [ -n "$_defects" ]; then
    echo "" >&2
    echo "re-verifying $(printf '%s\n' $_defects | wc -l | tr -d ' ') nub-defect verdict(s) SERIALLY on a quiet box …" >&2
    for spec in $_defects; do
      pkg="${spec%@*}"; ver="${spec##*@}"
      d="$RUNS_ROOT/$PLAT/$(printf '%s' "$pkg" | tr '/' '+')/$ver"
      "$TIMEOUT" "$PKG_BUDGET" node "$here/search.mjs" "$spec" --nub "$NUB" --runs "$RUNS_ROOT" --force \
        2>"$d/harness-stderr-reverify.log" >/dev/null || true
      v="$(node -e 'try{console.log(require(process.argv[1]).verdict)}catch{console.log("?")}' \
           "$d/results.json" 2>/dev/null)"
      if [ "$v" = "BROKEN-EVEN-WITH-EVERYTHING" ]; then
        echo "  ✗ $spec — CONFIRMED under serial re-verify: a real nub defect" >&2
      else
        echo "  ↺ $spec — was a LOAD-INDUCED FALSE defect; now $v" >&2
      fi
    done
  fi
fi

# COVERAGE IS THE HEADLINE. A sweep that measured half its worklist is not a sweep, and the number
# that says so belongs at the end where it cannot be missed.
echo "" >&2
echo "attempted $ATTEMPTED   recorded $RECORDED   FAILED $FAILED" >&2
# ⛔ NEVER LET A DEADLINE STOP LOOK LIKE A COMPLETE SLICE. If packages were skipped the caller must
# know, or a partial slice reads as full coverage and those rows are silently never re-run.
if [ "$SKIPPED_PAST_DEADLINE" -gt 0 ]; then
  echo "DEADLINE: stopped before $SKIPPED_PAST_DEADLINE package(s) — the job cap would have killed the run" >&2
  echo "  slowest package took ${_pkg_budget}s; those rows stay pending for the next slice" >&2
fi
[ "$FAILED" -eq 0 ] || echo "⛔ $FAILED of $ATTEMPTED PRODUCED NO MEASUREMENT — the recorded set is a BIASED SAMPLE, not the corpus" >&2
