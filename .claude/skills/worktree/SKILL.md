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
2. `git worktree add ~/.cache/nub/worktrees/<slug> -b <slug> origin/main` — tracked files only; the shared tree is untouched. vendor/aube is plain in-tree files (Pattern B) — checked out by this step, no submodule init needed. The script creates `~/.cache/nub/worktrees/` if it doesn't exist. (Non-temp location: not auto-swept like `/tmp`; out of the repo dir; same volume as the repo so APFS clonefile stays fast.)
3. Apply `.worktreeinclude` — copy/symlink the listed gitignored entries in (see below).
4. Pre-create + print the shared `CARGO_TARGET_DIR` (`~/.cache/nub/shared-target`) to export — one cache for all worktrees, not a per-worktree dir.

Options: `--base <ref>` (default `origin/main`), `--path <dir>` (default `~/.cache/nub/worktrees/<slug>`), `--no-fetch`, `--help`.

After it prints the ready line:

```bash
cd ~/.cache/nub/worktrees/<slug>
export CARGO_TARGET_DIR=~/.cache/nub/shared-target   # ONE shared cache for all worktrees
cargo build -p nub-cli --profile fast                # ~3 min cold; a later worktree reuses deps, recompiles only workspace crates
```

The build loop, profiles, and crate map live in the `dev-loop` skill (`.claude/skills/dev-loop/SKILL.md`). The key fact: all worktrees share ONE target dir (`~/.cache/nub/shared-target`), so a second worktree reuses the crates.io dependency artifacts another worktree already compiled (the bulk of a build) and recompiles only the ~10 workspace crates — and the disk cost is one target dir instead of ~30 multi-GB private ones. Tradeoff: cargo locks the target dir during a build, so a build in one worktree waits while another is building. If two builds genuinely must run at once, set a private `CARGO_TARGET_DIR` for the second; otherwise keep the shared default. Don't clean the shared dir between iterations — that throws away cargo's incremental cache.

## `.worktreeinclude` — bringing gitignored things in

`git worktree add` checks out tracked files only, so a worktree is lean by default (no `target/`, `node_modules/`, `.repos/`). `.worktreeinclude` at the repo root lists the gitignored, untracked things a worktree may still need; the script copies or symlinks each one in.

Format — one entry per line, `#` comments and blank lines ignored:

```
[copy|symlink] <path>      # path is relative to the repo root, both sides
```

The leading verb is optional; the default is `copy`. Use `symlink` for large, read-only things you don't want duplicated on disk. The sources are read from the MAIN working tree (where the gitignored files actually live), even when you run the script from inside another worktree.

The shipped default symlinks `.repos/` (the read-only reference checkouts of Node, Bun, pnpm, …) so worktree agents can Read/Grep them without a multi-GB copy. Do NOT add `target/` — the shared `CARGO_TARGET_DIR` (`~/.cache/nub/shared-target`) is the build cache; an in-worktree `target/` would be a redundant private copy, which is exactly the disk bloat the shared dir exists to avoid.

## Eagerly pull the shared tree

The shared tree drifts behind `origin/main` because every landing goes worktree → push → merge and nothing pulls the shared checkout back. After merging any PR or pushing to origin, fast-forward the shared tree:

```bash
git -C <shared-tree> pull --ff-only
```

Corollary: do NOT commit directly in the shared tree's checkout — even control-surface/doc edits go via a worktree push, so the shared tree stays clean and always fast-forwardable. (Direct shared-tree commits are what make it diverge rather than merely fall behind.) This keeps the files current; loaded `.claude/` hooks still need a session restart to pick up changes.

## Clean up after a merge

```bash
git worktree remove ~/.cache/nub/worktrees/<slug> --force   # leave ~/.cache/nub/shared-target in place for the next worktree
```

`--force` is used to discard the worktree even with build artifacts present. Before discarding, make sure your work is pushed — a `git worktree remove --force` throws away anything uncommitted (vendor/aube edits are now plain in-tree files committed to the worktree's branch, so push before removing). Do NOT delete the shared `~/.cache/nub/shared-target` on cleanup — it's the warm cache the next worktree builds against; wiping it forces the next build cold.

**Remove ONLY the EXACT worktree path you own — NEVER hunt for one by HEAD SHA (this clobbers live siblings).** A cleanup agent removes the specific path it was told to and nothing else. If that path does NOT exist, STOP and report "nothing to clean" — do NOT go searching `git worktree list` for a worktree at a matching HEAD SHA and remove that. Multiple sibling worktrees routinely share a HEAD (e.g. two branches both cut from the same base commit, or a stacked branch off another's head), so SHA-matching will delete a DIFFERENT agent's active worktree and `--force`-discard its uncommitted WIP. (Burned 2026-07-06: a #334-merge agent's stated cleanup path didn't exist, so it matched a sibling `vite-bench` worktree by the shared head `26d04800` and force-removed it — orphaning a live bench agent's scaffolded fixture. The branch survived, the uncommitted work did not.) A dispatch prompt that includes a cleanup step must name the exact path AND add "remove only this path; if absent, report and stop — never match by SHA."

There is also an older bash helper, `scripts/worktree.sh` (worktrees under `.worktrees/`, branched off LOCAL main, with `rm`/`list`/`reap` subcommands and uncommitted/unpushed-work safety checks on removal). `new-worktree.ts` is the preferred entry for landing work (off `origin/main`, `.worktreeinclude` support, nub-dogfooding); reach for `worktree.sh reap` to prune stale dead-session worktrees.
