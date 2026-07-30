#!/usr/bin/env python3
"""Audit or delete regenerable Rust targets owned by clean Nub worktrees."""

from __future__ import annotations

import argparse
import datetime as dt
from pathlib import Path
import shutil
import subprocess
import sys


BUILD_COMMANDS = ("cargo", "rustc", "rust-build", "lld", "ld64", "cc1")
BUILD_PROCESS_NAMES = {"cargo", "rustc", "lld", "ld64", "cc1", "cc1plus"}


def command(*args: str, check: bool = True) -> str:
    result = subprocess.run(
        args,
        check=check,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.stdout


def note(message: str, log: Path | None) -> None:
    print(message, flush=True)
    if log:
        with log.open("a") as handle:
            handle.write(f"{message}\n")


def size_gib(path: Path) -> float:
    result = command("du", "-sk", str(path), check=False).split()
    return int(result[0]) / 1024 / 1024 if result else 0.0


def worktree_paths(repo: Path) -> list[Path]:
    output = command("git", "-C", str(repo), "worktree", "list", "--porcelain")
    return [
        Path(line.removeprefix("worktree ")).resolve()
        for line in output.splitlines()
        if line.startswith("worktree ")
    ]


def dirty(worktree: Path) -> bool:
    result = subprocess.run(
        ["git", "-C", str(worktree), "status", "--porcelain"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.returncode != 0 or bool(result.stdout)


def live_build_uses(path: Path, process_snapshot: str) -> bool:
    spelling = str(path)
    return any(
        spelling in line and any(command_name in line for command_name in BUILD_COMMANDS)
        for line in process_snapshot.splitlines()
    )


def running_build_processes() -> list[str]:
    output = command("ps", "-axo", "comm=,command=", check=False)
    builds = []
    for line in output.splitlines():
        fields = line.split(maxsplit=1)
        if fields and Path(fields[0]).name in BUILD_PROCESS_NAMES:
            builds.append(line)
    return builds


def installed_dev_binaries() -> list[Path]:
    binaries = []
    for path in (Path("/usr/local/bin/nub-dev"), Path("/usr/local/bin/nubx-dev")):
        if path.exists() or path.is_symlink():
            binaries.append(path.resolve())
    return binaries


def owns_installed_dev_binary(path: Path, binaries: list[Path]) -> bool:
    resolved = path.resolve()
    return any(
        binary.resolve() == resolved or resolved in binary.resolve().parents
        for binary in binaries
    )


def target_named(name: str) -> bool:
    return (
        name == "target"
        or name.endswith("-target")
        or name.endswith("wintarget")
        or "-target-" in name
        or name
        in {
            "target-darwin",
            "target-linux",
            "target-macos",
            "target-native",
            "target-win",
            "target-windows",
        }
    )


def internal_targets(worktree: Path) -> list[Path]:
    if not worktree.is_dir():
        return []
    return [
        path
        for path in worktree.iterdir()
        if path.is_dir() and target_named(path.name)
    ]


def sibling_targets(worktrees: list[Path], root: Path) -> list[tuple[Path, Path]]:
    owners = [worktree for worktree in worktrees if worktree.parent == root]
    registered = set(owners)
    candidates: list[tuple[Path, Path]] = []

    if not root.is_dir():
        return candidates

    for path in root.iterdir():
        if (
            not path.is_dir()
            or path.resolve() in registered
            or not target_named(path.name)
        ):
            continue
        matches = sorted(
            (
                worktree
                for worktree in owners
                if path.name.startswith(f"{worktree.name}-")
            ),
            key=lambda worktree: len(worktree.name),
            reverse=True,
        )
        if matches:
            candidates.append((matches[0], path))

    return candidates


def safe_target(owner: Path, path: Path, root: Path) -> bool:
    if path.is_symlink() or not target_named(path.name):
        return False
    return path.parent.resolve() == owner or path.parent.resolve() == root


def filesystem_row() -> str:
    result = command("df", "-h", "/System/Volumes/Data", check=False).splitlines()
    return result[-1] if result else "df unavailable"


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Audit or delete Rust build targets owned by clean Nub worktrees."
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="delete eligible targets; the default is an audit-only dry run",
    )
    parser.add_argument(
        "--repo",
        type=Path,
        default=Path.cwd(),
        help="Nub repository used for git worktree metadata (default: cwd)",
    )
    parser.add_argument(
        "--worktree-root",
        type=Path,
        default=Path.home() / ".cache" / "nub" / "worktrees",
        help="root holding Nub worktrees and their sibling targets",
    )
    args = parser.parse_args()

    repo = Path(
        command("git", "-C", str(args.repo), "rev-parse", "--show-toplevel").strip()
    ).resolve()
    root = args.worktree_root.expanduser().resolve()
    log = (
        Path("/tmp")
        / f"nub-worktree-target-cleanup-{dt.datetime.now():%Y%m%d-%H%M%S}.log"
        if args.apply
        else None
    )
    process_snapshot = command("ps", "-axo", "command=", check=False)
    dev_binaries = installed_dev_binaries()
    live_builds = running_build_processes()
    if args.apply and live_builds:
        note("refusing --apply while a Rust build process is present:", log)
        for line in live_builds:
            note(f"  {line}", log)
        return 2

    worktrees = worktree_paths(repo)
    candidates = [
        (worktree, target)
        for worktree in worktrees
        for target in internal_targets(worktree)
    ]
    candidates.extend(sibling_targets(worktrees, root))

    note(f"mode={'apply' if args.apply else 'audit'}", log)
    note(f"before: {filesystem_row()}", log)

    protected = 0
    eligible = 0
    deleted = 0

    for owner, path in sorted(set(candidates), key=lambda pair: str(pair[1])):
        size = size_gib(path)
        if not safe_target(owner, path, root):
            protected += 1
            note(f"PROTECTED unexpected-path {size:.2f} GiB {path}", log)
            continue
        if dirty(owner):
            protected += 1
            note(
                f"PROTECTED dirty-owner={owner.name} {size:.2f} GiB {path}",
                log,
            )
            continue
        if owns_installed_dev_binary(path, dev_binaries):
            protected += 1
            note(
                f"PROTECTED installed-dev-binary owner={owner.name} "
                f"{size:.2f} GiB {path}",
                log,
            )
            continue
        if live_build_uses(path, process_snapshot):
            protected += 1
            note(
                f"PROTECTED live-build owner={owner.name} {size:.2f} GiB {path}",
                log,
            )
            continue

        eligible += 1
        if not args.apply:
            note(f"ELIGIBLE owner={owner.name} {size:.2f} GiB {path}", log)
            continue

        # Status is volatile. Repeat both safety checks at the deletion boundary.
        if dirty(owner):
            protected += 1
            eligible -= 1
            note(
                f"PROTECTED became-dirty owner={owner.name} {size:.2f} GiB {path}",
                log,
            )
            continue
        if owns_installed_dev_binary(path, installed_dev_binaries()):
            protected += 1
            eligible -= 1
            note(
                f"PROTECTED now-owns-installed-dev-binary owner={owner.name} "
                f"{size:.2f} GiB {path}",
                log,
            )
            continue
        current_processes = command("ps", "-axo", "command=", check=False)
        if live_build_uses(path, current_processes):
            protected += 1
            eligible -= 1
            note(
                f"PROTECTED build-started owner={owner.name} {size:.2f} GiB {path}",
                log,
            )
            continue

        note(f"REMOVE owner={owner.name} {size:.2f} GiB {path}", log)
        shutil.rmtree(path)
        deleted += 1

    note(
        f"summary candidates={len(candidates)} eligible={eligible} "
        f"protected={protected} deleted={deleted}",
        log,
    )
    note("shared caches preserved", log)
    note(f"after:  {filesystem_row()}", log)
    if log:
        note(f"log={log}", None)
    return 0


if __name__ == "__main__":
    sys.exit(main())
