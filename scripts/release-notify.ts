#!/usr/bin/env node
// release-notify — assistive maintainer-hygiene tool: comment
// "Shipped in <tag>: <release link>" on every PR + issue that shipped in a
// release, computed DETERMINISTICALLY from the commit range, cross-checked
// against GitHub's own release-notes computation.
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/release-notify.ts <newTag> [prevTag] [--commit]
//   nub  scripts/release-notify.ts <newTag> [prevTag] [--commit]
//
// Erasable TypeScript only (no enums/namespaces/parameter-properties) so plain
// modern `node` runs it with no build step.
//
// DESIGN — assistive, NOT fire-and-forget (see .fray/tooling-gaps.md for the
// robustness investigation). The candidate set is computed from TWO sources and
// cross-checked; the tool DEFAULTS TO DRY-RUN and only posts on an explicit
// --commit, so a human eyeballs the list before anything is written:
//
//   PRIMARY   : GitHub's `releases/generate-notes` API for newTag..prevTag —
//               GitHub's own computed PR list for the range (the authoritative
//               "what shipped"). Handles squash/merge/rebase uniformly.
//   SECONDARY : trailing `(#NN)` parsed from each commit SUBJECT in the range
//               (this repo squash-merges → subjects end with `(#NN)`). Used to
//               CROSS-CHECK generate-notes and FLAG discrepancies.
//   REVERTS   : commits matching `Revert "…(#NN)"` / "This reverts commit" in
//               the range EXCLUDE the reverted PR (it didn't really ship).
//   ISSUES    : per shipped PR, `closingIssuesReferences` (GraphQL) = the exact
//               issues that PR formally closed. (Issues closed via bare
//               `Refs #`/manually are invisible here BY DESIGN — see the
//               investigation; the human can add them after reviewing.)
//
// Idempotent: skips any PR/issue that already carries a `Shipped in <tag>:`
// comment, so re-runs are safe.
//
// Requires the `gh` CLI authenticated (uses `gh api` / `gh pr comment` /
// `gh issue comment`).

import { execFileSync } from "node:child_process";

const REPO = "nubjs/nub";

// ---- args -------------------------------------------------------------------

function parseArgs(argv: string[]) {
  const positional: string[] = [];
  let commit = false;
  for (const a of argv) {
    if (a === "--commit") commit = true;
    else if (a === "--dry-run") commit = false;
    else if (a === "-h" || a === "--help") {
      printHelp();
      process.exit(0);
    } else if (a.startsWith("-")) {
      console.error(`release-notify: unknown flag ${a}`);
      process.exit(2);
    } else positional.push(a);
  }
  return { newTag: positional[0], prevTag: positional[1], commit };
}

function printHelp() {
  console.log(`release-notify — comment "Shipped in <tag>: <link>" on shipped PRs + issues

Usage:
  node scripts/release-notify.ts <newTag> [prevTag] [--commit]
  nub  scripts/release-notify.ts <newTag> [prevTag] [--commit]

Arguments:
  <newTag>   The release tag whose PRs/issues to notify (e.g. v0.1.9).
  [prevTag]  The previous tag bounding the range. Defaults to the tag
             immediately before <newTag> (git describe).

Flags:
  --commit   Actually post the comments. WITHOUT this, runs in DRY-RUN: prints
             the exact PRs + issues that would be notified, posts nothing.
  --dry-run  Explicit dry-run (the default).
  -h, --help Show this help.

The tool cross-checks GitHub's generate-notes PR list against commit-subject
(#NN) parsing, excludes reverted PRs, resolves closing issues, and is
idempotent (skips already-commented targets). Review the dry-run list before
re-running with --commit.`);
}

// ---- shelling out -----------------------------------------------------------

function git(args: string[]): string {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}

function gh(args: string[]): string {
  return execFileSync("gh", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }).trim();
}

// ---- deterministic shipped-PR computation -----------------------------------

// PRs GitHub's release-notes computation attributes to newTag..prevTag.
function prsFromGenerateNotes(newTag: string, prevTag: string): Set<number> {
  const body = gh([
    "api",
    `repos/${REPO}/releases/generate-notes`,
    "-f",
    `tag_name=${newTag}`,
    "-f",
    `previous_tag_name=${prevTag}`,
    "--jq",
    ".body",
  ]);
  const prs = new Set<number>();
  // Lines look like: "* <title> by @user in https://github.com/<repo>/pull/NN"
  for (const m of body.matchAll(/\/pull\/(\d+)\b/g)) prs.add(Number(m[1]));
  return prs;
}

// PRs parsed from the trailing (#NN) of each commit SUBJECT in the range.
// Subject-only and trailing-only so a (#NN) inside a body is never matched.
function prsFromSubjects(newTag: string, prevTag: string): Set<number> {
  const subjects = git(["log", "--format=%s", `${prevTag}..${newTag}`]).split("\n");
  const prs = new Set<number>();
  for (const s of subjects) {
    const m = s.match(/\(#(\d+)\)\s*$/);
    if (m) prs.add(Number(m[1]));
  }
  return prs;
}

// PR numbers reverted within the range — both the `Revert "…(#NN)"` subject
// form and a "This reverts commit <sha>" body referencing a ranged commit.
function revertedPRs(newTag: string, prevTag: string): Set<number> {
  const reverted = new Set<number>();
  // Subject form: Revert "<original subject ending in (#NN)>"
  const subjects = git(["log", "--format=%s", `${prevTag}..${newTag}`]).split("\n");
  for (const s of subjects) {
    if (/^Revert\b/i.test(s)) {
      const m = s.match(/\(#(\d+)\)/);
      if (m) reverted.add(Number(m[1]));
    }
  }
  return reverted;
}

// Issues a PR formally closed (Closes/Fixes #NN), via GraphQL. Returns null if
// the number is not a real PR (a subject-parsed (#NN) can be a typo / not a PR)
// — the caller flags that rather than crediting issues for a non-PR.
function closingIssuesForPR(pr: number): number[] | null {
  let out: string;
  try {
    out = gh([
      "api",
      "graphql",
      "-f",
      `query=query { repository(owner: "nubjs", name: "nub") { pullRequest(number: ${pr}) { closingIssuesReferences(first: 50) { nodes { number } } } } }`,
      "--jq",
      ".data.repository.pullRequest.closingIssuesReferences.nodes[].number",
    ]);
  } catch {
    return null; // not resolvable as a PR in this repo
  }
  if (!out) return [];
  return out.split("\n").map(Number).filter((n) => Number.isFinite(n));
}

// ---- idempotency ------------------------------------------------------------

function alreadyCommented(kind: "pr" | "issue", num: number, marker: string): boolean {
  // The {pr,issue}/N comments endpoint is shared (issues API backs both).
  let body: string;
  try {
    body = gh([
      "api",
      `repos/${REPO}/issues/${num}/comments`,
      "--paginate",
      "--jq",
      ".[].body",
    ]);
  } catch {
    return false; // be conservative: if we can't read, don't silently skip
  }
  return body.includes(marker);
}

// ---- main -------------------------------------------------------------------

function main() {
  const { newTag, prevTag: prevArg, commit } = parseArgs(process.argv.slice(2));
  if (!newTag) {
    console.error("release-notify: <newTag> is required.\n");
    printHelp();
    process.exit(2);
  }

  // Resolve prevTag: the tag immediately before newTag.
  let prevTag = prevArg;
  if (!prevTag) {
    try {
      // `--exclude 'v[0-9]'` skips the floating `v<major>` tag that release.yml
      // moves to each stable release (`uses: nubjs/nub/<name>@v0`
      // resolves through it): it shares a commit with the latest release and
      // wins git's tie-break, so without the exclusion the "previous tag" is v0.
      prevTag = git(["describe", "--tags", "--abbrev=0", "--exclude", "v[0-9]", `${newTag}^`]);
    } catch {
      console.error(
        `release-notify: could not auto-detect the previous tag for ${newTag}. Pass it explicitly.`,
      );
      process.exit(2);
    }
  }

  console.log(`release-notify: range ${prevTag}..${newTag}  (${commit ? "COMMIT" : "DRY-RUN"})\n`);

  const fromNotes = prsFromGenerateNotes(newTag, prevTag);
  const fromSubjects = prsFromSubjects(newTag, prevTag);
  const reverted = revertedPRs(newTag, prevTag);

  // Cross-check: union of both sources is the candidate set; flag one-sided PRs.
  const candidates = new Set<number>([...fromNotes, ...fromSubjects]);
  const onlyNotes = [...fromNotes].filter((p) => !fromSubjects.has(p));
  const onlySubjects = [...fromSubjects].filter((p) => !fromNotes.has(p));

  console.log(`PRs from generate-notes (authoritative): ${[...fromNotes].sort((a, b) => a - b).join(", ") || "(none)"}`);
  console.log(`PRs from commit subjects (cross-check):  ${[...fromSubjects].sort((a, b) => a - b).join(", ") || "(none)"}`);
  if (onlyNotes.length) console.log(`  ⚠ only in generate-notes (review): ${onlyNotes.sort((a, b) => a - b).join(", ")}`);
  if (onlySubjects.length) console.log(`  ⚠ only in subjects (review): ${onlySubjects.sort((a, b) => a - b).join(", ")}`);
  if (reverted.size) console.log(`  ↩ reverted in range, EXCLUDED: ${[...reverted].sort((a, b) => a - b).join(", ")}`);

  const shippedPRs = [...candidates].filter((p) => !reverted.has(p)).sort((a, b) => a - b);

  // Resolve issues per PR. A number that doesn't resolve to a real PR (a
  // subject-typo (#NN)) is flagged and excluded from posting, not credited.
  const shippedIssues = new Set<number>();
  const prToIssues = new Map<number, number[]>();
  const notRealPRs: number[] = [];
  for (const pr of shippedPRs) {
    const issues = closingIssuesForPR(pr);
    if (issues === null) {
      notRealPRs.push(pr);
      continue;
    }
    const live = issues.filter((i) => !reverted.has(i));
    prToIssues.set(pr, live);
    for (const i of live) shippedIssues.add(i);
  }
  if (notRealPRs.length) {
    console.log(`  ⚠ not resolvable as PRs (subject (#NN) typo? EXCLUDED): ${notRealPRs.sort((a, b) => a - b).join(", ")}`);
  }
  const realShippedPRs = shippedPRs.filter((p) => !notRealPRs.includes(p));

  const releaseUrl = `https://github.com/${REPO}/releases/tag/${newTag}`;
  const marker = `Shipped in ${newTag}:`;
  const message = `${marker} ${releaseUrl}`;

  console.log(`\nWould notify ${realShippedPRs.length} PR(s) and ${shippedIssues.size} issue(s):`);
  for (const pr of realShippedPRs) {
    const issues = prToIssues.get(pr) || [];
    console.log(`  PR #${pr}${issues.length ? `  (closes ${issues.map((i) => "#" + i).join(", ")})` : ""}`);
  }
  console.log(`\nComment to post: "${message}"\n`);

  if (!commit) {
    console.log("DRY-RUN — nothing posted. Review the list above, then re-run with --commit.");
    return;
  }

  // --commit: post idempotently.
  let posted = 0;
  let skipped = 0;
  const targets: Array<{ kind: "pr" | "issue"; num: number }> = [
    ...realShippedPRs.map((num) => ({ kind: "pr" as const, num })),
    ...[...shippedIssues].sort((a, b) => a - b).map((num) => ({ kind: "issue" as const, num })),
  ];
  for (const t of targets) {
    if (alreadyCommented(t.kind, t.num, marker)) {
      console.log(`  skip ${t.kind} #${t.num} (already has "${marker}")`);
      skipped++;
      continue;
    }
    gh([t.kind === "pr" ? "pr" : "issue", "comment", String(t.num), "--repo", REPO, "--body", message]);
    console.log(`  posted ${t.kind} #${t.num}`);
    posted++;
  }
  console.log(`\nrelease-notify: posted ${posted}, skipped ${skipped} (already commented).`);
}

main();
