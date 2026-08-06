---
name: ci-adhoc-test
description: >-
  Run ad-hoc / exploratory tests on a real OS or platform via CI when the
  behavior CANNOT be reproduced on the local host or in Docker — macOS Seatbelt
  / sandbox-exec / codesigning, Windows cmd.exe / --script-shell shell
  selection / .cmd resolution / Authenticode, musl-vs-glibc, Linux-arm64, a
  specific Node floor.
  Invoke (via the Skill tool) whenever the user asks to "test this on
  macOS/Windows CI", "probe this across operating systems / platforms", "run an
  ad-hoc cross-platform check", or any validation that needs a real macOS or
  Windows runner. THE KEY FACT this skill exists to carry: a pull request is NOT
  required — a branch-scoped GitHub Actions workflow (workflow_dispatch + push
  to the branch) runs the probe with no PR open. Pairs with `ad-hoc-test` (local
  host probing), `dev-loop` (build), `ci-watch` (await the run), and AGENTS.md's
  Docker section (Linux-only; this skill covers what Docker can't).
metadata:
  internal: true
---

# Ad-hoc testing on a real OS/platform via CI

Some behavior is only observable on a real target platform: macOS Seatbelt / `sandbox-exec` / Gatekeeper / codesigning, Windows `cmd.exe` / `--script-shell` selection / `.cmd`/`.bat` resolution / Authenticode / SmartScreen, musl-vs-glibc detection, Linux-arm64, a pinned Node floor. Docker closes the **Linux** corners cheaply but runs Linux containers only — it is not a substitute for macOS or Windows.

## The key fact: no PR is required

A GitHub Actions workflow does not need an open pull request. Trigger it on the **branch**:

```yaml
on:
  push:
    branches: [<your-probe-branch>]  # THE trigger: runs on every push to THIS branch
    paths:                           # only when the harness itself changes
      - 'tests/<probe-name>/**'
      - '.github/workflows/<wf>.yml'
  workflow_dispatch:                 # usable only once the NAME is registered on the default branch — see below
```

- **Push to the branch → the probe runs.** Open no PR, or close one you opened.
- **Re-run with another push:** `git commit --allow-empty -m rerun && git push`. Or re-run a finished run with `gh run rerun <run-id>` (`gh run list --branch <branch>` for the id).
- **The variable is REGISTRATION, not the branch — and registration is STICKY.** A workflow NAME becomes registered the first time its file lands on the default branch, once, and stays registered afterwards even if the file is later deleted from that branch. Everything else follows from this:
  - **A NEW workflow name is not dispatchable from a branch.** MEASURED 2026-08-06 with a file that had never been on `main`: `gh workflow run branch-only-dispatch-test.yml --ref probe/dispatch-test` → `HTTP 404: workflow branch-only-dispatch-test.yml not found on the default branch`, exit 1. **So land a brand-new probe workflow on `main` once, then iterate on the branch.**
  - **An ALREADY-REGISTERED name dispatches straight at a branch,** and needs no `main` commit: `gh workflow run <wf>.yml -R <repo> --ref <branch> -f k=v` creates a run with `event=workflow_dispatch` on that branch. This is the whole value of the dispatch path — **it re-runs a probe with different INPUTS without a push**, where a push can only ever re-run it with the inputs' defaults.
  - ⭐ **A push made before registration is not lost — it fires RETROACTIVELY.** MEASURED: a branch push at 18:14:54 produced nothing; the workflow file landed on `main` at ~18:18; at 18:19:22 GitHub created push-event runs for that same five-minute-old sha. So "my pushes did nothing" during the unregistered window turns into a burst of runs the moment registration happens — do not read the silence as a broken trigger and do not keep pushing into it.
  - **The tell for "not yet registered": no `github-actions` check suite on the commit at all.**
  - ⛔ **`git log origin/main -- <path>` coming back empty does NOT prove a workflow is unregistered**, and reading it that way is how this entry got corrected in the wrong direction on 2026-08-06: three probes were observed dispatching happily from branches with no `main` history for the file, and the conclusion drawn was "dispatch works for branch-only workflows." All three were long-registered names whose files had since left `main`. The discriminating experiment is a file whose NAME has never existed on the default branch.
  - Still keep the `push:` trigger: it is the only way to iterate on the workflow FILE itself, and it is what fires on each commit.
- **Do NOT open a PR just to get CI.** A PR signals "ready to land," which a prototype is not.
- Omit any `pull_request:` trigger — it ties runs to PR state, which is what you're avoiding.

## The harness shape

Keep the probe self-contained under `tests/<probe-name>/`, mirroring the existing ones:

- A generator / runner (e.g. an SBPL profile generator + a `sandbox-exec` runner; a `.cmd` resolver harness).
- Fast unit + smoke tests asserting the enforcement and the bypass/fail-closed cases.
- A `README.md` (what it validates, how to reproduce locally) and a `results.md` (findings, plus heavy runs reproduced on demand).
- The branch-scoped workflow `.github/workflows/<wf>.yml`.

Mirror `tests/sandbox-macos-writeconfine/` + `.github/workflows/sandbox-macos-writeconfine.yml`, or its Windows counterpart `tests/sandbox-win-probes/` (each on its own probe branch, not `main`).

**Keep CI lean — fast, deterministic core only:** unit tests + the enforcement/bypass smoke matrix, no network, no mega-fixture. Heavy or combinatorial runs are documented in `results.md` and reproduced on demand. CI capacity is shared; a 22-minute-per-push probe job is already a lot.

## Test every candidate FIX in one build-free run, not one per push

When the probe fails and you have several plausible repairs, do not guess-and-push: each loop costs a
full build plus queue time. Write a job that tries **all** candidates at once against a stand-in, with
no compile step — a `.cmd`/shell quoting question needs `cmd.exe` and a two-line stub, not the real
binary. Minutes instead of half an hour, and the losing candidates are results you keep.

Two rules that make the output trustworthy:

- **Include the CURRENT broken form as a control.** It must fail. If it passes, the reproduction is
  wrong and every other row is meaningless — say so in the output rather than reading the winners.
- **A losing alternative is a result, not a job failure.** Exit non-zero only when the control passes
  or when NO candidate works. Otherwise the job sits permanently red over a candidate you never
  intended to ship, and readers learn to ignore it.

## Run and watch

- Kick a run by pushing to the branch; list with `gh run list --workflow <wf>.yml --branch <branch>`.
- Await a specific run with the `ci-watch` skill (`scripts/ci-watch.ts`) rather than raw `gh run watch` — it waits for the run to exist, polls authoritative terminal status, fails fast, and exits 0 only on confirmed success.
- A failure is immediately actionable — read the job log, fix the harness, push again.

## Lifecycle

- **The branch is the durable home of the probe** while it's exploratory. Push, run, iterate.
- When it graduates into a permanent regression check, fold it into `main` through the normal flow (it's `tests/**` + a workflow file — a content/CI change routed straight to `main`). Decide its steady-state trigger then.
- If you only needed the one-time answer, leave the branch as the record (or delete it once `results.md` captures the findings) — never open a PR to "preserve" a throwaway probe.

## A cross-compile CHECK is not a test run

`cargo check -p nub-cli --all-targets --target x86_64-pc-windows-gnu` proves the code COMPILES for Windows. It says nothing about whether the tests PASS there, and the gap is exactly where platform behavior lives: `.cmd` shims vs `#!/usr/bin/env node`, path separators, `NODE_OPTIONS` tokenizing, `process.title`, mode bits that are no-ops.

A cross-compile is a necessary pre-flight, never the evidence. If you are un-gating tests from `#[cfg(unix)]`, or writing anything whose behavior could differ by platform, run a branch probe BEFORE pushing — it needs no PR and costs one workflow run.

## When to reach for this vs the alternatives

- **Local host probe** (`ad-hoc-test`) — the behavior reproduces on your dev machine. Default for anything not platform-gated.
- **Docker** (AGENTS.md) — a **Linux** corner: musl/glibc, a Node floor, a clean dependency-free environment, first-run install.
- **This skill (CI branch probe)** — a **macOS or Windows** behavior, or a real multi-runner matrix, that neither the host nor Docker can show.
