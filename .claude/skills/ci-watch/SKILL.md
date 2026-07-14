---
name: ci-watch
description: >-
  Watch GitHub Actions CI correctly with the gh CLI — block until a run / PR
  check rollup is TRULY terminal, then trust the exit code. Invoke (via the Skill
  tool) whenever you need to wait on CI after a push, tag, or PR-open and act on
  the result (merge-on-green, release-on-green, fail-fast on red). Encodes the
  premature-exit pitfall (raw `gh run watch` / `gh pr checks --watch` exit 0
  while the run is still QUEUED with no jobs registered, and exit non-zero on a
  transient API blip) and the blessed fix: `scripts/ci-watch.ts`, which waits for
  the target to EXIST, polls authoritative terminal status, fails fast on the
  first failing check, and exits with a status the orchestrator can trust. Run it
  as a detached run_in_background task.
metadata:
  internal: true
---

# Watching CI with the GitHub CLI

## The pitfall: raw watchers exit early

`gh run watch <id> --exit-status` and `gh pr checks <pr> --watch` are NOT safe to arm right after a `git push` / tag / PR-open:

- **Premature exit while QUEUED.** Armed immediately after a push, the run has no jobs registered yet. gh sees "nothing in progress" and returns **exit 0** — even though the run is still queued/in_progress. (Observed on the v0.1.11 release: the watcher exited 0 while the Test gate job was still running.)
- **Transient errors read as failure.** A mid-watch `HTTP 401: Bad credentials` (token refresh) or a 5xx makes the watcher exit **non-zero**, indistinguishable from a real CI failure. (Also observed on v0.1.11.)
- **No native fix.** There is no `gh run watch` flag that waits-for-existence or tolerates transient errors (`--interval` only tunes the poll cadence). The script below is the fix.

## The rule

**Never trust a raw watcher's exit code alone. Always re-verify terminal status** with `gh run view <id> --json status,conclusion` (a run is done only when `status == "completed"`) or `gh pr view <pr> --json statusCheckRollup` (done only when every item is terminal). And **always fail-fast** — act on the first failing check, never wait for all checks to finish (AGENTS.md fail-fast discipline).

The blessed tool bakes all of this in — prefer it over a hand-rolled watcher.

## The blessed tool: `scripts/ci-watch.ts`

Blocks until the target is truly terminal, then exits with a trustworthy status. Dogfoods nub; runs under plain Node too.

```bash
nub  scripts/ci-watch.ts --run <run-id> [--repo o/r] [--timeout <min>]
node scripts/ci-watch.ts --pr  <number> [--repo o/r] [--timeout <min>]
```

- `--run <run-id>` — watch a workflow run (polls `gh run view --json status,conclusion,jobs`).
- `--pr <number>` — watch a PR's check rollup (polls `gh pr view --json statusCheckRollup,…`).
- `--repo <owner/repo>` — defaults to the current repo.
- `--timeout <minutes>` — wall-clock cap before giving up as pending (default 45).
- `--required <names>` — comma-separated branch-protection check names to gate on (e.g. `--required "CI gate"`). Success fires the instant every required check is green; a ghost or a non-required check — pending *or* failed — never blocks, matching branch-protection semantics. The precise, hang-proof gate for a merge watcher — prefer it when you know the required check name.
- `--no-progress <minutes>` — how long an unchanged incomplete set (all required/named checks already green, only a ghost left) may sit before exiting 4 STUCK-but-safe (default 8).

What it fixes: **waits for the target to EXIST** (a not-found / no-jobs-yet target is "keep polling," never "done"); polls **authoritative** terminal state (`status == "completed"` / every required/named rollup item terminal+green); **fails fast** on the first FAILURE/CANCELLED/TIMED_OUT/STARTUP_FAILURE; **never hangs on a ghost** (see below); **tolerates transient** gh/API errors (retried with backoff, not treated as a run failure); uses gh's stored token implicitly (high rate limit) with exponential jittered backoff (10s → cap 60s, 90s if unauthenticated).

### The #327 ghost — why a strict "all checks terminal" gate hangs

GitHub occasionally registers a check-run that never reports a status: it stays PENDING, nameless, forever. A watcher that waits for *every* rollup item to be terminal then blocks indefinitely even though every real check is green — PR [#327](https://github.com/nubjs/nub/issues/327) sat reviewed-and-green for hours this way, its merge watcher parked on one nameless ghost. The fix: a nameless / never-terminating non-required check does not block a green verdict. Once every named check is green and the incomplete set has been unchanged for `--no-progress` minutes, the watcher exits **4 (STUCK-but-safe)** with an actionable summary — the caller `--admin` merges instead of hanging. A *named* pending check is never treated as a ghost, so a real in-flight check is never green-lit early.

### Exit-code contract

| code | meaning |
| ---- | ------- |
| 0 | completed AND all green |
| 1 | a check/job failed (the summary names which + the URL) |
| 2 | required/named checks still NOT green after `--timeout` (genuinely stuck) |
| 3 | usage / target-unresolvable / unrecoverable error |
| 4 | STUCK-but-safe — required/named checks all green, but a ghost check will never terminate; safe to `--admin` merge (the caller decides) |

The final stdout line is a single self-describing summary, e.g. `CI-WATCH run 27972328590: SUCCESS (25 job(s) green)`, `CI-WATCH pr 73: FAILURE — check "Test (ubuntu-latest, node 22.13)" → FAILURE (https://…)`, or `CI-WATCH pr 327: STUCK — required/named checks GREEN (51), 1 non-terminal ghost/non-required check(s): (unnamed); safe to --admin merge`.

### Run it detached (CAVEAT: a long wait STRANDS — see the cron-heartbeat section below; this is for SHORT, synchronously-observed gates only)

It's designed to run as a detached `run_in_background` Bash task that re-invokes the orchestrator on exit — read the outcome from the tail (the `CI-WATCH …` line) and gate on the exit code:

```bash
nub scripts/ci-watch.ts --run "$RUN_ID" --repo nubjs/nub   # run_in_background: true
```

For a merge-queue drain, prefer `scripts/merge-cascade.ts` (it gates positively and merges on green); reach for `ci-watch.ts` when you just need to block on one run/PR and branch on the result.

## Merge-on-green — the ORCHESTRATOR runs the watcher as a background shell; NEVER a watcher sub-agent (learned 2026-06-25)

What stranded EVERY merge in the v0.2.2 floor-fix batch (#163/#164) was NOT background shells and NOT the script — it was dispatching a ci-watch **sub-agent**. A sub-agent that backgrounds a watch and then rests ORPHANS the command: the sub-agent isn't the orchestrator, so when its background process exits there's nothing wired to re-invoke the orchestrator and merge. **Never dispatch a sub-agent to watch CI.**

**The proven pattern: the ORCHESTRATOR runs the blocking watcher as its OWN `run_in_background` Bash task.** A background Bash command persists ACROSS TURNS and re-invokes the orchestrator when it exits (the Bash tool contract). So `node scripts/merge-cascade.ts --max-minutes <n>` (drains `.fray/merge-queue.jsonl`: watch → gate → merge → ff-pull → exit), launched by the orchestrator with `run_in_background: true`, IS the durable merge mechanism — on exit the orchestrator is re-invoked and reconciles.

Recipe:
1. **Enqueue:** append `{"pr":N,"branch":"…","thread":"…","note":"…"}` (optional `"hold":true`) to `.fray/merge-queue.jsonl`. Enqueue UNHELD only once the PR's FINAL head is pushed — a stale head can be green-but-wrong, so verify the head/rebase before it's mergeable.
2. **Watch:** the ORCHESTRATOR runs `node scripts/merge-cascade.ts --max-minutes 40` with `run_in_background: true`. It gates positively on the required `CI gate` (present + SUCCESS) + mergeable, merges `--squash --admin`, ff-pulls, dequeues, exits → re-invokes the orchestrator. It shares ci-watch's #327 ghost carve-out (`scripts/lib/ci-rollup.ts`): a nameless/never-terminating ghost — or any non-required check, pending OR failed — is non-blocking, so the drain can't hang the way #327 stranded a merge; a still-running or failed REQUIRED gate always holds/blocks, so a red PR is never mis-merged.
3. **Landing agents PUSH-THEN-EXIT** — they never watch; they report `pushed <sha>, queued`. The orchestrator's background watcher owns merge-on-green.

A `CronCreate` heartbeat (every ~4 min, one non-blocking `gh pr view` poll per queued PR, merge-on-green) is a FALLBACK only if a background shell ever proves unreliable — the orchestrator background shell above is the default and what historically worked. Reach for the blocking `ci-watch.ts` directly only for a single-run gate you observe synchronously.

## Self-contained sub-agents — `run_in_background` works for them TOO; the orchestrator must NOT preempt (corrected 2026-06-26)

There is NO fundamental orchestrator-vs-sub-agent difference for `run_in_background`: a backgrounded Bash command persists and re-invokes ITS LAUNCHER on exit — orchestrator or sub-agent alike. **Proven:** a ci-watch sub-agent backgrounded a watch, rested, was re-invoked when CI went terminal, and reported the result. The earlier "a sub-agent watch ORPHANS" claim was a MISDIAGNOSIS — the watchers were working; the orchestrator PREEMPTED them by impatiently checking CI itself and merging manually mid-trace.

**The self-contained landing-agent pattern (the goal — one agent traces push→merge):**
1. push the branch;
2. launch `node scripts/merge-cascade.ts --max-minutes 40` for its OWN PR (or `ci-watch.ts`) via `run_in_background: true`;
3. END its turn (rest);
4. it is RE-INVOKED when the command exits → reports merged / red, and iterates (fix-if-red → re-push → re-watch).

**DO NOT preempt a landing agent's background watch** — let it trace to merge and report. That impatience, not any orphaning, is what broke the flow.

The FOREGROUND `ci-watch.ts --chunk` loop below is a FALLBACK only — for an agent that must actively iterate in the foreground and can't rest. Run `ci-watch.ts` in the FOREGROUND in chunks under the Bash cap, looping on pending:

```bash
# Bash tool: foreground (NOT run_in_background), timeout: 570000  (9.5 min, under the 600000 cap)
nub scripts/ci-watch.ts --pr <N> --chunk          # --chunk caps the watch ~9 min and exits 2 with "RERUN to continue" if still pending
#   exit 0 = green → act    exit 1 = red → fix + re-push    exit 2 = pending → RE-RUN the SAME command    exit 3 = error
```

Loop: while it exits 2, call it again — each chunk completes within the cap (no kill, no orphan). The sub-agent blocks in the foreground the whole time, which is FINE: it's backgrounded relative to the ORCHESTRATOR, so the main loop stays responsive.

**Dispatch prompts for a self-gating landing agent MUST spell this out** — a sub-agent won't infer the foreground-chunk loop. This is ONLY for when the sub-agent needs to SEE its own result to iterate. For the common "push and let it merge" case, the agent push-then-exits and the ORCHESTRATOR's background shell / merge-queue owns merge-on-green (above).
