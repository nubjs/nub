#!/usr/bin/env bash
# Lockfile conformance harness — nub writes the lockfile, real pnpm judges it.
# See README.md for the full loop; expected-failures.txt for the red list.
#
# Usage:  run.sh <path-to-nub> [fixture ...]
# Env:    LEGS="pnpm nub switch"      subset of legs to run
#         SANDBOX_ROOT=<dir>          reuse/inspect the sandbox (implies KEEP)
#         KEEP=1                      keep the sandbox on success
#
# Per-leg gates (a scenario passes only if every step holds):
#   pnpm   — a pnpm project (`packageManager: pnpm@<pin>`): nub writes
#            pnpm-lock.yaml; real pnpm `install --frozen-lockfile` accepts it,
#            and a follow-up real-pnpm mutable install leaves it byte-identical.
#   nub    — a nub project: nub writes nub.lock and no pnpm-named file, and a
#            frozen nub install works from it. nub.lock is pnpm's v9 format,
#            so the same file, renamed to pnpm-lock.yaml in a copy of the
#            project that declares pnpm, must then pass the pnpm leg's checks.
#   switch — the identity round trip: the pnpm leg's mutation, `nub pm use nub`
#            (nub.lock, zero pnpm-named files, a working frozen install), then
#            `nub pm use pnpm@<pin>`, and real pnpm judges the result.
#
# Every nub step in a nub project is also swept for `ERR_PNPM_`/`WARN_PNPM_`:
# there nub reports the engine's codes under its own prefix, so a pnpm-prefixed
# code is a leak. A pnpm project keeps pnpm's codes, as pnpm 12 prints them.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ $# -lt 1 ]; then
  echo "usage: run.sh <path-to-nub> [fixture ...]" >&2
  exit 2
fi
NUB="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
# Windows (Git Bash): tolerate a path given without the .exe suffix.
{ [ -x "$NUB" ] || ! [ -x "$NUB.exe" ]; } || NUB="$NUB.exe"
[ -x "$NUB" ] || { echo "error: nub binary not executable: $NUB" >&2; exit 2; }
shift

# The judge, fetched per run via npx into the sandbox HOME, so the pin is exact
# on every machine. It is the pnpm the engine tracks.
PNPM_PIN=12.4.1

ALL_FIXTURES=(simple workspace peer-heavy overrides platform-optional scoped git-dep patched)
FIXTURES=("$@")
[ ${#FIXTURES[@]} -gt 0 ] || FIXTURES=("${ALL_FIXTURES[@]}")
LEGS="${LEGS:-pnpm nub switch}"

# Hermetic sandbox: absolute HOME + XDG so neither the dev box's ~/.npmrc,
# caches, or stores leak in, nor the run leaves residue behind.
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-conformance.XXXXXX")"
  CREATED_SANDBOX=1
fi
mkdir -p "$SANDBOX_ROOT/home" "$SANDBOX_ROOT/runs" "$SANDBOX_ROOT/logs"
export HOME="$SANDBOX_ROOT/home"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"
# Windows (Git Bash): Node tools resolve os.homedir() from USERPROFILE, and
# npm roots its cache/userconfig in LOCALAPPDATA/APPDATA — HOME alone doesn't
# sandbox them on this OS, so point all three into the sandbox home too.
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*)
    mkdir -p "$HOME/AppData/Roaming" "$HOME/AppData/Local"
    USERPROFILE="$(cygpath -w "$HOME")"
    APPDATA="$(cygpath -w "$HOME/AppData/Roaming")"
    LOCALAPPDATA="$(cygpath -w "$HOME/AppData/Local")"
    export USERPROFILE APPDATA LOCALAPPDATA
    ;;
esac

run_pnpm() { npx -y "pnpm@$PNPM_PIN" "$@"; }

echo "== nub lockfile conformance =="
echo "nub:     $NUB ($("$NUB" --version 2>/dev/null || echo '?'))"
echo "node:    $(node --version)"
echo "pnpm:    $PNPM_PIN (pinned via npx)"
echo "sandbox: $SANDBOX_ROOT"
echo

# The mutation each fixture drives through nub. peer-heavy is the adversarial
# reviewer's exact blocker repro (clean project, `nub add`), everything else
# is a fresh `nub install` against the committed manifest.
fixture_cmd() {
  case "$1" in
    peer-heavy) echo "add react-dom@18.3.1 chokidar@3.6.0 react-redux@9.2.0" ;;
    *) echo "install" ;;
  esac
}

# nub_mutation <log> <proj> <fixture> — drive the fixture's nub mutation.
# `patched` is the only multi-step fixture: the full patch workflow (install →
# patch → edit → patch-commit, whose chained install is what must land the
# patch entry in the lockfile). Everything else is the single fixture_cmd verb.
nub_mutation() {
  local log="$1" proj="$2" fixture="$3"
  if [ "$fixture" = patched ]; then
    local edit="$proj/.patch-edit"
    nub_step "$log" "$proj" install || return $?
    nub_step "$log" "$proj" patch ms@2.1.3 --edit-dir "$edit" || return $?
    printf '\nmodule.exports.NUB_PATCHED = true;\n' >>"$edit/index.js" || return 1
    nub_step "$log" "$proj" patch-commit "$edit" || return $?
  else
    # shellcheck disable=SC2046
    nub_step "$log" "$proj" $(fixture_cmd "$fixture")
  fi
}

# fixture_post_check <fixture> <proj> <log> — fixture-specific assertion on
# the tree just linked. patched: the install must have applied the committed
# patch from the lockfile entry — an install that silently drops the patch is
# exactly the failure this fixture exists for.
fixture_post_check() {
  case "$1" in
    patched)
      grep -q NUB_PATCHED "$2/node_modules/ms/index.js" 2>/dev/null \
        || { echo "FAILED: the install did not apply the patch" >>"$3"; return 1; } ;;
  esac
  return 0
}

expected_reason() {
  # expected-failures.txt lines: "<fixture> <leg> <reason...>"
  awk -v f="$1" -v m="$2" '$1==f && $2==m { $1=""; $2=""; sub(/^  */,""); print; exit }' \
    "$HERE/expected-failures.txt" 2>/dev/null
}

wipe_node_modules() {
  find "$1" -name node_modules -type d -prune -exec rm -rf {} +
}

# step <log> <label> <cmd...> — append the command's output to the log,
# return its exit code without tripping -e at the call site (callers use if).
step() {
  local log="$1" label="$2"; shift 2
  {
    echo
    echo "### $label"
    echo "### \$ $*"
  } >>"$log"
  "$@" >>"$log" 2>&1
}

# is_nub_project <proj> — the step left a nub.lock and the manifest declares no pnpm.
is_nub_project() {
  [ -f "$1/nub.lock" ] && ! grep -qE '"packageManager": *"pnpm@' "$1/package.json"
}

# nub_step <log> <proj> <cmd...> — run nub in the project, then, when that left
# a nub project, sweep its captured output for a pnpm-prefixed code.
nub_step() {
  local log="$1" proj="$2"; shift 2
  local out="$log.nub-out"
  {
    echo
    echo "### nub $*"
  } >>"$log"
  local code=0
  (cd "$proj" && "$NUB" "$@") >"$out" 2>&1 || code=$?
  cat "$out" >>"$log"
  if is_nub_project "$proj" && grep -qE 'ERR_PNPM_|WARN_PNPM_' "$out"; then
    echo "### CODE LEAK: a nub project's output carries a pnpm-prefixed code" >>"$log"
    return 99
  fi
  return $code
}

# carry_patched_dependencies <proj> — move package.json's patchedDependencies,
# where a nub project records a patch, into pnpm-workspace.yaml, the only place
# pnpm reads them from.
carry_patched_dependencies() {
  (cd "$1" && node -e '
    const fs = require("fs");
    const manifest = JSON.parse(fs.readFileSync("package.json", "utf8"));
    const patched = manifest.patchedDependencies;
    if (!patched) process.exit(0);
    delete manifest.patchedDependencies;
    fs.writeFileSync("package.json", JSON.stringify(manifest, null, 2) + "\n");
    const entries = Object.entries(patched).map(([key, file]) => "  " + JSON.stringify(key) + ": " + JSON.stringify(file));
    const yaml = fs.existsSync("pnpm-workspace.yaml") ? fs.readFileSync("pnpm-workspace.yaml", "utf8") : "";
    const base = yaml === "" || yaml.endsWith("\n") ? yaml : yaml + "\n";
    fs.writeFileSync("pnpm-workspace.yaml", base + ["patchedDependencies:", ...entries].join("\n") + "\n");
  ')
}

# declare_pnpm <proj> — make the project a pnpm project by declaring the pin.
declare_pnpm() {
  (cd "$1" && node -e '
    const fs = require("fs");
    const manifest = JSON.parse(fs.readFileSync("package.json", "utf8"));
    manifest.packageManager = process.argv[1];
    fs.writeFileSync("package.json", JSON.stringify(manifest, null, 2) + "\n");
  ' "pnpm@$PNPM_PIN")
}

# stage_fixture <fixture> <proj> <identity> — copy the fixture as the kind of
# project the leg needs. A pnpm project declares the pinned pnpm. A nub project
# declares nothing and carries no pnpm-named file, so a fixture's
# pnpm-workspace.yaml gives way to the `workspaces` field it also carries.
stage_fixture() {
  local fixture="$1" proj="$2" identity="$3"
  rm -rf "$proj"
  mkdir -p "$proj"
  cp -R "$HERE/fixtures/$fixture/." "$proj/"
  case "$identity" in
    pnpm) declare_pnpm "$proj" ;;
    nub) rm -f "$proj/pnpm-workspace.yaml" ;;
  esac
}

no_pnpm_named_files() {
  local proj="$1" log="$2" after="$3" found
  found=$(find "$proj" -name '*pnpm*' -not -path '*/node_modules/*' 2>/dev/null || true)
  [ -z "$found" ] || { echo "FAILED: pnpm-named files after $after: $found" >>"$log"; return 1; }
}

# judge_pnpm <log> <proj> <fixture> <label> — real pnpm must accept the
# project's pnpm-lock.yaml frozen, link what the fixture checks for, and
# rewrite nothing on a mutable install.
# The project graph is the LAST of pnpm 12's two lockfile documents: a project
# pinning pnpm gets pnpm's own env lockfile first (`packageManagerDependencies`),
# then the project's.
project_document() {
  node -e '
    const fs = require("fs");
    const docs = fs.readFileSync(process.argv[1], "utf8").split(/^---$/m).filter((doc) => /^importers:/m.test(doc));
    if (docs.length === 0) process.exit(3);
    process.stdout.write(docs[docs.length - 1]);
  ' "$1"
}

# judge_pnpm <log> <proj> <fixture> <label> [churn-scope: all|project]
judge_pnpm() {
  local log="$1" proj="$2" fixture="$3" label="$4" scope="${5:-all}"
  wipe_node_modules "$proj"
  ( cd "$proj" && step "$log" "real pnpm frozen accept ($label)" run_pnpm install --frozen-lockfile ) \
    || { echo "FAILED: real pnpm rejected the lockfile ($label, --frozen-lockfile)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  cp "$proj/pnpm-lock.yaml" "$log.lock-before"
  ( cd "$proj" && step "$log" "real pnpm zero-churn rewrite ($label)" run_pnpm install ) \
    || { echo "FAILED: real pnpm mutable install errored ($label)" >>"$log"; return 1; }
  local before="$log.lock-before" after="$proj/pnpm-lock.yaml"
  if [ "$scope" = project ]; then
    project_document "$before" >"$log.doc-before" && project_document "$after" >"$log.doc-after" \
      || { echo "FAILED: no project document in the lockfile ($label)" >>"$log"; return 1; }
    before="$log.doc-before" after="$log.doc-after"
  fi
  cmp -s "$before" "$after" \
    || { echo "FAILED: real pnpm rewrote the lockfile ($label, churn):" >>"$log"; diff -u "$before" "$after" >>"$log" || true; return 1; }
}

leg_pnpm() {
  local proj="$1" log="$2" fixture="$3"
  stage_fixture "$fixture" "$proj" pnpm
  nub_mutation "$log" "$proj" "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: nub wrote no pnpm-lock.yaml" >>"$log"; return 1; }
  judge_pnpm "$log" "$proj" "$fixture" "pnpm project"
}

leg_nub() {
  local proj="$1" log="$2" fixture="$3"
  stage_fixture "$fixture" "$proj" nub
  nub_mutation "$log" "$proj" "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/nub.lock" ] || { echo "FAILED: nub wrote no nub.lock" >>"$log"; return 1; }
  no_pnpm_named_files "$proj" "$log" "the nub install" || return 1
  wipe_node_modules "$proj"
  nub_step "$log" "$proj" install --frozen-lockfile \
    || { echo "FAILED: frozen nub install from nub.lock (exit $?)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  # The judge's copy: the same manifests and lockfile as a pnpm project, with
  # the fixture's own pnpm-workspace.yaml back in place.
  local judge="$proj.as-pnpm"
  rm -rf "$judge"
  cp -R "$proj" "$judge"
  mv "$judge/nub.lock" "$judge/pnpm-lock.yaml"
  if [ -f "$HERE/fixtures/$fixture/pnpm-workspace.yaml" ]; then
    cp "$HERE/fixtures/$fixture/pnpm-workspace.yaml" "$judge/"
  fi
  declare_pnpm "$judge"
  carry_patched_dependencies "$judge"
  judge_pnpm "$log" "$judge" "$fixture" "nub.lock as pnpm-lock.yaml"
}

leg_switch() {
  local proj="$1" log="$2" fixture="$3"
  stage_fixture "$fixture" "$proj" pnpm
  nub_mutation "$log" "$proj" "$fixture" || { echo "FAILED: nub step (exit $?)" >>"$log"; return 1; }
  nub_step "$log" "$proj" pm use nub || { echo "FAILED: pm use nub (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/nub.lock" ] || { echo "FAILED: pm use nub left no nub.lock" >>"$log"; return 1; }
  no_pnpm_named_files "$proj" "$log" "pm use nub" || return 1
  wipe_node_modules "$proj"
  nub_step "$log" "$proj" install --frozen-lockfile \
    || { echo "FAILED: frozen nub install after pm use nub (exit $?)" >>"$log"; return 1; }
  fixture_post_check "$fixture" "$proj" "$log" || return 1
  nub_step "$log" "$proj" pm use "pnpm@$PNPM_PIN" || { echo "FAILED: pm use pnpm (exit $?)" >>"$log"; return 1; }
  [ -f "$proj/pnpm-lock.yaml" ] && [ ! -f "$proj/nub.lock" ] \
    || { echo "FAILED: pm use pnpm did not rename nub.lock back" >>"$log"; return 1; }
  # Churn is judged on the PROJECT document alone here. `pm use pnpm` declares a
  # `devEngines.packageManager` range the project did not carry before, and pnpm 12
  # records that range as the specifier in its own env lockfile — so its next
  # MUTABLE install refreshes that one line, exactly as it does after any hand
  # edit of the declaration. The frozen accept above is the guard that matters and
  # it passes byte-identical. Handing back a lockfile WITHOUT the env document is
  # not the alternative: pnpm 12 then refuses `--frozen-lockfile` outright with
  # "Cannot update packageManagerDependencies".
  judge_pnpm "$log" "$proj" "$fixture" "after the round trip" project
}

RESULTS=()
FAILS=0
XPASSES=0

for fixture in "${FIXTURES[@]}"; do
  [ -d "$HERE/fixtures/$fixture" ] || { echo "error: unknown fixture '$fixture'" >&2; exit 2; }
  for leg in $LEGS; do
    echo "--- $fixture × $leg"
    proj="$SANDBOX_ROOT/runs/$fixture--$leg"
    log="$SANDBOX_ROOT/logs/$fixture--$leg.log"
    : >"$log"
    ok=0
    case "$leg" in
      pnpm)   leg_pnpm   "$proj" "$log" "$fixture" || ok=$? ;;
      nub)    leg_nub    "$proj" "$log" "$fixture" || ok=$? ;;
      switch) leg_switch "$proj" "$log" "$fixture" || ok=$? ;;
      *) echo "error: unknown leg '$leg'" >&2; exit 2 ;;
    esac
    reason="$(expected_reason "$fixture" "$leg")"
    if [ "$ok" -eq 0 ] && [ -z "$reason" ]; then
      status="PASS"
    elif [ "$ok" -eq 0 ] && [ -n "$reason" ]; then
      # The red list must shrink the moment a fix lands — a passing entry is
      # stale and fails the run so the green flip can't go unrecorded.
      status="XPASS-STALE"
      XPASSES=$((XPASSES + 1))
      echo "    XPASS: now passes — delete its expected-failures.txt entry: $reason"
    elif [ -n "$reason" ]; then
      status="RED (expected)"
      echo "    expected red: $reason"
    else
      status="FAIL"
      FAILS=$((FAILS + 1))
      echo "    FAIL — log: $log"
      tail -n 15 "$log" | sed 's/^/    | /'
    fi
    RESULTS+=("$fixture|$leg|$status")
  done
done

echo
echo "== results =="
printf '%-18s %-7s %s\n' "fixture" "leg" "result"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r f m s <<<"$row"
  printf '%-18s %-7s %s\n' "$f" "$m" "$s"
done
echo

if [ "$FAILS" -gt 0 ] || [ "$XPASSES" -gt 0 ]; then
  echo "RESULT: FAIL ($FAILS unexpected failure(s), $XPASSES stale expected-failure entry/entries)"
  echo "sandbox kept for forensics: $SANDBOX_ROOT"
  exit 1
fi

echo "RESULT: OK (expected reds, if any, are listed above and tracked in expected-failures.txt)"
if [ "$CREATED_SANDBOX" -eq 1 ] && [ "${KEEP:-0}" != "1" ]; then
  rm -rf "$SANDBOX_ROOT"
else
  echo "sandbox kept: $SANDBOX_ROOT"
fi
