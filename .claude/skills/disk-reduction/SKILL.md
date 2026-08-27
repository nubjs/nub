---
name: disk-reduction
description: Reclaim disk on the maintainer's Mac when the volume is full or filling — ENOSPC, "no space left on device", a failed build or agent harness, or a routine sweep of Rust build residue. Invoke whenever free space is the problem. Covers the two families that actually hold the space (worktree-private `target/` dirs and orphaned content-hashed shared-target buckets under `~/.cache/nub`), the safety gates that keep a live bucket and the installed `nub-dev` binary alive, and why `du` and `df` disagree by tens of GB on APFS. For CPU load, orphaned processes, or a build hung on a target lock, use `cpu-reduction`; for not creating the residue, `rust-build-hygiene`.
---

# Disk reduction — reclaim space from Rust build residue

The dev Mac has a 1.8 TiB data volume that fills with Rust build output. Measured 2026-08-14: the volume hit **340 MiB free** — every agent tool call failed `ENOSPC` before it could run — and a sweep took it to **189 GiB** without deleting one line of source.

**Judge every reclaim by `df`, never `du`.** `scripts/rust-build.sh` CoW-clones buckets with `cp -c`, so APFS bills the same physical blocks to every referencing path. Summed `du` sizes overstate what you get back — measured, a set of buckets `du` called 135 G returned 73 GiB of `df`. A `du -sh` over `~/.cache/nub` also takes minutes; `df` is instant.

```sh
df -h /System/Volumes/Data
```

## Where the space actually is

Ranked by what a sweep returns, with the measured 2026-08-14 figures:

| Family | Reclaimed | Tool |
| --- | --- | --- |
| Worktree-private `target/` dirs | **101 GiB** | `cpu-reduction`'s `clean-worktree-targets.py` |
| Orphaned `shared-target-<key>` buckets | **87 GiB** | `clean-shared-buckets.py` (this skill) |
| Merged worktree checkouts | ~1 GiB | `git worktree remove` |

**Build output is the whole story; checkouts are not.** A worktree checkout is CoW-cloned source — removing four of them returned about a gigabyte. Never delete a checkout to recover space.

## 1. Worktree-private targets — the biggest single win

A worktree that diverges a depended-on crate builds into a private `$root/target`, and these are what dominate. The cleaner lives in the sibling skill; do not duplicate it here.

```sh
python3 .claude/skills/cpu-reduction/scripts/clean-worktree-targets.py           # audit
python3 .claude/skills/cpu-reduction/scripts/clean-worktree-targets.py --apply
```

It protects the entire target set of any worktree with uncommitted work, refuses while a Rust build runs, and never touches a shared bucket.

## 2. Orphaned shared buckets — the win nothing else covers

```sh
python3 .claude/skills/disk-reduction/scripts/clean-shared-buckets.py           # audit
python3 .claude/skills/disk-reduction/scripts/clean-shared-buckets.py --apply
```

**Why buckets orphan so fast.** A bucket is `~/.cache/nub/shared-target-<key>`, where `<key>` hashes the content of the depended-on crates plus `runtime/` (`vendor/aube`, `crates` excluding the leaves `nub-cli`/`nub-native`/`nub-phantom`, and `runtime`). Two facts compound:

- **Only a NON-diverged worktree resolves to a bucket at all.** The moment a branch touches a depended-on crate, `rust-build.sh` sends it to a private `$root/target` instead — so most feature branches reference no bucket, and a bucket's referrers are only ever the worktrees sitting at some exact content state.
- **`main` advancing moves the key.** Every merge that touches `vendor/aube`, a non-leaf crate, or `runtime/` strands the previous bucket.

`rust-build.sh` has its own GC, but it only retires a bucket after **14 days** of untouched mtime, so a fortnight of churn accumulates first. On 2026-08-14, 13 of 15 buckets proved orphaned — 12 in the first pass, holding ~135 G by `du` and returning 73 GiB of `df`, plus a 15 G bucket that orphaned the moment its last worktree was removed. Re-run the audit after removing any worktree.

**What the script never deletes:**

- the bucket owning the installed `nub-dev` / `nubx-dev` symlink — deleting it breaks the dev binary with no error until you run it;
- the **newest** bucket, which `newest_bucket()` CoW-clones as the seed for every isolated worktree — lose it and the next fresh worktree pays a cold build;
- anything, while a `cargo`/`rustc`/`rust-lld` process runs or a `.seeding` clone is in flight — deleting a bucket mid-build truncates an rlib.

**The built-in positive control.** The key computation is duplicated from `rust-build.sh` and can drift, so the script recomputes the main tree's key and compares it against what `scripts/rust-build.sh --print-target` actually resolves. `--apply` refuses unless that control passes. Read the `control:` line before trusting any verdict.

## 3. Merged worktrees — hygiene, not reclaim

Remove only worktrees that are **clean AND merged**. `git worktree remove` refuses a dirty tree; never force it.

```sh
gh pr list --state merged --limit 400 --json headRefName --jq '.[].headRefName' | sort -u > /tmp/merged.txt
git worktree list --porcelain | awk '/^worktree /{print $2}'   # then check each: dirty? merged?
git worktree remove ~/.cache/nub/worktrees/<slug> && git worktree prune
```

**`git branch --merged origin/main` misses most of them** — this repo squash-merges, which leaves no ancestry link, so a squash-merged branch reports unmerged. Ask GitHub for merged PR head refs and check ancestry only as a fallback for detached-HEAD worktrees.

Removing a clean worktree is lossless — the branch and its commits stay in the repo — and the CHECKOUT itself returns almost nothing. **The reclaim is indirect: a removed worktree can be the last referrer pinning a bucket.** Measured 2026-08-17, removing three clean+merged worktrees flipped one bucket from `REFERENCED` to `ORPHANED` and returned 16 GiB of `df`. So always re-run the §2 bucket audit afterwards rather than stopping here.

## 4. Gotchas that each cost a round trip

- **Expect a CONCURRENT sweeper, and never read the audit as a stable plan.** Dozens of agent sessions share this host, so another one is often mid-sweep: measured 2026-08-17, free space climbed 14 → 87 GiB during two read-only audits, five targets listed as eligible were already gone by the apply pass, and one worktree flipped dirty→clean between the two runs. Consequences: re-audit rather than trusting a list you printed minutes ago, and treat a vanished path as success. `clean-worktree-targets.py` used a bare `shutil.rmtree`, which raises `FileNotFoundError` on exactly that race and aborted the whole run with ~70 GiB of candidates still pending; it now deletes via `rm -rf` and reports a `failed=` count so a real permission error is still visible.
- **`du -sh -d1` silently prints nothing.** BSD `du` rejects `-s` with `-d` and writes the usage error to stderr, so a `2>/dev/null` scan for big directories returns an empty list that reads as "nothing here". Use `du -h -d1`.
- **This shell is zsh; `rust-build.sh` is `#!/bin/sh`.** zsh does not word-split an unquoted `$var`, so lifting the `$leaves` pathspec idiom into an interactive shell collapses three `:(exclude)` pathspecs into one bogus one and yields a **different hash**. Every bucket then looks orphaned, including the live one. Write the pathspecs as separate literal arguments, or drive git from Python where argv is explicit.
- **`--print-target` is now a pure read on both branches** — it exits before the seeding, the `mkdir`/`touch` liveness signal, and the 14-day GC, so mapping buckets by running it per worktree no longer recreates the dirs you just deleted. (It was a write until 2026-08-27; on an older checkout of `rust-build.sh`, use it once, in the main tree, as the control.)
- **You cannot fake a `cargo` process by copying a system binary.** `cp /bin/sleep /tmp/cargo` breaks the code signature, and arm64 macOS kills the copy on exec — so a "is a build running?" negative control silently tests nothing and reports the guard as broken. Test the parser against synthetic `ps` output instead.
- **`grep -E 'lld'` matches `installd`.** macOS runs `installd`, `system_installd` and `appinstalld` continuously, so a loose linker grep reports builds on a completely idle machine. Match the executable basename exactly.
- **Read `ps -Ao comm=`, not `command=`, when hunting build processes.** An argv listing includes your own command line, so any check that mentions `cargo` matches itself and blocks forever.

## 5. Verify

```sh
df -h /System/Volumes/Data                       # the only number that counts
nub-dev --version                                # proves the surviving bucket still owns the binary
ls -dt ~/.cache/nub/shared-target-* | head -1    # the seed for the next isolated worktree
```

A sweep that leaves `nub-dev` broken has not succeeded. Re-run `make install-dev` if the bucket it pointed at is gone.
