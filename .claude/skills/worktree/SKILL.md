---
name: worktree
description: >-
  Create and manage isolated git worktrees for parallel build/test/landing work
  on the nub repo. Invoke (via the Skill tool) whenever you need a fresh
  worktree to land a change, when you want to know what `.worktreeinclude` does
  or how to add an entry, or when cleaning up after a merge. Encodes the
  one-command setup (`nub scripts/new-worktree.ts <slug>` or `node …`) that bakes
  in the proven recipe — worktree off origin/main (vendor/aube is plain in-tree
  files now, no submodule init), the stable per-worktree CARGO_TARGET_DIR fast
  loop, and applying
  `.worktreeinclude` — plus the eagerly-pull-the-shared-tree discipline and the
  safe cleanup path. Pairs with the `nub-dev` build skill.
---

# Worktrees for parallel nub work

Substantive nub-repo work lands via a PR opened from an isolated git worktree; the shared working tree always stays on `main` and is never branched, reset, or stashed (see AGENTS.md "Default to a PR flow"). This skill is the fast, correct way to spin up a worktree, what `.worktreeinclude` brings into it, and how to clean up.

The whole setup is one command. Do not hand-roll the `git worktree add` + target-dir recipe — the script encodes it and is harder to get wrong. (vendor/aube is plain in-tree files since 2026-06-22 (Pattern B) — no submodule init step.)

---

## Create a worktree

The script runs under both nub (dogfood) and plain Node — pick either:

```bash
nub  scripts/new-worktree.ts <slug>
node scripts/new-worktree.ts <slug>
```

It performs the proven recipe, in order:

1. `git fetch origin` (skip with `--no-fetch`).
2. `git worktree add /tmp/nub-wt-<slug> -b <slug> origin/main` — tracked files only; the shared tree is untouched. vendor/aube is plain in-tree files (Pattern B) — checked out by this step, no submodule init needed.
3. Apply `.worktreeinclude` — copy/symlink the listed gitignored entries in (see below).
4. Print the stable per-worktree `CARGO_TARGET_DIR` convention to export.

Options: `--base <ref>` (default `origin/main`), `--path <dir>` (default `/tmp/nub-wt-<slug>`), `--no-fetch`, `--help`.

After it prints the ready line:

```bash
cd /tmp/nub-wt-<slug>
export CARGO_TARGET_DIR=/tmp/nub-wt-<slug>-target   # keep this stable for the whole session
cargo build -p nub-cli --profile fast               # ~3 min cold, ~5s incremental
```

The build loop, profiles, and crate map live in the `nub-dev` skill (`.claude/skills/nub-dev/SKILL.md`). The one rule that makes iteration fast: keep ONE stable target dir per worktree for the whole session — cargo's incremental fingerprints are keyed to the absolute target path, so cleaning, moving, or re-seeding it forces a full cold rebuild. The script never seeds a target dir for exactly this reason; it just prints the dir to export.

## `.worktreeinclude` — bringing gitignored things in

`git worktree add` checks out tracked files only, so a worktree is lean by default (no `target/`, `node_modules/`, `.repos/`). `.worktreeinclude` at the repo root lists the gitignored, untracked things a worktree may still need; the script copies or symlinks each one in.

Format — one entry per line, `#` comments and blank lines ignored:

```
[copy|symlink] <path>      # path is relative to the repo root, both sides
```

The leading verb is optional; the default is `copy`. Use `symlink` for large, read-only things you don't want duplicated on disk. The sources are read from the MAIN working tree (where the gitignored files actually live), even when you run the script from inside another worktree.

The shipped default symlinks `.repos/` (the read-only reference checkouts of Node, Bun, pnpm, …) so worktree agents can Read/Grep them without a multi-GB copy. Do NOT add `target/` — a copied/symlinked target dir is invalidated and rebuilt anyway (the fingerprint finding above); the stable dedicated `CARGO_TARGET_DIR` is the fast loop, not a seeded one.

## Eagerly pull the shared tree

The shared tree drifts behind `origin/main` because every landing goes worktree → push → merge and nothing pulls the shared checkout back. After merging any PR or pushing to origin, fast-forward the shared tree:

```bash
git -C <shared-tree> pull --ff-only
```

Corollary: do NOT commit directly in the shared tree's checkout — even control-surface/doc edits go via a worktree push, so the shared tree stays clean and always fast-forwardable. (Direct shared-tree commits are what make it diverge rather than merely fall behind.) This keeps the files current; loaded `.claude/` hooks still need a session restart to pick up changes.

## Clean up after a merge

```bash
git worktree remove /tmp/nub-wt-<slug> --force && rm -rf /tmp/nub-wt-<slug>-target
```

`--force` is used to discard the worktree even with build artifacts present. Before discarding, make sure your work is pushed — a `git worktree remove --force` throws away anything uncommitted (vendor/aube edits are now plain in-tree files committed to the worktree's branch, so push before removing).

There is also an older bash helper, `scripts/worktree.sh` (worktrees under `.worktrees/`, branched off LOCAL main, with `rm`/`list`/`reap` subcommands and uncommitted/unpushed-work safety checks on removal). `new-worktree.ts` is the preferred entry for landing work (off `origin/main`, `.worktreeinclude` support, nub-dogfooding); reach for `worktree.sh reap` to prune stale dead-session worktrees.
