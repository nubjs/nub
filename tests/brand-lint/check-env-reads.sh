#!/usr/bin/env bash
# Static brand-boundary gate: the nub crates must never READ an AUBE_*-branded
# environment variable.
#
# WHY a grep-gate and not a clippy lint: forcing every env access through a
# wrapper (uv's `disallowed-methods` pattern) would mean migrating ~89 direct
# `std::env::var` sites — a large refactor, tracked as a follow-up. This gate is
# the bounded, zero-violation guard that actually holds the invariant TODAY: it
# catches the specific regression (a new AUBE_* read sneaking into nub) without
# the refactor.
#
# The invariant (AGENTS.md "The nub runtime respects ZERO AUBE_* env vars"): the
# embedded aube engine's brand is never nub's config surface. Standalone aube
# reads AUBE_*; under the nub embedder profile those are dead, and a nub-facing
# knob is a neutral npmrc field / NUB_* — never AUBE_*. SETTING an AUBE_* var for
# a child the engine spawns (e.g. AUBE_NODE_GYP_EXE via Command::env) is fine and
# not matched; only READING one (env::var / env::var_os) is the violation.
#
# Scope: crates/*/src (production source). Tests are excluded — a test may
# legitimately set an AUBE_* canary to PROVE it has no effect (see
# tests/brand-sweep/run.sh).
#
# Usage: tests/brand-lint/check-env-reads.sh
# CI: a step in the `clippy` job (run_rust-gated, no build needed).
set -euo pipefail

cd "$(dirname "$0")/../.."

# env::var / env::var_os calls whose argument carries an "AUBE_ string literal.
# Matches `std::env::var("AUBE_X")`, `env::var_os("AUBE_X")`, and a leading `&`.
pattern='env::var(_os)?\s*\(\s*&?"AUBE_'

hits=$(grep -rnE "$pattern" crates/*/src --include='*.rs' || true)

if [ -n "$hits" ]; then
  echo "FAIL: nub crate source reads an AUBE_*-branded env var (brand-boundary violation)."
  echo "      Under the nub embedder profile AUBE_* env vars are dead; use a neutral"
  echo "      npmrc field or a NUB_* knob instead. See AGENTS.md, brand boundary."
  echo
  echo "$hits"
  exit 1
fi

echo "ok: no AUBE_* env-var reads in nub crate source"
