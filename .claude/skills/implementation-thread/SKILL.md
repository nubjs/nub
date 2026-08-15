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
  told "merge it" / "all the way through". It governs how ONE implementation
  effort runs internally, so it composes with — rather than replaces — whatever
  is driving the wider campaign.
metadata:
  internal: true
---

# implementation-thread

The end-to-end workup for taking ONE task/issue from nothing to a reviewed, CI-green PR **held at the merge gate**. The unit is a single coherent change — not a campaign of many (that's the `orchestrator` skill).

## Scope gate — is this even an implementation-thread?

**This is the workup for a SUBSTANTIAL change.** A change confined to a file or two, whose correctness its own diff plus the pre-push loop demonstrates, does not get a design pass, an L2 fleet, or a multi-lens review — you write it, verify it, and open the PR. Invoking this machinery on a small fix costs more than the fix, and every L2 hop returns a claim you then have to re-verify.

Use the full shape when the change crosses module boundaries, touches a default / security posture / public surface / serialized format, or has effects you can't hold in your head while reading the diff. Otherwise take only the phases you need — **the phase list is a menu sized to the change, and the L2 counts are ceilings, never targets.**

## The shape — ONE owning L1, L2s underneath

> **If you are yourself a dispatched sub-agent, this skill's fan-out does not apply to you.** Take its phase methodology and run the phases INLINE — you are the L1 it describes, and an L1 does not spawn a fleet of its own. The repo-wide depth cap in `AGENTS.local.md` ("fan-out is ONE level deep") outranks the diagram below. Only a top-level session dispatches.

A **single L1 sub-agent** dispatched by L0 carries the work through every phase and **spawns its own L2 sub-agents** for sub-steps that benefit from a fresh context or parallelism. The L1 holds continuity; L0 stays lean.

```
L0 (orchestrator)
└── L1  ── owns the implementation-thread end-to-end ──┐
      ├── L2: design                                    │ continuity lives here,
      ├── L2: design review                             │ not re-packed at every
      ├── L2: multi-lens review (×N, split by dimension)│ phase boundary
      └── L2: adversarial verification                  ┘
```

**Do NOT decompose it into a SERIES of L0-dispatched phase-agents.** That bloats L0's context with every phase's full return, forces L0 to re-pack context into each fresh dispatch, and loses the continuity a single owner keeps for free.

## Pause to surface — a first-class pattern

**An L1 coming to rest to surface a question UP is valid and encouraged — not an incomplete handoff.** When it hits a decision it should not make alone, it stops, states the decision crisply, and rests; it is later **resumed via `SendMessage`** with full context intact and continues from where it paused.

Pause when:

- A **maintainer-owned decision** appears — a default, a security posture, a product behavior, a brand/API/config/env surface, an architecture call. Recommend-only: surface options + a recommendation, don't land it.
- The design **forks** in a way a human should weigh in on.
- A discovered fact **invalidates the task's premise** (the bug isn't real; the feature already exists).
- The blast radius turns out **larger than briefed** and warrants re-scoping.

Mechanically: the L1 writes the question into its thread (`## Open questions`) and its rest message. L0 surfaces it to the human and, on the answer, **RESUMES the same L1 by id** — never cold-redispatches a replacement, which loses the runbook and context. The L1 moves the answered question into `## Decisions` and continues.

## The phases (the L1 owns all of these)

1. **Plan / design.** Map the REAL code (cite file:line; ground in code or an experiment, never memory). Produce candidate approaches + a recommendation. For a non-trivial change, run this as an L2.
2. **Self-review the plan.** A fresh-context L2 critiques the design for elegance, minimalism, and correctness, settles open calls, and blesses it or sends it back. Most of the leverage is here — a wrong design caught before any code exists is free.
3. **Implement.** In an isolated git worktree. The blessed design + tests + **docs** (a user-facing change isn't done until `site/content/docs/` reflects it). Run the pre-push local-verification loop.
4. **Verify by RUNNING it — an ad-hoc fixture sweep against a built binary.** The step that finds real defects, and the one most often skipped in favor of another read of the diff. Build the binary with your change, drive the behavior you touched against real fixtures, and hunt for what you broke *somewhere you weren't looking*. Build the sweep to FALSIFY: if every fixture passed on the first run, you tested your intent. Load the `ad-hoc-test` skill. Promote durable checks into committed tests. **Reviewers structurally cannot find the bugs that matter most here** — they read code and hypothesize, so they miss the silent wrong answer (the resolution that returns a different module with no error), reachable only by executing the thing and checking *which* file answered.

   **Then self-review the diff IN-THREAD.** You hold the richest context, so your own read beats a fresh agent that must re-derive it. Spawning a dedicated reviewer is an ESCALATION for a change that earns it — wide blast radius, security posture, serialized format, memory/UB-adjacent — not a default leg. When you do escalate, an `impact-analysis` pass is the most valuable lens to buy. Never let a review round substitute for the sweep.
5. **Open the PR** from the worktree. If it resolves an issue, the body MUST carry `Closes #N` (verify before `gh pr create`). Report the URL. **Do not merge.**
6. **Await CI.** Watch with `--fail-fast` (the `ci-watch` skill). A failure is immediately actionable — diagnose (it's often a test that assumed the dev host), fix in the worktree, re-push. Loop until green.
7. **Integrate external reviews.** Check the PR for external/bot reviews. Fold in the valid findings and re-verify; decline the invalid ones tersely or silently — never chat with a bot.
8. **Return control — HOLD at the merge gate.** Surface the final state to L0 (PR URL, what landed, review outcome, CI status, any behavior change needing ratification) and STOP.

## Return control before merge — the hard gate

**The implementation-thread STOPS at a green, reviewed, held PR. It does not merge** — unless the user explicitly said "merge it" / "take it all the way through" at dispatch time. The human (or L0 on their behalf) gets the last look, especially for anything changing a shipped behavior, a default, or a public surface. When the merge WAS pre-authorized, the L1 still gets to green + reviewed first, then merges on a directly-verified-green rollup — not a watcher's exit code.

## Mechanics

- **Worktree + PR flow** (the `worktree` skill): substantive work lands via a PR from an isolated worktree off `origin/main`. Never branch/reset/stash the shared main tree. Content/UI/docs-only changes commit direct to main.
- **Pre-push local-verification loop** (AGENTS.md): incremental build → the EXACT CI cheap gates (`NUB_ALLOW_INCOMPLETE_RUNTIME=1 cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, scoped tests) → an e2e tmp-fixture run of the actual feature → promote a durable check into the suite. Green locally, push ONCE. The env var is what CI's clippy job sets; without it `--all-features` panics on an incompletely staged `runtime/`.
- **Model tiering:** tier each L2 by the judgment its task needs, not by the fact that it belongs to an implementation-thread. A repro, a harvest, a grep-and-report, a doc edit, or a CI watch is Sonnet or Haiku work — or yours. Every L2 prompt is self-contained, which is itself a cost: if packing the context takes longer than answering the question, answer it.
- **Thread hygiene:** the L1 keeps ONE living note for the effort — Goal · Status · Decisions · Open questions · Steps · Next step — in whatever scratch directory its harness provides, and moves each answered question out of Open questions and into Decisions. The hold-before-merge state is `blocked`; work in flight is `active`.

## Relationship to the other profiles

If the deliverable is not landed code, this is the wrong profile. A settled DESIGN is a **plan thread** (the `plan-thread` skill); a FACT or a set of findings is a **research thread** (the `research-thread` skill), which terminates `done` with its write-up as the artifact. An implementation-thread follows if that design or finding is actioned.
