#!/usr/bin/env python3
"""Delete orphaned shared-target buckets under ~/.cache/nub.

A bucket is named shared-target-<key>, where <key> is a content hash of the
depended-on crates (scripts/rust-build.sh). Only a NON-diverged worktree resolves
to a bucket -- one that diverges a depended-on crate builds into a private
$root/target instead. So every feature branch that touches vendor/aube or a
non-leaf crate stops referencing its bucket the moment it diverges, and main
advancing orphans whatever key it moved off. Buckets therefore accumulate far
faster than rust-build.sh's own 14-day mtime GC retires them.

Orphaned = no live worktree resolves to it. Audit by default; --apply deletes.

Never deleted: the bucket owning the installed nub-dev/nubx-dev binary, and the
newest bucket, which rust-build.sh's newest_bucket() CoW-clones as the seed for
every isolated worktree.

The key computation is duplicated from rust-build.sh, so it can drift. Guard: the
script recomputes the main tree's key and compares it against what
`scripts/rust-build.sh --print-target` actually resolves -- a positive control on
a case whose answer is already known. --apply refuses if that control fails.
"""

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# Must match the `leaves` pathspecs in scripts/rust-build.sh. Passed as separate
# argv entries, which is also why this is Python: copying the shell idiom into an
# interactive zsh silently fails to word-split and yields a different hash.
LEAVES = [
    ":(exclude)crates/nub-cli",
    ":(exclude)crates/nub-native",
    ":(exclude)crates/nub-phantom",
]
# `runtime` rides along for the same reason it joined rust-build.sh: a dev
# binary resolves runtime/*.cjs from the tree that compiled nub-core, so the
# hash covers it and a runtime-divergent worktree isolates.
CRATE_PATHS = ["vendor/aube", "crates", "runtime"]
BUCKET_RE = re.compile(r"^shared-target-([0-9a-f]+)$")


def git(root, *args, check=False):
    p = subprocess.run(
        ["git", "-C", str(root), *args],
        capture_output=True,
        text=True,
        check=check,
    )
    return p.stdout


def bucket_key(root):
    """sha of the staged blob OIDs of every depended-on crate; first 12 chars."""
    listing = git(root, "ls-files", "-s", "--", *CRATE_PATHS, *LEAVES)
    if not listing:
        return None
    return hashlib.sha1(listing.encode()).hexdigest()[:12]


def is_diverged(root):
    """True when this worktree builds into a private target, not a bucket."""
    base = git(root, "merge-base", "HEAD", "origin/main").strip()
    if base:
        diff = git(root, "diff", "--name-only", base, "--", *CRATE_PATHS, *LEAVES)
        if diff.strip():
            return True
    untracked = git(
        root, "ls-files", "--others", "--exclude-standard", "--", *CRATE_PATHS, *LEAVES
    )
    return bool(untracked.strip())


def worktrees(root):
    out = git(root, "worktree", "list", "--porcelain")
    return [ln.split(" ", 1)[1] for ln in out.splitlines() if ln.startswith("worktree ")]


def installed_buckets():
    """Buckets owning an installed dev binary -- deleting one breaks it."""
    keys = set()
    for name in ("nub-dev", "nubx-dev"):
        exe = shutil.which(name)
        if not exe:
            continue
        real = os.path.realpath(exe)
        m = re.search(r"/shared-target-([0-9a-f]+)/", real)
        if m:
            keys.add(m.group(1))
    return keys


def builds_running(ps_output=None):
    """Deleting a bucket mid-build truncates an rlib, so refuse while one runs.

    Reads `comm=` (executable path, no argv) rather than `command=`: an argv
    listing contains our OWN command line, so any invocation that merely mentions
    "cargo" -- including this script's -- matches itself and blocks forever.
    """
    if ps_output is None:
        ps_output = subprocess.run(
            ["ps", "-Ao", "comm="], capture_output=True, text=True
        ).stdout
    for line in ps_output.splitlines():
        exe = line.strip().rsplit("/", 1)[-1]
        if exe in ("cargo", "rustc", "rust-lld"):
            return line.strip()
    return None


def df_line():
    p = subprocess.run(
        ["df", "-h", "/System/Volumes/Data"], capture_output=True, text=True
    )
    lines = p.stdout.strip().splitlines()
    return lines[-1] if lines else "(df unavailable)"


def control_passes(root):
    """Does our recomputed key match what rust-build.sh actually resolves?"""
    wrapper = Path(root) / "scripts" / "rust-build.sh"
    if not wrapper.exists():
        return None, "rust-build.sh not found"
    p = subprocess.run(
        [str(wrapper), "--print-target"], capture_output=True, text=True, cwd=root
    )
    resolved = p.stdout.strip().splitlines()
    resolved = resolved[-1] if resolved else ""
    name = Path(resolved).name
    m = BUCKET_RE.match(name)
    if not m:
        return None, f"main tree is isolated, not on a bucket ({name or 'no output'})"
    ours = bucket_key(root)
    if ours == m.group(1):
        return True, f"{ours} matches rust-build.sh"
    return False, f"we computed {ours}, rust-build.sh resolved {m.group(1)}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true", help="delete; default is audit")
    args = ap.parse_args()

    root = git(Path.cwd(), "rev-parse", "--show-toplevel").strip()
    if not root:
        sys.exit("not inside a git repo")
    cache = Path.home() / ".cache" / "nub"

    print(f"mode={'apply' if args.apply else 'audit'}")
    print(f"before: {df_line()}")

    ok, why = control_passes(root)
    print(f"control: {'PASS' if ok else 'FAIL' if ok is False else 'SKIP'} -- {why}")

    live = set()
    for wt in worktrees(root):
        if is_diverged(wt):
            continue
        k = bucket_key(wt)
        if k:
            live.add(k)

    protected = installed_buckets()
    buckets = sorted(
        (d for d in cache.glob("shared-target-*") if BUCKET_RE.match(d.name)),
        key=lambda d: d.stat().st_mtime,
        reverse=True,
    )
    if buckets:
        protected.add(BUCKET_RE.match(buckets[0].name).group(1))  # newest = CoW seed

    blocker = builds_running()
    if args.apply and (blocker or ok is not True):
        if blocker:
            print(f"REFUSING --apply: a build is running: {blocker}")
        if ok is not True:
            print("REFUSING --apply: positive control did not pass")
        args.apply = False

    if args.apply and list(cache.glob("*.seeding")):
        print("REFUSING --apply: a .seeding clone is in flight")
        args.apply = False

    deleted = 0
    for d in buckets:
        key = BUCKET_RE.match(d.name).group(1)
        if key in protected:
            verdict = "PROTECTED"
        elif key in live:
            verdict = "REFERENCED"
        else:
            verdict = "REMOVE" if args.apply else "ORPHANED"
        print(f"{verdict} {key}")
        if verdict == "REMOVE":
            shutil.rmtree(d, ignore_errors=True)
            deleted += 1

    print(f"summary buckets={len(buckets)} live_keys={len(live)} deleted={deleted}")
    print(f"after:  {df_line()}")


if __name__ == "__main__":
    main()
