---
name: prose-writing
description: >-
  Copywriting / prose / tone style guide for EVERY user-facing or public-facing
  text artifact. INVOKE THIS SKILL (via the Skill tool) BEFORE writing or editing
  any of the following — and if you are a SUB-AGENT instructed to do any of them,
  invoke it yourself first; the agent that writes the copy loads this skill, the
  orchestrator does not load it on a sub-agent's behalf:
  (1) ANY GitHub-facing text — before `gh issue comment`, `gh pr comment`,
  `gh pr create` / any `--body`, `gh issue close --comment`, `gh pr review`, or
  `gh release create` / `gh release edit --notes`.
  (2) ANY documentation — `site/content/docs/**`, `site/content/blog/**`,
  `README.md`, `CHANGELOG`, `wiki/**` user-facing pages, or any `.md`/`.mdx`
  that ships to users.
  (3) Marketing / homepage / blog copy, a `package.json` or npm `description`,
  or any other description/summary field.
  (4) Whenever you APPLY a general copy-style correction — sweep it everywhere it
  applies, not just the one spot it was raised, and record it in PROSE.md.
  (5) A substantive prose chat reply.
  (6) Reformatting existing copy for scannability. A bold-sentence lead-in does
  NOT count as a block-level break: a run of bold-led paragraphs is still a wall
  of text, and the fix is a real list/table/callout.
  Through-line: factual, neutral, terse, scannable — never two-plus dense
  paragraphs in a row without a block-level element. Encodes GitHub
  maintainer-hygiene tone, sentence/heading mechanics (never open a sentence or
  heading with inline code), scannability, honesty/restraint, release-notes +
  marquee-announcement + migration-entry shape, and markdown mechanics (never
  hard-wrap paragraphs). Canonical full guide: PROSE.md at the repo root (this
  skill is its trigger + index). Project-specific copy rules live in AGENTS.md.
metadata:
  internal: true
---

# prose-writing

**The canonical guide is [`PROSE.md`](../../../PROSE.md) at the repo root — read it.** This skill exists to surface that guide whenever copy is being written, and to index it. PROSE.md is the single source of truth: do not duplicate its content elsewhere, and when a general copy rule is added or corrected, update PROSE.md and sweep every doc/page it applies to.

## When this applies

Any user-facing text: a GitHub issue/PR comment, a docs page, blog/marketing/homepage copy, release notes, a package/PR description field, or a chat reply. Also whenever you receive a *general* copy-style correction — apply it everywhere it's relevant, not only where it was raised.

**The agent that WRITES the copy loads this skill — not the orchestrator on its behalf.** When delegating copy work, put "load the prose skill before writing" in the sub-agent's prompt. The orchestrator loads it only for copy it writes in its own turn.

## What's in PROSE.md

- **GitHub issues & PRs** — factual/neutral/professional tone, no niceties or preamble; acknowledge an external report the moment you start; never reply to a bot as if human; `Closes #N`/`Fixes #N` in a fix-PR body (`Refs #N` if it only relates); close issues with a brief factual comment, never silently; on release, comment the version + link on every shipped issue/PR.
- **Public-facing docs** — terse, code-first, no marketing fluff; show the thing working. Sentence/heading mechanics (never start a sentence or heading with inline code or a command), structure & density, honesty & restraint, description-field rules.
- **Blog & marketing** — open with the thing working; code blocks carry the argument; no walls of text; benchmarks as visuals; protective refusals shown with real output; asides sparing.
- **Markdown mechanics** — never hard-wrap paragraphs; scannable over dense; release-notes shape; marquee-announcement shape; migration / breaking-change entry shape.
- **Naming & capitalization**, and the **universal tone rules**.

## The through-line

Factual, neutral, terse, scannable. State what is true; cut everything that does not add a fact. Never braggy, competitive, hyped, or over-promising. Build for a reader who skims. Then open PROSE.md for the specifics of the surface you're writing.
