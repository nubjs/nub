---
name: ci-triage
description: >-
  Find and diagnose RED CI with the gh CLI — answer "is anything failing?" and
  "why?" correctly, without being handed a URL. Invoke (via the Skill tool)
  whenever you need to check whether a branch, commit, or PR is green, whenever
  someone reports failing CI, before cutting a release, and before claiming any
  commit is green. THE FAILURE THIS SKILL EXISTS TO PREVENT: `gh run list`
  reports a RUN-level `conclusion`, and a run whose rollup is `cancelled` can
  contain a job whose conclusion is `failure` — so filtering runs on
  `conclusion=="failure"` silently misses real red, and taking the latest run
  per workflow hides an older failed run on the same SHA. The authoritative
  instrument is the commit's CHECK-RUNS, which is what the GitHub UI renders.
  Pairs with `ci-watch` (blocking until a run is terminal) and `ci-adhoc-test`
  (running a probe on a real OS).
---

# Investigating CI

`ci-watch` answers *"has it finished, and did it pass?"* for one run you are waiting on. This skill answers the two questions you get asked cold: **"is anything red?"** and **"why is this red?"** — starting from a branch name or a commit, never from a URL someone hands you.

## The rule

**Never answer "is it green?" from run-level `conclusion`. Query the commit's check-runs.**

A GitHub Actions *run* rolls up its jobs into one conclusion, and that rollup is lossy in the exact case that matters:

| What happened | Run `conclusion` | Job conclusions | `gh run list` shows |
| --- | --- | --- | --- |
| A job failed | `failure` | one `failure` | ✅ red — you see it |
| **A job failed inside a run that also got cancelled** | **`cancelled`** | **one `failure`** | ❌ **nothing — filtered out** |
| Superseded by `cancel-in-progress` | `cancelled` | all `cancelled` | ❌ nothing (correctly) |

The middle row is real and it is what the UI paints red on the commit. It happens whenever a workflow has an aggregation/gate job that runs after its matrix legs: `concurrency: cancel-in-progress` cancels the legs, the gate still runs, sees `cancelled`, and **fails**. The run rolls up `cancelled`; the gate job is `failure`.

## The one command that is always correct

```bash
SHA=$(git rev-parse origin/main)   # or a PR head, or any commit
gh api repos/nubjs/nub/commits/$SHA/check-runs --paginate \
  --jq '.check_runs[] | select(.conclusion != "success" and .conclusion != "skipped" and .conclusion != "neutral")
        | "\(.conclusion // .status) | \(.name) | \(.html_url)"'
```

Empty output means genuinely green. Anything else is exactly what a human sees in the Actions tab, with a clickable URL per finding. This endpoint is the source of truth because it is what the UI renders — run-level data is downstream of it.

Wrap it as a branch check:

```bash
# Is main green, right now, by the UI's own definition?
gh api repos/nubjs/nub/commits/$(git rev-parse origin/main)/check-runs --paginate \
  --jq '[.check_runs[] | select(.conclusion=="failure" or .conclusion=="timed_out" or .conclusion=="action_required")] | length'
```

## Diagnosing one failure

Given a failing check, get the run, then the job, then the log — in that order.

```bash
gh run view <run-id> --json name,status,conclusion,headBranch,headSha,event,createdAt
gh run view <run-id> --json jobs --jq '.jobs[] | "\(.conclusion // .status) | \(.name)"'
gh run view <run-id> --log-failed > /tmp/fail.log 2>&1; echo "EXIT=$?"
sed -e 's/\x1b\[[0-9;]*m//g' /tmp/fail.log | grep -viE '^\s*$' | tail -40
```

Strip ANSI escapes (`sed -e 's/\x1b\[[0-9;]*m//g'`) or the log is unreadable. `--log-failed` returns only failing steps, which is usually 40 lines rather than 40,000.

**Read the `event` and `createdAt` fields before concluding anything.** They tell you whether the run was superseded, and by what.

## Superseded-run triage — is this red REAL?

A red check on a commit is not automatically a broken commit. Decide with this:

1. **List every run of that workflow on that SHA.** More than one is the tell.
   ```bash
   gh run list --workflow <file>.yml --limit 30 \
     --json databaseId,status,conclusion,headSha,headBranch,event,createdAt \
     --jq '.[] | select(.headSha=="'"$SHA"'") | "id=\(.databaseId) | \(.conclusion // .status) | \(.event) | \(.createdAt)"'
   ```
2. **If a later run of the same workflow on the same SHA is fully green, the code is fine** — the red one was superseded. The commit still *shows* red, which is a workflow defect worth fixing, not a code defect.
3. **Check the concurrency group.** `group: ${{ github.workflow }}-${{ github.ref }}` means a `schedule` run and a `push` run on `main` share a group and will cancel each other, because both carry `refs/heads/main`. That is a common and surprising source of self-inflicted red.

## The gate-job anti-pattern, and its fix

A gate job that aggregates matrix legs must treat `cancelled` as *not a verdict*. This is wrong:

```bash
[[ "$smoke" == "success" || "$smoke" == "skipped" ]] || { echo "smoke failed"; exit 1; }
```

A superseded run sets `smoke=cancelled`, so the gate fails and paints the commit red forever. Accept `cancelled` too — it does not mask a real failure, because a genuinely failing leg aggregates to `failure`, not `cancelled`:

```bash
[[ "$smoke" == "success" || "$smoke" == "skipped" || "$smoke" == "cancelled" ]] || { echo "smoke failed"; exit 1; }
```

## What NOT to do

- **Do not** filter `gh run list` on `conclusion=="failure"` and call the absence of hits "green". That is the whole bug this skill exists for.
- **Do not** take the latest run per workflow (`group_by(.name) | max_by(.createdAt)`). A later scheduled run can hide an earlier failed push run on the same SHA.
- **Do not** trust `--branch main` alone to find everything; a check can be attached by an app or a `workflow_run`. Check-runs on the SHA catch all of them.
- **Do not** conclude "not failing" from a bot issue being absent. The trunk-red bot files on its own schedule and its silence proves nothing.
- **Do not** read an exit code through a pipe (`gh ... | head` then `$?`). Redirect, capture `$?` on its own line, then inspect.

## Reporting a finding

State the commit, the check name, the run URL, whether a later run on the same SHA passed, and the mechanism. "CI is red" without those is not a diagnosis. If a superseded run is the cause, say so plainly and name the fix — the commit is still red in the UI and someone has to look at it.
