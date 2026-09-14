#!/usr/bin/env node
// new-worktree — create an isolated git worktree off origin/main for parallel
// build/test/landing agents, with the proven nub recipe baked in.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/new-worktree.ts <slug>
//   nub  scripts/new-worktree.ts <slug>
//
// This file uses ERASABLE TypeScript only (type annotations Node's
// --experimental-strip-types removes at load): no enums, no namespaces, no
// parameter properties, no non-erasable syntax. Keep it that way so plain
// modern `node` runs it with zero build step.
//
// What it does, in order:
//   1. `git fetch origin` so the base ref is current.
//   2. `git worktree add <path> -b <branch> origin/main` (tracked files only;
//      the shared tree is untouched).
//   3. Apply `.worktreeinclude` — copy/symlink the listed gitignored entries
//      INTO the worktree (things `git worktree` won't bring, e.g. `.repos/`).
//   4. Print the build convention: use `scripts/rust-build.sh` instead of bare
//      `cargo`. Worktrees SHARE one target dir (~/.cache/nub/shared-target) so a
//      fresh worktree reuses the crates.io DEP artifacts a sibling compiled and
//      only recompiles the workspace crates — the fast path. But sharing clobbers
//      when two worktrees diverge the SAME depended-on crate (e.g. nub-core on
//      different branches), so rust-build.sh shares by default and auto-isolates
//      to a private target dir the moment THIS worktree diverges such a crate.
//      See .claude/skills/rust-build/SKILL.md. (Bare `cargo` + a hardcoded shared
//      CARGO_TARGET_DIR still works but risks that contamination — prefer the
//      wrapper.)

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, mkdirSync, cpSync, symlinkSync, lstatSync } from "node:fs";
import { dirname, join, resolve, isAbsolute } from "node:path";
import { homedir } from "node:os";

const HELP = `new-worktree — create an isolated git worktree off origin/main

Usage:
  node scripts/new-worktree.ts <slug> [--base <ref>] [--path <dir>] [--no-fetch]
  nub  scripts/new-worktree.ts <slug> [--base <ref>] [--path <dir>] [--no-fetch]

Arguments:
  <slug>              Branch name and default path suffix (worktree lands at
                     ~/.cache/nub/worktrees/<slug>, branch <slug>).

Options:
  --base <ref>       Base ref for the new branch (default: origin/main).
  --path <dir>       Explicit worktree path (default: ~/.cache/nub/worktrees/<slug>).
  --no-fetch         Skip the initial \`git fetch origin\`.
  -h, --help         Show this help.

After creation:
  cd <path>
  scripts/rust-build.sh build -p nub-cli --profile fast   # shares the cache; auto-isolates if you diverge a depended-on crate

Cleanup when done:
  git worktree remove <path> --force   # the shared target dir is intentionally NOT removed
`;

type Opts = {
  slug: string;
  base: string;
  path: string;
  fetch: boolean;
};

function die(msg: string): never {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(1);
}

function run(cmd: string, args: string[], cwd?: string): void {
  process.stderr.write(`$ ${cmd} ${args.join(" ")}\n`);
  execFileSync(cmd, args, { cwd, stdio: "inherit" });
}

function capture(cmd: string, args: string[], cwd?: string): string {
  return execFileSync(cmd, args, { cwd, encoding: "utf8" }).trim();
}

function parseArgs(argv: string[]): Opts {
  let slug: string | undefined;
  let base = "origin/main";
  let path: string | undefined;
  let fetch = true;

  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "-h" || a === "--help") {
      process.stdout.write(HELP);
      process.exit(0);
    } else if (a === "--base") {
      base = argv[++i] ?? die("--base requires a ref");
    } else if (a === "--path") {
      path = argv[++i] ?? die("--path requires a directory");
    } else if (a === "--no-fetch") {
      fetch = false;
    } else if (a.startsWith("-")) {
      die(`unknown flag: ${a}`);
    } else if (slug === undefined) {
      slug = a;
    } else {
      die(`unexpected argument: ${a}`);
    }
  }

  if (slug === undefined) die("missing <slug> (try --help)");
  if (/[/\s]|\.\./.test(slug)) die(`invalid slug: '${slug}' (no slashes, spaces, or '..')`);

  return {
    slug,
    base,
    path: path ?? `${homedir()}/.cache/nub/worktrees/${slug}`,
    fetch,
  };
}

// Apply .worktreeinclude: copy/symlink each listed gitignored entry into the
// worktree. Format: one entry per line; `#` comments and blank lines ignored;
// each entry is `[copy|symlink] <relative-path>` (default copy). Paths are
// relative to the repo root on both sides. `mainRoot` is the MAIN working tree
// (where the gitignored sources actually live) — when this script is itself run
// from inside a worktree, that is NOT the same as the worktree's own root.
function applyInclude(mainRoot: string, wt: string): void {
  const includeFile = join(mainRoot, ".worktreeinclude");
  if (!existsSync(includeFile)) return;

  const lines = readFileSync(includeFile, "utf8").split("\n");
  for (const raw of lines) {
    const line = raw.replace(/#.*$/, "").trim();
    if (line === "") continue;

    const parts = line.split(/\s+/);
    let mode = "copy";
    let rel: string;
    if (parts[0] === "copy" || parts[0] === "symlink") {
      mode = parts[0];
      rel = parts.slice(1).join(" ");
    } else {
      rel = parts.join(" ");
    }
    if (rel === "" || isAbsolute(rel) || rel.includes("..")) {
      process.stderr.write(`  .worktreeinclude: skipping invalid entry '${line}'\n`);
      continue;
    }

    const src = join(mainRoot, rel);
    const dest = join(wt, rel);
    if (!existsSync(src)) {
      process.stderr.write(`  .worktreeinclude: source missing, skipping '${rel}'\n`);
      continue;
    }
    if (existsSync(dest) || isSymlink(dest)) {
      process.stderr.write(`  .worktreeinclude: '${rel}' already present in worktree, skipping\n`);
      continue;
    }

    mkdirSync(dirname(dest), { recursive: true });
    if (mode === "symlink") {
      symlinkSync(resolve(src), dest);
      process.stderr.write(`  .worktreeinclude: symlinked ${rel}\n`);
    } else {
      cpSync(src, dest, { recursive: true });
      process.stderr.write(`  .worktreeinclude: copied ${rel}\n`);
    }
  }
}

// The main working tree is the first `worktree` line of `git worktree list
// --porcelain`. Fall back to repoRoot if parsing turns up nothing.
function mainWorktree(repoRoot: string): string {
  const out = capture("git", ["-C", repoRoot, "worktree", "list", "--porcelain"]);
  for (const line of out.split("\n")) {
    if (line.startsWith("worktree ")) return line.slice("worktree ".length).trim();
  }
  return repoRoot;
}

function isSymlink(p: string): boolean {
  try {
    return lstatSync(p).isSymbolicLink();
  } catch {
    return false;
  }
}

function main(): void {
  const opts = parseArgs(process.argv.slice(2));

  const repoRoot = capture("git", ["rev-parse", "--show-toplevel"]);
  // The MAIN working tree (first entry of `git worktree list`) holds the
  // gitignored sources .worktreeinclude points at — distinct from repoRoot when
  // this script is run from inside a worktree.
  const mainRoot = mainWorktree(repoRoot);

  if (existsSync(opts.path)) die(`worktree path already exists: ${opts.path}`);

  // Ensure the parent directory exists (e.g. ~/.cache/nub/worktrees/ on first run).
  mkdirSync(dirname(opts.path), { recursive: true });

  if (opts.fetch) {
    const remoteRef = opts.base.includes("/") ? opts.base.split("/")[0] : "origin";
    run("git", ["fetch", remoteRef]);
  }

  run("git", ["-C", repoRoot, "worktree", "add", opts.path, "-b", opts.slug, opts.base]);

  applyInclude(mainRoot, opts.path);

  // Build via scripts/rust-build.sh, not a hardcoded CARGO_TARGET_DIR: it points
  // at the shared dir (~/.cache/nub/shared-target) for the fast incremental path,
  // but auto-isolates to a private target dir the moment this worktree diverges a
  // depended-on crate (nub-core, nub-settings, …) — which is when the shared dir
  // would otherwise clobber a sibling and fail with a phantom compile error on
  // correct source. Pre-create the shared dir so the fast path is real on run 1.
  const sharedTarget = `${homedir()}/.cache/nub/shared-target`;
  mkdirSync(sharedTarget, { recursive: true });
  process.stderr.write("\n");
  process.stderr.write(`worktree ready: ${opts.path}\n`);
  process.stderr.write(`  cd ${opts.path}\n`);
  process.stderr.write(`  scripts/rust-build.sh build -p nub-cli --profile fast   # shared cache; auto-isolates on depended-on-crate divergence\n`);
  process.stderr.write(`  # cleanup when done (leave the shared target dir in place for the next worktree):\n`);
  process.stderr.write(`  git worktree remove ${opts.path} --force\n`);
}

main();
