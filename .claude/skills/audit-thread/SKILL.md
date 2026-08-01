---
name: audit-thread
description: Use when running a compatibility/parity AUDIT — enumerating where nub diverges from a reference it claims parity with (pnpm CLI grammar, a lockfile format, a Node behavior, a flag surface). Encodes the hard gates that stop an audit from surfacing false positives. Auto-triggers on "audit", "compat audit", "parity audit", "find all the gaps", "what are we missing vs <tool>".
metadata:
  internal: true
---

# Audit threads

An **audit**'s deliverable is a CLEAN, COMPLETE, VERIFIED list of real gaps between nub and a reference it claims parity with. A single false positive destroys trust in the whole audit; the bar is zero garbage.

**Canonical methodology: AGENTS.md → "Audit threads."** Read it first. This skill is the operational surface: the gate checklist, the verbatim dispatch template, and the orchestration shape.

## The 5 gates

1. **Pin the reference target surgically.** State the EXACT major (e.g. pnpm 10) and VERIFY every reference checkout's version before reading it (`git -C .repos/<tool> describe --tags`, its `package.json` `version`, the installed tool's `--version`). Wrong-major reference is the #1 garbage source. **When the audit target is a branch/SHA of THIS repo**, `git fetch origin` FIRST and pin `origin/<branch>@<sha>` — never a local worktree/branch checkout, which routinely lags origin. Verify reachability (`git merge-base --is-ancestor <sha> origin/<branch>`) before reading a byte.
2. **Empirical over source.** A candidate is not a finding until a differential fixture reproduces it by RUNNING the real pinned tool + nub on identical input and diffing. Source and `--help` reading yield leads only.
3. **Cross-check the decision record.** Deprecated/removed flags, npm-isms nub rejects, the deliberate pnpm-compat divergences, and already-decided/already-built work are NOT findings. Filter against AGENTS.md "Core design positions", `wiki/` decision docs, and prior `.fray/` threads.
4. **Mandatory adversarial self-refutation.** Fresh-context reviewer(s) try to REFUTE each surfaced finding by re-pinning, re-reproducing, and re-checking the decision record. They must also challenge any claim that a gap is *irreducible* by exhausting the mechanism's documented alternatives and testing plausible closures. Default to refuted when uncertain. Surface only survivors, each with reproduction evidence; never forward the raw breadth-pass output.
5. **Tier + deliverable.** Opus at high+ effort for judgment AND refutation (a cheap tier may harvest breadth, but every item is Opus-verified). Thoroughness is two-dimensional: COVERAGE (enumerate the FULL surface from the pinned reference's own authoritative source) AND PRECISION (every item verified). Catalog → `wiki/research/<topic>.md` with all buckets explicit (real gaps / confirmed-OK / intentional-divergence); each finding records reproduction + decision-record cross-check + severity + confidence.

## Orchestration shape

Enumerate the full surface (coverage) → harvest candidate gaps → cross-check the decision record → reproduce each against the pinned real tool → adversarially refute in fresh context → surface ONLY survivors, with evidence.

Run it as a fray thread with individually-dispatched agents — not a blind parallel Workflow fan-out that buries the gates.

**Only a top-level session dispatches those agents.** If you are yourself a dispatched sub-agent, the repo-wide depth cap in `AGENTS.local.md` applies: run the gates INLINE and return. Gate 4's "fresh context" is then a fresh PASS, not a fresh agent — re-pin, re-run the fixture, and re-check the decision record yourself, defaulting to refuted when uncertain. An audit prong that cannot be refuted inline returns saying so rather than spawning a refuter.

**When a finding is ACTIONED.** An audit is investigation-scope: it surfaces gaps, it does not land fixes. The moment a finding becomes a code change, verify it the standard way — an ad-hoc fixture sweep against a built binary, re-running the audit's own differential against the pinned reference, then an in-thread read of the diff. A parity fix touching a shared verb/flag dispatch path routinely ripples to sibling commands, which is one of the cases that earns escalating to a fresh-context `impact-analysis` pass — but that never substitutes for running the sibling commands and diffing them against the reference. Gate 4 asks whether a FINDING is real; impact analysis asks whether a FIX is safe.

## Dispatch template (every audit sub-agent prompt must be self-contained)

```
You are running a <SCOPE> audit: find where nub diverges from <REFERENCE> <EXACT MAJOR> on <SURFACE>.
This is an AUDIT — the deliverable is a CLEAN, VERIFIED list of REAL gaps. A single false positive is
unacceptable. Follow all 5 gates; do not skip any.

GATE 1 — PIN: The target is <REFERENCE> <EXACT MAJOR>. BEFORE reading anything, verify the version of
  every reference you use: `git -C .repos/<tool> describe --tags` and its package.json version, and
  `<tool> --version` for any installed binary. If a checkout is the wrong major, check out the right
  tag / install the right version FIRST. State the verified versions at the top of your output.
  If the AUDIT TARGET is a branch/SHA of THIS repo (not a reference tool): `git fetch origin` FIRST,
  pin `origin/<branch>@<sha>` (NEVER a local worktree/branch — it routinely lags origin), and verify
  `git merge-base --is-ancestor <sha> origin/<branch>` before reading. State the pinned SHA at the top.
GATE 2 — EMPIRICAL: A gap is NOT a finding until you reproduce it by RUNNING <REFERENCE> <MAJOR> AND nub
  on identical input and diffing the actual output. Source-reading and --help parsing are LEADS only.
  Build a minimal differential fixture per candidate; capture both commands + both outputs.
GATE 3 — DECISION RECORD: Drop any candidate that is a deprecated/removed flag in <REFERENCE> <MAJOR>,
  an npm-ism nub deliberately rejects, one of nub's intentional pnpm-compat divergences, or already
  decided/built. Cross-check AGENTS.md "Core design positions", wiki/ decision docs, and .fray/ threads.
  Deeply evaluate what has ALREADY been discussed — surfacing a settled call is as bad as a false positive.
GATE 4 — REFUTE: After harvesting, re-verify every surviving candidate adversarially (try to REFUTE it:
  re-pin, re-reproduce, re-check the record). Treat "irreducible" or "cannot close" as claims that also
  require evidence: exhaust the documented mechanism surface and test plausible closures. Keep only
  what survives, with its reproduction evidence.
GATE 5 — COVERAGE + PRECISION: Enumerate the FULL surface from <REFERENCE> <MAJOR>'s own authoritative
  source (its --help / source), so nothing is missed; AND verify every surfaced item. Output ALL buckets:
  real gaps / confirmed-OK / intentional-divergence. Each real finding: reproduction command + both
  outputs + decision-record cross-check + severity + confidence.

Deliverable: a catalog at wiki/research/<topic>.md (all buckets) + a tight triaged list of REAL gaps for
the maintainer. Investigation-scope — do NOT land fixes; surface findings recommend-only.
```

When an audit reveals a NEW pitfall not covered above, fold it into the gate it strengthens.
