---
name: implementation-thread
description: >-
  Take a single task or issue through the COMPLETE engineering workup — plan →
  review the plan → implement → review the implementation (multi-lens where the
  blast radius warrants) → open a PR → await CI → fix CI → check for and
  integrate external reviews → RETURN CONTROL before merge. Invoke (via the
  Skill tool) whenever the user says "implementation-thread", or asks to drive a
  task/issue all the way through to a held PR. The defining shape: the effort is
  owned end-to-end by ONE sub-agent (an L1) that spawns its OWN L2 sub-agents for
  design and multi-lens review, carries continuity across phases, and PAUSES —
  comes to rest to surface a question UP — whenever a decision the human owns
  arises. It returns control to the orchestrator BEFORE merging unless explicitly
  told "merge it" / "all the way through". This is NOT a fray skill (it governs
  how one implementation effort runs internally); it COMPOSES with fray — an
  implementation-thread is one fray thread.
metadata:
  internal: true
---

# implementation-thread

The end-to-end workup for taking ONE task/issue from nothing to a reviewed, CI-green PR **held at the merge gate**. The unit of work is a single coherent change (a feature, a fix, a refactor) — not a campaign of many (that's fray's job).

## The shape — ONE owning L1, L2s underneath (the doubly-nested pattern)

The implementation-thread is owned by a **single L1 sub-agent** dispatched by L0 (the orchestrator). That L1 carries the work through every phase, and **spawns its own L2 sub-agents** for the sub-steps that benefit from a fresh context or parallelism — the design pass, the design review, the multi-lens review fan-out, an adversarial verification. The L1 holds continuity; L0 stays lean.

**Do NOT decompose an implementation-thread into a SERIES of L0-dispatched phase-agents** (design-agent returns to L0 → L0 dispatches review-agent → returns → L0 dispatches impl-agent → …). That bloats L0's context with every phase's full return, forces L0 to re-pack context into each fresh dispatch, and loses the continuity a single owner keeps for free. One L1 owns it; L2s do the fan-out work beneath it.

```
L0 (orchestrator)
└── L1  ── owns the implementation-thread end-to-end ──┐
      ├── L2: design                                    │ continuity lives here,
      ├── L2: design review                             │ not re-packed at every
      ├── L2: multi-lens review (×N, split by dimension)│ phase boundary
      └── L2: adversarial verification                  ┘
```

The series-of-L0-phases shape is only ever justified when you KNOW every single phase boundary ends in a heavy human decision — and even then the nested shape subsumes it, because the L1 can simply pause and surface (below). Default to nested.

## Pause to surface — a FIRST-CLASS pattern (use it; it is not a failure)

**An L1 (or any sub-agent) coming to rest to surface a question UP to its parent is a valid, encouraged pattern — not an incomplete handoff, not a failure.** A sub-agent does not have to run to terminal completion. When the L1 hits a decision it should not make alone, it **stops, states the decision crisply, and rests** — surfacing the question to L0, which surfaces it to the human. The L1 is later **resumed** (via SendMessage, with its full context intact) once the answer comes back, and continues from exactly where it paused.

Pause to surface when:
- A **maintainer-owned decision** appears — a default, a security posture, a product behavior, a brand/API/config/env surface, an architecture call. These are recommend-only; the L1 surfaces options + a recommendation and waits, it does NOT pick and land them.
- The design **forks** in a way a human should weigh in on (two defensible approaches with a real trade-off).
- A discovered fact **invalidates the task's premise** (the bug isn't real; the feature already exists; the approach is blocked).
- The change's blast radius turns out **larger than briefed** and warrants re-scoping.

How it works mechanically:
- The L1 writes the open question into its thread (`## Open questions`) and its rest message, then comes to rest.
- L0 reconciles, surfaces the question to the human, and on the answer **RESUMES the same L1 by id** (never cold-redispatches a replacement — that loses the runbook + context).
- The L1 moves the answered question into `## Decisions` and continues.

This is the release valve that makes the single-owning-L1 shape strictly better than a forced L0 checkpoint at every phase: you get the human-in-the-loop checkpoint **exactly when a decision needs it**, and nowhere else.

## The phases (the L1 owns all of these)

1. **Plan / design.** Map the REAL code (cite file:line; ground in code or an experiment, never memory). Produce candidate approaches + a recommendation. For a non-trivial change, run this as an L2 so the design is a clean artifact.
2. **Self-review the plan.** A fresh-context L2 critiques the design for elegance + minimalism + correctness, settles open calls, and BLESSES it or sends it back. Catch the design error before any code exists. (This is where most of the leverage is — a wrong design caught here is free.)
3. **Implement.** In an isolated git worktree (never the shared main tree — see Mechanics). The blessed design + tests + **docs** (a user-facing change isn't done until `site/content/docs/` reflects it). Run the pre-push local-verification loop.
4. **Self-review the implementation.** MANDATORY for any significant change: spawn fresh-context L2 reviewer(s) over the diff. **At least one reviewer MUST be an IMPACT-ANALYSIS pass** — tracing the diff's blast radius through the whole codebase (every call site / reader-writer / impl / match-arm of a changed symbol, plus downstream behavioral/serialized/cross-process effects), per the `impact-analysis` skill. This is a required lens, not optional. For large / security-critical / memory-or-UB-adjacent work, spawn **multiple L2s split by dimension** (correctness, impact analysis, the relevant safety axis, portability/platform, docs/test-honesty). Fix → re-review until clean.
5. **Open the PR.** From the worktree. If it resolves an issue, the body MUST carry `Closes #N` (verify before `gh pr create`). Report the URL. **Do not merge.**
6. **Await CI.** Watch with `--fail-fast` (the `ci-watch` skill / a background watcher). A failure is immediately actionable — diagnose it (it's often a test that assumed the dev host, not the change), fix in the worktree, re-push. Loop until green. Distinguish a real failure from a known-cosmetic flake.
7. **Integrate external reviews.** Before considering it ready, check the PR for external / bot reviews. Integrate the VALID findings (fold them in, re-verify); decline the invalid ones without conversational back-and-forth (terse or silent — never chat with a bot).
8. **Return control — HOLD at the merge gate.** The PR is green + reviewed + held. The L1 surfaces the final state to L0 (PR URL, what landed, review outcome, CI status, any behavior change needing ratification) and STOPS. **L0 / the human reviews and merges.**

## Return control before merge — the hard gate

**The implementation-thread STOPS at a green, reviewed, held PR. It does not merge** — unless the user explicitly said "merge it" / "take it all the way through to completion" / equivalent at dispatch time. This is the defining property of the skill: the human (or L0 on the human's behalf) gets the last look, especially for anything that changes a shipped behavior, a default, or a public surface. When the user DID pre-authorize the merge, the L1 still gets to green + reviewed first, then merges on a directly-verified-green rollup (not a watcher's exit code).

## Mechanics

- **Worktree + PR flow** (the `worktree` skill): substantive work lands via a PR from an isolated worktree off `origin/main`, own `CARGO_TARGET_DIR`. NEVER branch/reset/stash the shared main tree. Content/UI/docs-only changes are the exception that commit direct to main.
- **Pre-push local-verification loop** (AGENTS.md): incremental build → the EXACT CI cheap gates (`cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, scoped tests) → an e2e tmp-fixture run of the actual feature → promote a durable check into the suite. Green locally, push ONCE.
- **Model tiering:** the owning L1 and its judgment/engineering/review L2s are Opus (or Fable for the hardest synthesis); mechanical L2s can be cheaper. Every L2 prompt is SELF-CONTAINED (a fresh context carries nothing over).
- **Thread hygiene (fray):** the implementation-thread IS a fray thread — the L1 owns its `.fray/<slug>.md` (Goal · Status · Decisions · Open questions · Steps · Next step), edits it in place, and moves answered questions into Decisions. The hold-before-merge state is `blocked` (waiting on the human) or `active` (work in flight).

## Relationship to research-thread

If the deliverable is a PLAN or findings — not landed code — that's a **research-thread**, not this. A research-thread terminates `done` with the plan as its artifact; you spin up a SEPARATE implementation-thread later if/when the plan is actioned. Use this skill only when the deliverable is a shipped (held-at-gate) change.
