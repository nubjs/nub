#!/usr/bin/env node
// ci-watch — block until a GitHub Actions run (or a PR's check rollup) is TRULY
// terminal, then exit with a status the orchestrator can trust. The robust
// replacement for `gh run watch`/`gh pr checks --watch`, which exit EARLY when
// armed right after a push (the run is QUEUED with no jobs registered yet, so gh
// sees "nothing in progress" and returns success) and also surface a non-zero
// exit on a TRANSIENT API error (a 401/5xx mid-watch reads as "the run failed").
//
// Runs under BOTH plain Node (type-stripping) and nub:
//   node scripts/ci-watch.ts --run <run-id>  [--repo o/r] [--timeout <min>]
//   nub  scripts/ci-watch.ts --pr  <number>  [--repo o/r] [--timeout <min>]
//   nub  scripts/ci-watch.ts --pr  <number>  --chunk[=<min>]   # foreground sub-agent loop
//
// Erasable TypeScript only (no enums/namespaces/parameter-properties) so plain
// modern `node` runs it with no build step — same constraint as the other
// scripts/*.ts.
//
// ORCHESTRATOR tooling — designed to run as a detached `run_in_background` task
// that re-invokes the orchestrator on exit. The final stdout line is a single
// self-describing summary (CI-WATCH …: SUCCESS/FAILURE/TIMEOUT/ERROR) so the
// outcome is readable from the tail.
//
// Exit codes (the contract the orchestrator gates on):
//   0  completed AND all green
//   1  a check/job concluded FAILURE/CANCELLED/TIMED_OUT/STARTUP_FAILURE
//   2  still pending after --timeout wall-clock (or --chunk cap)
//   3  usage / target-unresolvable / unrecoverable error
//
// Core fixes over the raw watchers:
//   * WAIT-FOR-EXISTENCE: a not-found / no-jobs-yet target is "keep polling",
//     never "done". This is the premature-exit fix.
//   * AUTHORITATIVE terminal check: done only when status == "completed" (run) /
//     every rollup item terminal (pr) — never inferred from "nothing running".
//   * FAIL-FAST: exit non-zero the instant ANY job/check is a failure, without
//     waiting for the rest (mirrors the AGENTS.md fail-fast rule).
//   * TRANSIENT-ERROR TOLERANCE: a gh/API hiccup is retried with backoff, not
//     treated as a run failure.

import { execFileSync } from "node:child_process";

// ---- args -------------------------------------------------------------------

type Mode = "run" | "pr";
type Opts = {
  mode: Mode;
  target: string;
  repo: string | null;
  timeoutMin: number;
  // --chunk: per-invocation wall-clock cap for sub-agent foreground loops.
  // When set and the cap expires, exits 2 with a RERUN message instead of the
  // generic TIMEOUT message, signalling the agent to re-run the same command.
  chunkMin: number | null;
};

const HELP = `ci-watch — block until a CI run / PR check rollup is truly terminal

Usage:
  node scripts/ci-watch.ts --run <run-id> [flags]
  nub  scripts/ci-watch.ts --pr  <number> [flags]

Modes (exactly one):
  --run <run-id>     Watch a workflow run (gh run view).
  --pr  <number>     Watch a PR's check rollup (gh pr view).

Flags:
  --repo <owner/repo>  Repository (default: current repo from gh).
  --timeout <minutes>  Max wall-clock before giving up as pending (default 45).
  --chunk[=<minutes>]  Sub-agent foreground-loop mode (default 9 min). Caps each
                       invocation under the 10-min Bash tool timeout; exits 2 with
                       a RERUN message when the cap expires so the agent can loop.
                       While exit 2 (pending): re-run the SAME command to continue.
  -h, --help           Show this help.

Exit codes: 0 success · 1 a check failed · 2 timed out / chunk pending · 3 usage/error.

Designed to run as a detached run_in_background task; the final stdout line is a
single CI-WATCH summary the orchestrator reads from the tail.

Sub-agent foreground-loop pattern (use when a sub-agent must gate on its own CI):
  # Bash tool: foreground (NOT run_in_background), timeout: 570000
  nub scripts/ci-watch.ts --pr <N> --chunk
  # exit 0 → green   exit 1 → red, fix + re-push   exit 2 → pending, re-run   exit 3 → error
  # While exit code is 2, call the same command again. Each chunk completes within
  # the Bash timeout cap so no call is ever killed mid-watch.`;

function die(msg: string): never {
  process.stderr.write(`ci-watch: ${msg}\n`);
  process.exit(3);
}

const CHUNK_DEFAULT_MIN = 9;

function parseArgs(argv: string[]): Opts {
  let mode: Mode | null = null;
  let target = "";
  let repo: string | null = null;
  let timeoutMin = 45;
  let chunkMin: number | null = null;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "-h" || a === "--help") {
      process.stdout.write(HELP + "\n");
      process.exit(0);
    } else if (a === "--run") {
      if (mode) die("--run and --pr are mutually exclusive");
      mode = "run";
      target = argv[++i] ?? die("--run requires a run-id");
    } else if (a === "--pr") {
      if (mode) die("--run and --pr are mutually exclusive");
      mode = "pr";
      target = argv[++i] ?? die("--pr requires a number");
    } else if (a === "--repo") {
      repo = argv[++i] ?? die("--repo requires owner/repo");
    } else if (a === "--timeout") {
      timeoutMin = Number(argv[++i]);
      if (!Number.isFinite(timeoutMin) || timeoutMin <= 0) die("--timeout must be a positive number of minutes");
    } else if (a === "--chunk") {
      // bare --chunk: use the next arg as the value only if it looks like a number
      const next = argv[i + 1];
      if (next !== undefined && /^\d+(\.\d+)?$/.test(next)) {
        chunkMin = Number(next);
        i++;
      } else {
        chunkMin = CHUNK_DEFAULT_MIN;
      }
      if (!Number.isFinite(chunkMin) || chunkMin <= 0) die("--chunk requires a positive number of minutes");
    } else if (a.startsWith("--chunk=")) {
      // --chunk=<N> form
      chunkMin = Number(a.slice("--chunk=".length));
      if (!Number.isFinite(chunkMin) || chunkMin <= 0) die("--chunk= requires a positive number of minutes");
    } else {
      die(`unknown arg: ${a} (try --help)`);
    }
  }
  if (!mode) die("specify --run <run-id> or --pr <number>");
  return { mode, target, repo, timeoutMin, chunkMin };
}

// ---- gh plumbing ------------------------------------------------------------

// A gh call that may transiently fail (network blip, 401 token refresh, 5xx).
// Returns the stdout on success, or null on failure — the caller decides whether
// a null is "keep polling" (transient / not-yet-existing) or fatal. We never let
// a single failed gh call abort the watch.
function ghTry(args: string[]): string | null {
  try {
    return execFileSync("gh", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }).trim();
  } catch {
    return null;
  }
}

function repoArgs(repo: string | null): string[] {
  return repo ? ["--repo", repo] : [];
}

// gh uses its stored auth token implicitly for every call above, which gives the
// authenticated (high) rate limit for free. We surface a one-time warning if no
// token is resolvable and stretch the backoff so an unauthenticated fallback
// stays well under the lower anonymous limit.
function hasAuthToken(): boolean {
  const t = ghTry(["auth", "token"]);
  return t !== null && t.length > 0;
}

// ---- terminal-state classification ------------------------------------------

const FAILURE_CONCLUSIONS = new Set(["FAILURE", "CANCELLED", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED", "STALE"]);
const OK_CONCLUSIONS = new Set(["SUCCESS", "NEUTRAL", "SKIPPED"]);

type Verdict = { kind: "pending"; reason: string } | { kind: "success"; reason: string } | { kind: "failure"; reason: string };

// A run is done only when its top-level status is "completed". Until then —
// including QUEUED with zero jobs (the premature-exit case) — it is pending.
// Fail-fast: a failed job short-circuits to failure without waiting for siblings.
function classifyRun(json: string): Verdict {
  let d: { status?: string; conclusion?: string; jobs?: { name?: string; status?: string; conclusion?: string }[] };
  try {
    d = JSON.parse(json);
  } catch {
    return { kind: "pending", reason: "unparseable run JSON (transient)" };
  }
  const jobs = d.jobs || [];
  for (const j of jobs) {
    if ((j.status || "").toLowerCase() === "completed") {
      const c = (j.conclusion || "").toUpperCase();
      if (FAILURE_CONCLUSIONS.has(c)) return { kind: "failure", reason: `job "${j.name || "?"}" → ${c}` };
    }
  }
  if ((d.status || "").toLowerCase() !== "completed") {
    const running = jobs.filter((j) => (j.status || "").toLowerCase() !== "completed").length;
    return { kind: "pending", reason: jobs.length === 0 ? "no jobs registered yet (queued)" : `${running}/${jobs.length} job(s) still running` };
  }
  // status==completed: trust the run-level conclusion.
  const c = (d.conclusion || "").toUpperCase();
  if (OK_CONCLUSIONS.has(c)) return { kind: "success", reason: `${jobs.length} job(s) green (${c})` };
  return { kind: "failure", reason: `run concluded ${c || "no-conclusion"}` };
}

type RollupItem = { name?: string; status?: string; conclusion?: string; state?: string };

// A PR rollup is done only when EVERY item is terminal. An empty rollup is "no
// checks registered yet" — pending, not done (the PR-side premature-exit case).
// Fail-fast on the first failing item.
function classifyPr(json: string): Verdict {
  let d: { statusCheckRollup?: RollupItem[] };
  try {
    d = JSON.parse(json);
  } catch {
    return { kind: "pending", reason: "unparseable PR JSON (transient)" };
  }
  const rollup = d.statusCheckRollup || [];
  if (rollup.length === 0) return { kind: "pending", reason: "no checks registered yet" };
  const pending: string[] = [];
  for (const it of rollup) {
    const name = it.name || "(unnamed)";
    if (it.status !== undefined) {
      // CheckRun: QUEUED | IN_PROGRESS | COMPLETED
      if ((it.status || "").toUpperCase() !== "COMPLETED") {
        pending.push(name);
      } else {
        const c = (it.conclusion || "").toUpperCase();
        if (FAILURE_CONCLUSIONS.has(c) || (!OK_CONCLUSIONS.has(c) && c !== "")) return { kind: "failure", reason: `check "${name}" → ${c || "no-conclusion"}` };
      }
    } else if (it.state !== undefined) {
      // StatusContext (legacy commit status): SUCCESS | PENDING | FAILURE | ERROR
      const s = (it.state || "").toUpperCase();
      if (s === "PENDING" || s === "") pending.push(name);
      else if (s !== "SUCCESS") return { kind: "failure", reason: `status "${name}" → ${s}` };
    }
  }
  if (pending.length > 0) return { kind: "pending", reason: `${pending.length} check(s) pending: ${pending.slice(0, 4).join(", ")}${pending.length > 4 ? " …" : ""}` };
  return { kind: "success", reason: `${rollup.length} check(s) green` };
}

// ---- poll loop --------------------------------------------------------------

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

// Exponential backoff with jitter: 10s → 20s → 40s → cap. authenticated caps at
// 60s; unauthenticated stretches to 90s to stay under the anonymous rate limit.
function nextDelay(prev: number, cap: number): number {
  const grown = Math.min(prev * 2, cap);
  const jitter = grown * 0.2 * (Math.random() - 0.5); // ±10%
  return Math.round(grown + jitter);
}

async function watch(opts: Opts): Promise<{ code: number; summary: string }> {
  const label = opts.mode === "run" ? `run ${opts.target}` : `pr ${opts.target}`;
  const authed = hasAuthToken();
  if (!authed) process.stderr.write("ci-watch: no gh auth token resolvable — falling back to slower polling to respect the anonymous rate limit\n");

  const viewArgs =
    opts.mode === "run"
      ? ["run", "view", opts.target, ...repoArgs(opts.repo), "--json", "status,conclusion,jobs"]
      : ["pr", "view", opts.target, ...repoArgs(opts.repo), "--json", "statusCheckRollup,mergeable,mergeStateStatus"];
  const classify = opts.mode === "run" ? classifyRun : classifyPr;

  const cap = authed ? 60_000 : 90_000;
  const deadline = Date.now() + opts.timeoutMin * 60_000;
  // chunk deadline: per-invocation cap for sub-agent foreground loops. When hit,
  // exits 2 with a RERUN message (distinct from the generic TIMEOUT message) so
  // the agent knows to call the same command again rather than give up.
  const chunkDeadline = opts.chunkMin !== null ? Date.now() + opts.chunkMin * 60_000 : null;
  let delay = 10_000;
  let consecutiveErrors = 0;

  for (;;) {
    const out = ghTry(viewArgs);
    if (out === null) {
      // gh call failed: target may not exist YET (just pushed) or a transient
      // API error. Either way → keep polling. Never treat as completion.
      consecutiveErrors++;
      // A long run of hard failures (e.g. genuinely unresolvable target / auth
      // wholly broken) is fatal rather than spinning to the timeout.
      if (consecutiveErrors >= 12) {
        return { code: 3, summary: `CI-WATCH ${label}: ERROR — gh unreachable / target unresolvable after ${consecutiveErrors} attempts` };
      }
      process.stderr.write(`    … gh call failed (attempt ${consecutiveErrors}); target not visible yet or transient — retrying\n`);
    } else {
      consecutiveErrors = 0;
      const v = classify(out);
      if (v.kind === "success") return { code: 0, summary: `CI-WATCH ${label}: SUCCESS (${v.reason})` };
      if (v.kind === "failure") {
        const url = ghTry(opts.mode === "run" ? ["run", "view", opts.target, ...repoArgs(opts.repo), "--json", "url", "--jq", ".url"] : ["pr", "view", opts.target, ...repoArgs(opts.repo), "--json", "url", "--jq", ".url"]);
        return { code: 1, summary: `CI-WATCH ${label}: FAILURE — ${v.reason}${url ? ` (${url})` : ""}` };
      }
      process.stderr.write(`    … ${v.reason}\n`);
    }

    const now = Date.now();
    // Chunk cap takes priority: exit with a RERUN message so the agent loops.
    if (chunkDeadline !== null && now > chunkDeadline) {
      return { code: 2, summary: `CI-WATCH ${label}: PENDING after ${opts.chunkMin}m — RERUN the SAME command to continue` };
    }
    if (now > deadline) return { code: 2, summary: `CI-WATCH ${label}: TIMEOUT — still pending after ${opts.timeoutMin}min` };
    await sleep(delay);
    delay = nextDelay(delay, cap);
  }
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));
  const { code, summary } = await watch(opts);
  // The final stdout line IS the handoff — orchestrator reads it from the tail.
  console.log(summary);
  process.exit(code);
}

main();
