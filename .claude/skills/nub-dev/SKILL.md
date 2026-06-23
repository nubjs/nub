---
name: nub-dev
description: >-
  Build and test nub during development. Invoke (via the Skill tool) whenever
  you need to compile the dev `nub` binary, set up a worktree for fast
  incremental iteration, run a specific test file or a single test, or get
  oriented in the codebase (the crate map). Encodes the measured fast-build
  loop: the `fast` profile + one stable per-worktree target dir gives a ~3-min
  cold build then ~5s incremental rebuilds; a shared cross-worktree compile
  cache (sccache) was measured to give 0% Rust speedup and is NOT used. Covers
  the real incantations (`cargo build -p nub-cli --profile fast`, `make
  install-dev`, `make addon-fast`), the test invocations, and the exact CI
  cheap gates.
---

# Building & testing nub

nub is a Rust workspace — three crates (`nub-cli`, `nub-core`, `nub-native`) plus the vendored aube PM engine (`vendor/aube`, plain in-tree files since Pattern B, its own Cargo workspace, linked in-process as a library). This skill is the fast, measured way to build and test it in a worktree, plus a crate map so you know where things live.

**The one rule that makes iteration fast:** build with the `--profile fast` profile (NEVER `release`), and keep ONE stable target dir per worktree for the whole session. Cold ≈ 3 min, every rebuild after ≈ 5s. Don't clean/move/re-seed the target dir between iterations — that throws away cargo's incremental cache and forces a full rebuild.

---

## Step 1 — Set up a worktree to iterate in

Substantive work lands via a PR from an isolated worktree (see AGENTS.md "Default to a PR flow"). Set one up with its own target dir:

```bash
git worktree add /tmp/nub-wt-<slug> -b <slug> origin/main      # vendor/aube comes along (plain in-tree files)
cd /tmp/nub-wt-<slug>
export CARGO_TARGET_DIR=/tmp/nub-wt-<slug>-target               # per-worktree isolation; keep it stable
```

The per-worktree target dir keeps a sibling's build from contaminating yours. Keep the SAME dir for the whole session — cargo's incremental fingerprints are keyed to its absolute path, so changing/moving/re-seeding it forces a cold rebuild.

> `vendor/aube` is plain in-tree files (Pattern B, 2026-06-22), NOT a submodule — `git worktree add` checks it out directly, no submodule-init step.

## Step 2 — Build the dev binary (the `fast` profile)

```bash
# The dev CLI binary -> target/fast/nub. This is the iteration build.
cargo build -p nub-cli --profile fast

# Full dev binary + N-API addon, symlinked on PATH as nub-dev / nubx-dev:
make install-dev        # runs addon-fast, then `cargo build --profile fast`, then symlinks target/fast/nub

# Just the native addon (oxc transpiler), fast profile:
make addon-fast         # -> runtime/addons/nub-native.node
# Release-profile addon (only when you specifically need release behavior):
make addon
```

There is **no `nub build` command** — the dev build is `cargo build -p nub-cli --profile fast` (or `make install-dev` for the full binary+addon on PATH).

**Why `fast`, never `release`, for iteration** (measured 2026-06-20, macOS arm64):

| build | wall time |
|---|---|
| `--profile fast`, cold, fresh worktree | **~3 min** |
| `--profile fast`, rebuild after a 1-file change, same target dir | **~5s** |
| `--profile release`, cold | **~15 min** (and re-LTOs the whole binary on every change) |

The `fast` profile (defined in `Cargo.toml`) inherits `dev` (debug-assertions + overflow checks stay on), drops LTO, uses `codegen-units=256`, line-tables-only debuginfo, and `incremental=true`. It is the iteration profile; `release` is a ship profile and must not be used to iterate.

**Do NOT reach for a shared compile cache (sccache).** It was measured against this workspace: across separate worktrees the **Rust cache-hit rate is 0%** (rustc embeds per-target-dir artifact paths in sccache's cache keys; `--remap-path-prefix` + `CARGO_INCREMENTAL=0` does not fix it). It gives no speedup over a cold build and only adds a wrapper + a multi-hundred-MB cache to maintain. The `fast` profile + a stable per-worktree target dir is the entire answer. (Seeding a fresh worktree's target dir from a warm sibling via APFS clone is instant but useless — cargo invalidates the cloned fingerprints and rebuilds everything.)

## Step 3 — Run tests

```bash
# A specific integration-test file (file stem under crates/nub-cli/tests/):
cargo test -p nub-cli --test pm_verbs
cargo test -p nub-cli --test install_engine

# A single test by name substring (across the crate):
cargo test -p nub-cli <substring>
# Pin exactly one test:
cargo test -p nub-cli -- --exact <full::module::path::to::test>

# A core/native crate's tests:
cargo test -p nub-core
cargo test -p nub-native

# Everything (slow):
cargo test          # or `make test`
```

The `nub-cli` integration suite lives in `crates/nub-cli/tests/*.rs` — e.g. `pm_verbs`, `install_engine`, `info_engine`, `cli_grammar_parity`, `pm_identity`, `pm_two_mode`, `resolution_compat`, `node_compat`, `version_tiers`, `workspace_run`, the `pm_shim*` / `*_config` files. Use the file stem as `--test <stem>`.

## Step 4 — Before pushing: the exact CI cheap gates

Match `.github/workflows/ci.yml` exactly — a scoped `-p` without `--all-targets` misses test-code lints:

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test -p <crate>        # scoped to what you changed
```

Then run the full [pre-push local verification loop in AGENTS.md](../../../AGENTS.md) (incremental build → exact CI gates → e2e tmp-fixture run → Docker for global-cache/config behavior → promote durable checks into the suite). For the e2e probe loop specifically, use the `ad-hoc-test` skill. Get it green locally and push ONCE — fix-after-fix pushes saturate the shared CI runner pool.

---

## Crate map — where things live

**`crates/nub-cli`** — the CLI (clap dispatch + PM verb routing).
- `src/cli.rs` — the clap command grammar + dispatch (the pnpm-compatible PM surface, `run`/`watch`/`nubx`/`upgrade`/`node`, the top-level file runner).
- `src/main.rs` — entry point.
- `src/pm_engine/` — routes PM verbs into the vendored aube engine in-process. `mod.rs` (`ENGINE_VERBS`), `present.rs` (rebrands engine output: `ERR_AUBE_*`→`ERR_NUB_*`, `aube`→`nub`), `config_scope.rs` (mirror-active-PM / brand-boundary config policy), `identity.rs` (PM-identity inference), `install_family.rs`, `info_family.rs`, `publish_family.rs`, `store_config_family.rs`, `use_*.rs`, and `bun_config.rs` / `yarn_*` / `unsupported_config.rs` for incumbent-PM compat.
- `src/agent/` — agent surface.
- `tests/*.rs` — integration tests.

**`crates/nub-core`** — runtime/orchestration.
- `src/node/` — Node integration: `discovery.rs` (find the user's Node on PATH), `version.rs` (version management), `flags.rs` (V8 / Node flag injection), `feature_matrix.rs` (tier + Node-version gating — the source of truth for version-gated feature claims), `spawn.rs` (process spawn), `mod.rs`.
- `src/pm/`, `src/workspace/`, `src/version_management/`.
- `src/pnp.rs` — Yarn PnP support.

**`crates/nub-native`** — the N-API addon (a cdylib loaded into the user's Node process). The oxc-based transpiler + resolver: `transform.rs` (TS/JSX transform), `resolve.rs` (module resolution), `tsconfig.rs`, `cache.rs` (transpile cache), `detect.rs`.

**`vendor/aube`** — the vendored aube package-manager engine (plain in-tree files since Pattern B, vendored from `nubjs/aube`). Its own Cargo workspace; nub takes path deps into `vendor/aube/crates/*` and calls `aube::commands::<verb>::run(...)` in-process. NEVER a subprocess. Changes to it are normal nub edits/PRs touching `vendor/aube/*` — no pin, no `nub-fork` workflow; upstream to `jdx/aube` via `subtree split` / `format-patch --relative`. See AGENTS.md.

---

## Quick reference

```bash
# fresh worktree (vendor/aube comes along — plain in-tree files)
git worktree add /tmp/nub-wt-<slug> -b <slug> origin/main
cd /tmp/nub-wt-<slug> && export CARGO_TARGET_DIR=/tmp/nub-wt-<slug>-target

# build (fast profile)
cargo build -p nub-cli --profile fast          # -> target/fast/nub  (~3 min cold, ~5s incremental)
make install-dev                                # full binary + addon on PATH as nub-dev/nubx-dev
make addon-fast                                 # native addon only

# test
cargo test -p nub-cli --test <file_stem>        # one file
cargo test -p nub-cli <substring>               # one test by name

# CI cheap gates
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```
