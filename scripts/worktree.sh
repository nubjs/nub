#!/usr/bin/env bash
# nub worktree — isolated git worktrees for parallel build/test agents.
#
# Why this exists: nub's build lane normally serializes (two agents editing +
# `cargo build`ing the same tree cause torn reads). A worktree gives each landing
# agent its own checkout + its own target/, so they build/test in true isolation
# and commit back to main with no merge ceremony (shared object store).
#
# usage:
#   scripts/worktree.sh <name>            # new worktree at .worktrees/<name>, branched off LOCAL main
#   scripts/worktree.sh <name> --base <ref>   # branch off <ref> instead of main
#   scripts/worktree.sh rm <name>         # remove worktree + its branch
#   scripts/worktree.sh rm <name> --force # ignore unmerged commits / dirty tree
#   scripts/worktree.sh list              # list worktrees
#   scripts/worktree.sh reap              # prune dead-session worktrees (metadata + empty dirs)
#
# SUBMODULES. `git worktree add` does NOT populate submodules. vendor/aube is
# NO LONGER a submodule (plain in-tree files since Pattern B, 2026-06-22) — it
# comes along with the checkout, build-critical and already present. The only
# remaining submodule is tests/node-suite (the ENTIRE nodejs/node repo — huge,
# compat-corpus only), left empty on purpose; init it by hand in a worktree if
# you actually need the corpus.
#
# Base is LOCAL main (not origin/main) — no push required, and the worktree pins
# to main's current HEAD as a stable base. nub works on main; merge a worktree
# branch back with a trivial `git merge` in the primary tree (same object store).

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
WORKTREE_BASE="${REPO_ROOT}/.worktrees"

die() { echo "error: $*" >&2; exit 1; }
run() { echo "\$ $*" >&2; "$@"; }

validate_name() {
  local name="$1"
  case "$name" in
    -*|*/*|*..*) die "invalid worktree name: '$name'" ;;
    rm|list|reap) die "'$name' is a reserved subcommand; pick another worktree name" ;;
  esac
}

branch_exists() { git -C "$REPO_ROOT" rev-parse --verify --quiet "refs/heads/$1" >/dev/null 2>&1; }

post_create() {
  # vendor/aube is plain in-tree files now (Pattern B) — checked out by
  # `git worktree add`, no submodule init needed.
  local wt="$1"
  echo "" >&2
  echo "worktree ready: $wt" >&2
  echo "  cd .worktrees/$(basename "$wt")   # has its own target/ — builds in isolation" >&2
  echo "  # when done:  scripts/worktree.sh rm $(basename "$wt")" >&2
}

create() {
  local name="$1" base="${2:-main}"
  validate_name "$name"
  local wt="${WORKTREE_BASE}/${name}"
  [ -e "$wt" ] && die "worktree path already exists: $wt"
  branch_exists "$name" && die "local branch '$name' already exists; remove it or pick another name"
  git -C "$REPO_ROOT" rev-parse --verify --quiet "refs/heads/${base}" >/dev/null 2>&1 \
    || die "base ref '${base}' not found locally"
  mkdir -p "$WORKTREE_BASE"
  run git -C "$REPO_ROOT" worktree add --no-track -b "$name" "$wt" "$base"
  post_create "$wt"
}

remove() {
  local name="$1" force="${2:-}"
  validate_name "$name"
  local wt="${WORKTREE_BASE}/${name}"
  if [ -e "$wt" ]; then
    # We ALWAYS pass --force to git (to discard build artifacts), but first enforce
    # the real safety check ourselves: NEVER lose work. vendor/aube is plain in-tree
    # files now (Pattern B), so its edits are ordinary tracked changes caught by the
    # single `git status` check below — no separate submodule-clone check needed.
    if [ "$force" != "--force" ]; then
      if [ -n "$(git -C "$wt" status --porcelain 2>/dev/null)" ]; then
        die "worktree '$name' has uncommitted changes; commit them first, or 'rm $name --force' to discard"
      fi
    fi
    run git -C "$REPO_ROOT" worktree remove --force "$wt"
  else
    echo "worktree path missing; pruning metadata only" >&2
    run git -C "$REPO_ROOT" worktree prune
  fi
  if branch_exists "$name"; then
    if [ "$force" = "--force" ]; then run git -C "$REPO_ROOT" branch -D "$name"
    else run git -C "$REPO_ROOT" branch -d "$name"; fi
  fi
  echo "removed worktree '$name'" >&2
}

# Reaper for the dead-session failure mode: a crashed/quota-killed session can
# leave a stale worktree registration. Prune metadata for vanished trees; report
# (do NOT auto-delete) any worktree with uncommitted changes so nothing is lost.
reap() {
  run git -C "$REPO_ROOT" worktree prune -v
  echo "--- worktrees with uncommitted changes (NOT reaped — commit or rm --force yourself): ---" >&2
  git -C "$REPO_ROOT" worktree list --porcelain | awk '/^worktree /{p=$2} /^HEAD /{h=$2} END{}'
  local wt
  while IFS= read -r wt; do
    [ "$wt" = "$REPO_ROOT" ] && continue
    if [ -d "$wt" ] && [ -n "$(git -C "$wt" status --porcelain 2>/dev/null)" ]; then
      echo "  DIRTY: $wt" >&2
    fi
  done < <(git -C "$REPO_ROOT" worktree list --porcelain | awk '/^worktree /{print $2}')
  echo "reap done" >&2
}

main() {
  local cmd="${1:-}"
  case "$cmd" in
    "" ) die "usage: scripts/worktree.sh <name> [--base <ref>] | rm <name> [--force] | list | reap" ;;
    rm )
      shift; local name="" force=""
      for a in "$@"; do case "$a" in --force) force="--force";; *) name="$a";; esac; done
      [ -n "$name" ] || die "usage: scripts/worktree.sh rm <name> [--force]"
      remove "$name" "$force" ;;
    list ) run git -C "$REPO_ROOT" worktree list ;;
    reap ) reap ;;
    * )
      local name="$cmd" base="main"; shift
      while [ $# -gt 0 ]; do
        case "$1" in
          --base) base="${2:-}"; [ -n "$base" ] || die "--base requires a ref"; shift 2;;
          *) die "unknown flag: $1";;
        esac
      done
      create "$name" "$base" ;;
  esac
}

main "$@"
