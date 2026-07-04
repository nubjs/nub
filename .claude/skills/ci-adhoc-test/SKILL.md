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
---

# Ad-hoc testing on a real OS/platform via CI

Some behavior can only be observed on a real target platform: macOS Seatbelt / `sandbox-exec` / Gatekeeper / codesigning, Windows `cmd.exe` / `--script-shell` shell selection / `.cmd`/`.bat` resolution / Authenticode / SmartScreen, musl-vs-glibc detection, Linux-arm64, a pinned Node floor. Docker closes the **Linux** corners cheaply (see AGENTS.md's Docker section) but runs Linux containers only — it is **not** a substitute for macOS or Windows. CI runners (`macos-latest`, `windows-latest`) are the only way to exercise those, and this skill is how you do it as a throwaway probe.

## The key fact: no PR is required

A GitHub Actions workflow does **not** need an open pull request to run. Trigger it on the **branch** instead:

```yaml
on:
  push:
    branches: [<your-probe-branch>]  # THE trigger: runs on every push to THIS branch
    paths:                           # only when the harness itself changes
      - 'tests/<probe-name>/**'
      - '.github/workflows/<wf>.yml'
  workflow_dispatch:                 # INERT until the file lands on the default branch — see below
```

With this trigger the CI capability is keyed to the **branch + workflow file**, not to a PR. Consequences:

- **Push to the branch → the probe runs.** Open no PR, or close one you opened — the runs keep working.
- **Re-run with another push** — an empty commit is the simplest: `git commit --allow-empty -m rerun && git push`. Or re-run a finished run as-is with `gh run rerun <run-id>` (`gh run list --branch <branch>` to get the id).
- **`push` is load-bearing; `workflow_dispatch` alone will NOT work for a branch-only probe.** GitHub only registers a `workflow_dispatch` workflow if the file is on the **default branch** — so for a workflow that lives only on the feature branch, `gh workflow run <wf>.yml --ref <branch>` errors ("no workflows found") and it never appears in the Actions UI. Keep the `workflow_dispatch` entry for the day the probe graduates to `main`, but do not rely on it before then; the `push` trigger is the only thing that fires a not-on-`main` workflow.
- **Do NOT open a PR just to get CI.** A PR signals "ready to land / please review," which a prototype is not. Opening one to trigger CI is the wrong tool and reads as premature-ship. (Worked example: PR #205 added a `macos-latest` Seatbelt probe; it was closed as premature, and the branch `push` trigger meant the macOS validation kept working off the `fs-write-confine` branch with the PR closed.)
- Omit any `pull_request:` trigger — it ties runs to PR state, which is exactly what you're avoiding.

## The harness shape

Keep the probe **self-contained** under `tests/<probe-name>/`, mirroring the existing ones, so the whole thing is one reviewable, reproducible unit:

- A generator / runner (e.g. an SBPL profile generator + a `sandbox-exec` runner; a `.cmd` resolver harness).
- Fast **unit + smoke tests** that assert the enforcement and the bypass/fail-closed cases.
- A `README.md` (what it validates, how to reproduce locally) and a `results.md` (the findings, plus any heavy runs reproduced on demand).
- The branch-scoped workflow `.github/workflows/<wf>.yml`.

Examples to mirror: `tests/sandbox-macos-writeconfine/` + `.github/workflows/sandbox-macos-writeconfine.yml` (macOS Seatbelt write-confine), and its Windows counterpart `tests/sandbox-win-probes/` (each lives on its own probe branch, not `main`).

## Keep CI lean — fast core only

The CI job runs the **fast, deterministic core**: unit tests + the enforcement/bypass smoke matrix, no network, no mega-fixture. Heavy or combinatorial runs (a frameworks mega-fixture, per-cache-family sweeps) are documented in `results.md` and reproduced **on demand**, not baked into every CI run. CI capacity is shared (AGENTS.md) — a probe job that takes 22 minutes per push is already a lot; don't make it a per-commit tax.

## Run and watch

- Kick a run by pushing to the branch (an empty commit if the harness is unchanged: `git commit --allow-empty -m rerun && git push`); list with `gh run list --workflow <wf>.yml --branch <branch>`. (`gh workflow run` only works once the file is on the default branch — see the trigger note above.)
- Await a specific run with the `ci-watch` skill (`scripts/ci-watch.ts`) rather than raw `gh run watch` — it waits for the run to exist, polls authoritative terminal status, fails fast, and exits 0 only on confirmed success.
- A failure is immediately actionable — read the job log, fix the harness, push again (the `push` trigger re-runs it).

## Lifecycle

- **The branch is the durable home of the probe** while it's exploratory — it persists with no PR ceremony. Push, run, iterate.
- When the harness graduates into a permanent regression check, fold it into `main` through the normal flow (it's `tests/**` + a workflow file — a content/CI change, which AGENTS.md routes straight to `main`, no review-gate PR). Decide its steady-state trigger then (e.g. path-filtered on `main`, or `workflow_dispatch`-only).
- If you only needed the one-time answer, leave the branch as the record (or delete it once `results.md` captures the findings) — never open a PR to "preserve" a throwaway probe.

## When to reach for this vs the alternatives

- **Local host probe** (`ad-hoc-test` skill) — the behavior reproduces on your dev machine. Cheapest; default for anything not platform-gated.
- **Docker** (AGENTS.md) — a **Linux** corner: musl/glibc, a Node floor, a clean dependency-free environment, first-run install. Linux containers only.
- **This skill (CI branch probe)** — a **macOS or Windows** behavior, or a real multi-runner matrix, that neither the host nor Docker can show. The PR-free, branch-scoped path.
