#!/usr/bin/env bash
# Static brand-boundary gate: no ON-DISK PATH may carry the embedded engine's
# brand. The sibling of check-env-reads.sh — that one guards what nub READS from
# the environment, this one guards what nub WRITES to the filesystem.
#
# The invariant (AGENTS.md "The brand boundary"): a path nub creates lands in a
# user's home, cache, store, or node_modules, so an `aube`-named leaf puts the
# embedded engine's brand in a surface the user sees. The seam is
# `aube_util::prog()` (and `embedder().data_namespace` / `.cache_namespace`),
# which renders exactly `"aube"` under the default profile — so routing a name
# through it is byte-for-byte identical for standalone aube and correct under
# every embedder.
#
# WHY this gate exists: the runtime regression test
# (`pm_identity.rs::nub_never_writes_an_aube_branded_path`) only covers paths a
# `nub run` actually produces. Most brand-crossed names live on paths no test
# exercises — dlx temp dirs, the security scanner, git-prepare, self-install —
# so a static gate is what holds the invariant across the whole tree. Both were
# needed: the bug that motivated this shipped for months with the runtime path
# untested AND the literal unlinted.
#
# Scope: production source only (`src/`) across the nub crates AND the vendored
# engine, since the engine is where the literals actually live. Tests are
# excluded: an aube-side test legitimately asserts on aube's own default-profile
# names (`node_modules/.aube`, `aube-lock.yaml`), which are correct there.
#
# Usage: tests/brand-lint/check-path-literals.sh
# CI: a step in the `clippy` job (run_rust-gated, no build needed).
set -euo pipefail

cd "$(dirname "$0")/../.."

# A brand-carrying string literal, in a statement that also builds a path.
#
# The literal must contain no quote, comma, paren, or WHITESPACE. Excluding
# quotes/commas/parens stops `f("/x", aube.join("y"))` matching as one "string"
# and reporting a variable named `aube`. Excluding whitespace is what separates
# a path component from prose — every false positive here was an English
# sentence reaching a `Vec::push` or a `slice.join(", ")`, and no path literal
# in this tree contains a space.
BRAND_LITERAL='"[^",()[:space:]]*aube[^",()[:space:]]*"'
PATH_CALL='join\(|push\(|prefix\(|with_file_name\(|set_file_name\(|create_dir|PathBuf::from|OsString::from|tempdir_in'

# Intentional exceptions, matched on file + CONTENT (never a line number — a
# line-pinned entry silently goes stale the moment anything above it moves, and
# then the gate exempts whatever slid into that slot). Keep this list SHORT and
# justify every entry: an entry is a claim the string never becomes a path nub
# writes.
#   argv.rs          — `args[0]` for aube's own CLI parse; an argument, not a
#                      path. Scoped to the `args[0]` assignment rather than the
#                      whole file, so an unrelated literal there is still caught.
#   patch.rs         — the engine's fallback edit dir. Unreachable under nub:
#                      `install_family.rs::run_patch` always injects `edit_dir`
#                      (`get_or_insert_with(nub_patch_edit_parent)`) before
#                      calling the engine, so `default_edit_parent` never runs.
#   self_install.rs  — the mise TOOL NAME being looked up ("where did mise
#                      install aube"), not a path nub composes. Self-update is
#                      off under the nub profile besides.
#   aube-scripts     — `#[tokio::test]` / test-helper fixture paths. The
#                      brace-counting stripper loses sync on `format!` braces
#                      inside those test bodies, so they leak through.
#   settings.rs      — test code (same stripper limitation); `aube` there is a
#                      local variable, not a literal.
#   self_install.rs  — the aube EXECUTABLE's own filename, used to detect an
#                      installed aube. Discovering that tool is the function's
#                      purpose; it is not a path nub composes for its own state.
#   recursive.rs     — `argv[0]` for a recursive engine invocation, matched only
#                      because the window catches an adjacent `Vec::push`.
is_allowed() {
  case "$1" in
    vendor/aube/crates/aube/src/argv.rs:*args\[0\]*) return 0 ;;
    vendor/aube/crates/aube/src/commands/patch.rs:*aube-patch-*) return 0 ;;
    vendor/aube/crates/aube-runtime/src/self_install.rs:*mise_tool_installs_dir*) return 0 ;;
    vendor/aube/crates/aube-scripts/src/lib.rs:*aube-test-grandchild*) return 0 ;;
    vendor/aube/crates/aube-scripts/src/lib.rs:*aube-unshare-test-*) return 0 ;;
    vendor/aube/crates/aube/src/commands/install/settings.rs:*symlink\(*) return 0 ;;
    vendor/aube/crates/aube-runtime/src/self_install.rs:*exe_name*) return 0 ;;
    vendor/aube/crates/aube/src/commands/recursive.rs:*vec!\[*) return 0 ;;
  esac
  return 1
}

# Rust keeps unit tests INLINE in production files, and an aube-side test
# legitimately asserts on aube's own default-profile names (`node_modules/.aube`,
# `aube-lock.yaml`) — so test modules must be dropped before matching.
#
# The skip is BRACE-COUNTED, not truncate-at-first-`#[cfg(test)]`. Files here
# carry several test modules interleaved with production code (aube-store's
# `cas.rs` has one at line 135 and real code through line 932), so truncating
# would silently stop scanning most of the tree — a gate that passes by not
# looking. Verified against a planted violation past the first test module.
# Emitted units are a FIXED SLIDING WINDOW of consecutive production lines, not
# physical lines and not paren-balanced statements. rustfmt splits a long call,
# so `join(format!(\n  ".aube-unshare-…"` puts the literal and the path call on
# different lines — a per-line matcher reports CLEAN while the leak sits right
# there. That exact shape was a live leak into consumers' node_modules that the
# per-line version of this gate passed over.
#
# Paren-balancing was the obvious way to join those lines and it is the WRONG
# one: awk cannot lex Rust, so a paren inside `split('(')`, a raw string, or a
# lifetime tick latches the balance positive and every later statement in the
# file is swallowed. Measured on this tree, that silently dropped ~7.4k lines
# across 14 of 415 files — including `fetch.rs`, a file this gate is meant to
# protect. Erasing literals first fixed 8 of the 14 and still lost 6.
#
# A fixed window CANNOT under-scan: every line appears in WINDOW consecutive
# windows regardless of syntax, so no construct can shrink coverage. The cost is
# that one violation can match in several overlapping windows (deduped below on
# file + literal) and that two unrelated adjacent statements can join into a
# false positive — which errs toward reporting, the safe direction for a gate.
WINDOW=3
scan_file() {
  awk -v f="$1" -v w="$WINDOW" '
    skipping {
      depth += gsub(/\{/, "{") - gsub(/\}/, "}")
      if (depth <= 0) skipping = 0
      next
    }
    /^[[:space:]]*#\[cfg\(test\)\]/ { pending = 1; next }
    pending && /mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{/ {
      pending = 0
      depth = gsub(/\{/, "{") - gsub(/\}/, "}")
      if (depth > 0) skipping = 1
      next
    }
    pending { pending = 0; next }
    {
      line = $0
      sub(/^[[:space:]]+/, "", line)
      if (line ~ /^(\/\/|\*)/) next
      n++; txt[n] = line; num[n] = FNR
    }
    END {
      for (i = 1; i <= n; i++) {
        joined = txt[i]
        for (j = i + 1; j < i + w && j <= n; j++) joined = joined " " txt[j]
        print f ":" num[i] ":" joined
      }
    }
  ' "$1"
}

scanned="$(mktemp)"
trap 'rm -f "$scanned"' EXIT
find crates vendor/aube/crates -type d -name src 2>/dev/null \
  | xargs -I{} find {} -name '*.rs' -type f 2>/dev/null \
  | grep -vE '(/tests?\.rs$|/tests/|_tests\.rs$)' \
  | sort \
  | while IFS= read -r f; do scan_file "$f"; done > "$scanned"

violations=()
while IFS= read -r hit; do
  # Strip the line NUMBER so allowlist patterns match `<file>:<text>`.
  entry="$(printf '%s' "$hit" | sed -E 's/^([^:]+):[0-9]+:/\1:/')"
  is_allowed "$entry" || violations+=("$hit")
done < <(
  grep -E "$BRAND_LITERAL" "$scanned" \
    | grep -E "$PATH_CALL" \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|///|//!|\*)' \
    | awk -F: '
        # Overlapping windows re-report one violation. Key on file + the brand
        # literal itself and keep the lowest line number.
        { rest = $0; sub(/^[^:]+:[0-9]+:/, "", rest)
          if (match(rest, /"[^",()[:space:]]*aube[^",()[:space:]]*"/)) {
            key = $1 "|" substr(rest, RSTART, RLENGTH)
            if (!seen[key]++) print
          } else print
        }'
)

if [ ${#violations[@]} -gt 0 ]; then
  printf 'brand-boundary violation: aube-branded path literal(s)\n\n' >&2
  printf '  %s\n' "${violations[@]}" >&2
  cat >&2 <<'EOF'

Compose the name from the active embedder instead of hardcoding the brand:

  - let p = root.join(".aube-side-effects-cache");
  + let p = root.join(format!(".{}-side-effects-cache", aube_util::prog()));

`prog()` is "aube" under the default profile, so standalone aube's on-disk
layout is unchanged. For a namespace directory use
`aube_util::embedder().data_namespace` / `.cache_namespace`.

If the string genuinely never becomes a path, add it to is_allowed() above
with a one-line justification.
EOF
  exit 1
fi

echo "brand-lint: no aube-branded path literals"
