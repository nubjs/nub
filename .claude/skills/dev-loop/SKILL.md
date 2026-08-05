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

nub is a Rust workspace — `nub-cli`, `nub-core`, `nub-native` plus the vendored aube PM engine (`vendor/aube`, plain in-tree files, its own Cargo workspace, linked in-process as a library).

**The rule that makes iteration fast:** build with `--profile fast` (never `release`), through `scripts/rust-build.sh`, which points `CARGO_TARGET_DIR` at the shared dir `~/.cache/nub/shared-target` — cold ≈ 3 min, every rebuild after ≈ 5s. Don't clean the shared dir between iterations.

**Use the wrapper, not a raw `export CARGO_TARGET_DIR`.** The shared dir is safe only while every worktree agrees on the depended-on crates; two that diverge the same one (classically `vendor/aube`) clobber each other's rlib and fail with a phantom `E0063`-class error on correct source. The wrapper auto-isolates exactly then. **The `rust-build` skill owns the target-dir decision.**

## Step 1 — Set up a worktree

Create one with the `worktree` skill (`nub scripts/new-worktree.ts <slug>`), then build through the wrapper:

```bash
cd ~/.cache/nub/worktrees/<slug>
scripts/rust-build.sh build -p nub-cli --profile fast          # shared cache; auto-isolates on divergence
```

Two sharing worktrees serialize on cargo's build lock — a latency cost, never a correctness one.

## Step 2 — Build the dev binary (the `fast` profile)

```bash
# The dev CLI binary -> target/fast/nub. This is the iteration build.
cargo build -p nub-cli --profile fast

# Full dev binary + N-API addon, symlinked on PATH as nub-dev / nubx-dev:
make install-dev        # addon-fast, then `scripts/rust-build.sh build --profile fast`, then symlinks
                        # nub-dev/nubx-dev -> $(scripts/rust-build.sh --print-target)/fast/nub — the
                        # wrapper's hashed bucket under ~/.cache/nub/, NOT the repo's target/. The bucket
                        # id tracks depended-on crate content, so a change under vendor/aube or nub-core
                        # moves it: re-run install-dev or nub-dev keeps resolving to the previous bucket.

# Just the native addon (oxc transpiler), fast profile:
make addon-fast         # -> runtime/addons/nub-native.node
# Release-profile addon (only when you specifically need release behavior):
make addon
```

There is **no `nub build` command**.

**Build politeness — the maintainer works on this machine.**

- **Job cap (already set, machine-wide):** `~/.cargo/config.toml` pins `[build] jobs = 6` of 8 perf cores. CI is unaffected. Leave it in place.
- **Background QoS — wrap every agent build:** `taskpolicy -b cargo build -p nub-cli --profile fast` (macOS background QoS → E-cores, yields to interactive) or `nice -n 10 cargo build …`. For a build already hammering the host: `renice 20 -p <pid>` + `taskpolicy -b -p <pid>` on the running `cargo`/`rustc` tree.

**Why `fast`, never `release`, for iteration** (measured, macOS arm64):

| build | wall time |
|---|---|
| `--profile fast`, cold, empty shared target dir | **~3 min** |
| `--profile fast`, fresh worktree against a WARM shared target dir | only the ~10 workspace crates recompile |
| `--profile fast`, rebuild after a 1-file change, same target dir | **~5s** |
| `--profile release`, cold | **~15 min** (and re-LTOs the whole binary on every change) |

`fast` (defined in `Cargo.toml`) inherits `dev` — debug-assertions + overflow checks stay on — drops LTO, uses `codegen-units=256`, line-tables-only debuginfo, `incremental=true`. `release` is a ship profile.

**sccache is NOT used** — measured at a 0% Rust cache-hit rate across separate target dirs (rustc embeds per-target-dir artifact paths in its cache keys; `--remap-path-prefix` + `CARGO_INCREMENTAL=0` doesn't fix it). One shared target dir sidesteps it entirely.

## Step 3 — Run tests

```bash
# A specific integration-test file (file stem under crates/nub-cli/tests/):
cargo test -p nub-cli --test pm_verbs
cargo test -p nub-cli --test install_engine

# A single test by name substring (across the crate):
cargo test -p nub-cli <substring>
# Pin exactly one test:
cargo test -p nub-cli -- --exact <full::module::path::to::test>

# A core crate's tests:
cargo test -p nub-core

# nub-native is its OWN workspace (excluded from the root one), so `-p nub-native`
# from the repo root fails — run it from inside the crate. The cdylib sets
# `test = false`, so this just compiles the addon; its unit-testable logic lives
# in the napi-free nub-cache-key crate (`cargo test -p nub-cache-key`).
(cd crates/nub-native && cargo test)

# The VENDORED AUBE crates are their own workspace, and here the CWD is load-bearing
# for CORRECTNESS. `vendor/aube/.cargo/config.toml` pins RUST_TEST_THREADS = "1" because
# several aube-util tests mutate the process env and setenv/getenv are not thread-safe.
# Cargo discovers config from the CWD, NOT from --manifest-path, so running from the repo
# root silently bypasses the pin, runs those tests in parallel, and produces failures CI
# never sees (`set_allow_builds_*`, `pnpmfile::tests::detect_*`) that look like real bugs.
(cd vendor/aube && cargo test -p aube-resolver)   # RIGHT — inherits the serial pin
# cargo test --manifest-path vendor/aube/Cargo.toml -p aube-resolver
#   ^ WRONG from the repo root: resolves the crate but drops the pin.

# Everything (slow):
cargo test          # or `make test`
```

The `nub-cli` integration suite lives in `crates/nub-cli/tests/*.rs` — `pm_verbs`, `install_engine`, `info_engine`, `cli_grammar_parity`, `pm_identity`, `pm_two_mode`, `resolution_compat`, `node_compat`, `version_tiers`, `workspace_run`, the `pm_shim*` / `*_config` files. Use the file stem as `--test <stem>`.

### NEVER judge pass/fail from a piped `cargo test`

```bash
cargo test 2>&1 | tail -80        # ← the pipe's status is tail's, not cargo's
echo $?                           # 0, even with failures above
```

A pipeline reports the LAST command's status, so `| tail`, `| grep`, `| head` all
report success while tests were failing. The failure lines scroll past the window
you kept, and a green-looking `$?` is what you act on. This has produced a
confident "all green" on a red suite in this repo more than once.

Judge from the process itself:

```bash
cargo test; echo "EXIT=$?"                      # status is cargo's
cargo test > /tmp/t.log 2>&1; echo "EXIT=$?"    # then grep the file at leisure
set -o pipefail                                 # if you must pipe
```

Same trap for `clippy` and any gate whose verdict is its exit code. Grepping the
output for `FAILED` is not equivalent either: a suite that fails to COMPILE prints
`error[E…]` and no `FAILED` line at all, so a grep-based check reads it as clean.

## Step 4 — Before pushing: the exact CI cheap gates

Match `.github/workflows/ci.yml` exactly — a scoped `-p` without `--all-targets` misses test-code lints:

```bash
cargo clippy --all-targets --all-features --profile fast -- -D warnings
cargo fmt --check
cargo test -p <crate>        # scoped to what you changed; DEFAULT profile, as CI runs it
```

Keep `--profile fast` on clippy — it is what CI's check and clippy jobs run and it keeps the gates in the same artifact universe as the dev loop; without it, gating drives a second full dependency build under `dev`. `cargo test` stays on the default profile, matching CI's test jobs.

**The gates above do NOT cover `vendor/aube` — check that workspace separately.** A dependent's `--all-targets` builds only its own test targets, so a path dependency's `#[cfg(test)]` targets never compile as part of the nub workspace; every gate here can be green while the aube crates do not build. For any change touching `vendor/aube` — **which includes every merge from `main`, since main routinely carries aube changes** — also run:

```bash
cd vendor/aube && CARGO_TARGET_DIR=<its own dir> cargo check --workspace --all-targets
```

This has already cost real time twice. A main-into-sandbox merge passed every nub-side gate while `vendor/aube` was broken (main had added four `ResolveCtx` initializers omitting a tier the branch adds), found only by running the aube check by hand; the same gap let `crates/nub-sandbox/tests/windows_deelevated_jail.rs` land not compiling at all, after a merge gave `compile_build_jail` a `package_name` parameter and updated one of its two call sites.

**After merging anything that touches `crates/nub-native`, rebuild the addon** — `make addon-fast`. A worktree that inherited a prebuilt `runtime/addons/nub-native.node` and then merged a change to the addon's signature produces failures that read exactly like a regression in your own work.

**The two heavy gates now belong on a remote builder.** `cargo clippy --all-targets --all-features` and a full `cargo test` are the most expensive steps here, and with many worktrees building concurrently they are what saturates the host. Run them off-box — `nub scripts/remote-build.ts --job clippy` / `--job test` (the `remote-build` skill) — for a few cents each, with the byte-identical CI invocation. The `--profile fast` inner loop above stays local; remote does not win there. For a macOS BINARY, `nub scripts/mac-build.ts` builds natively on a real macOS runner and pulls the signed artifact back.
**The two heavy gates belong on a remote builder.** `cargo clippy --all-targets --all-features` and a full `cargo test` are what saturate the host when many worktrees are building. Run them off-box — `nub scripts/remote-build.ts --job clippy --detach`, then `--attach <vm-name>` to collect (the `remote-build` skill) — for a few cents each, with the byte-identical CI invocation. **Use `--detach`/`--attach`, not the plain foreground form**: a foreground run is SIGKILLed at the agent harness's timeout, which no handler can catch, so cleanup is skipped and the builder leaks until its server-side TTL. The `--profile fast` inner loop stays local. For a macOS binary, `nub scripts/mac-build.ts` builds natively on a real macOS runner and pulls the signed artifact back.

Then run the full [pre-push local verification loop in AGENTS.md](../../../AGENTS.md). For the e2e probe loop, use the `ad-hoc-test` skill. Get it green locally and push ONCE.

---

## Crate map

**`crates/nub-cli`** — the CLI (clap dispatch + PM verb routing).
- `src/cli.rs` — the clap command grammar + dispatch (the pnpm-compatible PM surface, `run`/`watch`/`nubx`/`upgrade`/`node`, the top-level file runner).
- `src/main.rs` — entry point.
- `src/pm_engine/` — routes PM verbs into the vendored aube engine in-process. `mod.rs` (`ENGINE_VERBS`), `present.rs` (rebrands engine output: `ERR_AUBE_*`→`ERR_NUB_*`, `aube`→`nub`), `config_scope.rs` (mirror-active-PM / brand-boundary config policy), `identity.rs` (PM-identity inference), `install_family.rs`, `info_family.rs`, `publish_family.rs`, `store_config_family.rs`, `use_*.rs`, and `bun_config.rs` / `yarn_*` / `unsupported_config.rs` for incumbent-PM compat.
- `src/agent/` — agent surface.
- `tests/*.rs` — integration tests.

**`crates/nub-core`** — runtime/orchestration.
- `src/node/` — `discovery.rs` (find the user's Node on PATH), `version.rs`, `flags.rs` (V8 / Node flag injection), `feature_matrix.rs` (tier + Node-version gating — the source of truth for version-gated feature claims), `spawn.rs`, `mod.rs`.
- `src/pm/`, `src/workspace/`, `src/version_management/`.
- `src/pnp.rs` — Yarn PnP support.

**`crates/nub-native`** — the N-API addon (a cdylib loaded into the user's Node process). oxc-based transpiler + resolver: `transform.rs`, `resolve.rs`, `tsconfig.rs`, `cache.rs`, `detect.rs`.

**`vendor/aube`** — the vendored PM engine. Its own Cargo workspace; nub takes path deps into `vendor/aube/crates/*` and calls `aube::commands::<verb>::run(...)` in-process, never as a subprocess. Changes are normal nub edits/PRs (no pin, no submodule). For upstream sync, see the `aube-bump` skill.

---

## Quick reference

```bash
# fresh worktree (see the `worktree` skill: nub scripts/new-worktree.ts <slug>)
cd ~/.cache/nub/worktrees/<slug>
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
cargo clippy --all-targets --all-features --profile fast -- -D warnings
cargo fmt --check
```
