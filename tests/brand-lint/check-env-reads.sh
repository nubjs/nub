#!/usr/bin/env bash
# Static brand-boundary gate: the nub crates must never READ an AUBE_*-branded
# environment variable, nor STAMP one on a child process.
#
# WHY a grep-gate and not a clippy lint: forcing every env access through a
# wrapper (uv's `disallowed-methods` pattern) would mean migrating ~89 direct
# `std::env::var` sites — a large refactor, tracked as a follow-up. This gate is
# the bounded, zero-violation guard that actually holds the invariant TODAY: it
# catches the specific regression (a new AUBE_* read sneaking into nub) without
# the refactor.
#
# The invariant (AGENTS.md "The nub runtime respects ZERO AUBE_* env vars"): the
# embedded aube engine's brand is never nub's env surface. Standalone aube reads
# AUBE_*; under the nub embedder profile those are dead, and a nub-facing knob is
# a neutral npmrc field / NUB_* — never AUBE_*.
#
# BOTH DIRECTIONS ARE VIOLATIONS. This gate once exempted the write side ("SETTING
# an AUBE_* var for a child is fine"), and that carve-out is exactly how
# AUBE_NODE_GYP_EXE reached the build jail's withheld-env policy — the engine's
# brand ended up load-bearing inside nub's own sandbox decisions. A var nub stamps
# is nub's, so it wears nub's brand: cross-process plumbing goes through
# `aube_util::env::internal_env_name`, which composes the embedder profile's
# `internal_env_prefix` (`__NUB_*` under nub) at the one point that keeps the
# stamp, the generated shim, and the jail's scrub list on a single spelling.
#
# Scope: crates/*/src (production source). vendor/aube is excluded — the engine
# legitimately owns AUBE_* for its standalone profile. Test files under tests/ are
# excluded: a test may set an AUBE_* canary to PROVE it has no effect (see
# tests/brand-sweep/run.sh). A `#[cfg(test)]` block inside crates/*/src is IN
# scope, which is intended — such a test asserts nub's own behavior and should
# name nub's own vars.
#
# Usage: tests/brand-lint/check-env-reads.sh
# CI: a step in the `clippy` job (run_rust-gated, no build needed).
set -euo pipefail

cd "$(dirname "$0")/../.."

# Both call shapes, matched on the literal argument so a doc comment mentioning an
# AUBE_* var (there are many, explaining why nub does NOT read it) never trips the
# gate. Reads: `std::env::var("AUBE_X")`, `env::var_os("AUBE_X")`, optional `&`.
# Writes: `.env("AUBE_X", …)` / `.env_remove("AUBE_X")` on a Command builder.
patterns=(
  'env::var(_os)?\s*\(\s*&?"AUBE_'
  '\.env(_remove)?\s*\(\s*&?"AUBE_'
)

hits=$(grep -rnE "$(IFS='|'; echo "${patterns[*]}")" crates/*/src --include='*.rs' || true)

if [ -n "$hits" ]; then
  echo "FAIL: nub crate source reads or stamps an AUBE_*-branded env var"
  echo "      (brand-boundary violation)."
  echo "      Reads: AUBE_* env vars are dead under the nub embedder profile; use a"
  echo "      neutral npmrc field or a NUB_* knob instead."
  echo "      Writes: compose the name via aube_util::env::internal_env_name(\"…\"),"
  echo "      which yields __NUB_* under nub. See AGENTS.md, brand boundary."
  echo
  echo "$hits"
  exit 1
fi

echo "ok: no AUBE_* env-var reads or stamps in nub crate source"
