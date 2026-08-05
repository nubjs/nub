---
name: aube-bump
description: >-
  Bump nub's vendored aube engine to a newer jdx/aube upstream. Invoke (via the
  Skill tool) whenever you need to pull upstream aube changes into
  `vendor/aube/**` — a new release, or arbitrary commits. Encodes the
  venue-less merge (build an ephemeral merge commit from nub's own objects with
  `git commit-tree`; no clone, no `nub-fork`, no push-back) and the
  two-conflict-list methodology: git's markers find TEXTUAL conflicts, the
  compiler finds SEMANTIC ones, both lists are machine-generated and exhaustive,
  so you never hand-review a diff. Also covers merge-base correctness (the
  single biggest lever), merge-not-rebase, `rerere`, the nub delta inventory
  that must survive, and the nub-side breaks upstream feature-gating causes.
  Supersedes the older `aube-sync` skill.
---

# Bumping vendored aube to a new upstream

## Mental model

`vendor/aube/**` is plain tracked files in nub's history — no submodule, no pin. An aube change is an ordinary nub PR touching `vendor/aube/**`. Everything lives in the nub repo; you do not need a clone of `nubjs/aube` and you do not touch the `nub-fork` branch.

| Thing | Where |
| --- | --- |
| Source of truth | `vendor/aube/**` on nub `main` |
| Upstream | `jdx/aube`, as the `aube-upstream` remote |
| The base we're on | `vendor/aube/UPSTREAM` (commit + tag) |
| Merge venue | built on demand, thrown away |

**The venue is derived, not stored.** A 3-way merge needs a commit whose tree is the vendored state and whose ancestry contains the upstream base — both already in nub's object store. Deriving it at bump time means it cannot drift, and `rerere`'s cache lives in nub and persists across bumps.

## The core: two machine-generated conflict lists

You never hand-review the upstream diff.

1. **Textual conflicts** — `git merge` gives every place both sides edited the same lines. One merge, one complete list, resolved once. This is what a merge buys over a rebase.
2. **Semantic conflicts** — `cargo check --workspace --all-targets` gives every place both sides edited *different* lines in ways that don't compose (upstream adds a required param, nub's caller keeps the old arity; upstream renames a field under a nub-added method). Git merges text, not meaning, so these produce **zero conflict markers and a perfectly clean merge**.

"All conflicts resolved" is worthless as a correctness signal — the compiler is the second half of the merge. List 2 is layered: each fix lets the compiler reach further, so iterate to a clean exit.

**The biggest lever is merge-base correctness.** Conflict count is a function of the base, not of how far upstream moved: a stale base re-presents changes you already have as conflicts (once, 72 files / 205 hunks instead of 23 / 48). `vendor/aube/UPSTREAM` exists so this is a fact you read — keep it accurate.

## Recipe

Work in a nub worktree off latest `origin/main` (see the `worktree` skill). Never touch the shared tree.

### 1. Read the base and fetch upstream

```sh
cat vendor/aube/UPSTREAM                       # commit + tag this tree derives from
git remote add aube-upstream https://github.com/jdx/aube 2>/dev/null
git fetch aube-upstream main
git log --oneline <base-sha>..aube-upstream/main | wc -l    # size of the bump
```

Sanity-check the marker before trusting it — the delta should look like *nub delta only*, not half of upstream:

```sh
grep -m1 '^version' vendor/aube/Cargo.toml            # e.g. 1.25.1
git log --oneline aube-upstream/main | grep 'release v1.25.1'
git diff --name-only <base-sha>^{tree} origin/main:vendor/aube | wc -l   # should be ~nub delta size
```

Already done? `git merge-base --is-ancestor aube-upstream/main <venue>` → nothing to do.

### 2. Build the ephemeral venue and merge

```sh
git config rerere.enabled true       # BEFORE resolving — resolve each conflict once, ever
VENUE=$(git commit-tree origin/main:vendor/aube -p <base-sha> -m "venue: vendored aube @ <base-tag>")
git merge-base $VENUE aube-upstream/main    # MUST print <base-sha>

git worktree add -b _aube_venue /tmp/aube-venue "$VENUE"
cd /tmp/aube-venue                           # this worktree's ROOT is aube's tree
git merge aube-upstream/main --no-ff --no-commit

git diff --name-only --diff-filter=U > /tmp/conflicts.txt
while read -r f; do printf "%3s %s\n" "$(grep -c '^<<<<<<<' "$f")" "$f"; done < /tmp/conflicts.txt | sort -rn
```

**Merge, never rebase** — a rebase re-surfaces the same conflict once per nub-delta commit.

### 3. Resolve list 1, in one shot, in parallel

Partition conflicted files by crate/area and dispatch one Opus agent per partition — they edit disjoint files in one working tree, which is safe. A workable split:

- `crates/aube/src/commands/**` (install, add, dlx, script settings)
- `crates/aube-lockfile/**` (pnpm/npm/yarn readers + writers)
- `crates/aube-linker/**`, `aube-scripts`, `aube-util`, `aube-registry`

Every dispatch prompt must carry the **doctrine below verbatim** (agents inherit nothing) and must say: *do not run `cargo check` — siblings are mid-edit and the tree will not compile; the orchestrator owns the build gate*, and *do not run mutating git commands*.

### 4. Resolve list 2 (the compiler), iterating to clean

```sh
cd /tmp/aube-venue && git add -A
export CARGO_TARGET_DIR=~/.cache/nub/aube-venue-target
cargo check --workspace --all-targets --message-format short   # iterate until exit 0
cargo clippy --workspace --all-targets --all-features -- -D warnings
REAL_HOME="$HOME"; mkdir -p /tmp/clean-aube-home
env HOME=/tmp/clean-aube-home RUSTUP_HOME="$REAL_HOME/.rustup" CARGO_HOME="$REAL_HOME/.cargo" \
  cargo test --workspace                # registry/config tests read ~/.npmrc
```

- **`--workspace` with NO `--lib` — that is what CI runs** (`aube-parity.yml`, `working-directory: vendor/aube`). `--lib` skips `crates/*/tests/**` entirely, including the fork-discipline integration tests that pin default-preservation — exactly what a bump is most likely to break.
- **Run aube's suite from inside `vendor/aube` (or the venue), never via `--manifest-path` from the nub root.** Cargo reads `.cargo/config.toml` from the invocation directory, and aube's sets `RUST_TEST_THREADS = "1"` because `aube-util`'s `concurrency` and `http::ticket_cache` tests mutate process env. From the nub root that is silently dropped and those tests flake, which reads exactly like a regression your merge caused.
- For each error ask: **is this symbol nub delta or upstream?** Then apply the doctrine. Never paper over with `.unwrap()`/`.expect()` or by deleting a capability.
- **Check exit codes, not piped output** — `cargo check … | tail` reports `tail`'s status.

### 5. Bring the result into `vendor/aube`

```sh
cd /tmp/aube-venue && git commit                       # keep the merge commit for reference
rsync -a --delete --exclude '.git/' /tmp/aube-venue/ <worktree>/vendor/aube/
diff -rq /tmp/aube-venue <worktree>/vendor/aube --exclude .git    # MUST be identical
```

Update `vendor/aube/UPSTREAM` to the new upstream commit + tag **in the same commit**.

Check open PRs touching `vendor/aube/**` first (`gh pr list --json number,title,files`). Where they overlap the upstream delta the in-tree version wins and whoever merges second resolves — say so in the PR body.

### 6. nub-side gates (do not skip — see the feature-gating section)

```sh
cd <worktree>
scripts/rust-build.sh check -p nub-cli --all-targets
mkdir -p runtime/addons && printf 'placeholder-addon' > runtime/addons/nub-native.node  # clippy ONLY
scripts/rust-build.sh clippy --all-targets --all-features -- -D warnings
cargo fmt --check
rm -f runtime/addons/nub-native.node && make addon-fast    # REQUIRED before any test run
```

Then open an ordinary nub PR with the `vendor/aube/**` diff: summarize behavior-affecting upstream changes, and flag anything touching a default or security posture for maintainer sign-off.

### 7. Tear down

```sh
git worktree remove /tmp/aube-venue --force && git branch -D _aube_venue
```

---

## Conflict doctrine (paste into every resolver dispatch)

Priority order. `HEAD`/ours = nub's vendored aube; `aube-upstream/main` = jdx/aube.

1. **UNION FIRST.** Most conflicts are add/add at a shared anchor — one side's block is empty, or the blocks are unrelated additions (new match arms, `pub mod` lines, tests, use-list entries). Keep **both**. A blind "ours wins" here silently deletes upstream features — the #1 failure mode.
2. **OURS WINS on genuine semantic conflict in code nub owns** (inventory below).
3. **UPSTREAM WINS in code nub does not own** — upstream bugfixes, new commands, tests for upstream features, refactors with no nub delta in them.
4. **CONVERGENCE** (both sides built the same feature differently): keep **ours** as the base, then graft any capability upstream has that ours lacks. Name every graft in the report.
5. **NEVER drop an upstream cancellation/safety call** (e.g. `control::check_cancelled()?`) because it landed in a region nub restructured — re-site it at the equivalent point in nub's structure.
6. Comments stay **sparse and dense** — design, invariant, provenance only. Do not narrate.
7. Anything turning on a **default / security posture / product behavior**: resolve ours-preserving and **flag it** for maintainer sign-off.

**A graft that breaks a test is a wrong graft.** Let the tests arbitrate; don't defend a graft.

---

## The nub delta that must survive

Grep after every bump — if one vanished, a resolution was wrong:

```sh
grep -rn "workspace_markers\|lockfile_basename\|virtual_store_subdir\|branded_env_alias_enabled\|read_branded_pnpm_config\|env_prefix\|cache_namespace\|engine_context\|env_overlay\|path_prepends\|runtime_node\|cold_path" vendor/aube/crates
```

- **Embedder profile plumbing** — `env_prefix`, `cache_namespace`, `lockfile_basename`, `workspace_markers`, `virtual_store_subdir`, `read_branded_pnpm_config` gating. Holds the brand + config boundary. Largely upstreamed, so it usually converges. Profile type is `Embedder` (`aube-util/src/identity.rs`), reached via `aube_util::embedder()`. `virtual_store_subdir` earns its grep slot: upstream call sites hardcoding `aube_store::VIRTUAL_STORE_SUBDIR` (`"virtual-store"`) over nub's profile-named leaf auto-merge with no markers. `branded_env_alias_enabled` (`aube-util/src/env.rs`) is the single switch gating every `AUBE_*` alias in `settings.toml`.
- **Linker** — GVS, collective hidden tree as the sole phantom mechanism, per-package force-materialization (`diskMaterializePackages`), workspace-spanning hoisted planning, memoized clonedir probes, whole-dir `clonefile` on macOS, direct-exec of native bins.
- **Install** — concurrent OSV gating and trust-policy validation via `JoinSet` overlapping the download tail, the `defaultTrust` floor, the nub TTY progress line (`files_linked`).
- **Lockfile** — `nub.lock` naming via the profile, pnpm-10 `{ hash, path }` patch shape, patch-group range resolution, pnpm-11 `namedRegistries`, git classification in the yarn-classic reader. Patches declared against a package's registry name also apply to an npm-aliased install (`PatchGroups::resolve_package`), and an unused patch key fails the install unless `allowUnusedPatches` is set — both pnpm-parity fixes upstream lacks.
- **Build approval** — `collect_ignored` surfaces source-backed (`file:`) deps from the install-recorded unreviewed set, and `approve-builds` writes their **source** approval key rather than a bare name. Stock aube drops them, so a dep it warns about cannot be approved. Changes standalone-aube behavior, so expect fork delta rather than convergence.
- **Registry** — `registry_url_for` returns an owned `String` (a `namedRegistries` route lookup borrows through a lock guard), mTLS/`npmAlwaysAuth`, the Android hickory carve-out.
- **`cold_path()` hints + the 1.95 MSRV** — `core::hint::cold_path()` on the rare arms of the hot install loops (`aube-linker/materialize.rs` link fallbacks, `aube-resolver/semver_util.rs` cache misses, `aube-lockfile/pnpm/subset.rs` parser bails, `aube-store/tarball.rs` validation rejects), plus `rust-version = "1.95"` in `vendor/aube/Cargo.toml`. Upstream holds aube at 1.91 for mise's packaging; nub does not. Standing fork delta — the Cargo.toml `rust-version` conflicts every bump; **resolve OURS every time**. `crates/nub-cli/Cargo.toml` also sets 1.95 (workspace floor stays 1.93); the CI MSRV `Check` job runs the root leg on 1.95.0 and the aube-free nub-phantom leg on 1.93.0.
- **Runtime / lifecycle-script env** — the embedder runtime seam. `crates/aube-util/src/engine_context.rs` is a nub-only module (`EngineContext` = process-global `OnceLock<RwLock<..>>`); its `runtime_node_dir` / `runtime_node_bin` / `env_overlay` / `path_prepends` / `lifecycle_user_agent_product` fields drive the augmentation. The `resolve_node_bin` / `resolve_path_entry` ladders are grafted into `runtime::node_program` / `path_entries` / `apply_child_env`; `env_overlay` + `path_prepends` ride on `ScriptSettings`, applied last / ahead of PATH via `compose_overlay_path`. Default-empty everywhere, so standalone aube is byte-identical and this delta converges silently — only the grep catches it going missing. Upstream's `seed_embedder_node` / `embedder_node_bin_dir` seam overlaps semantically; expect re-collision.

---

## nub-side integration breaks (upstream feature-gating)

**Check every bump.** Upstream keeps making things optional so embedders can drop them; nub depends with `default-features = false` and silently loses them. Diff the feature tables:

```sh
git show aube-upstream/main:crates/aube/Cargo.toml | sed -n '/\[features\]/,/^\[/p'
```

Live requirements in `crates/nub-cli/Cargo.toml`:

- `aube` → `features = ["rustls", "publish"]`. `publish` gates `commands::publish`, which `pm_engine/publish_family.rs` calls directly; `rustls` is the crate's only TLS backend. Without both, nub-cli does not compile.
- `aube-registry` → `features = ["hickory-dns"]`. Default-preserving, not new.
- `hickory-dns` stays **off** the `aube` crate: reqwest's feature flips the default resolver for every client in the final binary.

Also check the brand boundary on the incoming delta — new `AUBE_*` vars must read through `aube_util::env::embedder_env()`, and no new *unconditional* pnpm-named file read may appear:

```sh
git diff <base>^{tree} aube-upstream/main^{tree} -- 'crates/*' | grep '^+' | grep -oE '"AUBE_[A-Z0-9_]+"' | sort -u
```

---

## Environment gotchas

- **`nub-cli` has no lib target** — binary crate. `--lib` errors; use `--bins`.
- **`make addon-fast` is required before any nub test run** in a fresh worktree. Without the real N-API addon the TS transpile path fails with `Cannot read properties of null (reading 'transformCached')`. CI's placeholder-addon trick is clippy-only — never leave it in place for tests.
- **`pm_two_mode` takes ~41 minutes alone**, so `cargo test --workspace` will not finish in any harness window. Run sync-relevant binaries in bounded batches; let CI run the whole thing.
- **`nohup setsid` does not survive** — the harness reaps the process group when the call returns. Split long runs into separate bounded background calls.
- **This host runs a build fleet** (load 8–270). A "test has been running for over 60 seconds" warning is usually contention, not a hang — confirm with a direct repro before chasing it as a regression.
- **`cargo fmt --check` fails on ~18 files inside `vendor/aube`** with current rustfmt. Verify the set is *identical* pre- and post-merge before worrying; nub's root workspace excludes `vendor/aube`.

---

## Keeping future bumps cheap

- **Bump often.** Conflict count scales with delta size: 12 upstream commits produced one conflict; 133 produced 48 hunks.
- **Keep `vendor/aube/UPSTREAM` accurate.** It is the only stored state the method depends on.
- **Keep the delta thin by upstreaming.** Pluggable/additive changes that are no-ops for standalone aube converge on the next bump and leave the fork delta entirely. **Never upstream, or even propose or offer upstreaming, without the maintainer's explicit in-the-moment instruction** (AGENTS.md hard rule). Filing an upstream *issue* is smaller than offering a PR, but still ask.
- **Watch for upstream superseding fork delta.** When upstream grows an official version of something we forked, that is a chance to delete delta — surface it rather than migrating unilaterally.

## Upstreaming mechanics

`nubjs/aube` the repo is the fork you push branches from to open cross-fork PRs to `jdx/aube` (`gh pr create --repo jdx/aube --base main --head nubjs:<branch>`); those branches are cut from `upstream/main`. Extract commits with `git subtree split --prefix=vendor/aube` or `git format-patch --relative=vendor/aube` — subject to the never-upstream-without-instruction rule above. The `nub-fork` branch is historical record only.
