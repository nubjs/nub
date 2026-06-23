#!/usr/bin/env node
// merge-cascade — drive the orchestrator's merge-queue: for each enqueued PR,
// wait for CI to go green, squash-merge, and fast-forward the shared tree.
// Automates the manual watch → merge → pull → next-PR loop. (vendor/aube is plain
// in-tree files now (Pattern B) — its edits ride the normal pull, no submodule
// sync.)
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/merge-cascade.ts [--dry-run] [--queue <path>] [--shared-tree <dir>]
//   nub  scripts/merge-cascade.ts [--dry-run] [--queue <path>] [--shared-tree <dir>]
//
// Erasable TypeScript only (no enums/namespaces/parameter-properties) so plain
// modern `node` runs it with no build step.
//
// ORCHESTRATOR tooling — NOT run in CI. Reads .fray/merge-queue.jsonl (one JSON
// object per line: {pr, branch?, thread?, note?, hold?}). For each entry, in
// order:
//   1. Skip if held — the AUTHORITATIVE signal is `"hold": true`; a note
//      containing HELD / "do NOT merge" is a mutable FALLBACK only.
//   2. Skip if the PR is already merged/closed.
//   3. Watch the PR's CI: poll `gh pr view --json statusCheckRollup` with
//      exponential backoff (capped) until the decision is terminal.
//   4. SAFETY (positive gating — see mergeDecision): merge ONLY when every
//      check concluded SUCCESS/NEUTRAL/SKIPPED, the required `CI gate`
//      aggregator check is PRESENT and SUCCESS (it registers last — a partial
//      all-green rollup is NOT done), AND mergeable === "MERGEABLE" (UNKNOWN is
//      treated as not-ready and re-polled, never as green). Any FAILURE/
//      cancelled, a CONFLICTING PR, blocks. State is re-read immediately before
//      the merge to close the poll→merge staleness window.
//   5. `gh pr merge <pr> --squash --admin`.
//   6. `git -C <shared-tree> pull --ff-only`.
//
// --dry-run reports the decision for every entry (held / would-merge / blocked)
// and merges nothing. Default is --dry-run-safe: it WILL merge on green unless
// --dry-run is passed; pass --dry-run to preview.

import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(__dirname, "..");
const REPO = "nubjs/nub";

// ---- args -------------------------------------------------------------------

function parseArgs(argv: string[]) {
  let dryRun = false;
  let queue = join(REPO_ROOT, ".fray", "merge-queue.jsonl");
  let sharedTree = REPO_ROOT;
  let pollMaxMinutes = 60;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--dry-run") dryRun = true;
    else if (a === "--queue") queue = resolve(argv[++i]);
    else if (a === "--shared-tree") sharedTree = resolve(argv[++i]);
    else if (a === "--max-minutes") pollMaxMinutes = Number(argv[++i]);
    else if (a === "-h" || a === "--help") {
      printHelp();
      process.exit(0);
    } else {
      console.error(`merge-cascade: unknown arg ${a}`);
      process.exit(2);
    }
  }
  return { dryRun, queue, sharedTree, pollMaxMinutes };
}

function printHelp() {
  console.log(`merge-cascade — drain .fray/merge-queue.jsonl: watch CI, merge on green, ff-pull

Usage:
  node scripts/merge-cascade.ts [flags]
  nub  scripts/merge-cascade.ts [flags]

Flags:
  --dry-run             Report the decision per PR (held/would-merge/blocked); merge nothing.
  --queue <path>        Queue file (default .fray/merge-queue.jsonl).
  --shared-tree <dir>   Tree to ff-pull after each merge (default repo root).
  --max-minutes <n>     Cap CI watch per PR (default 60).
  -h, --help            Show this help.

Safe by construction: never merges a PR with a failing/cancelled check or a
merge conflict; honors holds (\"hold\": true or a HELD / do-not-merge note).`);
}

// ---- shelling out -----------------------------------------------------------

function gh(args: string[]): string {
  return execFileSync("gh", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }).trim();
}
function git(args: string[]): string {
  return execFileSync("git", args, { encoding: "utf8" }).trim();
}
function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

// ---- queue parsing ----------------------------------------------------------

interface QueueEntry {
  pr: number;
  branch?: string;
  thread?: string;
  note?: string;
  hold?: boolean;
}

function readQueue(path: string): QueueEntry[] {
  if (!existsSync(path)) {
    console.error(`merge-cascade: queue not found: ${path}`);
    process.exit(2);
  }
  const out: QueueEntry[] = [];
  const raw = readFileSync(path, "utf8");
  // Support both JSONL (one object per line — the live format) and a JSON array.
  const trimmed = raw.trim();
  if (trimmed.startsWith("[")) {
    for (const e of JSON.parse(trimmed)) out.push(e);
  } else {
    for (const line of trimmed.split("\n")) {
      const l = line.trim();
      if (!l) continue;
      out.push(JSON.parse(l));
    }
  }
  return out;
}

// Hold detection. The AUTHORITATIVE signal is the structured `"hold": true`
// field (immutable to a note edit). Prose detection in `note` (HELD / do-not-
// merge) is a FALLBACK for entries not yet marked with the field — it is
// mutable (a note edit can evaporate it), so it's secondary, never primary.
function holdReason(e: QueueEntry): string | null {
  if (e.hold === true) return 'structured "hold": true';
  const note = (e.note || "").toLowerCase();
  if (note.includes("held") || note.includes("do not merge") || note.includes("don't merge")) {
    return "note prose (fallback — prefer a structured \"hold\": true field)";
  }
  return null;
}

// ---- CI status --------------------------------------------------------------

type RollupItem = {
  name?: string;
  status?: string; // CheckRun: QUEUED | IN_PROGRESS | COMPLETED
  conclusion?: string; // CheckRun: SUCCESS | FAILURE | NEUTRAL | SKIPPED | CANCELLED | TIMED_OUT | ...
  state?: string; // StatusContext: SUCCESS | PENDING | FAILURE | ERROR
};

function prState(pr: number): { state: string; mergeable: string; rollup: RollupItem[] } {
  const json = gh([
    "pr",
    "view",
    String(pr),
    "--repo",
    REPO,
    "--json",
    "state,mergeable,statusCheckRollup",
  ]);
  const d = JSON.parse(json);
  return { state: d.state, mergeable: d.mergeable, rollup: d.statusCheckRollup || [] };
}

// Classify a rollup into pending / failed / all-green.
function classifyRollup(rollup: RollupItem[]): {
  pending: string[];
  failed: string[];
} {
  const pending: string[] = [];
  const failed: string[] = [];
  for (const it of rollup) {
    const name = it.name || "(unnamed)";
    if (it.status !== undefined) {
      // CheckRun
      if (it.status !== "COMPLETED") {
        pending.push(name);
      } else {
        const c = (it.conclusion || "").toUpperCase();
        if (c === "SUCCESS" || c === "NEUTRAL" || c === "SKIPPED") {
          /* ok */
        } else {
          failed.push(`${name} (${c || "no-conclusion"})`);
        }
      }
    } else if (it.state !== undefined) {
      // StatusContext (legacy commit status)
      const s = (it.state || "").toUpperCase();
      if (s === "PENDING" || s === "") pending.push(name);
      else if (s !== "SUCCESS") failed.push(`${name} (${s})`);
    }
  }
  return { pending, failed };
}

// The single required branch-protection check for this repo — the aggregator
// gate that `needs:` every conditional job and registers LAST. Merging before
// it is present + green can merge a PR whose heavy matrix never ran.
const REQUIRED_GATE = "CI gate";

// THE single source of truth for "may this PR be merged right now?" — used by
// the watch loop, the dry-run preview, and the pre-merge re-read, so the gating
// logic cannot drift between them.
//
// Verdicts:
//   merge   — safe to merge NOW.
//   wait    — not ready yet; keep polling (no checks, pending checks, mergeable
//             still UNKNOWN, or the required gate not yet present/green).
//   block   — terminal NOT-mergeable; do not merge (failing check or conflict).
function mergeDecision(st: {
  state: string;
  mergeable: string;
  rollup: RollupItem[];
}): { verdict: "merge" | "wait" | "block"; reason: string } {
  if (st.state !== "OPEN") return { verdict: "block", reason: `PR is ${st.state}, not OPEN` };

  const { pending, failed } = classifyRollup(st.rollup);
  if (failed.length > 0) return { verdict: "block", reason: `failing checks: ${failed.join(", ")}` };

  if (st.rollup.length === 0) return { verdict: "wait", reason: "no checks registered yet" };
  if (pending.length > 0) {
    return {
      verdict: "wait",
      reason: `${pending.length} check(s) pending: ${pending.slice(0, 4).join(", ")}${pending.length > 4 ? " …" : ""}`,
    };
  }

  // BLOCK 2 fix: the registered checks being all-green is NOT sufficient — the
  // required aggregator gate must be PRESENT and SUCCESS. A partial rollup
  // (a few fast jobs green, the matrix + `CI gate` not yet registered) has
  // zero pending but is not actually done. Require the gate explicitly.
  const gate = st.rollup.find((it) => (it.name || "") === REQUIRED_GATE);
  if (!gate) {
    return { verdict: "wait", reason: `required "${REQUIRED_GATE}" check not present yet (rollup still filling in)` };
  }
  const gateOk =
    gate.status !== undefined
      ? gate.status === "COMPLETED" && (gate.conclusion || "").toUpperCase() === "SUCCESS"
      : (gate.state || "").toUpperCase() === "SUCCESS";
  if (!gateOk) {
    return { verdict: "wait", reason: `"${REQUIRED_GATE}" not green yet (${gate.status || gate.state || "?"}/${gate.conclusion || ""})` };
  }

  // BLOCK 1 fix: gate POSITIVELY on mergeability. GitHub returns UNKNOWN while
  // it lazily computes mergeability (and the poll itself nudges the compute) —
  // UNKNOWN is NOT green. Only MERGEABLE proceeds; CONFLICTING blocks; anything
  // else (UNKNOWN, "") is wait-and-repoll.
  const m = (st.mergeable || "").toUpperCase();
  if (m === "CONFLICTING") return { verdict: "block", reason: "merge conflict (CONFLICTING)" };
  if (m !== "MERGEABLE") return { verdict: "wait", reason: `mergeability ${m || "unknown"} (GitHub still computing)` };

  return { verdict: "merge", reason: "all green + gate passed + mergeable" };
}

// ---- thread flip ------------------------------------------------------------

// A status the board treats as already-closed — flipping one of these would
// clobber a deliberate end-state, so we leave it alone.
const CLOSED_STATUSES = new Set(["done", "dismissed"]);

// On a CONFIRMED merge, flip the bound fray thread to `status: done` and stamp
// today's date. The PR→thread binding is the queue entry's `thread` field — the
// drift this fixes is merge-cascade merging a PR but leaving its thread at its
// old status, so the board says "PR open" for a thread whose PR merged days ago.
//
// Surgical: parse the leading `---` frontmatter block and replace ONLY the
// `status:`/`last_update:`/`status_text:` values + append one line to the
// `## Status` section; never regenerate the body. Idempotent + safe: missing
// file or already-closed status logs and skips, never errors, never clobbers.
function flipThreadDone(thread: string, pr: number, dryRun: boolean): void {
  const path = join(REPO_ROOT, ".fray", `${thread}.md`);
  if (!existsSync(path)) {
    console.log(`    thread "${thread}": file not found (${path}) — skipping flip.`);
    return;
  }
  const raw = readFileSync(path, "utf8");

  // Frontmatter is a leading `---\n…\n---` block. Bail (don't corrupt) if absent.
  const fmMatch = raw.match(/^---\n([\s\S]*?)\n---/);
  if (!fmMatch) {
    console.log(`    thread "${thread}": no frontmatter block — skipping flip.`);
    return;
  }
  const fm = fmMatch[1];
  const statusMatch = fm.match(/^status:[ \t]*(\S+)/m);
  const current = statusMatch ? statusMatch[1].toLowerCase() : "";
  if (CLOSED_STATUSES.has(current)) {
    console.log(`    thread "${thread}": already status: ${current} — skipping flip (idempotent).`);
    return;
  }

  const today = new Date().toISOString().slice(0, 10);
  const mergeLine = `MERGED via merge-cascade: PR #${pr} (${today}).`;

  if (dryRun) {
    console.log(`    thread "${thread}": would flip status: ${current || "?"} → done (${mergeLine})`);
    return;
  }

  // Frontmatter edits, confined to the matched block so the body is untouched.
  let newFm = fm;
  newFm = statusMatch
    ? newFm.replace(/^status:[ \t]*\S+.*$/m, "status: done")
    : `status: done\n${newFm}`;
  newFm = /^last_update:/m.test(newFm)
    ? newFm.replace(/^last_update:[ \t]*.*$/m, `last_update: ${today}`)
    : `${newFm}\nlast_update: ${today}`;
  if (/^status_text:/m.test(newFm)) {
    newFm = newFm.replace(/^status_text:[ \t]*.*$/m, `status_text: "${mergeLine}"`);
  }

  let body = raw.slice(fmMatch[0].length);
  // Append the one merge line under the `## Status` heading if present; else
  // tack a minimal `## Status` block on the end. One line only — no pile-up.
  if (/^## Status\b/m.test(body)) {
    body = body.replace(/^(## Status\b[^\n]*\n)/m, `$1${mergeLine}\n`);
  } else {
    body = `${body.replace(/\s*$/, "")}\n\n## Status\n${mergeLine}\n`;
  }

  writeFileSync(path, `---\n${newFm}\n---${body}`);
  console.log(`    thread "${thread}": flipped status: ${current || "?"} → done.`);
}

// ---- main -------------------------------------------------------------------

async function watchUntilTerminal(
  pr: number,
  maxMinutes: number,
): Promise<{ ok: boolean; reason: string }> {
  const deadline = Date.now() + maxMinutes * 60_000;
  let delay = 15_000; // start at 15s
  const maxDelay = 120_000; // cap at 2min
  for (;;) {
    const st = prState(pr);
    const d = mergeDecision(st);
    if (d.verdict === "merge") return { ok: true, reason: d.reason };
    if (d.verdict === "block") return { ok: false, reason: d.reason };
    // wait: keep polling (the poll also nudges GitHub to compute mergeability).
    console.log(`    … ${d.reason}`);
    if (Date.now() > deadline) return { ok: false, reason: `timed out after ${maxMinutes}min (last: ${d.reason})` };
    await sleep(delay);
    delay = Math.min(delay * 1.5, maxDelay);
  }
}

async function main() {
  const { dryRun, queue, sharedTree, pollMaxMinutes } = parseArgs(process.argv.slice(2));
  const entries = readQueue(queue);
  console.log(`merge-cascade: ${entries.length} queued PR(s)  (${dryRun ? "DRY-RUN" : "LIVE"})  queue=${queue}\n`);

  let merged = 0;
  let skipped = 0;
  for (const e of entries) {
    const tag = `PR #${e.pr}${e.branch ? ` (${e.branch})` : ""}`;
    const held = holdReason(e);
    if (held) {
      console.log(`  ${tag}: HELD (${held}) — skipping.${e.note ? ` note: ${e.note}` : ""}`);
      skipped++;
      continue;
    }

    let st: { state: string; mergeable: string; rollup: RollupItem[] };
    try {
      st = prState(e.pr);
    } catch (err) {
      console.log(`  ${tag}: could not read PR state — skipping. (${(err as Error).message.split("\n")[0]})`);
      skipped++;
      continue;
    }
    if (st.state !== "OPEN") {
      console.log(`  ${tag}: already ${st.state} — skipping.`);
      skipped++;
      continue;
    }

    if (dryRun) {
      const d = mergeDecision(st);
      const label = d.verdict === "merge" ? "WOULD MERGE" : d.verdict === "block" ? "BLOCKED" : "WAIT";
      console.log(`  ${tag}: ${label} — ${d.reason}`);
      // Preview the thread flip a real merge would perform.
      if (d.verdict === "merge" && e.thread) flipThreadDone(e.thread, e.pr, true);
      continue;
    }

    // LIVE: watch then merge.
    console.log(`  ${tag}: watching CI…`);
    const res = await watchUntilTerminal(e.pr, pollMaxMinutes);
    if (!res.ok) {
      console.log(`  ${tag}: NOT merged — ${res.reason}.`);
      skipped++;
      continue;
    }

    // NIT fix: re-read state immediately before merging — close the staleness
    // window between the final poll and the merge (a failure or conflict could
    // land in between). If it's no longer a clean merge, skip.
    const fresh = mergeDecision(prState(e.pr));
    if (fresh.verdict !== "merge") {
      console.log(`  ${tag}: NOT merged — state changed before merge (${fresh.reason}).`);
      skipped++;
      continue;
    }

    console.log(`  ${tag}: green — squash-merging…`);
    gh(["pr", "merge", String(e.pr), "--repo", REPO, "--squash", "--admin"]);
    merged++;

    // CONFIRMED merge — flip the bound fray thread to done (the queue entry's
    // `thread` field is the binding). Fixes board-drift where a merged PR's
    // thread was left at its old status. Best-effort: a flip hiccup must not
    // mask the successful merge.
    if (e.thread) {
      try {
        flipThreadDone(e.thread, e.pr, false);
      } catch (err) {
        console.log(`  ${tag}: merged, but thread flip failed: ${(err as Error).message.split("\n")[0]}`);
      }
    }

    // Fast-forward the shared tree (vendor/aube edits ride the normal pull).
    try {
      git(["-C", sharedTree, "pull", "--ff-only"]);
      console.log(`  ${tag}: merged + shared tree fast-forwarded.`);
    } catch (err) {
      console.log(`  ${tag}: merged, but shared-tree update failed: ${(err as Error).message.split("\n")[0]}`);
    }
  }

  console.log(`\nmerge-cascade: merged ${merged}, skipped ${skipped}.`);
}

main();
