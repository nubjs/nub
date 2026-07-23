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

`vendor/aube/**` is **plain tracked files in nub's history**. No submodule, no pin (Pattern B,
nub#81). An aube change is an ordinary nub PR touching `vendor/aube/**`.

Everything you need lives in the **nub repo**:

| Thing | Where |
| --- | --- |
| Source of truth | `vendor/aube/**` on nub `main` |
| Upstream | `jdx/aube`, configured as the `aube-upstream` remote |
| The base we're on | `vendor/aube/UPSTREAM` (commit + tag) |
| Merge venue | **built on demand, thrown away** — see below |

You do **not** need a clone of `nubjs/aube`, and you do **not** touch the `nub-fork` branch. (See
"Historical note" at the bottom for why that used to be the flow.)

---

## The venue is derived, not stored

A 3-way merge needs a commit whose **tree** is the vendored state and whose **ancestry** contains the
upstream base. Both are already in nub's object store: `<commit>:vendor/aube` is a tree object in
aube's exact layout, and the upstream commits arrive via `git fetch aube-upstream`. So:

```sh
VENUE=$(git commit-tree origin/main:vendor/aube -p <base-sha> -m "venue: vendored aube @ <base-tag>")
```

That single command gives a venue whose **tree equals `vendor/aube` by construction** — it cannot
drift, because it is derived from the source of truth at bump time rather than stored in a second
repo that someone forgets to update.

**This is verified, not theoretical.** Reproducing the v1.32 bump this way yielded byte-identical
results: `git merge-base $VENUE aube-upstream/main` returned the correct base, and the merge produced
**the same 23 conflicted files / 48 hunks** as the original clone-based run. Git's 3-way merge is a
function of (base, ours, theirs); all three are identical, so the outcome is too.

Merging inside the nub repo also means **`rerere`'s cache lives in nub** and persists across bumps.
Under the old clone-based flow every resolution was recorded in a throwaway `.git/rr-cache` and lost.

---

## The elegant core: TWO machine-generated conflict lists

You never hand-review the upstream diff. The v1.32 bump auto-merged **193 files, +13,476/−1,035** and
not one was read by a human or an agent. You work two lists, both produced by a tool, both exhaustive:

**List 1 — textual conflicts.** `git merge` gives every place both sides edited the *same lines*. One
merge, one complete list, resolved once. This is what a merge buys you over a rebase.

**List 2 — semantic conflicts.** `cargo check --workspace --all-targets` gives every place both sides
edited *different lines* in ways that don't compose: upstream adds a required parameter and nub's
caller still passes the old arity; upstream ships a new file written against a signature nub changed;
upstream renames a field under a nub-added method. **Git cannot see these — it merges text, not
meaning — so they produce zero conflict markers and a perfectly clean merge.**

> "All conflicts resolved" is worth nothing as a correctness signal. In the v1.32 bump list 1 had 48
> hunks and list 2 had **7 defects, every one in a file git merged silently**. The compiler is not a
> formality after the merge; it is the second half of the merge.

So: *why care about hunks with no conflict markers?* You don't — not as diffs, and you never open
them. You care that the merged program compiles, and you delegate finding that to the compiler.

**List 2 is layered.** Each fix lets the compiler reach further. The v1.32 bump took four rounds
(3 → 1 → 1 → 4 errors). Iterate to a clean exit; the first error list is never the whole list.

---

## The lever that decides everything: merge-base correctness

Conflict count is a function of the base, not of how far upstream moved. A wrong base makes git
re-present changes **you already have** as conflicts. In the v1.32 bump the recorded venue was two
releases stale:

| base | conflicted files | hunks |
| --- | --- | --- |
| v1.23.0 (stale) | 72 | 205 |
| v1.25.1 (correct) | **23** | **48** |

Same upstream delta. Fixing the base removed three quarters of the work, and everything mechanical
(`Cargo.lock`, 14 changelogs, generated docs, benchmarks) went from conflicted to auto-merging.

`vendor/aube/UPSTREAM` exists so this is a fact you read, not archaeology. **Keep it accurate** — it
is the one piece of state the whole method depends on.

---

## Recipe

Work in a nub worktree off latest `origin/main` (see the `worktree` skill). Never touch the shared tree.

### 1. Read the base and fetch upstream

```sh
cat vendor/aube/UPSTREAM                       # commit + tag this tree derives from
git remote add aube-upstream https://github.com/jdx/aube 2>/dev/null
git fetch aube-upstream main
git log --oneline <base-sha>..aube-upstream/main | wc -l    # size of the bump
```

**Sanity-check the marker before trusting it.** If `UPSTREAM` is missing or looks wrong, cross-check
against the version string and confirm the delta looks like *nub delta only*, not half of upstream:

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

**Merge, never rebase.** A rebase replays each nub-delta commit onto the new upstream tip and
re-surfaces the same conflict once per commit. A merge resolves each conflict exactly once.

### 3. Resolve list 1, in one shot, in parallel

Partition conflicted files **by crate/area** and dispatch one Opus agent per partition — they edit
disjoint files in one working tree, which is safe. A workable split from v1.32:

- `crates/aube/src/commands/**` (install, add, dlx, script settings)
- `crates/aube-lockfile/**` (pnpm/npm/yarn readers + writers)
- `crates/aube-linker/**`, `aube-scripts`, `aube-util`, `aube-registry`

Every dispatch prompt must carry the **doctrine below verbatim** (agents start empty and inherit
nothing) and must say: *do not run `cargo check` — siblings are mid-edit and the tree will not
compile; the orchestrator owns the build gate*, and *do not run mutating git commands*.

### 4. Resolve list 2 (the compiler), iterating to clean

```sh
cd /tmp/aube-venue && git add -A
export CARGO_TARGET_DIR=~/.cache/nub/aube-venue-target
cargo check --workspace --all-targets --message-format short   # iterate until exit 0
cargo clippy --workspace --all-targets --all-features -- -D warnings
REAL_HOME="$HOME"; mkdir -p /tmp/clean-aube-home
env HOME=/tmp/clean-aube-home RUSTUP_HOME="$REAL_HOME/.rustup" CARGO_HOME="$REAL_HOME/.cargo" \
  cargo test --workspace --lib          # registry/config tests read ~/.npmrc
```

For each error ask: **is this symbol nub delta or upstream?** Then apply the doctrine. Never paper
over with `.unwrap()`/`.expect()` or by deleting a capability.

**Check exit codes, not piped output.** `cargo check … | tail` reports `tail`'s status — redirect to
a file and test `$?`, or a failed build will look green.

### 5. Bring the result into `vendor/aube`

```sh
cd /tmp/aube-venue && git commit                       # keep the merge commit for reference
rsync -a --delete --exclude '.git/' /tmp/aube-venue/ <worktree>/vendor/aube/
diff -rq /tmp/aube-venue <worktree>/vendor/aube --exclude .git    # MUST be identical
```

Update `vendor/aube/UPSTREAM` to the new upstream commit + tag **in the same commit**.

**Check open PRs touching `vendor/aube/**` first** (`gh pr list --json number,title,files`). Where they
overlap the upstream delta the in-tree version wins and whoever merges second resolves — say so in the
PR body.

### 6. nub-side gates (do not skip — see the feature-gating section)

```sh
cd <worktree>
scripts/rust-build.sh check -p nub-cli --all-targets
mkdir -p runtime/addons && printf 'placeholder-addon' > runtime/addons/nub-native.node  # clippy ONLY
scripts/rust-build.sh clippy --all-targets --all-features -- -D warnings
cargo fmt --check
rm -f runtime/addons/nub-native.node && make addon-fast    # REQUIRED before any test run
```

Then open an ordinary nub PR with the `vendor/aube/**` diff: summarize behavior-affecting upstream
changes, and flag anything touching a default or security posture for maintainer sign-off.

### 7. Tear down

```sh
git worktree remove /tmp/aube-venue --force && git branch -D _aube_venue
```

The venue is disposable. Nothing to push, nothing to keep in sync — that is the entire point.

---

## Conflict doctrine (paste into every resolver dispatch)

Priority order. `HEAD`/ours = nub's vendored aube; `aube-upstream/main` = jdx/aube.

1. **UNION FIRST.** Most conflicts are add/add at a shared anchor — one side's block is empty, or the
   blocks are unrelated additions (new match arms, `pub mod` lines, tests, use-list entries). Keep
   **both**. *A blind "ours wins" here silently DELETES upstream features — the #1 failure mode.*
2. **OURS WINS on genuine semantic conflict in code nub owns** (inventory below).
3. **UPSTREAM WINS in code nub does not own** — upstream bugfixes, new commands, tests for upstream
   features, refactors with no nub delta in them.
4. **CONVERGENCE** (both sides built the same feature differently): keep **ours** as the base, then
   graft any capability upstream has that ours lacks. Name every graft in the report.
5. **NEVER drop an upstream cancellation/safety call** (e.g. `control::check_cancelled()?`) because it
   landed in a region nub restructured — re-site it at the equivalent point in nub's structure.
6. Comments stay **sparse and dense** — design, invariant, provenance only. Do not narrate.
7. Anything turning on a **default / security posture / product behavior**: resolve ours-preserving and
   **flag it** for maintainer sign-off.

**A graft that breaks a test is a wrong graft.** In v1.32 an `alias_of` patch-group fallback looked
like a clean capability graft, compiled fine, and broke a round-trip test — nub keys `graph.packages`
by the snapshot key, so stamping the alias made the lockfile claim a patched identity the linker never
applies. Reverting was correct. Let the tests arbitrate; don't defend a graft.

---

## The nub delta that must survive

Grep after every bump — if one vanished, a resolution was wrong:

```sh
grep -rn "workspace_markers\|lockfile_basename\|EmbedderProfile\|read_branded_pnpm_config\|env_prefix\|cache_namespace\|engine_context\|env_overlay\|path_prepends\|runtime_node" vendor/aube/crates
```

- **Embedder profile plumbing** — `env_prefix`, `cache_namespace`, `lockfile_basename`,
  `workspace_markers`, `read_branded_pnpm_config` gating. Holds the brand + config boundary. Largely
  upstreamed, so it usually converges rather than conflicts.
- **Linker** — GVS, collective hidden tree as the sole phantom mechanism, per-package
  force-materialization (`diskMaterializePackages`), workspace-spanning hoisted planning, memoized
  clonedir probes, whole-dir `clonefile` on macOS, direct-exec of native bins.
- **Install** — concurrent OSV gating and trust-policy validation via `JoinSet` overlapping the
  download tail, the `defaultTrust` floor, the nub TTY progress line (`files_linked`).
- **Lockfile** — `nub.lock` naming via the profile, pnpm-10 `{ hash, path }` patch shape, patch-group
  range resolution, pnpm-11 `namedRegistries`, git classification in the yarn-classic reader. Patches
  declared against a package's registry name also apply to an npm-aliased install
  (`PatchGroups::resolve_package`), and an unused patch key fails the install unless
  `allowUnusedPatches` is set — both pnpm-parity fixes aube v1.32.0 does not have.
- **Build approval** — `collect_ignored` surfaces source-backed (`file:`) deps from the
  install-recorded unreviewed set, and `approve-builds` writes their **source** approval key rather
  than a bare name. Stock aube v1.32.0 silently drops them, so a dep it warns about cannot be
  approved. This one changes standalone-aube behavior (a latent-bug fix matching pnpm 11), so expect
  it to show as fork delta rather than converge on the next bump.
- **Registry** — `registry_url_for` returns an owned `String` (a `namedRegistries` route lookup borrows
  through a lock guard), mTLS/`npmAlwaysAuth`, the Android hickory carve-out.
- **Runtime / lifecycle-script env** — the embedder runtime seam. `crates/aube-util/src/engine_context.rs`
  is a whole nub-only module (`EngineContext` = process-global `OnceLock<RwLock<..>>`); its
  `runtime_node_dir` / `runtime_node_bin` / `env_overlay` / `path_prepends` / `lifecycle_user_agent_product`
  fields drive the augmentation. The `resolve_node_bin` / `resolve_path_entry` fallback ladders are grafted
  into `runtime::node_program` / `path_entries` / `apply_child_env`, and `env_overlay` + `path_prepends`
  ride on `ScriptSettings`, applied last / ahead of PATH via `compose_overlay_path`. Default-empty
  everywhere ⇒ standalone aube is byte-identical, so this delta CONVERGES silently on a bump — the grep
  above is the only thing that catches it going missing. (Upstream's own `seed_embedder_node` /
  `embedder_node_bin_dir` seam is adjacent and semantically overlapping; expect re-collision here.)

---

## nub-side integration breaks (upstream feature-gating)

**Check every bump.** Upstream keeps making things optional so embedders can drop them; nub depends
with `default-features = false` and silently loses them. Diff the feature tables:

```sh
git show aube-upstream/main:crates/aube/Cargo.toml | sed -n '/\[features\]/,/^\[/p'
```

Live requirements in `crates/nub-cli/Cargo.toml`:

- `aube` → `features = ["rustls", "publish"]`. `publish` gates `commands::publish`, which
  `pm_engine/publish_family.rs` calls directly; `rustls` is the crate's only TLS backend. Without both,
  nub-cli does not compile.
- `aube-registry` → `features = ["hickory-dns"]`. **Default-preserving, not new** — this crate called
  `.hickory_dns(cfg!(not(target_os = "android")))` unconditionally before v1.32.
- `hickory-dns` stays **off** the `aube` crate: reqwest's feature flips the default resolver for every
  client in the final binary.

Also check the brand boundary on the incoming delta — new `AUBE_*` vars must read through
`aube_util::env::embedder_env()` (which resolves via the profile prefix), and no new *unconditional*
pnpm-named file read may appear:

```sh
git diff <base>^{tree} aube-upstream/main^{tree} -- 'crates/*' | grep '^+' | grep -oE '"AUBE_[A-Z0-9_]+"' | sort -u
```

---

## Environment gotchas (all cost real time)

- **`nub-cli` has no lib target** — binary crate. `--lib` errors; use `--bins`.
- **`make addon-fast` is required before any nub test run** in a fresh worktree. Without the real N-API
  addon the TS transpile path fails with `Cannot read properties of null (reading 'transformCached')`.
  CI's placeholder-addon trick is **clippy-only** — never leave it in place for tests.
- **`pm_two_mode` takes ~41 minutes alone** (nub#523), so `cargo test --workspace` will not finish in
  any harness window. Run sync-relevant binaries in bounded batches; let CI run the whole thing.
- **`nohup setsid` does not survive** — the harness reaps the process group when the call returns.
  Split long runs into separate bounded background calls.
- **This host runs a build fleet** (load 8–270). A "test has been running for over 60 seconds" warning
  is usually contention, not a hang — confirm with a direct repro against the built binary before
  chasing it as a regression.
- **`cargo fmt --check` fails on ~18 files inside `vendor/aube`** with current rustfmt. Verify the set
  is *identical* pre- and post-merge before worrying; nub's root workspace excludes `vendor/aube` so it
  does not gate nub CI.

---

## Keeping future bumps cheap

- **Bump often.** Conflict count scales with delta size: 12 upstream commits produced one conflict;
  133 produced 48 hunks.
- **Keep `vendor/aube/UPSTREAM` accurate.** It is the only stored state the method depends on.
- **Keep the delta thin by upstreaming.** Pluggable/additive changes that are no-ops for standalone
  aube (the embedder profile, env hooks, source-branding helpers, exit-code sweeps) belong upstream —
  once merged they *converge* on the next bump and leave the fork delta entirely. **Never upstream, or
  even propose or offer upstreaming, without the maintainer's explicit in-the-moment instruction**
  (AGENTS.md hard rule). Filing an upstream *issue* is a smaller act than offering a PR, but still ask.
- **Watch for upstream superseding fork delta.** v1.32 shipped an official embedder hook for supplying
  the Node runtime to lifecycle scripts (jdx/aube#1079), overlapping nub's `env_overlay` +
  `path_prepends`. When upstream grows an official version of something we forked, that is a chance to
  delete delta — surface it rather than migrating unilaterally.

---

## Historical note: `nub-fork`

Bumps used to run through a `nub-fork` branch on `nubjs/aube`: clone it, snapshot `vendor/aube` onto
it, merge upstream there, push it back. It worked, but the flow depended on an invariant nothing
enforced — *`nub-fork`'s tip tree must equal `vendor/aube`* — and any aube fix landed in `vendor/aube`
without being mirrored broke it silently. That is exactly what happened before v1.32: `vendor/aube` was
at v1.25.1, `nub-fork` at v1.23.0, and the resulting stale base inflated the merge from 48 hunks to
205. The venue-less flow removes the invariant by deriving the venue instead of storing it.

`nubjs/aube` the **repo** still has a job: it is the fork you push branches from to open cross-fork PRs
to `jdx/aube` (`gh pr create --repo jdx/aube --base main --head nubjs:<branch>`), and those branches are
cut from `upstream/main`, not from `nub-fork`. Extract commits for upstreaming with
`git subtree split --prefix=vendor/aube` or `git format-patch --relative=vendor/aube` — subject to the
never-upstream-without-instruction rule above. The `nub-fork` branch itself is now historical record.
