---
name: corpus-remeasure
description: The iteration cycle for the build-jail corpus — change the harness or nub, push it, re-measure ONE package@version on ONE platform, and read the answer. Invoke (via the Skill tool) whenever you need to test a hypothesis about why a package got the grant it did, verify a harness or nub fix on a real runner, or re-measure specific packages without disturbing the corpus. Carries the traps that each cost a wasted run: a bare `git commit` that clobbers the queue, `--force` argument order, a watcher that reports a pass off an empty log, and the fact that a grant is not evidence until you read the failing cell's log.
---

# Re-measuring a package in the build-jail corpus

The loop is: **edit → push → dispatch a debug run → watch → read the CELL LOGS**. Each step has a trap that has already cost a run.

Corpus repo: `nubjs/build-jail-corpus` (public). Local clone: `~/nub-corpus-staging`. nub: branch `sandbox/integration` at `~/.cache/nub/worktrees/integ`.

## 1. Push the harness change — the queue is the trap

⛔ **ALWAYS `git commit -- <explicit paths>`.** A bare `git commit` sweeps in whatever is staged, and `queue.ndjson` is almost always staged because reading the corpus stages it. A commit carrying a stale queue **releases rows other runners are actively measuring**. Measured: one such commit carried 194 lines of stale queue.

```sh
cd ~/nub-corpus-staging
git commit -q -m "…" -- harness/search.mjs          # explicit paths, always
git show --stat --oneline HEAD | head -5             # one second; catches exactly this
```

The push races ~12 concurrent runners, so it needs a retry loop — and the cleanup before each attempt is what makes it work:

```sh
for a in 1 2 3 4 5; do
  git restore --staged . 2>/dev/null                 # unstage (checkout STAGES; restore is what unstages)
  git checkout -- records queue.ndjson 2>/dev/null   # drop working-copy edits — origin owns these
  git clean -fdq -- records 2>/dev/null              # reading the corpus leaves UNTRACKED records behind
  git fetch -q origin main
  git rebase -q origin/main >/dev/null 2>&1 || { git rebase --abort 2>/dev/null; sleep 3; continue; }
  git push origin HEAD:main >/dev/null 2>&1 && { echo "PUSHED"; break; }
  sleep 3
done
git fetch -q origin main
git merge-base --is-ancestor HEAD origin/main && echo "✓ LANDED" || echo "⛔ NOT LANDED"
```

⛔ **Verify it landed. Never infer it from the push command's apparent success** — read `merge-base --is-ancestor`, because a rejected push inside a loop is easy to miss.

**Reading the corpus for analysis leaves two kinds of debris**, both of which break the next rebase: `git checkout origin/main -- records` STAGES those files, and it leaves records your HEAD lacks as UNTRACKED. Before cleaning untracked records, confirm they exist at origin (`git cat-file -e origin/main:<path>`) so a clean can never destroy an unpushed measurement.

## 2. Dispatch the re-measure

The workflow has a **`packages` debug input**. When set, the runner SKIPS the queue entirely — no claim, no completion, no commit of results — and measures exactly those specs with `--force`, publishing only the ARTIFACT. That is what makes it safe to run against a live corpus.

```sh
gh workflow run corpus-queue-runner.yml -R nubjs/build-jail-corpus \
  -f os=macos \                       # linux | macos | windows
  -f nub_sha=<40-char sha> \
  -f chain=false \                    # ⛔ or it self-dispatches a whole chain
  -f slice=2 \
  -f packages="husky@3.1.0 detox@20.9.1"
```

- ⛔ **`nub_sha` must be PUSHED.** CI clones `nubjs/nub` anonymously. Check with `git ls-remote https://github.com/nubjs/nub sandbox/integration` before dispatching, or the run dies at clone.
- **Pick the sha deliberately.** Using the sha the fleet already built gets a binary-cache hit (~0 min instead of a ~26 min cold Windows build) AND makes the result comparable with corpus records. Use a NEW sha only when testing a nub change.
- **Platform speed differs enormously.** macOS/Linux ≈ 1 min/package; Windows ≈ 9 min/package average and up to ~40 for a package that walks the full 55-cell ladder, plus ~26 min for a cold build. **Prototype on macOS or Linux whenever the question is not Windows-specific.**
- ⛔ **In `run-batch.sh`, `--force` must come BEFORE `--file`** — it is parsed only as the first argument after `<nub>`. The workflow already does this. A run with the order wrong "succeeded" in 17 seconds having measured nothing.

## 3. Watch it — and gate the watcher, or it will lie to you

⛔ **NEVER dispatch CI without a bounded background watcher.** A stall is silent. And the watcher itself must gate, because **`gh run view --log` returns an EMPTY string for an in-progress run** — so `grep -q <bad-thing>` returns false and reports a PASS off nothing. That exact bug appeared inside a gate written to prevent it.

Three gates, in order, all mandatory:

1. **Terminal state**, on a BOUNDED loop that exits non-zero on timeout (`for i in $(seq 1 240); do … sleep 45; done`) so it can never hang forever.
2. **Log non-empty** — `wc -l` over 100 lines, else refuse to interpret.
3. **The canary RAN** — assert `fixture canary OK: <n> files` is PRESENT, never infer it from the absence of `REFUSING TO RUN`.

The canary is the harness's own guard: it installs `puppeteer@25.4.0` and refuses the whole batch unless the fixture produces >5,000 files. **If it refuses, nothing below it means anything** — that is how a broken isolation change gets caught before it writes garbage.

## 4. Read the CELL LOGS, not just the grant

⛔ **THE SINGLE MOST EXPENSIVE LESSON HERE.** Three consecutive fixes for one Windows defect were wrong because each was theorised from a grant and a path list. The failing cell's log named the exact cause in one line, every time.

Per-cell logs are gitignored, so they exist ONLY in the run artifact — and only when the batch actually ran:

```sh
gh run download <run-id> -R nubjs/build-jail-corpus -D /tmp/art -n records-<os>-<run-id>
find /tmp/art -path '*<pkg>*' -name '*.log'     # one log per cell, named for the state
```

Read the cell **one rung below the winner** — that is where the reason lives. Read it RAW before grepping: a grep that matches nothing looks identical to a clean run. Real examples, each of which redirected an investigation:

| log line | what it actually meant |
|---|---|
| `spawnSync git EPERM` | needs to EXECUTE git — an exec denial, not a write denial |
| `mkdir '…\Packages\nub_sbx_<pid>_<nonce>_0'` | the leaf is the PROFILE dir, not `Packages` — a fixture cannot pre-create it |
| `Could not find JAVA, skipping …` | a real line that was the WRONG explanation — both platforms skipped identically |
| *(no error at all)* | a silent partial install; the cell failed on DIGEST, not exit code |

⛔ **A record's `grant` tells you WHAT was needed; only the log tells you WHY.** And a plausible log line can still be the wrong explanation — check whether it differs across the platforms that disagree.

## 5. Attribute the result

- **`corpusGitSha`** in `provenance` names the corpus commit exactly. Use it — `harnessSha256` is a content hash and git rewrites line endings on Windows, so the same commit hashes differently there and **cannot be compared across platforms**.
- **Confounds before blaming a platform:** same `nubGitSha`? same `nodeSelection.chosenMajor`? And note darwin records are arm64 while linux records are x64, so OS and ARCH move together and cannot be separated from these records alone.
- **A debug run writes NOTHING to the corpus** — results live only in the artifact. To make a re-measurement stick, let the fleet re-claim the row (reopen it) rather than hand-editing records.

## Scope

**Use it for** a hypothesis about one package, verifying a harness or nub fix on a real runner, or getting cell logs for a package whose artifact expired (30-day retention).

**Not for** running the corpus — that is the self-dispatching queue chain (`chain=true`). Not for local iteration either: build nub locally and run `harness/search.mjs` directly against a `/tmp` fixture when the question does not need a specific OS.

**Related:** `harness/divergence-scan.mjs` finds packages whose grant disagrees across OSes and ranks them — the usual source of the hypothesis you are about to test.
