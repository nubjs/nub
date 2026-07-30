# macOS version matrix for the sandbox env-read closure

**Question (Q17, maintainer-confirmed).** The macOS ascendant-env-read closure — the
Seatbelt `(deny process-info*)` + `(allow process-info* (target self))` fragment that
blocks cross-process `KERN_PROCARGS2` env reads — is version-independent *in code* but
was only ever exercised on `macos-latest`. Does the closure actually HOLD on older macOS
majors, and what should the **minimum supported macOS version** be? "macOS 13/14 likely
leaks" was asserted, never verified.

**Verdict (one line).** The closure **HOLDS on macOS 14 and macOS 15** — verified against
real GitHub runners, each with its own negative control proving the `KERN_PROCARGS2`
vector was live and the confinement shut it. **macOS 13 is UNVERIFIED** — the Intel
`macos-13` runner (a deprecating GitHub-hosted class) never cleared the scheduler queue in
the run window, so no result was obtained (an infrastructure gap, **not** a leak).
Recommended supported floor: **macOS 14**, conservatively (the oldest major we could
actually verify); macOS 13 is expected-to-hold on the mechanism argument but is noted as
unverified.

---

## What was tested

An ad-hoc, branch-scoped CI probe (per the `ci-adhoc-test` skill — no PR) ran the sandbox
enforcement suite across a 3-version macOS matrix. The probe is the workflow + README; the
tests it runs are the real committed suites on branch `sandbox-primitives`.

- **Branch:** `sandbox-macos-envread-matrix` (off `origin/sandbox-primitives`).
- **Workflow:** `.github/workflows/sandbox-macos-envread-matrix.yml` (matrix `os: [macos-13, macos-14, macos-15]`, `fail-fast: false`).
- **Probe home / README:** `tests/sandbox-macos-envread-matrix/README.md`.
- **Flagship suite:** `cargo test -p nub-sandbox --test macos_envread` (`crates/nub-sandbox/tests/macos_envread.rs`).
- **Supporting suite:** `cargo test -p nub-sandbox --test macos_enforcement`.
- **The run:** [Actions run 29119601635](https://github.com/nubjs/nub/actions/runs/29119601635).

The flagship suite is self-validating: every enforcement assertion is paired with a
**negative control** that lifts the closure and confirms the read LEAKs. So a HOLD is
never hollow — the neg-control proves the runner's kernel genuinely exposes a co-resident
process's environment via `KERN_PROCARGS2`, and the confined read proves the Seatbelt
profile denies it (EPERM / `BLOCKED`). The four cases: sibling-process env read,
same-sandbox-child env read (the `(target self)` vs `(target same-sandbox)`
discriminator), self-read survival, and `node` running under the closure.

The workflow classifies each runner from the suite's own assertion output —
**HOLD** (confined read denied, neg-control leaked), **LEAK** (secret recovered under
confinement), **INFRA** (neg-control never leaked → inconclusive), **OTHER** (a non-leak
regression). Only HOLD passes the job.

## Per-version result (grounded in the actual run)

| macOS | Runner | Arch | Job status | Env-read closure |
|---|---|---|---|---|
| 15 | `macos-15` | arm64 (Apple Silicon) | `completed / success` | **HOLD** |
| 14 | `macos-14` | arm64 (Apple Silicon) | `completed / success` | **HOLD** |
| 13 | `macos-13` | x86-64 (Intel) | `queued` (never scheduled) | **UNVERIFIED — infra-blocked** |

- **macOS 15 — HOLD.** Job `completed / success`. Job success is definitive: the classify
  gate passes only on verdict `HOLD`, which requires the neg-control to have LEAKed *and*
  the confined `KERN_PROCARGS2` read to have returned EPERM. A LEAK, an inconclusive
  neg-control, or a self/node regression would each have failed the job.
- **macOS 14 — HOLD.** Job `completed / success`, same definitive interpretation. This is
  the oldest major with a real pass, so it anchors the conservative floor.
- **macOS 13 — UNVERIFIED (infra-blocked, not a leak).** The `macos-13` leg sat `queued`
  for the entire run window (>66 minutes, run start 19:54 UTC) and never got a runner,
  while both arm64 legs scheduled and finished within minutes. `macos-13` is GitHub's last
  Intel-based hosted image and is on the deprecation path; Intel-macOS capacity is scarce
  and queue starvation of this length is consistent with that, not with a test failure. No
  runner output was produced, so **no HOLD/LEAK verdict can be stated for macOS 13** — per
  the "don't guess the result" directive, it is recorded as an infrastructure gap.

Note on log depth: GitHub only serves per-job step logs (the `sw_vers` capture, the
neg-control lines) once the *whole run* reaches a terminal state; because the macos-13 leg
kept the run non-terminal, the raw per-line output for the two passing legs could not be
downloaded. The HOLD verdicts rest on the job conclusions, which are load-bearing exactly
because the classify gate only lets `HOLD` pass — a real runner result, not a prediction.

## Recommended supported floor

**Floor: macOS 14**, with macOS 13 as expected-to-hold-but-unverified.

Rationale, and why not 13:

- The closure is **version-independent in code**: the SBPL fragment
  `(deny process-info*)` + `(allow process-info* (target self))` uses Seatbelt profile
  grammar and the `process-info*` operation that have been stable in macOS's sandbox
  subsystem for many major versions — there is no version gate in nub's emitter
  (`crates/nub-sandbox/src/backend/macos.rs`). On the mechanism alone, macOS 13 is
  expected to hold identically.
- But this decision follows the project's **empirical-over-source** discipline: a floor
  should be set to the oldest version we have actually *observed* the closure holding on.
  That is **macOS 14**. Setting the floor to 13 on the mechanism argument alone would ship
  an unverified support claim — precisely the kind of "confirmed by intent, not by a run"
  gap this probe exists to close.
- macOS 14 is a low-cost conservative floor: both currently-shipping Apple-Silicon macOS
  majors (14, 15) are covered and verified, which is the real-world user population.

**If macOS 13 support is wanted as a stated claim**, the honest path is to obtain one real
`macos-13` result before asserting it — either by re-running the probe when Intel capacity
frees (the branch trigger is live; an empty-commit push re-runs it), or via a self-hosted /
paid Intel macOS runner. Until such a result exists, macOS 13 should be described as
"expected to hold (identical mechanism), unverified," never as "supported/verified."

## Caveats

- **No leak was observed on any tested version.** The only non-HOLD outcome is the macOS 13
  *absence of data*, which is an infra queue gap, not evidence of a leak.
- **Intel vs Apple Silicon.** Both verified legs are arm64. `KERN_PROCARGS2` and the
  Seatbelt `process-info*` operation are not arch-specific, so no arch-dependent divergence
  is expected — but the Intel path specifically remains the unverified one, which happens
  to coincide with the untested macOS 13.
- **Re-running.** The probe is branch-scoped and PR-free. `git commit --allow-empty -m rerun
  && git push` on `sandbox-macos-envread-matrix` re-fires the matrix;
  `gh run list --workflow sandbox-macos-envread-matrix.yml --branch sandbox-macos-envread-matrix`
  lists runs. This is the mechanism to opportunistically capture a macOS 13 datapoint later.

## Changelog

- 2026-07-10 — Initial write-up. Ad-hoc CI matrix (run 29119601635) verifies the env-read
  closure HOLDS on macOS 14 + 15 (both `macos-14`/`macos-15` jobs `completed/success`, each
  neg-control-backed); macOS 13 left UNVERIFIED — `macos-13` Intel runner queue-starved
  (>66 min, never scheduled), recorded as an infrastructure gap, not a leak. Recommends a
  conservative **macOS 14** supported floor, with macOS 13 expected-to-hold-but-unverified
  on the version-independent-mechanism argument.
