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

# A string literal containing the brand, on a line that also builds a path.
# Keying on the path-constructing call (rather than "any line with aube") is
# what keeps this low-false-positive: prose, comments, and output strings are
# the brand-literal lint's job, not this one.
# The literal must not span a quote boundary: `f("/x", aube.join("y"))` would
# otherwise match as one "string" and report a variable named `aube`.
BRAND_LITERAL='"[^",()]*\baube\b[^",()]*"'
PATH_CALL='join\(|push\(|prefix\(|with_file_name\(|set_file_name\(|create_dir|PathBuf::from|OsString::from'

# Intentional exceptions, matched as `<file>:<line>`. Keep this list SHORT and
# justify every entry — an entry is a claim that the string never becomes a
# filesystem path.
# Intentional exceptions, matched on file + CONTENT (never a line number — a
# line-pinned entry silently goes stale the moment anything above it moves, and
# then the gate is exempting whatever slid into that slot). Keep this list SHORT
# and justify every entry: an entry is a claim the string never becomes a path.
#   argv.rs      — argv[0] for aube's own CLI parse; an argument, not a path.
#   runtime.rs   — a provenance LABEL stuffed in a PathBuf for display
#                  ("aube runtime set -g"), never opened or created.
#   aube-scripts — a `#[tokio::test]` fixture path. The brace-counting stripper
#                  below loses sync on `format!` braces inside that test body,
#                  so the line leaks through; it is test-only.
is_allowed() {
  case "$1" in
    vendor/aube/crates/aube/src/argv.rs:*) return 0 ;;
    vendor/aube/crates/aube/src/commands/runtime.rs:*origin:*'"aube runtime set'*) return 0 ;;
    vendor/aube/crates/aube-scripts/src/lib.rs:*aube-test-grandchild*) return 0 ;;
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
scan_file() {
  awk -v f="$1" '
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
    { print f ":" FNR ":" $0 }
  ' "$1"
}

violations=()
while IFS= read -r hit; do
  # Strip the line NUMBER so allowlist patterns match `<file>:<text>`.
  entry="$(printf '%s' "$hit" | sed -E 's/^([^:]+):[0-9]+:/\1:/')"
  is_allowed "$entry" || violations+=("$hit")
done < <(
  find crates vendor/aube/crates -type d -name src 2>/dev/null \
    | xargs -I{} find {} -name '*.rs' -type f 2>/dev/null \
    | grep -vE '(/tests?\.rs$|/tests/|_tests\.rs$)' \
    | sort \
    | while IFS= read -r f; do scan_file "$f"; done \
    | grep -E "$BRAND_LITERAL" \
    | grep -E "$PATH_CALL" \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|///|//!|\*)'
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
