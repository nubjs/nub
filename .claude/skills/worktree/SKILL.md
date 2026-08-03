---
name: worktree
description: >-
  Create and manage isolated git worktrees for parallel build/test/landing work
  on the nub repo. Invoke (via the Skill tool) whenever you need a fresh
  worktree to land a change, when you want to know what `.worktreeinclude` does
  or how to add an entry, or when cleaning up after a merge. Encodes the
  one-command setup (`nub scripts/new-worktree.ts <slug>` or `node …`) that bakes
  in the proven recipe — worktree off origin/main (vendor/aube is plain in-tree
  files now, no submodule init), the shared CARGO_TARGET_DIR
  (`~/.cache/nub/shared-target`) that all worktrees reuse, and applying
  `.worktreeinclude` — plus the eagerly-pull-the-shared-tree discipline and the
  safe cleanup path. Pairs with the `dev-loop` build skill.
metadata:
  internal: true
---

# Worktrees for parallel nub work

Substantive nub-repo work lands via a PR opened from an isolated git worktree; the shared working tree stays on `main` and is never branched, reset, or stashed. The whole setup is one command — don't hand-roll the `git worktree add` + target-dir recipe.

## Create a worktree

The script runs under both nub (dogfood) and plain Node:

```bash
nub  scripts/new-worktree.ts <slug>
node scripts/new-worktree.ts <slug>
```

It performs the proven recipe, in order:

1. `git fetch origin` (skip with `--no-fetch`).
2. `git worktree add ~/.cache/nub/worktrees/<slug> -b <slug> origin/main` — tracked files only; the shared tree is untouched. `vendor/aube` is plain in-tree files, checked out by this step with no submodule init. (Non-temp location: not auto-swept like `/tmp`, out of the repo dir, same volume so APFS clonefile stays fast.)
3. Apply `.worktreeinclude` — copy/symlink the listed gitignored entries in.
4. Pre-create + print the shared `CARGO_TARGET_DIR` (`~/.cache/nub/shared-target`).

Options: `--base <ref>` (default `origin/main`), `--path <dir>` (default `~/.cache/nub/worktrees/<slug>`), `--no-fetch`, `--help`.

Then build **through the wrapper** — do not export `CARGO_TARGET_DIR` yourself; `scripts/rust-build.sh` picks the right dir and CoW-seeds an isolated one when this worktree diverges a depended-on crate (see the `rust-build` skill):

```bash
cd ~/.cache/nub/worktrees/<slug>
scripts/rust-build.sh build -p nub-cli --profile fast
```

All worktrees share ONE target dir (`~/.cache/nub/shared-target`), so a second worktree reuses the crates.io dependency artifacts another already compiled and recompiles only the ~10 workspace crates — one target dir instead of ~30 multi-GB private ones. Tradeoff: cargo locks the target dir during a build, so two sharing worktrees serialize. Don't clean the shared dir between iterations. Build loop, profiles, and crate map: the `dev-loop` skill.

## `.worktreeinclude` — bringing gitignored things in

`git worktree add` checks out tracked files only, so a worktree is lean by default (no `target/`, `node_modules/`, `.repos/`). `.worktreeinclude` at the repo root lists the gitignored things a worktree still needs; the script copies or symlinks each in.

Format — one entry per line, `#` comments and blank lines ignored:

```
[copy|symlink] <path>      # path is relative to the repo root, both sides
```

The verb is optional and defaults to `copy`; use `symlink` for large read-only things. Sources are read from the MAIN working tree even when you run the script from inside another worktree.

The shipped default symlinks `.repos/` (read-only reference checkouts of Node, Bun, pnpm, …). **Do NOT add `target/`** — the shared `CARGO_TARGET_DIR` is the build cache; an in-worktree `target/` is exactly the disk bloat the shared dir avoids.

## Eagerly pull the shared tree

The shared tree drifts behind `origin/main` because every landing goes worktree → push → merge and nothing pulls it back. After merging any PR or pushing to origin:

```bash
git -C <shared-tree> fetch origin && git -C <shared-tree> merge --ff-only origin/main
```

**Never `git pull --ff-only` here.** This repo sets `pull.rebase=true`, so `pull` runs rebase's precondition check first and aborts with `cannot pull with rebase: You have unstaged changes` on ANY dirty file — and the shared tree always carries some agent's WIP, so that form fails 100% of the time. `merge --ff-only` has no clean-tree precondition; it refuses only if the incoming commits would overwrite a locally-modified file.

Corollary: **do NOT commit directly in the shared tree's checkout** — a direct commit makes it diverge rather than merely fall behind, and a diverged tree cannot be fast-forwarded at all. If you take the sanctioned docs/control-surface exception, sync immediately before committing and push in the same breath; never rest with an unpushed commit there. This keeps files current; loaded `.claude/` hooks still need a session restart.

## Clean up after a merge

```bash
git worktree remove ~/.cache/nub/worktrees/<slug> --force   # leave ~/.cache/nub/shared-target in place
```

`--force` discards the worktree even with build artifacts present — so push your work first; anything uncommitted is thrown away. **Do NOT delete `~/.cache/nub/shared-target`** — it's the warm cache the next worktree builds against.

**Remove ONLY the EXACT worktree path you own — never hunt for one by HEAD SHA.** If the stated path does not exist, STOP and report "nothing to clean." Sibling worktrees routinely share a HEAD (two branches cut from the same base, a stacked branch off another's head), so SHA-matching deletes a different agent's active worktree and `--force`-discards its uncommitted WIP. A dispatch prompt with a cleanup step must name the exact path and add: "remove only this path; if absent, report and stop — never match by SHA."

There is also an older bash helper, `scripts/worktree.sh` (worktrees under `.worktrees/`, branched off LOCAL main, with `rm`/`list`/`reap` subcommands and uncommitted/unpushed-work safety checks). `new-worktree.ts` is the preferred entry for landing work; reach for `worktree.sh reap` to prune stale dead-session worktrees.
