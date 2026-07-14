---
name: audit-thread
description: Use when running a compatibility/parity AUDIT — enumerating where nub diverges from a reference it claims parity with (pnpm CLI grammar, a lockfile format, a Node behavior, a flag surface). Encodes the hard gates that stop an audit from surfacing false positives. Auto-triggers on "audit", "compat audit", "parity audit", "find all the gaps", "what are we missing vs <tool>".
metadata:
  internal: true
---

# Audit threads

An **audit** is a distinct thread profile — alongside the implementation thread and the research thread — whose deliverable is a CLEAN, COMPLETE, VERIFIED list of real gaps between nub and a reference it claims parity with. A single false positive surfaced to the maintainer destroys trust in the whole audit; the bar is zero garbage.

**Canonical methodology + rationale: AGENTS.md → "Audit threads — a distinct thread profile."** Read that section first — it holds the full WHY and the named scars. This skill is the OPERATIONAL surface: the gate checklist, the dispatch template every audit sub-agent must receive verbatim, the orchestration shape, and the running learnings log. (Same split as the `prose-writing` skill ↔ `PROSE.md`: the skill never re-derives the canonical doc, it operationalizes it.)

## The 5 gates (checklist — AGENTS.md is canonical)

1. **Pin the reference target surgically.** State the EXACT major (e.g. pnpm 10). VERIFY every reference checkout's version before reading it (`git -C .repos/<tool> describe --tags`, its `package.json` `version`, the installed tool's `--version`). Wrong-major reference = #1 garbage source.
2. **Empirical over source.** A candidate is not a finding until a differential fixture reproduces it by RUNNING the real pinned tool + nub on identical input and diffing. Source/`--help` reading = leads only.
3. **Cross-check the decision record.** Deprecated/removed flags, npm-isms nub rejects, the deliberate pnpm-compat divergences, and already-decided/already-built work are NOT findings. Filter against AGENTS.md "Core design positions", `wiki/` decision docs, and prior `.fray/` threads.
4. **Mandatory adversarial self-refutation.** Fresh-context reviewer(s) whose job is to REFUTE each surfaced finding (re-pin, re-reproduce, re-check the record); default to refuted when uncertain. Surface ONLY survivors, each with reproduction evidence. Never forward the raw breadth-pass output.
5. **Tier + deliverable.** Opus/Fable high+ for judgment AND refutation (Sonnet/Haiku may harvest breadth, but every item is Opus-verified). Thoroughness is two-dimensional: COVERAGE (enumerate the FULL surface from the pinned reference's own authoritative source) AND PRECISION (every item verified). Catalog → `wiki/research/<topic>.md` with all buckets explicit (real gaps / confirmed-OK / intentional-divergence); each finding records reproduction + decision-record cross-check + severity + confidence.

## Orchestration shape (in order)

enumerate the full surface (coverage) → harvest candidate gaps → cross-check the decision record → reproduce each against the pinned real tool (empirical) → adversarially refute in fresh context → surface ONLY survivors, with evidence.

Run it as a fray thread with individually-dispatched agents (an Opus audit lead that fans its own breadth/reproduction/refutation L2s, or orchestrator-driven L2s one stage at a time) — NOT a blind parallel Workflow fan-out that buries the gates.

**Companion lens — impact analysis when a finding is ACTIONED.** An audit is investigation-scope (recommend-only); it surfaces gaps, it does not land fixes. But the moment an audit finding becomes a code change, that change runs the standard significant-change self-review — which MANDATES at least one impact-analysis pass tracing the fix's blast radius through the codebase (see the `impact-analysis` skill + AGENTS.md). A parity fix that touches a shared verb/flag dispatch path routinely ripples to sibling commands; the impact-analysis lens is what keeps the fix from regressing them. Gate 4's adversarial refutation is about whether a FINDING is real; impact analysis is about whether a FIX is safe — distinct lenses, both required at their respective stages.

## Dispatch template (every audit sub-agent prompt MUST be self-contained — the L2 starts fresh)

```
You are running a <SCOPE> audit: find where nub diverges from <REFERENCE> <EXACT MAJOR> on <SURFACE>.
This is an AUDIT — the deliverable is a CLEAN, VERIFIED list of REAL gaps. A single false positive is
unacceptable. Follow all 5 gates; do not skip any.

GATE 1 — PIN: The target is <REFERENCE> <EXACT MAJOR>. BEFORE reading anything, verify the version of
  every reference you use: `git -C .repos/<tool> describe --tags` and its package.json version, and
  `<tool> --version` for any installed binary. If a checkout is the wrong major, check out the right
  tag / install the right version FIRST. State the verified versions at the top of your output.
GATE 2 — EMPIRICAL: A gap is NOT a finding until you reproduce it by RUNNING <REFERENCE> <MAJOR> AND nub
  on identical input and diffing the actual output. Source-reading and --help parsing are LEADS only.
  Build a minimal differential fixture per candidate; capture both commands + both outputs.
GATE 3 — DECISION RECORD: Drop any candidate that is a deprecated/removed flag in <REFERENCE> <MAJOR>,
  an npm-ism nub deliberately rejects, one of nub's intentional pnpm-compat divergences, or already
  decided/built. Cross-check AGENTS.md "Core design positions", wiki/ decision docs, and .fray/ threads.
  Deeply evaluate what has ALREADY been discussed — surfacing a settled call is as bad as a false positive.
GATE 4 — REFUTE: After harvesting, re-verify every surviving candidate adversarially (try to REFUTE it:
  re-pin, re-reproduce, re-check the record). Keep only what survives, with its reproduction evidence.
GATE 5 — COVERAGE + PRECISION: Enumerate the FULL surface from <REFERENCE> <MAJOR>'s own authoritative
  source (its --help / source), so nothing is missed; AND verify every surfaced item. Output ALL buckets:
  real gaps / confirmed-OK / intentional-divergence. Each real finding: reproduction command + both
  outputs + decision-record cross-check + severity + confidence.

Deliverable: a catalog at wiki/research/<topic>.md (all buckets) + a tight triaged list of REAL gaps for
the maintainer. Investigation-scope — do NOT land fixes; surface findings recommend-only.
```

## Learnings log — append a dated bullet for each NEW pitfall an audit reveals; map it to the gate it strengthens

- **2026-06-26 (pnpm-CLI-compat, first run — the scar):** read `.repos/pnpm` at **v11.3.0** against a **pnpm-10** target → flagged lowercase `-d/-e/-o` as "missing" when pnpm 10 uses uppercase `-D/-E/-O` and lowercase means other things there. → **Gate 1** (verify the reference version before reading).
- **2026-06-26:** asserted `npm_config_loglevel` "not honored" **from source-reading**; FALSE — `npm_config_loglevel=silent pnpm install` → zero output, pnpm honors it. → **Gate 2** (reproduce against the real tool, never infer from code).
- **2026-06-26:** counted deprecated / npm-ism / already-decided flags as gaps, inflating ~112 raw candidates that collapsed to ~1 real. → **Gate 3** (cross-check the decision record before surfacing).
- **2026-06-26:** surfaced the raw breadth-pass output without an adversarial re-verification pass → the catalog was mostly false positives. → **Gate 4** (refute in fresh context; surface only survivors with evidence).
