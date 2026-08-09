---
name: dynamic-churn
description: Run ONE agent session, in series, through work far bigger than a single turn or a single context — a corpus sweep, a multi-phase epic, a cross-platform debugging campaign, "keep working through this until it's done". Invoke (via the Skill tool) whenever an effort needs a long-running serial loop over a task list that is continuously rewritten, or whenever asked to set up a keep-going loop, a heartbeat, a stop hook, or an autonomous work loop. The mechanism is ALREADY BUILT — `mcp__frizz__recurring_prompt` with `stop_hook` and `post_compaction`, armed by the agent itself, no config files and no hooks to install. This skill covers how to write the task list that makes the loop convergent, how to arm the loop safely (`get` before `start` — a thread holds only ONE), and how it ends (`ALLDONE` pauses, `action: "stop"` disarms). Works identically on Claude Code and Codex fray threads.
---

# Dynamic churn

A way to run **one** session, in **series**, through work far larger than one context — where the task list is a living document the agent rewrites as it learns, and the harness (not a human) is what tells it to keep going.

The name is the shape: the queue **churns** — items are added, split, reworded, closed and archived continuously — while a **dynamic** loop keeps the session turning over it.

## The mechanism already exists — do not build it

**[`mcp__frizz__recurring_prompt`](https://github.com/colinhacks/fray) is the engine.** One tool call arms a piece of text that frizz re-sends you on any of three triggers:

| Trigger | Fires | Use for |
| --- | --- | --- |
| `stop_hook` | every time you come to REST | driving the effort forward without a human re-prompting each turn |
| `post_compaction` | every time your context is COMPACTED, into the emptied window | surviving compaction — the prompt LINKS your task file, so the pointer comes back the moment you have lost everything else |
| `heartbeat_seconds` | on a clock (60–86400s), mid-turn | something that must be revisited on a schedule regardless of what you believe at the time |

**The ordinary churn shape is `stop_hook: true` + `post_compaction: true`.** No settings file, no hook script, no per-repo wiring — the agent arms it on its own thread, and it works the same on a Claude Code or a Codex fray thread (both worker prompts carry the tool).

⛔ **Do not hand-roll this with agent hooks.** Beyond duplicating a tool that already exists, the obvious wiring silently fails: `PostCompact` cannot reach the model on *either* agent — Claude Code's handler returns only `userDisplayMessage` ("stdout shown to user"), and Codex's `post-compact.command.output` schema has no `additionalContext` field at all. Frizz delivers its own message and is unaffected.

⛔ **Do not use `CronCreate` or `ScheduleWakeup`.** They cannot fire in the runtime frizz runs you in: their gate stays shut while any background task of yours is outstanding — exactly when a wake would matter.

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

Put it in the thread's own scratch directory, so it is per-effort and outlives every compaction:

```
.frizz/threads/<session-id>/TASKS.md
```

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

## Step 2 — arm the loop

**Call `get` before any `start` that is not a fresh arming.** A thread holds **at most one** recurring prompt, so `start` REPLACES whatever is there — and the text you are about to destroy may not be yours: the human can edit it in the thread footer, and a compaction can take your own memory of arming it.

```
mcp__frizz__recurring_prompt({ action: "get" })     // read what is armed, change nothing
```

Then arm it. The prompt is delivered **verbatim, as a user turn, with none of your current context** — so it must be self-contained and name the task file by absolute path:

```
mcp__frizz__recurring_prompt({
  action: "start",
  stop_hook: true,
  post_compaction: true,
  prompt: "<the KEEP GOING text below>"
})
```

## The KEEP GOING text

Adapt this — it is the payload, and it is the whole standing instruction:

> ## KEEP GOING
>
> Your task list is at `/abs/path/.frizz/threads/<id>/TASKS.md`.
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
> **DONE CONDITION:** <the properties that must hold — not the tasks that must close>. Only then reply ALLDONE.

Worth keeping when you adapt it: **one item at a time**, **in series**, **the list is the source of truth**, **write for a reader with no context**, **what to do when blocked**, and **an explicit done condition**.

**Write the done condition as properties, not tasks** — otherwise the loop either stops early or never stops. Real example:

> Only when the harness is stable across all platforms, the catalog is generated across the full corpus, and the feature can realistically ship default-on without breaking the vast majority of users — only then ALLDONE.

## How it ends — `ALLDONE` pauses, `stop` disarms

These are different, and conflating them is how a loop either restarts unexpectedly or runs forever:

| | Effect |
| --- | --- |
| `ALLDONE` on its own line, as the **final word** of the final message | Frizz stops bumping — but it is a **fold**, not a disarm. A later message that omits it **re-opens the loop**. It means "nothing actionable right now." |
| `mcp__frizz__recurring_prompt({ action: "stop" })` | Genuinely disarmed. This is what you call when the effort is over. |
| The human toggles it off in the thread footer | Also disarmed — which is why you `get` before you `start`. |

**Disarm when the work it drives is finished.** One left armed on a finished thread bumps it forever.

## Traps

**1. The prompt arrives with zero context.** It is delivered verbatim as a user turn. Relative paths, "the file I mentioned", "continue where we left off" — all meaningless on delivery. Absolute paths and self-contained instructions only.

**2. `post_compaction` without a LINK is wasted.** The arming is what survives, not the file. The prompt must name the task file, or the emptied window gets a nudge with nothing to nudge toward.

**3. Churn does not make an agent ask — it makes it thrash.** With nobody re-prompting, a loop that hits a blocker it cannot clear will keep attacking it. Measured on a live test session: given two tasks blocked by a permission prompt, the loop tried 15+ approaches and dispatched sub-agents before concluding, rather than stopping after the first refusal. Put human-owned decisions in canon under a ⛔ marker, **and say explicitly what to do when blocked** — record it, move on, raise it at the end.

**4. The task file is the memory, not the transcript.** Anything recorded only in conversation is gone at the next compaction. Write decisions, measurements and traps into the file **as they happen**, mid-work — not at the end of the turn.

**5. `heartbeat_seconds` talks over you.** It is delivered mid-turn, at your next tool boundary. That is right for "re-check this on a clock no matter what," and wrong as a way to poll something that could wake you instead. Sub-minute cadences buy no promptness.

## Related

- **[`mcp__frizz__timer`](https://github.com/colinhacks/fray)** — a one-off instead of a repeat: fire once at a given instant, then gone. A thread may hold many. Use it to revisit something at a specific time; use `recurring_prompt` when it must repeat.
- **The scratch directory** (`.frizz/threads/<session-id>/`) — where the task file and any per-sub-agent notes live. Give each sub-agent its OWN file; never have several children edit one document.
- **[`orchestrator`](../orchestrator/SKILL.md)** — the opposite shape. Churn is one session in series; the orchestrator is many agents in parallel. If the work fans out rather than queues up, use that instead.
