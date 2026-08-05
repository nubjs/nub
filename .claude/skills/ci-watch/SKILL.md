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

`gh run watch <id> --exit-status` and `gh pr checks <pr> --watch` are not safe to arm right after a push / tag / PR-open:

- **Premature exit while QUEUED.** With no jobs registered yet, gh sees "nothing in progress" and returns **exit 0** even though the run is still queued.
- **Transient errors read as failure.** A mid-watch `HTTP 401: Bad credentials` (token refresh) or a 5xx exits non-zero, indistinguishable from a real CI failure.
- **No native fix** — `--interval` only tunes the poll cadence.

**Never trust a raw watcher's exit code alone.** Re-verify terminal status with `gh run view <id> --json status,conclusion` (done only when `status == "completed"`) or `gh pr view <pr> --json statusCheckRollup` (done only when every item is terminal). And always fail-fast — act on the first failing check, never wait for all checks to finish.

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
- `--required <names>` — comma-separated branch-protection check names to gate on (e.g. `--required "CI gate"`). Success fires the instant every required check is green; a ghost or a non-required check — pending *or* failed — never blocks, matching branch-protection semantics. Prefer it when you know the required check name.
- `--no-progress <minutes>` — how long an unchanged incomplete set (all named checks green, only a ghost left) may sit before exiting 4 STUCK-but-safe (default 8).

What it fixes: **waits for the target to EXIST** (not-found / no-jobs-yet is "keep polling," never "done"); polls authoritative terminal state; **fails fast** on the first FAILURE/CANCELLED/TIMED_OUT/ STARTUP_FAILURE; **never hangs on a ghost**; **tolerates transient** gh/API errors (retried with exponential jittered backoff, 10s → cap 60s, 90s unauthenticated).

### The ghost check — why a strict "all checks terminal" gate hangs

GitHub occasionally registers a check-run that never reports a status: PENDING, nameless, forever. A watcher waiting for *every* rollup item to be terminal blocks indefinitely even though every real check is green. So a nameless / never-terminating non-required check does not block a green verdict: once every named check is green and the incomplete set has been unchanged for `--no-progress` minutes, the watcher exits **4 (STUCK-but-safe)** and the caller `--admin` merges. A *named* pending check is never treated as a ghost, so a real in-flight check is never green-lit early.

### Exit-code contract

| code | meaning |
| ---- | ------- |
| 0 | completed AND all green |
| 1 | a check/job failed (the summary names which + the URL) |
| 2 | required/named checks still NOT green after `--timeout` (genuinely stuck) |
| 3 | usage / target-unresolvable / unrecoverable error |
| 4 | STUCK-but-safe — required/named checks all green, but a ghost check will never terminate; safe to `--admin` merge (the caller decides) |

The final stdout line is a single self-describing summary, e.g. `CI-WATCH run 27972328590: SUCCESS (25 job(s) green)`, `CI-WATCH pr 73: FAILURE — check "Test (ubuntu-latest, node 22.13)" → FAILURE (https://…)`, or `CI-WATCH pr 327: STUCK — required/named checks GREEN (51), 1 non-terminal ghost/non-required check(s): (unnamed); safe to --admin merge`.

### NEVER pipe the watcher — a pipe discards the verdict you gate on

The exit code IS the contract, and a pipeline throws it away: in `ci-watch.ts … | tail -20`, `$?` is **tail's** status, so a **FAILED CI reads as SUCCESS**. The pipe also buffers stdout until the pipeline ends, so the log stays empty while the watcher runs.

```bash
# WRONG — $? is tail's, a red CI looks green, and no interim output
node scripts/ci-watch.ts --pr 604 --repo nubjs/nub | tail -25; echo "exit=$?"

# RIGHT — redirect, then gate on the watcher's OWN status
node scripts/ci-watch.ts --pr 604 --repo nubjs/nub > /tmp/ci604.log 2>&1; rc=$?
tail -15 /tmp/ci604.log; echo "exit=$rc"
```

`main()` warns on stderr when it detects a piped stdout (fd 1 is a FIFO for `| cmd`, not for `> file`), but redirect by default rather than relying on the warning. Bash `${PIPESTATUS[0]}` / zsh `${pipestatus[1]}` recover the real status if a pipe is unavoidable.

**Cross-check the rollup regardless of what the watcher says.** Read `gh pr view <pr> --json statusCheckRollup,mergeStateStatus` directly and act on any FAILURE. A watcher that has produced nothing for a long stretch is a suspect, not a status.

For a merge-queue drain, prefer `scripts/merge-cascade.ts` (it gates positively and merges on green); reach for `ci-watch.ts` when you need to block on one run/PR and branch on the result.

## Who runs the watcher

`run_in_background` behaves the same for the orchestrator and for a sub-agent: a backgrounded Bash command persists across turns and re-invokes **its launcher** on exit. Both patterns below are valid.

**Merge-on-green (the default).** The orchestrator runs the blocking watcher as its own `run_in_background` Bash task:

1. **Enqueue:** append `{"pr":N,"branch":"…","thread":"…","note":"…"}` (optional `"hold":true`) to `.fray/merge-queue.jsonl`. Enqueue UNHELD only once the PR's FINAL head is pushed — a stale head can be green-but-wrong.
2. **Watch:** the orchestrator runs `node scripts/merge-cascade.ts --max-minutes 40` with `run_in_background: true`. It gates positively on the required `CI gate` (present + SUCCESS) + mergeable, merges `--squash --admin`, ff-pulls, dequeues, exits → re-invokes the orchestrator. It shares ci-watch's ghost carve-out (`scripts/lib/ci-rollup.ts`), so a still-running or failed REQUIRED gate always blocks and a red PR is never mis-merged.
3. **Landing agents PUSH-THEN-EXIT** — they never watch; they report `pushed <sha>, queued`.

**Self-contained landing agent** (one agent traces push→merge): push the branch; launch `node scripts/merge-cascade.ts --max-minutes 40` (or `ci-watch.ts`) for its OWN PR via `run_in_background: true`; end its turn; it is re-invoked when the command exits, reports merged/red, and iterates. **Do not preempt a landing agent's background watch** by checking CI yourself and merging manually mid-trace — that impatience is what breaks the flow.

**Foreground chunk loop (fallback only)** — for an agent that must actively iterate and cannot rest:

```bash
# Bash tool: foreground (NOT run_in_background), timeout: 570000  (9.5 min, under the 600000 cap)
nub scripts/ci-watch.ts --pr <N> --chunk          # --chunk caps the watch ~9 min and exits 2 with "RERUN to continue" if still pending
#   exit 0 = green → act    exit 1 = red → fix + re-push    exit 2 = pending → RE-RUN the SAME command    exit 3 = error
```

While it exits 2, call it again — each chunk completes within the cap (no kill, no orphan). Blocking in the sub-agent's foreground is fine: it is backgrounded relative to the orchestrator. **A dispatch prompt for a self-gating landing agent must spell this loop out** — a sub-agent won't infer it.

A `CronCreate` heartbeat (every ~4 min, one non-blocking `gh pr view` poll per queued PR) is a FALLBACK only if a background shell ever proves unreliable.
