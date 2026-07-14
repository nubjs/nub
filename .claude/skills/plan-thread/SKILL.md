---
name: plan-thread
description: Use when an effort's APPROACH isn't settled yet and needs collaborative design BEFORE implementation — fleshing out how a feature/change should work, weighing options, resolving open questions with the human. The design-in-progress thread profile. Auto-triggers on "spin up a plan", "let's design", "plan thread", "how should we approach", "think through the approach".
metadata:
  internal: true
---

# Plan threads

A **plan thread** is one of the four fray thread profiles, alongside the *implementation thread* (build-the-decided-thing), the *research thread* (find-out-what's-true → `wiki/research/`), and the *audit thread* (verify-parity → `wiki/research/`, see the `audit-thread` skill). Its deliverable is a **settled design / approach** — not code, not findings, not a gap catalog. It is the thread you open when *how* to do something is the open question.

## The defining property: the deliverable is the DESIGN

A plan thread's work IS the thinking. You'd staff it with the human and/or a Plan/architect agent (Claude Code's `Plan` agent type) — **never an implementer**, because there's nothing settled to implement yet. Its `## Open questions` are the live work; its `## Decisions` accrete as questions resolve. When the design locks, the plan thread's job is done and an implementation thread takes over.

## Status lifecycle (fray)

- A plan thread carries `status: planning` (the fray status for "design-in-progress"; see the `fray` skill for the vocab). It is **non-terminal** but **parked** — it is NOT auto-surfaced in the per-turn / stop-hook nag (only `enqueued`/`active`/`blocked` nag the orchestrator). You pull a plan thread up deliberately via the `fray` board when you choose to work it — it does not chase you. (Legacy `plan`/`todo`/`needs-decision` are still accepted as read-aliases — `plan`/`todo`→`planned`, `needs-decision`→`blocked` — but write only the canonical words.)
- **THE TRANSITION RULE (load-bearing): a plan thread flips to `planned` the moment the design is LOCKED** — open questions resolved, approach decided, only implementation remaining. Planning *ends* by promoting to `planned` (or straight to `active` if you dispatch the implementer that turn). A plan thread that has no open design questions left but is still `status: planning` is mis-statused — promote it.
- Distinguish from `blocked`: that's waiting on ONE specific human yes/no. `planning` is the broader ongoing design with multiple open questions being worked collaboratively.

## How to run a plan thread

1. **Create the thread FIRST** (per fray: the `.fray/<slug>.md` exists before any dispatch), `status: planning`. Write the Goal (the objective + why) and seed `## Open questions` with the real unknowns.
2. **Work the open questions** — with the human (a plan thread routinely carries human-owned decisions: defaults, product behavior, API/config surface, architecture) and/or by dispatching a Plan/architect agent or a focused research/audit sub-thread to get the facts a decision needs. Ground every design claim in code or an experiment, never memory (the probing-methodology discipline).
3. **Move each answered question to `## Decisions` the instant it's settled** — a decided thing lives under Decisions, never lingering in Open questions. The thread always reads as current truth (no changelog).
4. **Lock the design + hand off:** when the approach is settled, record the final design in `## Decisions`, write the implementation handoff in `## Next step` (what an implementer should build), and **flip the status to `planned`** (or dispatch the implementer and go `active`). The implementation thread inherits the locked design.

## Boundaries with the other profiles

- A plan thread that needs a FACT to decide → spin a research or audit sub-effort, fold the result back, keep deciding. The plan thread owns the decision; the research/audit owns the fact.
- A plan thread does NOT land code. The instant code should be written, the design is locked → it becomes a `planned`/implementation thread.
- Don't let a plan thread become a place to park indefinite "someday" design — if it's not being actively worked toward a lock, it's either `planned` (scoped, awaiting actioning) or should be dismissed.

## Anti-redundancy with `planned`

`planning` and `planned` are NOT the same parked state: `planning` = the design itself is still the work (you'd dispatch an architect or talk to the human); `planned` = the design is DONE and the next dispatch is an implementer. The whole point of the split is to tell, at a glance, whether a parked thread needs *thinking* or *building*. Keep the boundary crisp by always promoting `planning`→`planned` at design-lock.
