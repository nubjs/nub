#!/usr/bin/env bash
# Drop-in PM conformance harness — proves nub is a true drop-in package manager
# in both directions:
#
#   Direction A (nub READS others): real PM writes its lockfile → nub
#     frozen-installs from it successfully → node_modules are correct.
#
#   Direction B (others READ nub): nub writes the lockfile → real PM
#     frozen-installs from it without modification (zero churn).
#
# See README.md for the full loop and design rationale.
#
# Usage:  run.sh [<path-to-nub>] [fixture ...]
# Env:    SANDBOX_ROOT=<dir>    reuse/inspect the sandbox (implies KEEP)
#         KEEP=1                keep the sandbox on success
#         SKIP_YARN=1           skip yarn legs even if yarn is on PATH
#         SKIP_BUN=1            skip bun legs even if bun is on PATH
#
# Exit: 0 = all required legs pass (skips for missing tools are fine);
#       1 = at least one FAIL.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NUB="${1:-}"
if [ -z "$NUB" ]; then
  for candidate in \
    "$(cd "$HERE/../.." && pwd)/target/release/nub" \
    "$(cd "$HERE/../.." && pwd)/target/debug/nub"; do
    [ -x "$candidate" ] && { NUB="$candidate"; break; }
  done
fi
shift 2>/dev/null || true
NUB="$(cd "$(dirname "$NUB")" && pwd)/$(basename "$NUB")"
[ -x "$NUB" ] || { echo "error: nub binary not found/executable: $NUB" >&2; exit 2; }

NUB_VERSION="$("$NUB" --version 2>/dev/null || echo '?')"

# Fixture list — each is a subdirectory of fixtures/
ALL_FIXTURES=(simple peers scoped optional-deps alias file-dep peer-meta deep-graph postinstall overrides-ref overrides-nested patched-deps patched-deps-no-newline catalog workspace workspace-dedup empty-root-importer git-dep platform-optional dist-tag-spec range-forms alias-scoped has-install-script injected-deps)
FIXTURES=("$@")
[ ${#FIXTURES[@]} -gt 0 ] || FIXTURES=("${ALL_FIXTURES[@]}")

# Detect available PMs — skip with a loud note if absent.
HAVE_NPM=0;  command -v npm  >/dev/null 2>&1 && HAVE_NPM=1
HAVE_PNPM=0; command -v pnpm >/dev/null 2>&1 && HAVE_PNPM=1
HAVE_YARN=0; command -v yarn >/dev/null 2>&1 && [ "${SKIP_YARN:-0}" != "1" ] && HAVE_YARN=1
HAVE_BUN=0;  command -v bun  >/dev/null 2>&1 && [ "${SKIP_BUN:-0}"  != "1" ] && HAVE_BUN=1

NPM_VERSION="$(npm  --version 2>/dev/null || echo MISSING)"
PNPM_VERSION="$(pnpm --version 2>/dev/null || echo MISSING)"
YARN_VERSION="$(yarn --version 2>/dev/null || echo MISSING)"
BUN_VERSION="$(bun  --version 2>/dev/null || echo MISSING)"

# Hermetic sandbox: redirect HOME + XDG so no dev-box config leaks in or out.
# Mktemp template deliberately avoids "aube" (brand sweep would false-positive).
CREATED_SANDBOX=0
if [ -z "${SANDBOX_ROOT:-}" ]; then
  SANDBOX_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/nub-dropinpm.XXXXXX")"
  CREATED_SANDBOX=1
fi
mkdir -p "$SANDBOX_ROOT/home" "$SANDBOX_ROOT/runs" "$SANDBOX_ROOT/logs"
export HOME="$SANDBOX_ROOT/home"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_STATE_HOME="$HOME/.local/state"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_STATE_HOME"

# Clear any PM env that could steer lockfile format decisions.
unset npm_config_default_lockfile_format NPM_CONFIG_DEFAULT_LOCKFILE_FORMAT 2>/dev/null || true

# npm's audit report is a separate registry service with its own outages and no
# bearing on the lockfile: a degraded advisory endpoint stalled every npm case
# for 100-300 s (npm's fetch timeout) and timed the CI job out on every branch
# for an evening (2026-09-03), while a bare `npm install --no-audit` of the same
# fixture took 1 s. fund and the update notifier are the other two network side
# trips with nothing to say about the result.
export npm_config_audit=false npm_config_fund=false npm_config_update_notifier=false

echo "=== nub drop-in PM conformance ==="
echo "nub:      $NUB ($NUB_VERSION)"
echo "npm:      $NPM_VERSION  (HAVE=$HAVE_NPM)"
echo "pnpm:     $PNPM_VERSION  (HAVE=$HAVE_PNPM)"
echo "yarn:     $YARN_VERSION  (HAVE=$HAVE_YARN)"
echo "bun:      $BUN_VERSION  (HAVE=$HAVE_BUN)"
echo "sandbox:  $SANDBOX_ROOT"
echo ""

# step <log> <label> <cmd...> — append output to log, return exit code.
# Must be called from inside a ( cd "$proj" && ... ) subshell.
step() {
  local log="$1" label="$2"; shift 2
  { echo; echo "### $label"; echo "### \$ $*"; } >>"$log"
  "$@" >>"$log" 2>&1
}

wipe_node_modules() {
  find "$1" -name node_modules -type d -prune -exec rm -rf {} +
}

stage_fixture() {
  local fixture="$1" proj="$2"
  rm -rf "$proj"
  mkdir -p "$proj"
  cp -R "$HERE/fixtures/$fixture/." "$proj/"
}

# assert_node_modules <proj> <log> — every direct dep from package.json must exist
# Checks dependencies + devDependencies; skips optionalDependencies (platform-
# conditional) since their presence is legitimately OS-dependent.
assert_node_modules() {
  local proj="$1" log="$2"
  local pkg="$proj/package.json"
  local failed=0
  local deps
  deps=$(node -e "
    const p = require('$pkg');
    const all = Object.keys({...p.dependencies, ...p.devDependencies});
    all.forEach(d => console.log(d));
  " 2>/dev/null) || { echo "FAILED: could not parse package.json" >>"$log"; return 1; }
  while IFS= read -r dep; do
    [ -z "$dep" ] && continue
    if [ ! -d "$proj/node_modules/$dep" ]; then
      echo "FAILED: node_modules/$dep missing after frozen install" >>"$log"
      failed=1
    fi
  done <<< "$deps"
  return $failed
}

# skip_reason <fixture> <direction> <pm> — print a reason string if this combo
# is a permanent ecosystem-level impossibility (not a fixable nub bug). Empty
# output means "run it." These live here (not in expected-failures.txt) because
# no lockfile nub could write would ever make them pass.
skip_reason() {
  local fixture="$1" dir="$2" pm="$3"
  case "$fixture--$dir--$pm" in
    # yarn v1 has no npm: alias syntax in its lockfile; it records the alias
    # with a different key and bun rejects it. Both directions are ecosystem-
    # mismatches, not nub bugs.
    alias--A--yarn) echo "yarn v1 alias syntax diverges from npm: protocol" ;;
    alias--B--yarn) echo "yarn v1 alias syntax diverges from npm: protocol" ;;

    # alias-scoped: same yarn-v1 npm:-alias divergence as `alias`, here with a
    # SCOPED target (npm:@types/semver@7.5.0). yarn v1 records the alias under a
    # different key shape than the npm: protocol, so both directions are an
    # ecosystem mismatch, not a nub bug.
    alias-scoped--A--yarn) echo "yarn v1 alias syntax diverges from npm: protocol" ;;
    alias-scoped--B--yarn) echo "yarn v1 alias syntax diverges from npm: protocol" ;;

    # The workspace fixture declares its internal dep with the `workspace:*`
    # protocol. Only pnpm and bun (and yarn-berry) understand that protocol;
    # npm and yarn-v1 reject it outright (npm: EUNSUPPORTEDPROTOCOL, yarn-v1
    # can't resolve `@conform/util@workspace:*` off the npm registry), so they
    # never write a comparable lockfile. Ecosystem-level mismatch, not a nub bug.
    workspace--A--npm)  echo "npm does not support the workspace: protocol (EUNSUPPORTEDPROTOCOL)" ;;
    workspace--B--npm)  echo "npm does not support the workspace: protocol (EUNSUPPORTEDPROTOCOL)" ;;
    workspace--A--yarn) echo "yarn v1 does not support the workspace: protocol" ;;
    workspace--B--yarn) echo "yarn v1 does not support the workspace: protocol" ;;
  esac

  # Feature fixtures are scoped to the PM whose lockfile encodes the diverging
  # field — running them against the other PMs would test nothing. Skip the
  # off-PM combos by design (the field has no representation there).
  #   overrides-ref / overrides-nested / patched-deps / patched-deps-no-newline
  #   / catalog : pnpm-only (pnpm.overrides $-refs, nested overrides,
  #     patchedDependencies, and catalogs live in pnpm-workspace.yaml /
  #     pnpm-lock.yaml — no npm/bun/yarn lockfile representation).
  case "$fixture" in
    overrides-ref|overrides-nested|patched-deps|patched-deps-no-newline|catalog)
      [ "$pm" != "pnpm" ] && echo "$fixture exercises a pnpm-only lockfile field; not represented in $pm"
      ;;
    has-install-script)
      # hasInstallScript / deprecated / inBundle / hasShrinkwrap /
      # bundleDependencies are npm-package-lock.json-specific per-package
      # keys. pnpm/yarn/bun encode install-script trust differently and
      # carry no representation of these fields, so the round-trip only
      # has meaning against npm.
      [ "$pm" != "npm" ] && echo "$fixture exercises npm-only package-lock.json keys; not represented in $pm"
      ;;
    injected-deps)
      # dependenciesMeta.injected is a pnpm-only mechanism — npm/yarn/bun have
      # no equivalent (and reject the workspace:* protocol the fixture uses).
      # pnpm records no inject entry in the lockfile (the dep is a plain
      # `link:`), so this is pnpm-scoped: dir A guards that nub frozen-reads
      # pnpm's injected-workspace lockfile. (Dir B is skip-by-design — pnpm
      # 10.15.1 self-rejects injected dependenciesMeta under --frozen-lockfile
      # even from its own lockfile; see the injected-deps--B case below.) The
      # hard-copy-vs-symlink layout is config/version-sensitive and outside the
      # lockfile — not asserted here.
      [ "$pm" != "pnpm" ] && echo "$fixture exercises pnpm-only injected deps; not represented in $pm"
      ;;
  esac

  # overrides-ref carries a `pnpm.overrides` block. A nub-identity project
  # consumes only neutral cross-tool fields (overrides/resolutions), never
  # another PM's branded config — so nub deliberately IGNORES `pnpm.overrides`
  # when it writes the lockfile (the symmetric brand boundary). Direction B
  # then can't match real pnpm, which DOES honor `pnpm.overrides`: pnpm rejects
  # nub's override-free lockfile with ERR_PNPM_LOCKFILE_CONFIG_MISMATCH. That is
  # intended divergence, not a fixable lockfile-writer bug. (Direction A — pnpm
  # writes the lockfile, nub frozen-reads it — now passes: the $-ref read-as-
  # drift bug #16 was fixed by the vendor/aube pin bump c948a38; this fixture is
  # now its regression guard.)
  case "$fixture--$dir" in
    overrides-ref--B) echo "nub-identity ignores pnpm.overrides by design (brand boundary); pnpm honors it" ;;

    # injected-deps Direction B is an ecosystem impossibility, not a nub bug:
    # pnpm 10.15.1 cannot round-trip dependenciesMeta.injected under
    # --frozen-lockfile even from ITS OWN lockfile. It writes a lockfile whose
    # importer block omits the `dependenciesMeta` field, then on frozen-verify
    # demands it back and self-rejects with ERR_PNPM_OUTDATED_LOCKFILE
    # ("importer dependencies meta (undefined) doesn't match package manifest").
    # nub's lockfile is byte-identical to pnpm's here, so no lockfile nub could
    # write would frozen-pass — a pnpm self-inconsistency. Direction A (nub
    # frozen-READS pnpm's injected-workspace lockfile and materializes the dep)
    # is the meaningful guard and passes.
    injected-deps--B) echo "pnpm 10.15.1 self-rejects injected dependenciesMeta under --frozen-lockfile (writes a lockfile it then won't frozen-accept); not a nub bug" ;;
  esac
}

# expected_reason <fixture> <direction> <pm> — look up a known-red entry.
# Lines in expected-failures.txt: "<fixture> <dir> <pm> <reason...>"
expected_reason() {
  awk -v f="$1" -v d="$2" -v p="$3" \
    '$1==f && $2==d && $3==p { $1=""; $2=""; $3=""; sub(/^  */,""); print; exit }' \
    "$HERE/expected-failures.txt" 2>/dev/null
}

RESULTS=()
FAILS=0
XPASSES=0

# ── Direction A (PM → nub): real PM writes lockfile, nub frozen-installs ────
dir_a() {
  local fixture="$1" pm="$2" pm_version="$3" proj="$4" log="$5"

  case "$pm" in
    pnpm)
      ( cd "$proj" && step "$log" "pnpm install (write lockfile)" \
        pnpm install --no-frozen-lockfile ) \
        || { echo "FAILED: pnpm install failed" >>"$log"; return 1; }
      [ -f "$proj/pnpm-lock.yaml" ] || { echo "FAILED: no pnpm-lock.yaml written" >>"$log"; return 1; }
      ;;
    npm)
      ( cd "$proj" && step "$log" "npm install (write lockfile)" \
        npm install ) \
        || { echo "FAILED: npm install failed" >>"$log"; return 1; }
      [ -f "$proj/package-lock.json" ] || { echo "FAILED: no package-lock.json written" >>"$log"; return 1; }
      ;;
    yarn)
      # yarn --no-lockfile means "resolve fresh but still write a lockfile"
      ( cd "$proj" && step "$log" "yarn install (write lockfile)" \
        yarn install ) \
        || { echo "FAILED: yarn install failed" >>"$log"; return 1; }
      [ -f "$proj/yarn.lock" ] || { echo "FAILED: no yarn.lock written" >>"$log"; return 1; }
      ;;
    bun)
      ( cd "$proj" && step "$log" "bun install (write lockfile)" \
        bun install ) \
        || { echo "FAILED: bun install failed" >>"$log"; return 1; }
      [ -f "$proj/bun.lock" ] || { echo "FAILED: no bun.lock written" >>"$log"; return 1; }
      ;;
  esac

  # Wipe node_modules so the frozen install is a real test
  wipe_node_modules "$proj"

  # nub frozen install from the lockfile the real PM just wrote
  ( cd "$proj" && step "$log" "nub install --frozen-lockfile" \
    "$NUB" install --frozen-lockfile ) \
    || { echo "FAILED: nub install --frozen-lockfile failed" >>"$log"; return 1; }

  # Assert node_modules correctness
  assert_node_modules "$proj" "$log" || return 1

  return 0
}

# ── Direction B (nub → PM): nub writes lockfile, real PM frozen-installs ────
dir_b() {
  local fixture="$1" pm="$2" pm_version="$3" proj="$4" log="$5"
  local lockfile_format=""

  case "$pm" in
    pnpm) lockfile_format=pnpm ;;
    npm)  lockfile_format=npm  ;;
    bun)  lockfile_format=bun  ;;
    yarn)
      # Direction B for yarn — nub WRITES a classic (v1) yarn.lock that real
      # yarn frozen-accepts with zero churn. The write path is the proven one:
      # `nub pm use yarn` converts the project's resolution state into a
      # classic yarn.lock (the classic writer is frozen-accepted by yarn
      # 1.13/1.22 — empirically verified, which is why the old refusal gate was
      # lifted). We seed that state with a real pnpm resolve so the conversion
      # has a graph to write from, then check yarn v1 accepts it unchanged.
      command -v pnpm >/dev/null 2>&1 || { echo "SKIP: pnpm needed to seed the yarn conversion" >>"$log"; return 0; }
      ( cd "$proj" && step "$log" "seed: pnpm resolve" pnpm install --lockfile-only ) \
        || { echo "FAILED: pnpm seed resolve failed" >>"$log"; return 1; }
      ( cd "$proj" && step "$log" "nub pm use yarn (convert → classic yarn.lock)" \
        "$NUB" pm use yarn ) \
        || { echo "FAILED: nub pm use yarn did not produce a yarn.lock" >>"$log"; return 1; }
      [ -f "$proj/yarn.lock" ] || { echo "FAILED: no yarn.lock after nub pm use yarn" >>"$log"; return 1; }
      [ -f "$proj/pnpm-lock.yaml" ] && { echo "FAILED: source pnpm-lock not removed by conversion" >>"$log"; return 1; }
      cp "$proj/yarn.lock" "$log.lock-before"
      wipe_node_modules "$proj"
      ( cd "$proj" && step "$log" "yarn v1 --frozen-lockfile accept" \
        yarn install --frozen-lockfile --non-interactive ) \
        || { echo "FAILED: yarn rejected nub's yarn.lock (--frozen-lockfile)" >>"$log"; return 1; }
      cmp -s "$log.lock-before" "$proj/yarn.lock" || {
        echo "FAILED: yarn rewrote nub's yarn.lock after frozen install (churn)" >>"$log"
        diff -u "$log.lock-before" "$proj/yarn.lock" >>"$log" || true
        return 1
      }
      assert_node_modules "$proj" "$log" || return 1
      return 0
      ;;
  esac

  # Have nub write the lockfile for this format
  ( cd "$proj" && \
    step "$log" "nub install (write $lockfile_format lockfile)" \
    env npm_config_default_lockfile_format="$lockfile_format" "$NUB" install \
  ) || { echo "FAILED: nub install failed" >>"$log"; return 1; }

  local lockfile=""
  case "$pm" in
    pnpm) lockfile="$proj/pnpm-lock.yaml" ;;
    npm)  lockfile="$proj/package-lock.json" ;;
    bun)  lockfile="$proj/bun.lock" ;;
  esac
  [ -f "$lockfile" ] || { echo "FAILED: nub wrote no $lockfile_format lockfile at $lockfile" >>"$log"; return 1; }

  # Capture lockfile before real PM touches it (zero-churn baseline)
  cp "$lockfile" "$log.lock-before"

  # Wipe node_modules
  wipe_node_modules "$proj"

  # Real PM frozen install
  case "$pm" in
    pnpm)
      ( cd "$proj" && step "$log" "pnpm frozen accept" \
        pnpm install --frozen-lockfile ) \
        || { echo "FAILED: pnpm rejected nub's lockfile (--frozen-lockfile)" >>"$log"; return 1; }
      ;;
    npm)
      ( cd "$proj" && step "$log" "npm ci accept" \
        npm ci ) \
        || { echo "FAILED: npm rejected nub's lockfile (ci)" >>"$log"; return 1; }
      ;;
    bun)
      # Frozen-accept must verify package integrity from a COLD cache — exactly
      # what a clean CI box does. A warm dev-host bun cache (packages already
      # extracted) skips re-verification and would mask a wrong-integrity
      # lockfile (e.g. nub collapsing a git dep into its registry entry), making
      # the result diverge from CI. Point bun at a throwaway cache dir so the
      # integrity check is real on every platform.
      ( cd "$proj" && step "$log" "bun frozen accept (cold cache)" \
        env BUN_INSTALL_CACHE_DIR="$proj/.bun-cold-cache" \
        bun install --frozen-lockfile ) \
        || { echo "FAILED: bun rejected nub's lockfile (--frozen-lockfile)" >>"$log"; return 1; }
      ;;
  esac

  # Zero-churn check: a frozen install must not rewrite the lockfile.
  cmp -s "$log.lock-before" "$lockfile" || {
    echo "FAILED: $pm rewrote the lockfile after frozen install (churn)" >>"$log"
    diff -u "$log.lock-before" "$lockfile" >>"$log" || true
    return 1
  }

  return 0
}

for fixture in "${FIXTURES[@]}"; do
  [ -d "$HERE/fixtures/$fixture" ] || { echo "error: unknown fixture '$fixture'" >&2; exit 2; }

  for direction in A B; do
    declare -a pms=()
    [ "$HAVE_NPM"  -eq 1 ] && pms+=(npm)
    [ "$HAVE_PNPM" -eq 1 ] && pms+=(pnpm)
    [ "$HAVE_YARN" -eq 1 ] && pms+=(yarn)
    [ "$HAVE_BUN"  -eq 1 ] && pms+=(bun)

    for pm in "${pms[@]}"; do
      case "$pm" in
        npm)  pm_version="$NPM_VERSION"  ;;
        pnpm) pm_version="$PNPM_VERSION" ;;
        yarn) pm_version="$YARN_VERSION" ;;
        bun)  pm_version="$BUN_VERSION"  ;;
      esac

      label="$fixture × dir-$direction × $pm@$pm_version"
      echo "--- $label"

      skip="$(skip_reason "$fixture" "$direction" "$pm")"
      if [ -n "$skip" ]; then
        echo "    skip (by design): $skip"
        RESULTS+=("$fixture|dir-$direction|$pm|$pm_version|SKIP (by design)")
        continue
      fi

      proj="$SANDBOX_ROOT/runs/$fixture--$direction--$pm"
      log="$SANDBOX_ROOT/logs/$fixture--$direction--$pm.log"
      : >"$log"
      stage_fixture "$fixture" "$proj"

      ok=0
      if [ "$direction" = "A" ]; then
        dir_a "$fixture" "$pm" "$pm_version" "$proj" "$log" || ok=$?
      else
        dir_b "$fixture" "$pm" "$pm_version" "$proj" "$log" || ok=$?
      fi

      reason="$(expected_reason "$fixture" "$direction" "$pm")"
      if [ "$ok" -eq 0 ] && [ -z "$reason" ]; then
        echo "    PASS"
        RESULTS+=("$fixture|dir-$direction|$pm|$pm_version|PASS")
      elif [ "$ok" -eq 0 ] && [ -n "$reason" ]; then
        # Stale expected-failure entry: fix landed without removing the entry.
        echo "    XPASS-STALE: now passes — remove from expected-failures.txt: $reason"
        XPASSES=$((XPASSES + 1))
        RESULTS+=("$fixture|dir-$direction|$pm|$pm_version|XPASS-STALE")
      elif [ -n "$reason" ]; then
        echo "    expected red: $reason"
        RESULTS+=("$fixture|dir-$direction|$pm|$pm_version|RED (expected)")
      else
        FAILS=$((FAILS + 1))
        echo "    FAIL — log: $log"
        tail -n 20 "$log" | sed 's/^/    | /'
        RESULTS+=("$fixture|dir-$direction|$pm|$pm_version|FAIL")
      fi
    done
  done
done

# Report any PMs that were skipped
[ "$HAVE_NPM"  -eq 0 ] && echo "NOTE: npm not on PATH — npm legs skipped"
[ "$HAVE_PNPM" -eq 0 ] && echo "NOTE: pnpm not on PATH — pnpm legs skipped"
[ "$HAVE_YARN" -eq 0 ] && echo "NOTE: yarn not on PATH (or SKIP_YARN=1) — yarn legs skipped"
[ "$HAVE_BUN"  -eq 0 ] && echo "NOTE: bun not on PATH (or SKIP_BUN=1) — bun legs skipped"

echo ""
echo "=== results ==="
printf '%-14s %-8s %-6s %-12s %s\n' "fixture" "dir" "pm" "pm-version" "result"
for row in "${RESULTS[@]}"; do
  IFS='|' read -r f d p v s <<<"$row"
  printf '%-14s %-8s %-6s %-12s %s\n' "$f" "$d" "$p" "$v" "$s"
done
echo ""

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
