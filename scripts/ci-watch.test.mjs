// Tests for scripts/ci-watch.ts — the anti-hang logic that closed the #327 drop.
// Pure classifier/resolver only (no gh, no real clock). Run: node --test scripts/ci-watch.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyRollup, verdictForBuckets, classifyPr, classifyRun, signatureOf, resolvePendingExit } from "./ci-watch.ts";

const NP_MS = 8 * 60_000;
// Config with a far-future overall deadline so tests exercise the no-progress
// path in isolation unless they deliberately set `deadline` in the past.
const cfg = (over = {}) => ({ deadline: Date.now() + 60 * 60_000, chunkDeadline: null, noProgressMs: NP_MS, chunkMin: null, timeoutMin: 45, ...over });

const check = (name, conclusion) => ({ __typename: "CheckRun", name, status: "COMPLETED", conclusion, startedAt: "t", completedAt: "t" });
const ghost = () => ({ __typename: "CheckRun", status: "IN_PROGRESS" }); // nameless, never-terminating (the #327 shape)
const the327 = () => [check("CI gate", "SUCCESS"), ...Array.from({ length: 50 }, (_, i) => check(`check ${i}`, "SUCCESS")), ghost()];

test("#327 shape: 51 green named checks + 1 nameless ghost → ghostsOnly, not success/failure", () => {
  const v = verdictForBuckets(classifyRollup(the327(), new Set()), false);
  assert.equal(v.kind, "pending");
  assert.equal(v.ghostsOnly, true);
  assert.equal(v.greenNamed, 51);
  assert.deepEqual(v.ghosts, ["(unnamed)"]);
});

test("#327 ghost is given up on after the no-progress window → exit 4 STUCK-but-safe, promptly", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: the327() }), new Set());
  const timing = { lastSig: signatureOf(v), lastProgressAt: 0 }; // stalled long ago
  const exit = resolvePendingExit(v, "pr 327", NP_MS + 1, timing, cfg());
  assert.ok(exit, "must exit, not hang");
  assert.equal(exit.code, 4);
  assert.match(exit.summary, /STUCK/);
  assert.match(exit.summary, /safe to --admin merge/);
  assert.match(exit.summary, /GREEN \(51\)/);
});

test("ghost has NOT yet stalled long enough → keep waiting (null), never premature-exit-4", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: the327() }), new Set());
  const now = 1_000_000;
  const timing = { lastSig: signatureOf(v), lastProgressAt: now - (NP_MS - 5_000) }; // 5s short of window
  assert.equal(resolvePendingExit(v, "pr 327", now, timing, cfg()), null);
});

test("a REAL named pending check is NEVER green-lit early — waits, then exits 2 (not safe) at the deadline", () => {
  const rollup = [check("fast", "SUCCESS"), { __typename: "CheckRun", name: "CI gate", status: "IN_PROGRESS", startedAt: "t" }];
  const v = classifyPr(JSON.stringify({ statusCheckRollup: rollup }), new Set());
  assert.equal(v.kind, "pending");
  assert.equal(v.ghostsOnly, false, "a named pending check is real, not a ghost");
  // No-progress window must NOT fire for a real pending check (would green-light a running check).
  const timing = { lastSig: signatureOf(v), lastProgressAt: 0 };
  assert.equal(resolvePendingExit(v, "pr 9", NP_MS + 1, timing, cfg()), null);
  // Only the overall deadline stops it — and reports NOT green (exit 2).
  const exit = resolvePendingExit(v, "pr 9", 10, timing, cfg({ deadline: 0 }));
  assert.equal(exit.code, 2);
  assert.match(exit.summary, /TIMEOUT/);
  assert.match(exit.summary, /NOT green/);
  assert.match(exit.summary, /CI gate/);
});

test("fail-fast: a terminal failing check short-circuits to failure", () => {
  const v = verdictForBuckets(classifyRollup([check("ok", "SUCCESS"), check("build", "FAILURE"), ghost()], new Set()), false);
  assert.equal(v.kind, "failure");
  assert.match(v.reason, /build/);
});

test("--required mode: required gate green + nameless ghost pending → immediate SUCCESS (ghost ignored)", () => {
  const rollup = [check("CI gate", "SUCCESS"), check("some job", "SUCCESS"), ghost()];
  const v = classifyPr(JSON.stringify({ statusCheckRollup: rollup }), new Set(["CI gate"]));
  assert.equal(v.kind, "success");
});

test("--required mode: required gate still pending → pending, and exits 2 (not safe) at the deadline", () => {
  const rollup = [check("some job", "SUCCESS"), { __typename: "CheckRun", name: "CI gate", status: "IN_PROGRESS", startedAt: "t" }];
  const v = classifyPr(JSON.stringify({ statusCheckRollup: rollup }), new Set(["CI gate"]));
  assert.equal(v.kind, "pending");
  const exit = resolvePendingExit(v, "pr 9", 10, { lastSig: signatureOf(v), lastProgressAt: 0 }, cfg({ deadline: 0 }));
  assert.equal(exit.code, 2);
  assert.match(exit.summary, /CI gate/);
});

test("--required mode: a required check entirely ABSENT from the rollup blocks (partial-rollup guard)", () => {
  const b = classifyRollup([check("some job", "SUCCESS")], new Set(["CI gate"]));
  assert.deepEqual(b.requiredMissing, ["CI gate"]);
  assert.equal(verdictForBuckets(b, true).kind, "pending");
});

test("StatusContext is named by its `context` field (Vercel), not misread as unnamed", () => {
  const rollup = [check("job", "SUCCESS"), { __typename: "StatusContext", context: "Vercel", state: "PENDING" }];
  const b = classifyRollup(rollup, new Set());
  assert.deepEqual(b.realPending, ["Vercel"], "a named legacy status is a real pending check, not a ghost");
  assert.deepEqual(b.ghosts, []);
});

test("empty rollup → pending 'no checks registered yet' (wait-for-existence preserved)", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [] }), new Set());
  assert.equal(v.kind, "pending");
  assert.match(v.reason, /no checks registered yet/);
});

test("all checks terminal + green, no ghost → clean SUCCESS (exit 0 path)", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [check("CI gate", "SUCCESS"), check("b", "NEUTRAL"), check("c", "SKIPPED")] }), new Set());
  assert.equal(v.kind, "success");
});

test("chunk cap: exits 2 with a RERUN message when the chunk deadline passes (loop signal preserved)", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [{ __typename: "CheckRun", name: "CI gate", status: "IN_PROGRESS", startedAt: "t" }] }), new Set());
  const exit = resolvePendingExit(v, "pr 9", 100, { lastSig: signatureOf(v), lastProgressAt: 100 }, cfg({ chunkDeadline: 0, chunkMin: 9 }));
  assert.equal(exit.code, 2);
  assert.match(exit.summary, /RERUN/);
});

// --- review-hardening: branch-protection-faithful --required + deadline-during-gh-streak ---

test("--required mode: a FAILING non-required check does NOT block (branch-protection faithful)", () => {
  const rollup = [check("CI gate", "SUCCESS"), check("optional lint", "FAILURE"), ghost()];
  const v = classifyPr(JSON.stringify({ statusCheckRollup: rollup }), new Set(["CI gate"]));
  assert.equal(v.kind, "success", "an optional red check must not refuse to merge a mergeable PR");
});

test("--required mode: a FAILING required check DOES block → exit 1 failure", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [check("CI gate", "FAILURE"), check("optional", "SUCCESS")] }), new Set(["CI gate"]));
  assert.equal(v.kind, "failure");
  assert.match(v.reason, /CI gate/);
});

test("--required mode: duplicate required name (first green, second pending) blocks — every occurrence must be green", () => {
  const rollup = [check("CI gate", "SUCCESS"), { __typename: "CheckRun", name: "CI gate", status: "IN_PROGRESS", startedAt: "t" }];
  const b = classifyRollup(rollup, new Set(["CI gate"]));
  assert.deepEqual(b.requiredMissing, ["CI gate"], "a same-named still-running required check must not be green-lit");
  assert.equal(verdictForBuckets(b, true).kind, "pending");
});

test("deadline fires during a gh-failure streak: null verdict past the overall deadline → exit 2, does not hang", () => {
  const exit = resolvePendingExit(null, "pr 9", 10, { lastSig: null, lastProgressAt: 0 }, cfg({ deadline: 0 }));
  assert.ok(exit, "a stalled poll must not wait forever");
  assert.equal(exit.code, 2);
  assert.match(exit.summary, /no check status resolved/);
});

test("deadline fires during a gh-failure streak: chunk cap on a null verdict → exit 2 RERUN", () => {
  const exit = resolvePendingExit(null, "pr 9", 10, { lastSig: null, lastProgressAt: 0 }, cfg({ chunkDeadline: 0, chunkMin: 9 }));
  assert.equal(exit.code, 2);
  assert.match(exit.summary, /RERUN/);
});

// ---- opt-in PR CI: a SKIPPED check verifies nothing -------------------------
// Every PR-gated workflow now guards its jobs on the `ci` label, so a pull request
// nobody requested CI for produces skipped checks rather than green ones. These pin
// the carve-out that keeps that from reading as a pass.

test("all checks SKIPPED → pending 'no check has run', never success", () => {
  const v = verdictForBuckets(classifyRollup([check("CI gate", "SKIPPED"), check("Check", "SKIPPED")], new Set()), false);
  assert.equal(v.kind, "pending", "an unrequested PR must never classify green");
  assert.match(v.reason, /no check has run/);
  assert.match(v.reason, /--add-label ci/, "the reason must carry the command that fixes it");
  assert.equal(v.greenNamed, 0);
});

test("all checks SKIPPED alongside a ghost → NOT ghostsOnly (never 'safe to --admin merge')", () => {
  const v = verdictForBuckets(classifyRollup([check("CI gate", "SKIPPED"), ghost()], new Set()), false);
  assert.equal(v.kind, "pending");
  assert.equal(v.ghostsOnly, false, "exit 4 means every real check is green — a skipped rollup is not that");
});

test("--required mode: a SKIPPED required gate blocks the merge, it does not satisfy it", () => {
  const v = verdictForBuckets(classifyRollup([check("CI gate", "SKIPPED")], new Set(["CI gate"])), true);
  assert.equal(v.kind, "pending");
  assert.deepEqual(v.realPending, ["CI gate"]);
});

test("path-gated skips alongside a real pass still SUCCEED (docs-only PR that DID opt in)", () => {
  const rollup = [check("CI gate", "SUCCESS"), check("Check", "SKIPPED"), check("Clippy", "SKIPPED")];
  assert.equal(verdictForBuckets(classifyRollup(rollup, new Set()), false).kind, "success");
  assert.equal(verdictForBuckets(classifyRollup(rollup, new Set(["CI gate"])), true).kind, "success");
});

test("a run whose every job was gated off (conclusion SKIPPED) is not green", () => {
  const v = classifyRun(JSON.stringify({ status: "completed", conclusion: "skipped", jobs: [] }));
  assert.equal(v.kind, "failure");
  assert.match(v.reason, /no job ran/);
});

// ---- opt-in PR CI: third-party app checks cannot stand in for the gate --------
// Measured on the probe pull request that verified the opt-in change: a PR nobody
// requested CI for still carries Vercel + review-bot checks, and they go green on
// their own. Before this rule those three greens were a clean `success`.

test("app checks green but no `CI gate` → pending, never success", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [
    check("Vercel Preview Comments", "SUCCESS"), check("Vercel", "SUCCESS"), check("pullfrog", "SUCCESS"),
  ] }), new Set());
  assert.equal(v.kind, "pending");
  assert.match(v.reason, /CI gate` is not green/);
  assert.match(v.reason, /--add-label ci/);
});

test("app checks green + a green `CI gate` → success", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [
    check("Vercel", "SUCCESS"), check("pullfrog", "SUCCESS"), check("CI gate", "SUCCESS"),
  ] }), new Set());
  assert.equal(v.kind, "success");
});

test("app checks green + a SKIPPED `CI gate` → pending (the unrequested-PR shape)", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [
    check("Vercel", "SUCCESS"), check("CI gate", "SKIPPED"),
  ] }), new Set());
  assert.equal(v.kind, "pending");
});

test("no gate + a ghost is NOT ghostsOnly — exit 4 must never fire without the gate", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [check("Vercel", "SUCCESS"), ghost()] }), new Set());
  assert.equal(v.kind, "pending");
  assert.equal(v.ghostsOnly, false, "exit 4 tells the caller it is safe to --admin merge");
});

test("an explicit --required set still decides on its own (no implicit gate on top)", () => {
  const v = classifyPr(JSON.stringify({ statusCheckRollup: [check("lat check", "SUCCESS")] }), new Set(["lat check"]));
  assert.equal(v.kind, "success");
});

