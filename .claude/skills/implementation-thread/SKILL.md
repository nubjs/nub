---
name: implementation-thread
description: >-
  Take a single task or issue through the COMPLETE engineering workup — grill the
  brief with the human → plan → review the plan → implement → review the
  implementation (multi-lens where the blast radius warrants) → open a PR → await
  CI → fix CI → check for and integrate external reviews → RETURN CONTROL before
  merge. Invoke (via the Skill tool) whenever the user says
  "implementation-thread", or asks to drive a task/issue all the way through to a
  held PR. The defining shape: the effort is owned end-to-end by ONE sub-agent (an
  L1) that spawns its OWN L2 sub-agents for design and multi-lens review, carries
  continuity across phases, and PAUSES — comes to rest to surface a question UP —
  whenever a decision the human owns arises. It opens by loading the `grilling`
  skill and running its round-based INTERVIEW whenever the change writes
  substantial new code, so the human settles the design tree before a line of it
  exists. It returns control to the orchestrator BEFORE merging unless explicitly
  told "merge it" / "all the way through". It governs how ONE implementation
  effort runs internally, so it composes with — rather than replaces — whatever is
  driving the wider campaign.
metadata:
  internal: true
---

# implementation-thread

The end-to-end workup for taking ONE task/issue from nothing to a reviewed, CI-green PR **held at the merge gate**. The unit is a single coherent change — not a campaign of many (that's the `orchestrator` skill).

## Scope gate — is this even an implementation-thread?

**This is the workup for a SUBSTANTIAL change.** A change confined to a file or two, whose correctness its own diff plus the pre-push loop demonstrates, does not get a design pass, an L2 fleet, or a multi-lens review — you write it, verify it, and open the PR. Invoking this machinery on a small fix costs more than the fix, and every L2 hop returns a claim you then have to re-verify.

Use the full shape when the change crosses module boundaries, touches a default / security posture / public surface / serialized format, or has effects you can't hold in your head while reading the diff. Otherwise take only the phases you need — **the phase list is a menu sized to the change, and the L2 counts are ceilings, never targets.**

**The one phase with its own trigger is the interview.** Volume of NEW code is the tell: if you are about to write hundreds of lines that do not exist yet, or the change introduces a surface someone else will have to live with — a command, a flag, a config field, an error taxonomy, a file format, a public type — the design tree has branches and you have been picking them silently. Grill it. A bug with one right answer does not get an interview; a repro you can't reproduce gets exactly one round.

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

- A **round of the opening interview** is ready to put to the human — the highest-volume pause by far, and the one the section below is entirely about.
- A **maintainer-owned decision** appears — a default, a security posture, a product behavior, a brand/API/config/env surface, an architecture call. Recommend-only: surface options + a recommendation, don't land it.
- The design **forks** in a way a human should weigh in on.
- A discovered fact **invalidates the task's premise** (the bug isn't real; the feature already exists).
- The blast radius turns out **larger than briefed** and warrants re-scoping.

Mechanically: the L1 writes the question into its thread (`## Open questions`) and its rest message. L0 surfaces it to the human and, on the answer, **RESUMES the same L1 by id** — never cold-redispatches a replacement, which loses the runbook and context. The L1 moves the answered question into `## Decisions` and continues.

## Grill the brief — the interview that comes before the design

**A brief that arrives as a paragraph of prose has not been specified, it has been gestured at.** The defects that cost the most in this workup are not the ones review catches. They are the ones where the code was correct and the thing built was wrong, and every one of those traces back to a decision nobody made out loud — a default someone assumed, a surface nobody named, an edge case that only existed in one person's head. Review cannot find those, because there is nothing incorrect to find. So when the change writes substantial new code, the first phase is not design. It is an **interview**, and you run it against the human until the shape of the thing is settled and nothing is left silently assumed.

**Load the `grilling` skill and run it — do not improvise an interview.** That skill owns the method and is the single source of truth for it: the design tree, the frontier, round-based questioning, the question format, the split between facts you go and find and decisions only the human can make, and the confirmation gate at the end. It lives in this repo at [`.claude/skills/grilling/`](../grilling/SKILL.md), vendored byte-verbatim from [mattpocock/skills](https://github.com/mattpocock/skills) under MIT — a clone is enough, and it is not something a contributor has to have installed. If your agent does not auto-trigger skills, read that file directly and follow it; the tell that it never loaded is a round arriving with no recommendations attached, which is a model improvising an interview rather than running this one.

Everything below is only what that skill cannot know about this workup.

### A round costs a pause here, not a paragraph

**Batching the frontier is load-bearing in this workup, not a stylistic default.** In a live chat one question costs a paragraph; here it costs a full pause, a hop up to the human, and a resume. Forty sequential questions is forty round-trips and a thread that takes a week — the same forty in four rounds is four. So ask the whole frontier every time, and treat a one-question round as a defect unless the frontier honestly holds one question.

The same arithmetic sharpens the skill's facts-versus-decisions rule. **A question you could answer by looking is not a question, it is your homework** — and here it costs a round-trip you can never get back, so read the code, run the experiment, or dispatch an L2 rather than asking the human anything the repo can tell you.

### Where the interview happens

**Grill wherever the human is reachable in real time.** If a session with the human present is about to dispatch this workup, it runs the interview FIRST and hands a settled design down — that is strictly cheaper than an owner discovering the same questions from inside a sub-agent and paying a pause for each round. When the owner was dispatched cold against a loose brief, its first act is round one as a pause, surfaced up by the ordinary mechanism above.

Either way the rounds land in the thread note that already exists for them: open questions go under `## Open questions`, and each answer moves to `## Decisions` the moment it is settled. The design tree IS those two headings over time.

### What the confirmation bounds

The interview ends where `grilling` says it ends — at an explicit confirmation of shared understanding, never merely at an empty frontier. **What that confirmation additionally does here is flip the bias, hard.** This phase is the one place in the workup where the bias runs toward asking; everywhere after it the standing rule holds and you decide. A fork you could have surfaced in round three and instead raise mid-implementation costs the thread a pause it should never have paid, so front-load ruthlessly and treat a late question as evidence the interview was cut short. Grilling is a front-loaded budget, never a standing license to ask.

### Ungrillable questions

**Some questions cannot be settled by talking, and grinding on one is how a session balloons.** "How should this feel", "one command or three", "is this output readable" — the human has nothing to react to, so they guess, you rephrase, and the scope grows to fill the uncertainty. Stop the round. Build the throwaway version (the `ad-hoc-test` fixture machinery is enough — this is scaffolding, it does not get a PR), put it in front of them, and take the one-line answer that comes back.

## The phases (the L1 owns all of these)

1. **Grill the brief.** Load the `grilling` skill and run its interview, whenever the change writes substantial new code or introduces a surface. Ends at an explicitly confirmed shared understanding, and it ends BEFORE the design phase — you are settling *what* is being built, not yet *how*. Skip it for a change with one right answer.
2. **Plan / design.** Map the REAL code (cite file:line; ground in code or an experiment, never memory). Produce candidate approaches + a recommendation. For a non-trivial change, run this as an L2 — and brief it with the settled decisions, not the original prose, or it will re-litigate every branch the interview just closed.
3. **Self-review the plan.** A fresh-context L2 critiques the design for elegance, minimalism, and correctness, settles open calls, and blesses it or sends it back. Most of the leverage is here — a wrong design caught before any code exists is free.
4. **Implement.** In an isolated git worktree. The blessed design + tests + **docs** (a user-facing change isn't done until `site/content/docs/` reflects it). Run the pre-push local-verification loop.
5. **Verify by RUNNING it — an ad-hoc fixture sweep against a built binary.** The step that finds real defects, and the one most often skipped in favor of another read of the diff. Build the binary with your change, drive the behavior you touched against real fixtures, and hunt for what you broke *somewhere you weren't looking*. Build the sweep to FALSIFY: if every fixture passed on the first run, you tested your intent. Load the `ad-hoc-test` skill. Promote durable checks into committed tests. **Reviewers structurally cannot find the bugs that matter most here** — they read code and hypothesize, so they miss the silent wrong answer (the resolution that returns a different module with no error), reachable only by executing the thing and checking *which* file answered.

   **Then self-review the diff IN-THREAD.** You hold the richest context, so your own read beats a fresh agent that must re-derive it. Spawning a dedicated reviewer is an ESCALATION for a change that earns it — wide blast radius, security posture, serialized format, memory/UB-adjacent — not a default leg. When you do escalate, an `impact-analysis` pass is the most valuable lens to buy. Never let a review round substitute for the sweep.
6. **Open the PR** from the worktree. If it resolves an issue, the body MUST carry `Closes #N` (verify before `gh pr create`). Report the URL. **Do not merge.**
7. **Await CI.** Watch with `--fail-fast` (the `ci-watch` skill). A failure is immediately actionable — diagnose (it's often a test that assumed the dev host), fix in the worktree, re-push. Loop until green.
8. **Integrate external reviews.** Check the PR for external/bot reviews. Fold in the valid findings and re-verify; decline the invalid ones tersely or silently — never chat with a bot.
9. **Return control — HOLD at the merge gate.** Surface the final state to L0 (PR URL, what landed, review outcome, CI status, any behavior change needing ratification) and STOP.

## Return control before merge — the hard gate

**The implementation-thread STOPS at a green, reviewed, held PR. It does not merge** — unless the user explicitly said "merge it" / "take it all the way through" at dispatch time. The human (or L0 on their behalf) gets the last look, especially for anything changing a shipped behavior, a default, or a public surface. When the merge WAS pre-authorized, the L1 still gets to green + reviewed first, then merges on a directly-verified-green rollup — not a watcher's exit code.

## Mechanics

- **Worktree + PR flow** (the `worktree` skill): substantive work lands via a PR from an isolated worktree off `origin/main`. Never branch/reset/stash the shared main tree. Content/UI/docs-only changes commit direct to main.
- **Pre-push local-verification loop** (AGENTS.md): incremental build → the EXACT CI cheap gates (`NUB_ALLOW_INCOMPLETE_RUNTIME=1 cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, scoped tests) → an e2e tmp-fixture run of the actual feature → promote a durable check into the suite. Green locally, push ONCE. The env var is what CI's clippy job sets; without it `--all-features` panics on an incompletely staged `runtime/`.
- **Model tiering:** tier each L2 by the judgment its task needs, not by the fact that it belongs to an implementation-thread. A repro, a harvest, a grep-and-report, a doc edit, or a CI watch is Sonnet or Haiku work — or yours. Every L2 prompt is self-contained, which is itself a cost: if packing the context takes longer than answering the question, answer it.
- **Thread hygiene:** the L1 keeps ONE living note for the effort — Goal · Status · Decisions · Open questions · Steps · Next step — in whatever scratch directory its harness provides, and moves each answered question out of Open questions and into Decisions. The hold-before-merge state is `blocked`; work in flight is `active`.

## Relationship to the other profiles

If the deliverable is not landed code, this is the wrong profile. A settled DESIGN is a **plan thread** (the `plan-thread` skill); a FACT or a set of findings is a **research thread** (the `research-thread` skill), which terminates `done` with its write-up as the artifact. An implementation-thread follows if that design or finding is actioned.

**The interview does not turn this into a plan thread — the deliverable still decides.** Grilling here is bounded to settling the brief for code being written in this same thread, and it ends at a lock. What it sometimes uncovers is that the effort was never ready for code: two approaches both defensible on grounds nobody has measured, a scope that is really a campaign, an ungrillable question at the center rather than the edge. That is the pause-to-surface case, and the honest move is to say so and let the effort demote to a plan thread, not to grind rounds against a design tree whose trunk is unsettled.
