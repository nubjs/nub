---
name: aube-sync
description: >-
  Pull jdx/aube's latest upstream changes into nub's plain-vendored
  `vendor/aube/**` with the fewest merge-conflict iterations. Invoke (via the
  Skill tool) whenever you need to bring a new upstream aube release (or
  arbitrary jdx/aube commits) into nub. Encodes the post-Pattern-B model: the
  vendored tree is plain in-tree files (NO git submodule, NO pin) and has no
  merge-base with jdx/aube, so a naive overwrite would be a conflict mess. The
  blessed path uses `nubjs/aube` `nub-fork` (which DOES carry real upstream
  ancestry, merge-base with jdx) as the 3-way merge venue, then brings the
  merged tree into `vendor/aube` via a delta-apply that preserves any in-tree
  fixes. Covers the merge-not-rebase rule, ours-wins conflict resolution, the
  nub-fork drift reconciliation, and the build/test gates.
---

# Syncing jdx/aube upstream into nub's vendored aube

## Mental model (read first)

After **Pattern B** (#81), `vendor/aube/**` is **plain tracked files in nub's history** — there is **NO git submodule and NO pin**. An aube change is an ordinary nub PR touching `vendor/aube/**`. **nub `main` is the source of truth** for the vendored aube.

Three git objects matter, in **two separate repos**:

| Object | Repo | Role |
| --- | --- | --- |
| `vendor/aube/**` on nub `main` | `nubjs/nub` | **Source of truth** — what ships to users |
| `nub-fork` branch | `nubjs/aube` | **Merge venue** — carries our nub delta on top of real `jdx/aube` ancestry (merge-base exists) |
| `main` branch | `jdx/aube` | **Upstream** — what we pull from |

**WHY a naive overwrite is bad:** `vendor/aube` is plain files with NO git ancestry relationship to `jdx/aube`. Copying jdx's tree over `vendor/aube` gives git no common base, so every one of our nub-delta files (embedder profile, brand helpers, config-scope gating, pnpm fixes) collides → a giant manual conflict resolution, every sync.

**WHY `nub-fork` is the venue:** `nub-fork` was branched from `jdx/aube` and merge-synced ever since, so `git merge-base nub-fork jdx/aube/main` returns a real commit. git can do a true **3-way merge** there — it only surfaces conflicts where our delta and upstream genuinely touch the same lines. Everything else auto-merges. We do the merge on `nub-fork`, then bring the **result** into `vendor/aube`.

## The invariant that keeps this cheap

**After every sync, `nub-fork`'s tip tree MUST equal the newly-vendored `vendor/aube` tree.** When this holds, the next sync's bring-in is a clean delta-apply (the vendored tree == the merge's first parent, so `parent..merge` applies with zero conflicts). The recipe ends by restoring this invariant (FF or tree-snapshot, step 6).

The drift risk: post-Pattern-B, ordinary in-tree fixes land in `vendor/aube` on nub `main` but NOT on `nub-fork`. If that happens between syncs, `nub-fork`'s tree falls behind the source of truth. **Reconcile before merging (step 2b)** so the merge venue starts from the truth.

## Recipe

Work in a nub worktree off latest `origin/main` (see the `worktree` skill). All `aube-*` remotes below are the **aube** repos, added to the worktree.

### 1. Set up remotes + fetch all three objects

```sh
cd <nub-worktree>
git remote add aube-fork https://github.com/nubjs/aube 2>/dev/null
git remote add aube-upstream https://github.com/jdx/aube 2>/dev/null
git fetch aube-fork nub-fork
git fetch aube-upstream main
```

### 2a. Confirm the relationships

```sh
git merge-base aube-fork/nub-fork aube-upstream/main   # the real common ancestor — proves the venue works
git log aube-upstream/main --oneline -5                # what's new upstream
# Is upstream already merged into nub-fork? (idempotency check)
git merge-base --is-ancestor aube-upstream/main aube-fork/nub-fork \
  && echo "already synced" || echo "upstream moved; merge needed"
```

### 2b. Reconcile drift — make nub-fork's tree match the SOURCE OF TRUTH first

Compare `nub-fork`'s tip tree against the vendored tree. If they differ, in-tree fixes landed on `main` that `nub-fork` is missing — snapshot them onto `nub-fork` (ancestry-preserving) BEFORE merging upstream:

```sh
rm -rf /tmp/forktip /tmp/vendored && mkdir -p /tmp/forktip /tmp/vendored
git archive aube-fork/nub-fork | tar -x -C /tmp/forktip
git archive origin/main:vendor/aube | tar -x -C /tmp/vendored
diff -rq /tmp/forktip /tmp/vendored      # empty == in sync, skip the snapshot
```

If they differ, create a **tree-snapshot commit** on `nub-fork` whose tree == current `vendor/aube`, parented on `nub-fork` tip (this keeps `nub-fork`'s real jdx ancestry — do NOT `subtree split`, which fabricates synthetic commits and breaks the merge-base):

```sh
# In a clone of nubjs/aube on nub-fork, mirror the vendored tree in, commit, push.
rsync -a --delete /tmp/vendored/ <aube-clone>/    # excluding .git
git -C <aube-clone> add -A
git -C <aube-clone> commit -m "chore: sync nub-fork tree to vendored in-tree state"
git -C <aube-clone> push origin nub-fork
git fetch aube-fork nub-fork
```

> **Why a tree-snapshot, not `git subtree split`:** `subtree split` rewrites `vendor/aube/**`'s nub-side history into synthetic commits with no relation to `jdx/aube` — that destroys the merge-base that makes `nub-fork` a valid 3-way venue. A single snapshot commit parented on the real `nub-fork` tip keeps the ancestry and is the lowest-overhead correct answer.

### 3. Merge upstream into nub-fork — MERGE, never rebase; OURS-WINS

Do this in a clone of `nubjs/aube` on `nub-fork` (so the merge has the real ancestry). **Merge-commit, never rebase** — rebasing rewrites SHAs, multiplies conflict reps, and forces a force-push. Colin's explicit preference is merge.

```sh
git -C <aube-clone> checkout nub-fork
git -C <aube-clone> merge aube-upstream/main --no-ff
```

**Conflict resolution = OURS WINS** (Colin's explicit rule). Our nub delta always survives:

- A file **we own / modified** (embedder profile, `workspace_markers()`/`lockfile_basename()` brand helpers, config-scope gating, identity, our PM fixes) → take **ours**.
- A file **we don't own** that upstream changed → take **upstream**.
- **Convergence case** (both sides independently did the same work — e.g. the v1.23 sync's `audit.rs` tests, where production code auto-merged and only upstream's new *tests* conflicted): accept upstream's additive piece; we don't own that test logic. This is convergence, not a loss of our delta.

To bias auto-resolution toward ours while still taking upstream where we have no competing change, `-X ours` on the merge is acceptable for noisy files — but prefer **manual review of each conflict** so a real upstream behavior change isn't silently dropped. Flag any OURS-vs-THEIRS call that touches a **default / security posture / product behavior** for maintainer sign-off — resolve ours-wins but surface it.

Verify the merge in the aube clone, then push the staging branch:

```sh
cd <aube-clone>
cargo test -p aube --lib && cargo test -p aube-lockfile --lib && cargo test -p aube-resolver --lib
# registry/config tests read ~/.npmrc — run with an isolated HOME (keep RUSTUP_HOME/CARGO_HOME real):
HOME=/tmp/clean RUSTUP_HOME=$HOME/.rustup CARGO_HOME=$HOME/.cargo cargo test -p aube-registry --lib
git push origin nub-fork           # FF nub-fork to the merge (this is also step 6's invariant restore)
```

### 4. Bring the merged tree into vendor/aube — delta-apply, NOT blind overwrite

Because the invariant held (`nub-fork` tip tree == `vendor/aube` before the merge), the merge result's tree is exactly `vendor/aube` + the upstream delta. Mirror it in:

```sh
rm -rf /tmp/merged && mkdir -p /tmp/merged
git archive <merge-commit-sha> | tar -x -C /tmp/merged
rsync -a --delete /tmp/merged/ <nub-worktree>/vendor/aube/
git -C <nub-worktree> add vendor/aube
git -C <nub-worktree> status --short vendor/aube   # == the upstream delta filelist, nothing else
```

**Preserve in-tree fixes:** if a file you bring from upstream overlaps a concurrently-landing nub PR (check open PRs touching `vendor/aube/**`), **ours (the in-tree version) wins** — keep nub's. In the v1.23 sync the upstream delta was fully disjoint from the parallel PRs, so this never bit, but always check overlap before the rsync.

### 5. Build + test gates (the exact CI gates)

From the **nub** worktree root (vendor/aube is a path dep, compiled as a dependency):

```sh
export CARGO_TARGET_DIR=/tmp/<slug>-target
cargo check -p nub-cli                              # the integration gate — aube must build AS nub's dep
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check                                  # excludes vendor/aube (its own workspace), but run it
```

And from `vendor/aube/` (its own workspace, own target dir) for the engine tests:

```sh
cd vendor/aube && export CARGO_TARGET_DIR=/tmp/<slug>-vendor-target
HOME=/tmp/clean RUSTUP_HOME=~/.rustup CARGO_HOME=~/.cargo \
  cargo test -p aube --lib -p aube-lockfile --lib -p aube-registry --lib -p aube-resolver --lib
```

**Verify our delta survived:** confirm the nub-specific symbols are still present after the bring-in:

```sh
grep -rn "workspace_markers\|lockfile_basename\|EmbedderProfile\|read_branded_pnpm_config" vendor/aube/crates
```

### 6. Restore the invariant + open the nub PR

- **Restore the invariant:** `nub-fork`'s tip must now equal what you vendored — FF `nub-fork` to the merge commit (step 3's push already does this if you merged ON `nub-fork`). Confirm `git archive aube-fork/nub-fork` == `vendor/aube`.
- **Open an ordinary nub PR** with the `vendor/aube/**` diff. In the body, **summarize the behavior-affecting upstream changes** (so the reviewer sees what upstream behavior shifted) and **flag anything touching a default / security posture** for maintainer sign-off (recommend-only — never silently flip a default).
- **Push-then-exit** if dispatched: push the branch the instant a commit exists, report `pushed <sha>, awaiting CI`, do NOT arm a CI watcher.

## Conflict-minimization tactics

- **Sync frequently.** Smaller upstream deltas = fewer conflict reps. A 12-commit delta (v1.23) had exactly ONE conflict; a year's delta would be brutal.
- **Keep our delta THIN by upstreaming aggressively.** Pluggable/additive changes that are no-op for standalone aube (embedder profile, env-resolution hooks, source-branding helpers, exit-code sweeps) → PR them to `jdx/aube`. Once jdx merges, the next `merge upstream/main` CONVERGES them (git dedups identical content) and they graduate OUT of our fork-only delta — fewer files to conflict next time.
- **Merge, never rebase** (restated — it's the single biggest lever): rebase replays each of our delta commits onto the new upstream tip, re-surfacing the same conflict once per commit. A merge resolves each conflict ONCE.
- **The convergence-dedup case is a feature, not a problem:** when both sides did the same work, accept upstream's version of the not-ours-to-own piece; the production code usually auto-merges to the converged impl.

## Idempotency / "is it already synced?"

If `git merge-base --is-ancestor aube-upstream/main aube-fork/nub-fork` returns true, jdx HEAD is already merged into `nub-fork` — no merge needed; just verify `vendor/aube` == `nub-fork` tip (the invariant) and you're done.
