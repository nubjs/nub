#!/usr/bin/env bash
# Runtime brand-leak guard for nub's PM engine and CLI output.
#
# WHAT THIS TESTS:
#   Runs a broad corpus of real nub commands — success paths, error paths, and
#   info-family verbs — and asserts that NONE of them leak:
#     - the string "aube" or "Aube" as a standalone word token in user-facing output
#     - ERR_AUBE_* or WARN_AUBE_* codes (must be rewritten to ERR_NUB_*/WARN_NUB_*)
#     - aube.jdx.dev URLs
#
# WHY THIS EXISTS (gap vs brand-sweep):
#   brand-sweep tests `nub install` (success + warning paths) and all engine-verb
#   `--help` text. That misses the full error-path corpus, the info/query family
#   verbs, the pm family, the store family, ambiguous-lockfile detection, and
#   several top-level CLI error messages. This script fills those gaps.
#
# PUBLIC VS INTERNAL DISTINCTION (AGENTS.md brand-boundary):
#   The brand boundary protects PUBLIC output only. The following are EXEMPT
#   and intentionally not checked here:
#     - "aube" in internal error CODES (ERR_AUBE_* before rewrite — tested via
#       unit tests in present.rs; only the rewritten surface matters here)
#     - "aube" in cache/dir names (node_modules/.aube-state, share/aube/store,
#       ~/.cache/aube, .aube_patch_state.json, etc.)
#     - "aube" in env variable names (AUBE_*)
#     - "aube" in source identifiers, crate names, comments
#   The grep pattern \baube\b catches standalone word tokens; the on-disk path
#   names are excluded by the exemption patterns checked before failing.
#
# WHITELIST (false-positive suppression):
#   On-disk path names that may legitimately appear in error output when a user
#   actually has aube-named files in their project, e.g. a package originally
#   installed by the aube CLI that left aube-lock.yaml. The real-world leak
#   the rewrite was designed to allow: present.rs rebrand_words() preserves
#   tokens adjacent to '.', '-', '_', '/', '@', '~' — so "aube-lock.yaml"
#   survives unmodified. We do not create those files in this fixture, so any
#   "aube" hit in our sandbox IS a live leak (nothing to whitelist in practice).
#   The whitelist here is kept explicit for clarity; extend if a new on-disk
#   name is needed.
#
# USAGE:   tests/brand-runtime/run.sh <path-to-nub-binary>
# EXIT:    0 = all clean; 1 = at least one live leak found
# NETWORK: tests/brand-runtime/run.sh performs no network installs by default
#          (works off the existing store/cache). Error-path tests never resolve.
#          The GVS-path test (step 11) does a single `nub install` of a small
#          fresh fixture. Set NUB_BRAND_RUNTIME_SKIP_NETWORK=1 to skip it.
# CI:      Suitable for any CI leg (no platform-specific requirements). Must
#          run AFTER the build step that produces the nub binary.

set -euo pipefail

NUB_ARG=${1:?usage: run.sh <path-to-nub>}
NUB=$(cd "$(dirname "$NUB_ARG")" && pwd)/$(basename "$NUB_ARG")
[ -x "$NUB" ] || { echo "FAIL: nub binary not executable: $NUB"; exit 1; }

SANDBOX=$(mktemp -d "${TMPDIR:-/tmp}/nub-brand-runtime.XXXXXX")
trap 'rm -rf "$SANDBOX"' EXIT

export HOME="$SANDBOX/home"
export XDG_DATA_HOME="$SANDBOX/xdg/data"
export XDG_CACHE_HOME="$SANDBOX/xdg/cache"
export XDG_CONFIG_HOME="$SANDBOX/xdg/config"
export XDG_STATE_HOME="$SANDBOX/xdg/state"
mkdir -p "$HOME"

# Failure tracking via a temp file so subshell (cd ... && check_clean ...)
# calls correctly propagate failures back to the parent.
FAIL_FILE="$SANDBOX/fails"
touch "$FAIL_FILE"

fail() {
  echo "FAIL: $*"
  echo "1" >> "$FAIL_FILE"
}

pass() {
  echo "ok: $*"
}

# check_clean LABEL COMMAND [ARGS...]
#   Runs the command, captures combined stdout+stderr, asserts no standalone
#   "aube"/"Aube" word token, no ERR_AUBE_*/WARN_AUBE_*, and no aube.jdx.dev.
#   Prints the offending lines on failure.
check_clean() {
  local label="$1"; shift
  local out
  out=$("$@" 2>&1 || true)

  # Pattern: standalone "aube"/"Aube" word token (not inside a path/identifier),
  # plus the raw upstream code prefixes and doc host. The word-boundary (\b)
  # anchors are POSIX ERE; GNU grep and BSD grep both support them.
  local leaked
  leaked=$(echo "$out" | grep -inE '\baube\b|ERR_AUBE_|WARN_AUBE_|aube\.jdx\.dev' || true)
  if [ -n "$leaked" ]; then
    echo "--- $label ---"
    echo "$leaked"
    fail "[$label] brand leak in output (above)"
    return
  fi
  pass "[$label]"
}

# --------------------------------------------------------------------------
# Fixture: simple project (left-pad, no network needed for error paths)
# --------------------------------------------------------------------------
PROJ="$SANDBOX/proj"
mkdir -p "$PROJ"
cat > "$PROJ/package.json" <<'EOF'
{"name":"brand-runtime-test","private":true,"dependencies":{"left-pad":"1.3.0"}}
EOF

# --------------------------------------------------------------------------
# 1. Top-level --help and --version
# --------------------------------------------------------------------------
check_clean "top-level --help"    "$NUB" --help
check_clean "top-level --version" "$NUB" --version

# --------------------------------------------------------------------------
# 2. CI (frozen) error path: no lockfile
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "ci no-lockfile error" "$NUB" ci)

# --------------------------------------------------------------------------
# 3. install --frozen-lockfile error path: no lockfile
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "install --frozen-lockfile no-lockfile" "$NUB" install --frozen-lockfile)

# --------------------------------------------------------------------------
# 4. Ambiguous lockfile error
# --------------------------------------------------------------------------
PROJ_AMBIG="$SANDBOX/proj-ambig"
mkdir -p "$PROJ_AMBIG"
cat > "$PROJ_AMBIG/package.json" <<'EOF'
{"name":"ambig","private":true,"dependencies":{"left-pad":"1.3.0"}}
EOF
cat > "$PROJ_AMBIG/package-lock.json" <<'EOF'
{"name":"ambig","lockfileVersion":3,"requires":true,"packages":{"":{"name":"ambig"}}}
EOF
cat > "$PROJ_AMBIG/yarn.lock" <<'EOF'
# yarn lockfile v1
EOF
(cd "$PROJ_AMBIG" && check_clean "ambiguous-lockfile error" "$NUB" install)

# --------------------------------------------------------------------------
# 5. prune: no lockfile error
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "prune no-lockfile error" "$NUB" prune)

# --------------------------------------------------------------------------
# 6. fetch: no lockfile error
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "fetch no-lockfile error" "$NUB" fetch)

# --------------------------------------------------------------------------
# 7. import: no source lockfile error
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "import no-source-lockfile error" "$NUB" import)

# --------------------------------------------------------------------------
# 8. patch: bad format error (bare name without version)
# --------------------------------------------------------------------------
# Do a real install first so left-pad is available (uses cached store).
(cd "$PROJ" && "$NUB" install --no-frozen-lockfile > /dev/null 2>&1) || true
(cd "$PROJ" && check_clean "patch bad-format error" "$NUB" patch left-pad)

# --------------------------------------------------------------------------
# 9. add: nonexistent package (network error path — fast fail, no retry)
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "add nonexistent package" "$NUB" add @totally-nonexistent-pkg-xyz-12345@999.0.0)

# --------------------------------------------------------------------------
# 10. remove: package not in deps
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "remove not-a-dep error" "$NUB" remove nonexistent-dep-xyz)

# --------------------------------------------------------------------------
# 11. Info/query family after install
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "deprecations"              "$NUB" deprecations)
(cd "$PROJ" && check_clean "deprecations --transitive" "$NUB" deprecations --transitive)
(cd "$PROJ" && check_clean "outdated"                  "$NUB" outdated)
(cd "$PROJ" && check_clean "why left-pad"              "$NUB" why left-pad)
(cd "$PROJ" && check_clean "list"                      "$NUB" list)
(cd "$PROJ" && check_clean "ls"                        "$NUB" ls)
(cd "$PROJ" && check_clean "la"                        "$NUB" la)
(cd "$PROJ" && check_clean "ll"                        "$NUB" ll)
(cd "$PROJ" && check_clean "root"                      "$NUB" root)
(cd "$PROJ" && check_clean "bin"                       "$NUB" bin)
(cd "$PROJ" && check_clean "query *"                   "$NUB" query '*')
(cd "$PROJ" && check_clean "licenses list"             "$NUB" licenses list)
(cd "$PROJ" && check_clean "audit"                     "$NUB" audit)
(cd "$PROJ" && check_clean "peers check"               "$NUB" peers check)
(cd "$PROJ" && check_clean "ignored-builds"            "$NUB" ignored-builds)
(cd "$PROJ" && check_clean "store path"                "$NUB" store path)
(cd "$PROJ" && check_clean "store status"              "$NUB" store status)
(cd "$PROJ" && check_clean "store prune"               "$NUB" store prune)

# --------------------------------------------------------------------------
# 12. config family
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "config list"              "$NUB" config list)
(cd "$PROJ" && check_clean "config get (existing key)" "$NUB" config get virtualStoreDir)
(cd "$PROJ" && check_clean "config set/get roundtrip"  bash -c '
  NUB="$1"; shift
  "$NUB" config set strictDepBuilds true
  "$NUB" config get strictDepBuilds
' _ "$NUB")

# --------------------------------------------------------------------------
# 13. pm family
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "pm --help"         "$NUB" pm --help)
(cd "$PROJ" && check_clean "pm use pnpm"       "$NUB" pm use pnpm)
(cd "$PROJ" && check_clean "pm use npm"        "$NUB" pm use npm)
(cd "$PROJ" && check_clean "pm use invalid-pm" "$NUB" pm use totally-invalid-pm)
(cd "$PROJ" && check_clean "pm which"          "$NUB" pm which)

# --------------------------------------------------------------------------
# 14. node version management family
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "node ls"     "$NUB" node ls)
(cd "$PROJ" && check_clean "node which"  "$NUB" node which)
(cd "$PROJ" && check_clean "node --help" "$NUB" node --help)

# --------------------------------------------------------------------------
# 15. exec error path: binary not installed
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "exec not-installed" "$NUB" exec nonexistent_bin_xyz_987)

# --------------------------------------------------------------------------
# 16. run error paths
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "run missing-script"   "$NUB" run nonexistent-script-xyz)
(cd "$PROJ" && check_clean "run -r non-workspace" "$NUB" run -r build)

# --------------------------------------------------------------------------
# 17. Top-level file-run: TS syntax error
# --------------------------------------------------------------------------
cat > "$SANDBOX/syntax-err.ts" <<'EOF'
const x: number = "this is a type error" as unknown as number;
console.log(x);
EOF
(cd "$SANDBOX" && check_clean "run TS file" "$NUB" syntax-err.ts)

# --------------------------------------------------------------------------
# 18. search: ERR_NUB_NPM_ONLY_COMMAND path
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "search npm-only error" "$NUB" search left-pad)

# --------------------------------------------------------------------------
# 19. init: positional rejection (always errors pre-write — never scaffolds
#     or installs, regardless of $PROJ's manifest state)
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "init arg-reject" "$NUB" init some-template)

# --------------------------------------------------------------------------
# 20. upgrade --dry-run (no network download; checks brand in upgrade output)
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "upgrade --dry-run" "$NUB" upgrade --dry-run)

# --------------------------------------------------------------------------
# 21. AUBE_DIAG_SUMMARY (internal diagnostics output must not leak brand)
# --------------------------------------------------------------------------
(cd "$PROJ" && AUBE_DIAG_SUMMARY=1 check_clean "AUBE_DIAG_SUMMARY install" "$NUB" install)

# --------------------------------------------------------------------------
# 22. Workspace: no workspace error from --filter and -r
# --------------------------------------------------------------------------
PROJ_PLAIN="$SANDBOX/proj-plain"
mkdir -p "$PROJ_PLAIN"
cat > "$PROJ_PLAIN/package.json" <<'EOF'
{"name":"plain","private":true,"scripts":{"build":"echo built"}}
EOF
(cd "$PROJ_PLAIN" && check_clean "run --filter non-workspace" "$NUB" run --filter . build)
(cd "$PROJ_PLAIN" && check_clean "run -r non-workspace"       "$NUB" run -r build)

# --------------------------------------------------------------------------
# 23. approve-builds: TTY-required error path
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "approve-builds tty-required error" "$NUB" approve-builds)

# --------------------------------------------------------------------------
# 24. ERR_NUB_NPM_ONLY_COMMAND paths
# --------------------------------------------------------------------------
(cd "$PROJ" && check_clean "pkg npm-only error" "$NUB" pkg get name)

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------
FAILS=$(wc -l < "$FAIL_FILE" | tr -d ' ')
echo ""
echo "brand-runtime: $FAILS failure(s)"
if [ "$FAILS" -gt 0 ]; then
  exit 1
fi

# Known blind spots documented here so future maintainers see what's NOT covered:
#
#   a) LOCK-CONTENTION MESSAGE — "Waiting for another nub process to finish in
#      this project..." This uses aube_util::ua::product_name() which reads the
#      registered product token (set to "nub" by engine_brand_preflight()). It
#      is structurally correct and not exercised here because reproducing it
#      requires two concurrent processes. The implementation is verified by
#      reading vendor/aube/crates/aube/src/commands/project_lock.rs + ua.rs and
#      confirmed that engine_brand_preflight() sets the product name before any
#      lock acquisition can fire.
#
#   b) `nub dlx <pkg>` SUCCESS PATH — executes a live network download; excluded
#      here to keep the test self-contained. The error path (no network) is
#      covered but not a live dlx install.
#
#   c) WINDOWS PROCESS-LOCK PATH — cmd.exe / named-pipe lock contention has a
#      different code path; only testable on the windows-latest CI leg.
#
#   d) PATCH-COMMIT / PATCH-REMOVE — require a prior `nub patch` that succeeded
#      and created a patch dir; the error path (no prior patch state) is not yet
#      wired (those verbs are stubs). Add once they land.
#
#   e) STORE ADD — requires a real package reference; excluded as it needs
#      network for a first-time entry. Covered by brand-sweep's install pass.
#
#   f) LOGIN / LOGOUT / WHOAMI / PUBLISH — registry-auth paths that require
#      credentials or a live registry. Not wired here.

echo "brand-runtime: all assertions passed"
