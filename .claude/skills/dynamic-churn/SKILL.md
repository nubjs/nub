---
name: dynamic-churn
description: Run ONE agent session, in series, through work far bigger than a single turn or a single context — a corpus sweep, a multi-phase epic, a cross-platform debugging campaign, "keep working through this until it's done". Invoke (via the Skill tool) whenever an effort needs a long-running serial loop over a task list that is continuously rewritten, or whenever asked to set up a keep-going loop, a heartbeat, a stop hook, or an autonomous work loop. Covers the thing that actually decides whether such a loop converges or thrashes — how the task file is structured and what the standing prompt says — not the wiring, which your harness already provides and already documents.
metadata:
  internal: true
---

# Dynamic churn

A way to run **one** session, in **series**, through work far larger than one context — where the task list is a living document the agent rewrites as it learns, and the harness (not a human) is what tells it to keep going.

The name is the shape: the queue **churns** — items are added, split, reworded, closed and archived continuously — while a **dynamic** loop keeps the session turning over it.

## The loop mechanism is not your problem — the task list is

Every harness worth running this on already ships the loop: a standing prompt re-delivered when you come to rest and again when your context is compacted, armed by the agent itself with no config file and no hook script. Read your own harness contract for the exact affordance and its argument names; it also tells you which of its schedulers genuinely fire and which are inert.

Two things are worth knowing before you reach for something else:

⛔ **Do not hand-roll it with agent hooks.** Beyond duplicating a tool that already exists, the obvious wiring silently fails: a post-compaction hook cannot reach the MODEL on either agent — Claude Code's handler returns only `userDisplayMessage` ("stdout shown to user"), and Codex's `post-compact.command.output` schema has no `additionalContext` field at all. A harness that delivers its own message is unaffected; a hook you write is not.

⛔ **Read back whatever is already armed before you replace it.** A thread holds at most one standing prompt, so arming is destructive — and the text you would destroy may be the human's, not yours.

**Everything below is the part that decides whether the loop converges.** The wiring is five minutes; the task file is the work.

## When this is the right pattern

| Use it | Don't |
| --- | --- |
| Work measured in hours-to-days that decomposes into many small sequential steps | A single task you can finish in one turn |
| A campaign whose next step depends on what the last step found — a corpus sweep, a cross-platform debugging push, a multi-phase epic | Independent prongs that should fan out to parallel sub-agents instead |
| Work where the human is away and re-prompting each turn is the bottleneck | Anything whose next step needs a human decision — churn will guess instead of asking |
| Work you want done synchronously and iteratively (drive a VM now) | Work that is mostly waiting on an external system — that is a wait, not churn |

**Churn removes the human from the loop. That is the whole point and also the whole risk.** Before arming it, make sure the task list carries the standing constraints the human would otherwise enforce in conversation — what must not be touched, what needs approval, what "done" means.

## Step 1 — write the task file FIRST

**If there is not already a big task list, write one before arming anything.** A churn loop over a vague goal produces a hundred turns of invented work. The list is what makes the loop convergent.

Put it in the thread's own scratch directory, so it is per-effort and outlives every compaction — `TASKS.md` is the conventional name.

**Structure it as CANON + LIVE QUEUE.** This split is what stops a long loop drifting — the agent rewrites the bottom half constantly and must never rewrite the top half:

```markdown
# EPIC — <the goal in one line>

**This file is the ONLY thing that survives compaction.** §A is canon (changes only on explicit human instruction); §B is the live queue. If anything in §B contradicts §A, §A wins. If a plan of mine contradicts §A, the plan is the defect.

# §A — CANON

## A1. THE MISSION            <- what success IS, stated as properties, not tasks
## A2. NON-NEGOTIABLES        <- the ⛔ list: what must never happen, what needs a human
## A3. STANDING AUTHORITY     <- what to decide alone vs escalate
## A4. THE PER-ITEM METHOD    <- how each item gets worked and what closes it
## A5. HARD-WON FACTS         <- every trap already paid for, so it is not paid twice

# §B — LIVE QUEUE

## PHASE 1 — <name>
*Exit: <the condition that closes this phase>*

- [x] **1.1** <what was done, what was measured, on what artifact>
- [ ] **1.2** <the next concrete action>
```

Rules that make the list work rather than rot:

- **Granular.** An item is one focused piece of work, not a project.
- **Closed items keep their evidence.** `- [x] 1.1 DONE — run 31150302075, records 1 → 11` is worth ten times `- [x] 1.1 done`. You will re-read this having forgotten everything.
- **Every phase carries an explicit exit condition.** Without it nothing is ever finished.
- **Standing constraints are rules, not tasks** — keep them in a section that never gets checked off.
- **Cap it at ~400 lines.** Past that, rename the file to archive it (`CANON-archive-<date>.md`) and start fresh carrying only unfinished work plus canon.
- **Plain language, no private jargon.** The reader is you, three compactions from now, with none of today's context.

## Step 2 — write the KEEP GOING text

The standing prompt is delivered **verbatim, as a user turn, with none of your current context**. So it must be self-contained and name the task file **by absolute path** — relative paths, "the file I mentioned" and "continue where we left off" are all meaningless on delivery. A standing prompt that does not LINK the task file is wasted: the arming is what survives, not your memory of the file.

Adapt this — it is the payload, and it is the whole standing instruction:

> ## KEEP GOING
>
> Your task list is at `/abs/path/to/TASKS.md`.
>
> It is the canonical source of truth for your work here. Use it to document your progress and maintain a list of work yet to do. It is paramount that it is kept up to date. It can be granular. You should not hesitate to update it frequently as new problems arise. It is a living document. Reconcile/update/add to it frequently.
>
> If it gets too crowded, rename the current task list to save it for posterity, then wipe it and start fresh with only unfinished work. It should be at most 400 lines long.
>
> Work through your task list one item at a time. Focus on one thing until you have high confidence in it. If necessary spin up subagents to review your work. Work in series. See things through to completion. Use synchronous approaches (a VM you can drive now) over asynchronous ones (a CI round trip) whenever both are available.
>
> Record your thought processes in a clear way as you go along. It should not contain your own internal jargon. It should be as simple as possible without losing information. Use Markdown, lists, tables, headings, bold etc as useful for emphasis and visual clarity.
>
> If an item is blocked by something you cannot clear yourself, record the blocker in the task file and move to the next item. Do not spend the loop re-attacking it.
>
> Before acting, re-read the canon section of the task list to maintain perspective. You are empowered to make decisions yourself. Research prior art before designing — search the web, clone repos. Document any far-reaching decision in the task file so it can be presented to the human later.
>
> **DONE CONDITION:** <the properties that must hold — not the tasks that must close>.

Worth keeping when you adapt it: **one item at a time**, **in series**, **the list is the source of truth**, **write for a reader with no context**, **what to do when blocked**, and **an explicit done condition**.

**Write the done condition as properties, not tasks** — otherwise the loop either stops early or never stops. Real example:

> Only when the harness is stable across all platforms, the catalog is generated across the full corpus, and the feature can realistically ship default-on without breaking the vast majority of users — only then are we done.

**Disarm the loop when the work it drives is finished.** One left armed on a finished thread bumps it forever. Note that whatever "nothing actionable right now" signal your harness offers is usually a *fold*, not a disarm — a later message that omits it re-opens the loop.

## The two traps that are actually about churn

**1. Churn does not make an agent ask — it makes it thrash.** With nobody re-prompting, a loop that hits a blocker it cannot clear will keep attacking it. Measured on a live test session: given two tasks blocked by a permission prompt, the loop tried 15+ approaches and dispatched sub-agents before concluding, rather than stopping after the first refusal. Put human-owned decisions in canon under a ⛔ marker, **and say explicitly what to do when blocked** — record it, move on, raise it at the end.

**2. The task file is the memory, not the transcript.** Anything recorded only in conversation is gone at the next compaction. Write decisions, measurements and traps into the file **as they happen**, mid-work — not at the end of the turn.

## Related

- **[`orchestrator`](../orchestrator/SKILL.md)** — the opposite shape. Churn is one session in series; the orchestrator is many agents in parallel. If the work fans out rather than queues up, use that instead.
- **Per-sub-agent notes** go in the scratch directory too. Give each sub-agent its OWN file; never have several children edit one document.
