---
name: research-thread
description: Use when the deliverable is a FACT — not code, not a design, not a gap catalog. A prior-art survey, "how has X already been solved", "what did they try and abandon", a long empirical sweep, or any non-trivial question whose answer will be cited for months. Produces a living write-up at `wiki/research/<topic>.md`. Auto-triggers on "research this", "prior art", "survey", "find out how X works", "what's the state of the art", "dig into whether".
metadata:
  internal: true
---

# Research threads

A **research thread** is one of the four thread profiles, alongside the *implementation thread* (build-the-decided-thing, the `implementation-thread` skill), the *plan thread* (settle-the-approach, the `plan-thread` skill), and the *audit thread* (verify-parity, the `audit-thread` skill). Its deliverable is **a fact, written down** — `wiki/research/<topic>.md`. It is the thread you open when *what is true* is the open question.

## Scope gate — is this a research THREAD?

**Most questions are not.** The alternative to guessing is not only "dispatch a researcher" — it is "go read it". A few greps and the file they land in answer nearly every question about this codebase, first-hand and faster than a second-hand claim you must re-verify. AGENTS.md's prior-art reflex — spend the first few minutes finding out how a thing has already been solved — fires on EVERY non-trivial work item, and that scan is not a thread.

A research thread earns its own effort when the question needs breadth you cannot hold in one pass: a survey across several projects, independent sources that must be reconciled, a long empirical sweep, or a decision that will be cited for months. **If the answer fits in a paragraph of your handoff, write the paragraph and skip the doc.**

## The method

The order is the point — each step is cheaper than the one after it, and each kills theories the next would otherwise have to disprove.

1. **Search before theorizing.** Reach for web search eagerly and repeatedly; one search either hands you the answer or tells you you are first. When a dependency, runtime, or API surprises you, **search its issue tracker, release notes, and the integrating projects' issues FIRST**. A theory built from local evidence alone produces a confident, wrong story that survives until the next piece of evidence kills it.
2. **Read the source locally, never one file at a time over HTTP.** `git clone --depth 1 <repo> .repos/<name>` (gitignored), then grep and read a whole consistent tree. Check what `.repos/` already holds first — it carries 290 clones today, `node/`, `bun/`, `pnpm/` and `tsx/` among them. Never modify anything under `.repos/`.
3. **Read the resolution, not the issue body.** A body says what someone wanted; the maintainer comments and the reason it closed say what is true. An "open feature request" is often a deliberate rejection with the rationale in the thread.
4. **Hunt what was TRIED AND ABANDONED.** It lives in issue threads, release notes, and RFCs, and it is invisible in the code. A knob that shrank across releases, or a feature reverted one release after it shipped, tells you more than the current implementation does.
5. **Probe rather than reason.** A throwaway fixture, a standalone `rustc` file, or a differential run against the real tool answers "what does it actually do" faster and more reliably than tracing source. Ground every claim in code or an experiment, never memory — and label UNVERIFIED anything you could not run.
6. **Check whether their constraint is YOUR constraint.** Another project's rejected approach may have been rejected for a reason nub does not share, which can make an option they closed off correct here. A survey that only tells you what to copy is half-read.

**Reversing a conclusion on new evidence is correct, not flip-flopping.** State the current verdict, keep probing, and let evidence move you rather than defend the first answer.

## The publication gate — `wiki/` is TRACKED and PUBLIC

Read AGENTS.local.md → "What goes in the PUBLIC `wiki/` vs the PRIVATE `internal/`" before writing a line. **Default to `internal/`**: a wrong exclusion costs a later copy, a wrong inclusion is permanent. Four things never go public — unimplemented roadmap, the narrative of a dead end pursued too long, competitor SCORECARDS (a factual technical statement about another tool is fine and makes the mechanism legible), and any deliberation about how benchmark results were framed.

A doc that is mostly publishable with one bad section gets the section **cut**, not the doc dropped — then re-read the remainder for framing that only made sense next to what you removed. Separately, even a perfectly publishable doc about an UNSHIPPED feature stays on that feature's branch until it ships.

## The write-up

`wiki/research/<topic>.md`, kebab-case slug, never the repo root. Research docs do **not** carry the YAML front matter that `wiki/runtime/` and `wiki/commands/` use.

The shape the existing 70 docs converge on:

| Part | Convention |
| --- | --- |
| Title | `# <topic>` — the question or the finding, not "Research doc for X" |
| Orientation block | Directly under the title: status + date, the QUESTION it answers, the headline answer, and links to sibling docs |
| `## TL;DR` | Findings as a short numbered list — 40 of 70 docs. Write one whenever the body runs long |
| Body | The evidence, every load-bearing claim carrying its citation or its reproduction |
| `## Sources` | The external links — 31 of 70 |
| `## Changelog` | **Mandatory** — 68 of 70 |

**Research docs are living documents.** Edit in place: the body always reads as current best understanding, with no history inside it. Every change appends `- YYYY-MM-DD — <what changed and why>` under `## Changelog`; a new doc's first entry is `- YYYY-MM-DD — Initial write-up.` A major reversal gets a leading `**REVERSAL:**` marker. Never blow away a prior conclusion silently — say that the doc previously said X, now says Y, and what evidence moved it. When two docs overlap, merge into one canonical, record the merge in the survivor's changelog, and note the supersession at the top of the absorbed doc before deleting it.

## Boundaries with the other profiles

| The open question | The profile |
| --- | --- |
| What is true? | **research** → `wiki/research/<topic>.md` |
| How should we do it? | **plan** → a settled design (`plan-thread`) |
| Where do we diverge from a reference? | **audit** → a verified gap catalog, also under `wiki/research/` (`audit-thread`) |
| Build the decided thing | **implementation** → a held, green PR (`implementation-thread`) |

An **audit is the special case of research whose question is parity** — it inherits this method and adds five hard gates against false positives. Use `audit-thread` for it, not this skill. A **plan thread that needs a fact to decide** spins a research effort, folds the result back, and keeps deciding: the plan owns the decision, the research owns the fact.

A research thread does not land code. But **a clear bug with a clear fix found along the way gets run down, not parked under "still open"** — the context that found it is the cheapest context that will ever exist for it. That fix is an implementation thread and is held to the implementation gates: reproduce it, prove the fix with a differential, sweep fixtures against a built binary, read the diff. "I chased it and it dissolved" is a first-class successful outcome and usually takes minutes.

## Mechanics

- **Terminal state.** The thread ends when the doc is written and committed. That artifact outlives the thread, which is why a research thread finishes cleanly where a pre-fix investigation cannot.
- **Landing.** `wiki/` is markdown, so it commits direct to `main` with no PR. From the shared tree: sync first (`git fetch origin && git merge --ff-only origin/main`), commit path-scoped (`git commit -- wiki/research/<topic>.md`) so a sibling's WIP stays out, push, and read the push's exit code.
- **Tiering.** Synthesis and the verdict are Opus at high+ effort. A cheap tier may harvest breadth — a link sweep, a mechanical grep across a reference checkout — but every harvested item is re-verified before it enters the doc. A sub-agent's load-bearing claim is a lead, not a fact.
- **Dispatch.** A fresh-context sub-agent inherits neither the survey reflex nor the publication gate, and a child that does not know `wiki/` is public will write roadmap into it. Put both in the prompt. If you are yourself a dispatched sub-agent, the repo-wide depth cap in AGENTS.local.md applies: run the method inline and return.
