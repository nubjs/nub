---
name: epic
description: Run a long autonomous effort — days of work, many compactions, one mission — without losing the plot. Invoke (via the Skill tool) when you are handed a mission rather than a task ("drive this epic", "work on this until it's done", "keep going autonomously", a recurring heartbeat prompt), or when you notice an effort has outgrown a single context. Carries the three durable artifacts an epic needs (a CANON section that outranks memory, a working scratch doc, a task list), how to write an EVERGREEN heartbeat that re-grounds you instead of describing a moment, the altitude discipline that stops you drilling into mechanics while the mission drifts, and the verification rules that keep a long unsupervised run from accumulating confident wrong beliefs. Distinct from `orchestrator` (which is about DISPATCHING sub-agents) and `implementation-thread` (one task end to end) — this is about sustaining ONE mission across many contexts, whether or not you delegate.
---

# epic

An epic is a **mission that outlives your context window**. You will be compacted, resumed, and re-prompted many times. Everything that matters must live outside your head, and the structure below is what puts it there.

The failure this skill prevents is not incompetence. It is **drift**: each individual turn is defensible, and the mission quietly stops being served. You fix a build error, then another, then verify a flag, and eight hours later the thing you were actually asked to achieve has not moved.

## The three artifacts

Keep them separate. Collapsing them is what makes each one useless.

| artifact | holds | changes |
|---|---|---|
| **§0 CANON** (top of the scratch doc) | the mission, non-negotiables, the definition of done, standing authority | only on explicit human instruction |
| **the scratch doc** (below §0) | current verified state, decisions you made and why, traps, open findings | constantly — it is REPLACED, never appended to |
| **the task list** (a real tool, not prose) | the ordered queue of work | constantly — one `in_progress` at a time |

### §0 CANON

~100 lines at the very top, marked persistent and non-editable-except-by-the-human. State inside the section that it outranks everything below it and that a plan contradicting it is a defect in the plan.

**In it:** what the thing IS, the properties that must hold, what is banned and why, the acceptance test, any standing authority the human granted ("full autonomy on X", "never do Y").
**Not in it:** findings, measurements, status, to-dos. Those rot; canon must not.

The tell that you have lost canon: you are about to assert something about the design and your confidence comes from *recall* rather than from having just read it.

### The scratch doc

The reasoning that would be expensive to reconstruct. Compaction preserves *what you did* and destroys *why* — so write the why, mid-work, as you go.

Write: the approach and the approaches you REJECTED with the numbers that killed them; decisions the human made, in their words; what is VERIFIED by running versus merely believed; the traps that already cost you time.

**It is a working document, not a log.** When something is superseded, replace it. When something is finished, delete it. If a section is only interesting as history, it does not belong. A scratch doc that grows monotonically has stopped being read.

**⛔ RECORDED CONSTRAINTS ROT EXACTLY LIKE FINDINGS — and nothing re-tests them.** "That resource is unavailable", "the account is frozen", "that box is unreachable" are OBSERVATIONS with a timestamp, but they read as permanent and they quietly define the shape of everything you attempt. On one run a note that the VMs were off-limits (true of a *different* cloud provider) plus a stale "unreachable" survived a whole session, while an idle admin box that would have cut the iteration loop from twenty minutes to seconds sat unused the entire time. **Before you build a workaround, re-verify the blocker.** Re-testing is nearly always cheaper than the workaround, and a constraint you inherited from your own earlier note deserves more suspicion than one the human just gave you.

### The task list

Prose to-dos hide in a wall of text and die at the next compaction. Use the actual task tool. Work in ID order, mark `in_progress` before touching anything, and never mark complete on a partial.

When work reveals new work — and it will, constantly — add a task rather than chasing it inline. Chasing it inline is exactly how the mission drifts.

**⛔ Past roughly fifty open items it has stopped being a queue and become a second scratch doc** — nobody can scan it, including you, and the human will tell you so. Findings do not belong here: resolve them into the scratch doc and close the task. Keep only what you would genuinely pick up next. If the list has already rotted past saving, fold it into the scratch doc as goals and retire it rather than maintaining two decaying records.

## The heartbeat

A recurring prompt that re-grounds you. Its whole job is to survive your context loss, so:

**It must be EVERGREEN.** It states principles, never moments. The instant it says "now check whether the build passed" it has become a snapshot and will be wrong within the hour. If you notice it describing a moment, rewrite it.

**It points at SYSTEMS, not content.** It should not restate the mission in detail — it should tell you where the mission is written. Re-grounding order that works:

1. **The task list** — the live queue; claim the lowest unblocked item.
2. **§0 CANON** — the mission and the non-negotiables; it outranks your memory.
3. **Live sub-agents and background shells** — collect what finished; never re-run delegated work.

**It carries the standing gates.** The two or three things that are never yours ("do not merge", "do not release"), because those are exactly what a confident, context-poor agent talks itself into.

A serviceable skeleton:

> Re-ground, then keep going. You are driving **<mission in one line>**, autonomously, for as long as it takes.
> **THE MISSION.** <two or three sentences of what success IS, stated as a property, not a task list.>
> **RE-GROUND IN THIS ORDER.** (1) the task list — claim the lowest unblocked item and mark it in_progress; (2) `<path>/scratch.md` starting at §0 CANON, which outranks your memory; (3) any live sub-agent or background shell — collect, never re-run.
> **KEEP BOTH RECORDS TIGHT.** The task list is the plan, the scratch doc is the reasoning. Replace what is superseded, delete what is finished. Rewrite this prompt if it ever describes a moment.
> **<the project's quality filter — elegance, safety, whatever governs.>**
> **VERIFY BY RUNNING, NEVER BY READING.**
> Decide and proceed; escalate only <the genuinely human-owned categories>.
> <the standing gates.>

### A heartbeat that fires with nothing to do is a signal

If you wake, check status, and report "no change" — that is not the loop working. It means either the mission has work you are not seeing, or the thing you are waiting on should be waiting on *you*. Emitting the same status line repeatedly is the most expensive way to do nothing. Go find the next real weakness, or say plainly that the effort is finished.

**You have no sense of elapsed time, and the heartbeat is not a clock.** It fires when you REST, so fast turns make it fire fast — measured at ~1 minute apart on a run whose prompt claimed 20. Take every elapsed-time claim from subtracting two real timestamps (`date -u`), never from counting wake-ups. This is not cosmetic: it makes healthy work look stalled, and invites you to "rescue" something that is fine.

**When you are genuinely waiting, BLOCK — do not poll.** One foreground wait (`timeout 540 <watcher> --exit-status`, looped) covers nine real minutes in a single turn; nine wake-ups cover the same nine minutes and produce nine status lines. On a fast heartbeat, polling cannot observe anything that takes longer than a turn — so it is guaranteed to report "nothing changed" regardless of how well the work is going.

## Altitude — the discipline that actually matters

Mechanics are seductive because they are *tractable*. A failing build has an obvious next action; "is the mission served?" does not. So you drill down, and stay down.

**Every few cycles, ask: what is the mission, and did the last hour move it?** If the honest answer is "I fixed some build errors", surface, re-read canon, and pick a task that moves the goal.

Two specific rescues:

- **When the human sends a directional message, stop.** Do not finish the tool call you were about to make. A strategic message is worth more than the edit in flight, and the tell that you have failed here is that you replied with a status update to a message about direction.
- **Distinguish the mission from its scaffolding.** Getting the build green, the CI passing, the harness wired — none of these are the mission. They are the cost of doing it. Time spent there is fine; time spent there *instead* is drift.

## Verification, when nobody is watching

A long unsupervised run accumulates confident wrong beliefs unless you actively fight it.

- **Verify by running, not reading.** In a real epic, essentially every genuine defect will be invisible until you compare an artifact's actual output against a reference. Source reading generates leads, not findings.
- **A surprising result means a broken instrument until a positive control says otherwise.** This will happen more than you expect. A uniform failure across every row, a suspiciously round number, a filter with a tidy split — check the probe against a case whose answer you already know before you believe any of it.
- **A non-discriminating control is worse than none**, because it reads as evidence. If your control and your subject would produce the same output whether or not the thing you are testing is true, you have measured nothing. Find a discriminator that actually differs — hashing a function's source, diffing byte counts, breaking the feature and watching the test go red.
- **Results that arrive incrementally are a BIASED SAMPLE, and the bias is often the mechanism you are hunting.** The cases that land first are the ones that finish fast, and what makes them fast is rarely independent of what you are studying. On one run the first three failures shared a trait, that trait turned out to be exactly what made them fail instantly, and the full set split evenly. **Ask what makes a case arrive EARLY before treating early cases as representative** — and never publish a rate before the slow half lands.
- **You are experimenter and record-keeper at once, so your own runs can corrupt the data.** An experiment that writes back into the shared corpus can overwrite good measurements with worse ones, and nothing will report it. The detection is a number moving the WRONG WAY while everything else says it should improve. **Treat that contradiction as information rather than noise, and chase it before you report the number** — on one run that instinct was the only thing that surfaced eight records the agent had damaged itself.
- **A conclusion proven on one axis does not transfer to a parallel one.** "Most of the Windows tail was stale records" was measured and true; the identical signature on Linux came back 8/8 the other way. Re-run the test per axis; a shared signature is a hypothesis, not a result.
- **Correct yourself loudly and immediately.** You will assert things from memory that turn out wrong. When that happens, say so plainly, fix the record in the scratch doc, and move on — do not let a wrong belief propagate through five more decisions because correcting it felt awkward.
- **Distinguish a product defect from your own environment.** Before reporting anything broken, check whether your tree, your build, or your fixture is the broken thing. This is the single most common false alarm.

## When to involve the human

You were given the epic because they are not watching. Decide.

Escalate only what is genuinely theirs: the irreversible, a security or product-posture call, new public surface that is hard to withdraw, or a fork where both branches are defensible and being wrong is expensive. Everything else — a name, a default, a file location, which of two equivalent designs — is yours. Make it, record it in the scratch doc under a heading you can surface later, and keep moving.

**Batch the questions.** If you must ask, ask everything at once, each question self-contained with real options and a recommendation, because the round trip may cost hours.

**Record decisions as you make them, not at the end.** The scratch doc should carry a running list of the calls you made autonomously, so the human can review a session's judgement in one place instead of reconstructing it from a diff.

## Closing an epic

An epic ends when the mission's acceptance test passes, not when the task list empties — tasks are your model of the work, and the model is always incomplete. Before declaring done, reconcile the list against the actual tree: grep for the symbol that would prove each item landed, and confirm nothing regressed it.

If the effort points at future work at all, it is not done. Say what remains and hand it back cleanly.
