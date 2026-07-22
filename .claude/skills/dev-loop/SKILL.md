---
name: dev-loop
description: >-
  Build and test nub during development. Invoke (via the Skill tool) whenever
  you need to compile the dev `nub` binary, set up a worktree for fast
  incremental iteration, run a specific test file or a single test, or get
  oriented in the codebase (the crate map). Encodes the measured fast-build
  loop: the `fast` profile built through `scripts/rust-build.sh`, which shares
  ONE CARGO_TARGET_DIR across worktrees (`~/.cache/nub/shared-target`) so deps
  are reused and only the workspace crates recompile — but auto-isolates a
  worktree to a private target dir the moment it diverges a depended-on crate
  (vendor/aube, nub-core, …), which is when a shared dir would clobber a sibling
  and fail with a phantom compile error on correct source (the `rust-build`
  skill). A shared cross-worktree compiler-WRAPPER cache (sccache) was
  separately measured to give 0% Rust speedup and is NOT used. Covers the real
  incantations (`cargo
  build -p nub-cli --profile fast`, `make install-dev`, `make addon-fast`), the
  test invocations, and the exact CI cheap gates.
metadata:
  internal: true
---

# Building & testing nub

nub is a Rust workspace — three crates (`nub-cli`, `nub-core`, `nub-native`) plus the vendored aube PM engine (`vendor/aube`, plain in-tree files since Pattern B, its own Cargo workspace, linked in-process as a library). This skill is the fast, measured way to build and test it in a worktree, plus a crate map so you know where things live.

**The one rule that makes iteration fast:** build with the `--profile fast` profile (NEVER `release`), through `scripts/rust-build.sh` (`scripts/rust-build.sh build -p nub-cli --profile fast`). The wrapper points CARGO_TARGET_DIR at the ONE shared dir `~/.cache/nub/shared-target` for the fast path — cold ≈ 3 min, every rebuild after ≈ 5s, because a second worktree reuses all the crates.io dependency artifacts (the bulk of a build) and recompiles only the ~10 workspace crates. Don't clean the shared dir between iterations — that throws away cargo's incremental cache. **Why the wrapper and not a raw `export CARGO_TARGET_DIR`:** the shared dir is safe only while every worktree agrees on the depended-on crates; two worktrees that diverge the same one (classically `vendor/aube`) clobber each other's rlib and fail with a phantom `E0063`-class error on correct source. `rust-build.sh` shares by default and auto-isolates to a private target dir exactly when this worktree diverges such a crate — you get the fast path in the common case and correctness in the rare one, with no manual decision. See the `rust-build` skill. (Tradeoff of sharing: cargo build-locks the target dir, so concurrent builds in two sharing worktrees serialize — a latency cost, never a correctness one.)

---

## Step 1 — Set up a worktree to iterate in

Substantive work lands via a PR from an isolated worktree. Create one with the `worktree` skill (`nub scripts/new-worktree.ts <slug>` — it bakes in the proven `git worktree add … origin/main` recipe). Then build through the wrapper, from the worktree root:

```bash
cd ~/.cache/nub/worktrees/<slug>
scripts/rust-build.sh build -p nub-cli --profile fast          # shared cache; auto-isolates on divergence
```

The wrapper shares `~/.cache/nub/shared-target` so a fresh worktree reuses the crates.io dependency artifacts another worktree already compiled — it recompiles only the ~10 workspace crates instead of all ~400, and one shared dir replaces ~30 multi-GB private ones. It flips to a private target dir automatically when this worktree diverges a depended-on crate (see the `rust-build` skill for why). Two sharing worktrees serialize on cargo's build lock; if you need them to build at once, an isolated worktree (one that has diverged a lib crate) already runs in parallel, or set a private `CARGO_TARGET_DIR` by hand.

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

**Build politeness on this shared dev host (HIGH PRIORITY — the maintainer works on this machine).** A full cargo build can saturate all cores and make the maintainer's machine non-responsive. Two throttles keep it polite:
- **Job cap (already set, machine-wide):** `~/.cargo/config.toml` pins `[build] jobs = 6` (of 8 perf cores), so no build grabs every core. CI is unaffected (it runs on GitHub runners, not this config). Leave it in place.
- **Background QoS / nice — wrap every agent build:** launch builds as `taskpolicy -b cargo build -p nub-cli --profile fast` (macOS background QoS → schedules on E-cores + yields to interactive) or `nice -n 10 cargo build …`. This lets a build run without making the machine sluggish. If a build is already hammering the host, `renice 20 -p <pid>` + `taskpolicy -b -p <pid>` the running `cargo`/`rustc` tree for immediate relief.

**Why `fast`, never `release`, for iteration** (measured 2026-06-20, macOS arm64):

| build | wall time |
|---|---|
| `--profile fast`, cold, empty shared target dir | **~3 min** |
| `--profile fast`, fresh worktree against a WARM shared target dir | only the ~10 workspace crates recompile (deps reused) |
| `--profile fast`, rebuild after a 1-file change, same target dir | **~5s** |
| `--profile release`, cold | **~15 min** (and re-LTOs the whole binary on every change) |

The `fast` profile (defined in `Cargo.toml`) inherits `dev` (debug-assertions + overflow checks stay on), drops LTO, uses `codegen-units=256`, line-tables-only debuginfo, and `incremental=true`. It is the iteration profile; `release` is a ship profile and must not be used to iterate.

**A shared cargo target DIR (the default here) is NOT sccache — don't conflate them.** sccache (a compiler-WRAPPER cache) was measured against this workspace and gives a **0% Rust cache-hit rate** across separate target dirs (rustc embeds per-target-dir artifact paths in sccache's cache keys; `--remap-path-prefix` + `CARGO_INCREMENTAL=0` does not fix it) — so sccache is NOT used. A shared cargo *target dir* sidesteps that entirely: with one target dir there's a single artifact path, so cargo's own incremental reuses the dependency rlibs directly across worktrees. That is why the shared dir — not a per-worktree one — is the fast path. (Seeding a private worktree target dir from a warm sibling via APFS clone is still useless — cargo invalidates the cloned fingerprints and rebuilds; the shared dir avoids the copy in the first place. There is no copy-on-write shortcut.) The catch is that a single artifact path is safe only while every sharing worktree agrees on the depended-on crates; `scripts/rust-build.sh` is what keeps that invariant (auto-isolating a diverged worktree) so the shared dir stays both fast and correct — see the `rust-build` skill.

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
# nub-native is its OWN workspace (excluded from the root one), so `-p nub-native`
# from the repo root fails — run it from inside the crate. (The cdylib sets
# `test = false`, so this just compiles the addon; its unit-testable logic lives
# in the napi-free nub-cache-key crate, covered by `cargo test -p nub-cache-key`.)
(cd crates/nub-native && cargo test)

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

**`vendor/aube`** — the vendored aube package-manager engine (plain in-tree files since Pattern B, vendored from `nubjs/aube`). Its own Cargo workspace; nub takes path deps into `vendor/aube/crates/*` and calls `aube::commands::<verb>::run(...)` in-process. NEVER a subprocess. From a build standpoint it's just part of the workspace — `cargo build` compiles it as a dependency. Changes to it are normal nub edits/PRs touching `vendor/aube/*` (no pin, no submodule). For pulling FROM / pushing TO upstream `jdx/aube`, see the `aube-bump` skill.

---

## Quick reference

```bash
# fresh worktree (see the `worktree` skill: nub scripts/new-worktree.ts <slug>)
cd ~/.cache/nub/worktrees/<slug>
# build/test through the wrapper — shared cache, auto-isolates on depended-on-crate divergence (`rust-build` skill)
scripts/rust-build.sh build -p nub-cli --profile fast
scripts/rust-build.sh test  -p nub-cli --test <file_stem>

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
