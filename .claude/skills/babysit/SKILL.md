---
name: babysit
description: Bring one nubjs/nub pull request to merge-readiness — pull the inline reviews, verify each finding against the code, re-verify every fix round locally, fix CI, and loop. Invoke (via the Skill tool) when asked to babysit a PR, to get one merge-ready, or when you own a PR and are waiting on reviews or CI. Scope — babysit ENDS at merge-readiness and hands back; only an instruction to babysit it TO MERGE authorizes the merge, and that phrasing also delegates the readiness judgment, so decide rather than ask. Carries the traps that each cost a round trip: the GitHub CLI's PR-view JSON silently omits the inline review comments where the real findings live, the CI gate aggregator check registers LAST so a partial green rollup is not done, and re-basing a stacked PR fires no workflow event, so its low check count is a false green.
---

# Babysit a PR (nubjs/nub)

Loop: pull reviews → verify each finding → fix → re-verify locally → push → watch CI → repeat.

**Babysit ends at merge-ready.** Report that state and stop; the merge is the maintainer's. Only an instruction to babysit it *to merge* authorizes one — and that phrasing delegates the readiness judgment too, so decide against the criteria below instead of asking.

## Reviews

Two bots review within minutes of every push: `pullfrog`, which posts a top-level review plus inline threads with a `<details>` write-up per finding, and `copilot-pull-request-reviewer`. The maintainer outranks both. A PR carrying no review was merged before they ran rather than approved by them, and only the review list distinguishes those.

The `gh pr view --json reviews,comments` form silently omits inline `pulls/{n}/comments`, which is where the actionable findings are. Use [`scripts/pr-reviews.ts`](../../../scripts/pr-reviews.ts) — one GraphQL round trip for reviews, inline threads (id, resolved/outdated state, `file:line`) and PR conversation:

```bash
nub scripts/pr-reviews.ts <pr> | jq '.reviewThreads[] | select(.isResolved | not)'
```

## Triage

Skip resolved and outdated threads first — an outdated one anchors a line a later push already rewrote.

Every finding is a hypothesis. Verify it against the code before acting; a confident write-up is the one that gets acted on unchecked. Null guards, "just in case" branches, and try/catch around code that cannot throw are the slop shape [`AGENTS.md`](../../../AGENTS.md) names — reject those by default.

Reject with one factual line on the thread saying what you checked. Never ignore silently, and never reply conversationally to a bot. Load the `prose-writing` skill before writing any comment. Resolve each thread you address:

```bash
gh api graphql -f query='mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{isResolved}}}' -F id=<thread-id>
```

## Fix rounds

A review-driven fix earns the same gate as the original change. Run `make verify`. For a behavior change, sweep adversarial fixtures against the branch's MERGE-BASE build ([`ad-hoc-test`](../ad-hoc-test/SKILL.md)) — a run that only re-confirms the reported case tests your intent, not your fix. Escalate to the `impact-analysis` skill when the fix widened the blast radius. A one-line typo fix skips both.

Then push once. Each push costs roughly five workflows across eight platforms, and fix-after-fix pushes starve the shared runner pool.

## CI

```bash
nub scripts/ci-watch.ts --pr <N> --required "CI gate" --timeout 90
```

The `CI gate` job is an `if: always()` aggregator over every path-gated job, so it registers LAST: fifty green checks with it still pending means the run is not done. A raw `gh pr checks --watch` exits 0 while the run is queued with no jobs registered, so watch through [`scripts/ci-watch.ts`](../../../scripts/ci-watch.ts).

Diagnose red with [`ci-triage`](../ci-triage/SKILL.md); a run whose rollup is `cancelled` can hold a job that failed, so the run-level conclusion misses real red.

Fix only what this PR caused, and never edit a workflow to make a check pass. A merge-blocking failure that looks unrelated is often a stale base — merge `main` in and re-run. The heavy legs fail for environment reasons too, so read the job log before calling one a regression:

```
Docker smoke · Windows matrix · daily-driver · native-deps
```

**Stacked PR.** After a rebase plus `gh pr edit --base main`, a LOW check count is a FALSE green. The `--base` edit fires no `synchronize` event, so every workflow gated on `pull_request: branches: [main]` never runs. Force the real matrix with `gh pr close <N> && gh pr reopen <N>`, then wait for it — a Rust PR carries around sixty checks.

## Merge-ready

- Mergeable: no conflicts, base current.
- The `CI gate` check is green, or every failure is investigated and written up as unrelated.
- Every valid finding is addressed, or rejected with reasoning on the thread.
- Threads resolved.
- The body carries `Closes #N` if the PR resolves an issue. Without it the issue stays silently open after the merge; `gh pr edit <N> --body` fixes it.

Report that state and stop here, unless the instruction was to merge.

## Merging

The `Main` ruleset blocks `update` on `main`, so a plain merge fails with `the base branch policy prohibits the merge`, and auto-merge is disabled repo-wide. That ruleset sets no required status checks either, so nothing server-side stops a red merge. The flag below bypasses the protection rule, not the judgment — confirm `CI gate` yourself first.

```bash
gh pr merge <N> --squash --admin
git -C <shared-tree> fetch origin && git -C <shared-tree> merge --ff-only origin/main  # never pull --ff-only
make install-dev                                                                       # skip only if the diff was docs-only
git push origin --delete <branch> && git branch -D <branch>
git worktree remove <dir> --force
```

The shared tree always carries a sibling's uncommitted work, so `pull --ff-only` runs rebase's precondition check and aborts on any dirty file. For a queue of approved PRs, [`scripts/merge-cascade.ts`](../../../scripts/merge-cascade.ts) does all of the above per entry.
