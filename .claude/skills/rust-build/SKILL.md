---
name: rust-build
description: >-
  Use when building or testing the nub Rust workspace inside a git worktree —
  `cargo build`/`test`/`clippy` for nub-cli/nub-core/aube in a worktree off
  origin/main. Explains how worktrees share ONE cargo target dir for fast
  incremental builds, the cross-worktree artifact-contamination hazard that
  sharing creates (the phantom `E0063: missing field` on correct source), and the
  wrapper (`scripts/rust-build.sh`) that shares by default and auto-isolates the
  moment a worktree diverges a depended-on crate. Auto-triggers on a spurious
  cargo compile error that names a field/symbol absent from your checkout, on
  "worktree build contamination", and on setting CARGO_TARGET_DIR for a worktree.
metadata:
  internal: true
---

# rust-build

Cross-worktree cache reuse wants a shared target dir; correctness wants isolation. `scripts/rust-build.sh` resolves it — share by default, isolate automatically only when a worktree diverges a crate other crates link.

## Use it

Drop-in for `cargo`, from any worktree (or the main tree):

```sh
scripts/rust-build.sh build -p nub-cli --profile fast
scripts/rust-build.sh test  -p nub-cli --test integration
scripts/rust-build.sh clippy --all-targets --all-features -- -D warnings
```

It prints which target dir it chose and why, then execs `cargo` with `CARGO_TARGET_DIR` set. Two default-on contention controls ride along:

- **QoS clamp (darwin only):** cargo runs under `taskpolicy -c utility`, so interactive work preempts fleet builds; an uncontended build still gets all cores. `NUB_BUILD_FG=1` opts out.
- **Job cap on big hosts (>8 cores):** `CARGO_BUILD_JOBS = ncpu-4` unless the caller already chose (pre-set `CARGO_BUILD_JOBS`, `NUB_BUILD_JOBS`, or an explicit `-j`/`--jobs` — cargo's flag outranks the env var).

These cover builds going THROUGH the wrapper (or make). Direct `cargo` invocations are clamped by a machine-global control: `make qos-global` installs `scripts/rustc-qos.sh` as the cargo `rustc-wrapper` in `~/.cargo/config.toml`, so every rustc on the host compiles at utility QoS (`install-dev` re-runs it, so it self-heals). Same `NUB_BUILD_FG=1` opt-out; toggling it does not invalidate fingerprints.

## Why one shared target dir

All worktrees default to `~/.cache/nub/shared-target`. A fresh worktree reuses the crates.io dependency rlibs a sibling already compiled and recompiles only the ~3 workspace crates — ~5s instead of a ~3-min cold build.

**Relocation does NOT defeat reuse.** Cargo revalidates a relocated target dir in place: cloning a warm dir to a new path rebuilt 0 crates; an empty dir rebuilt all 13. (The "path is baked into fingerprints" claim is true of **sccache**, which keys on the rustc command line including absolute `--out-dir` / `-L dependency=` paths — not of cargo.) So sharing a live path buys convergence; a CoW clone buys a warm start, which is why the wrapper seeds a fresh private dir from the matching bucket instead of cold-starting.

## The hazard sharing creates

Cargo names a crate's output by **package id (name + version), not source content.** Two worktrees whose source for the same depended-on crate differs — classically `vendor/aube` on divergent branches — write the same output slot and clobber each other. A dependent crate then links the stale rlib:

```
error[E0063]: missing field `lockfile_legacy_basenames` in initializer of `aube_util::Embedder`
```

— a field that exists nowhere in your checkout. Only bites crates that **other crates link**; a divergent leaf binary (`nub-cli`) just rebuilds cleanly.

## The rule the wrapper enforces

Sharers are grouped by the **content** of their depended-on crates, hashed into the bucket name (`shared-target-<hash>`):

- **Depended-on crate sources unmodified → share** that content's bucket. Everyone in it agrees by construction. The common case: feature work in `nub-cli`, integration tests, docs, non-Rust files.
- **Diverged a depended-on crate → isolate** to a private per-worktree `target/` (removed with the worktree), CoW-seeded from the matching bucket so you rebuild only what differs.

**Content, not merge-base:** a merge-base proves only that *this* worktree made no local changes against *its own* base, so two worktrees whose bases straddle a `nub-core`/`aube` commit both pass while disagreeing on content. The key hashes the git **index**, which only matters on the shared branch (no local changes by definition), so it moves on rebase, never mid-edit.

Depended-on crates = every workspace/vendored crate **except leaf artifacts nothing links**: `crates/nub-cli` (bin), `crates/nub-native` (cdylib, own workspace), `crates/nub-phantom` (bin, own workspace). So: `crates/nub-core`, `crates/nub-cache-key`, `crates/nub-phantom-core`, `crates/nub-phantom-scan`, and all of `vendor/aube`. `nub-phantom-core`/`nub-phantom-scan` are **not** leaves — `nub-cli` depends on both — and the pathspec `:(exclude)crates/nub-phantom` matches only that directory, not those siblings.

## Letting the wrapper choose IS the caching strategy

`export CARGO_TARGET_DIR` **overrides** the wrapper's resolution — it is how caching is LOST, not gained: you land in a cold dir, or a multi-tenant one you can clobber.

**Never put an exported `CARGO_TARGET_DIR` in a dispatch prompt.** A sub-agent cannot know whether the value is the warm bucket, a cold path, or a dir a sibling is mid-build in; the wrapper does, and it prints which it chose and why.

**Pin one only when seeding cannot fire** — a branch cut before seeding landed, or a filesystem without CoW:

```sh
grep -c seed scripts/rust-build.sh      # 0 → this branch has no seeding; pinning may pay
```

When you must pin for a **serial multi-phase epic**, point the whole chain at ONE dedicated private target so deps compile cold once:

```sh
export CARGO_TARGET_DIR=~/.cache/nub/<epic>-target
```

- **Serial only** — cargo's target-dir lock serializes builds; never point two *concurrently-building* worktrees at it.
- **Dedicated, NOT `shared-target`** — the shared dir is multi-tenant.
- **A review/verify sub-agent reuses the implementer's already-warm target**, never a fresh one.

For a `orchestrator` run, pinning buys nothing: every lane edits ONE worktree and the orchestrator builds that same worktree.

## Trade-offs and edges

- **A worktree that edits aube pays a cold build even with no sibling diverging aube concurrently.** The invariant is "match origin/main," which doesn't depend on volatile sibling state — that's what makes it robust.
- **Concurrent builds in two sharing worktrees serialize** on cargo's target-dir lock. A latency cost, never a correctness one. Need two at once? Isolate one.
- **`NUB_SHARED_TARGET`** relocates the target dir and the path is used **exactly as given** — not content-keyed, because a caller naming a path is asking for that path (`make verify` relies on this to reach `$(CURDIR)/target`). The value must be **private to one checkout**; two worktrees on one relocated path recreates the phantom-`E0063` clobber. To relocate a cache several worktrees share, leave it unset and move `~/.cache/nub` itself.
- Cleanup: `git worktree remove <path> --force` drops the worktree and its private `target/`; the shared dir is intentionally left in place.
- **A build script can pin an ABSOLUTE PATH into the shared dir** — a second contamination shape. A build script resolving inputs through compile-time `env!("CARGO_MANIFEST_DIR")` bakes the compiling worktree's path into the cached build-script binary, which is cached per package id and survives into every sharing worktree. Symptom: `failed to read /…/worktrees/<some-other-tree>/…` naming a directory absent from your checkout. Fixed at the source; for a stale one, `rm -rf <target-dir>/*/build/<crate>-*` and rebuild. Note `cargo clean -p <crate>` from the repo ROOT is a **silent no-op** for the aube crates — not root workspace members, so it reports `Removed 0 files` and exits 0.
- **Running the vendored aube test suite from the repo root drops its serial-execution pin.** `vendor/aube/.cargo/config.toml` sets `RUST_TEST_THREADS = "1"` because some aube-util tests mutate process env, and cargo discovers config from the **CWD, not `--manifest-path`** — so `cargo test --manifest-path vendor/aube/Cargo.toml …` from the root runs them in parallel and produces failures CI never sees. Run it as CI does: `(cd vendor/aube && cargo test …)`.

## The gates run on TWO profiles — budget for two dependency builds

| Gate | Profile | CI line |
| --- | --- | --- |
| `cargo check --all-targets` | `fast` | 182 |
| `cargo clippy --all-targets --all-features` | `fast` | 220 |
| `cargo test` | **default (`dev`)** | 286, 790 |

`fast` and `dev` are different target subdirectories, so clippy artifacts do not serve `cargo test`.

Do not unify them: moving `cargo test` onto `fast` diverges from CI, and dropping `fast` from clippy drives a second full dependency build under `dev` (~26 GB of duplicated `target/debug` + `target/fast`). `fast` inherits `dev` — identical debug-assertions, overflow checks and opt-level, differing only in debuginfo, which no lint reads.

## A green gate run leaves NO runnable binary — build one explicitly

| Command | What it writes | Leaves `target/fast/nub`? |
| --- | --- | --- |
| `cargo fmt --check` | nothing | no |
| `cargo clippy --profile fast` | `.rmeta` only — clippy type-checks, does not link | **no** |
| `cargo test` | test harness binaries under `target/debug/` | **no** |
| `cargo build -p nub-cli --profile fast` | the linked CLI | yes |

So `fmt` + `clippy` + `test` can all be green while `target/fast/nub` is hours stale or absent — and it fails in the worst direction, because the old behavior usually still works, so the probe reads as a clean pass. Add an explicit `build` step whenever a gate run is followed by running the thing.

**Bind the artifact to the source before trusting a fixture result.** An mtime newer than your last edit is necessary and not sufficient — a concurrent build can overwrite the path mid-run. Prove it positively by exercising a behavior only your change produces, and re-hash afterwards (or copy the binary aside and probe the copy).

## Long cargo runs go in a BACKGROUND shell — never poll for them

A cold build, `--all-targets` clippy or a full `cargo test` outlives the foreground timeout. Start
it with the harness's background mechanism and let the completion notification wake you. **Do not
write a wait loop**, and do not sleep on it: the harness already tracks it, so a poller is pure
waste and it can be WRONG in both directions.

Measured cost of getting this wrong: a loop polling `pgrep -f "cargo|rustc"` reported "still
building" for ten minutes AFTER the build had finished — `pgrep -f` matches the full command line
INCLUDING the environment, so every process with `.cargo/bin` in its `PATH` matched. If you must
ask whether a compiler is running, match the executable name exactly:

```sh
ps -Ao comm= | grep -cE '^(rustc|cargo)$'      # 0 = idle
```

## ANY cargo command REWRITES that profile's binary — with ITS features, not yours

`cargo build -p nub-cli --profile fast --features nub-cli/build-jail-catalog-override` then
`cargo test -p nub-cli --profile fast <filter>` leaves `target/fast/nub` built **without** the
feature. `test` rebuilt the bin under the default feature set and clobbered it. Nothing warns;
the binary just quietly becomes a different one.

**So re-arm the binary after ANY bare cargo invocation on the same profile**, before running a
harness or a fixture against it:

```sh
cargo test -p nub-cli --profile fast <filter>          # clobbers target/fast/nub
scripts/rust-build.sh build -p nub-cli --profile fast \
  --features nub-cli/build-jail-catalog-override        # re-arm before probing
```

Cost when missed: a build-jail catalog probe reported `BROKEN-EVEN-WITH-EVERYTHING` for a package
that installs fine, because the featureless binary refused the override. It was diagnosable only
because nub fails LOUD there (*"NUB_BUILD_JAIL_CATALOG is set, but this binary was not built with
the build-jail-catalog-override feature… Refusing rather than running the compiled-in catalog
under an override's name"*). A feature that degrades silently would have cost far more.

Related and distinct: `--features nub-sandbox/build-jail-catalog-override` enables it on the
sandbox crate ONLY, so anything gated inside nub-cli compiles out while the sandbox-side banner
still prints — the override looks live and half of it is absent. `nub-cli` forwards the feature
(`crates/nub-cli/Cargo.toml`), so the bare name and `nub-cli/…` are equivalent; the `nub-sandbox/…`
form is the wrong one.

## The gate's exit status must be CARGO's — three ways it silently isn't

- **A pipe.** `cargo … | tail` gives you the PIPE's status.
- **`| head -N`** additionally closes the pipe and SIGPIPE-kills cargo outright.
- **Trailing commands.** `cargo … > log 2>&1; echo EXIT=$?; tail log` redirects correctly and still exits with **`tail`'s** status, so a harness reports 0 while cargo returned 101.

The habit that holds: redirect to a file, capture `RC=$?` immediately, and **end the command with `exit $RC`** so the shell's status IS cargo's. Read the log with `Read`/`grep`, never `tail` in the same command whose status you care about. Confirm the recorded `RC=` line before believing a gate passed.

## `--all-features` needs a staged addon

`--all-features` turns on `embed-runtime`, whose `build.rs` bakes an integrity digest of `runtime/addons/nub-native.node` — gitignored, so absent in a fresh worktree. Without it the gate fails with `cannot read entrypoint … for integrity hashing` and the lint never runs. Stage a placeholder as CI does:

```sh
mkdir -p runtime/addons && printf 'placeholder-addon' > runtime/addons/nub-native.node
```

The digest only needs bytes to hash — a real addon is not required to lint.

## Two crates are their OWN workspaces — `-p` from the root cannot see them

`crates/nub-native` (panic-strategy split) and `crates/nub-launcher` (size-tuned release profile) each have their own `[workspace]`, so `cargo build -p nub-launcher` from the root fails with `package ID specification 'nub-launcher' did not match any packages`. Build from inside the crate, or with `--manifest-path`. CI lints each separately.

## Build `nub-native` with `cd crates/nub-native`, NEVER `--manifest-path`

`crates/nub-native/.cargo/config.toml` sets `target-dir = "../../target"` so the addon lands in the repo-root `target/<profile>/` that the Makefile and CI copy from. Cargo discovers config by walking up from the CWD, not from the `--manifest-path` directory — so `cargo build --manifest-path crates/nub-native/Cargo.toml --release` silently writes to `crates/nub-native/target/`, and the follow-on `cp target/release/libnub_native.dylib runtime/addons/nub-native.node` copies a stale artifact or fails.

## An "unused import" warning does NOT mean the import is unused — check `cfg(test)` first

A non-test build warns `unused import: X` for an import only `mod tests` consumes through `use super::*`. Deleting it turns one warning into N compile errors in the test target. Before removing a flagged import, `grep` the symbol in the same file: if the hits are past the `#[cfg(test)]` line, move the import *into* `mod tests`.

## Related skills

`new-worktree.ts` creates the worktree and points you at this wrapper. `dev-loop` covers the fast-profile incremental loop; this skill owns the target-dir decision. When a build fails with a symbol/field absent from your source, this is almost always the cause — rebuild through `rust-build.sh` and it isolates you.
