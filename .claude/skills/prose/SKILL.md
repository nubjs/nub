---
name: prose
description: >-
  Project-agnostic copywriting / prose / tone style guide for any user-facing
  text: GitHub issue & PR comments, public-facing documentation, blog &
  marketing copy, release notes, and chat replies. Invoke (via the Skill tool)
  BEFORE writing or rewriting any of those — an issue/PR reply, a docs page, a
  blog or homepage passage, release notes, a description field — and before
  applying a general copy-style correction (which must be swept everywhere, not
  fixed in one spot). The through-line: factual, neutral, terse, scannable —
  state what's true, cut everything that doesn't add a fact, build for a reader
  who skims. Encodes GitHub maintainer-hygiene tone, sentence/heading mechanics
  (never open a sentence or heading with inline code), scannability, honesty/
  restraint, release-notes + marquee-announcement + migration-entry shape,
  markdown mechanics (never hard-wrap paragraphs), and the universal tone rules.
  The canonical full guide is PROSE.md at the repo root; this skill is its
  trigger + index. Project-specific copy rules (brand vocabulary, claim-tracing,
  product framing) layer ON TOP and live in AGENTS.md, not here.
---

# prose

**The canonical guide is [`PROSE.md`](../../../PROSE.md) at the repo root — read it.** This skill exists to make that guide auto-surface whenever copy is being written, and to index it. PROSE.md is the single source of truth; do not duplicate its content elsewhere, and when a general copy rule is added or corrected, update PROSE.md (then sweep every doc/page it applies to).

## When this applies

Any user-facing text: a GitHub issue/PR comment, a docs page (`site/content/docs/**`), blog/marketing/homepage copy, release notes, a package/PR description field, or a chat reply. Also whenever you receive a *general* copy-style correction — apply it everywhere it's relevant, not only at the spot it was raised, and record it in PROSE.md.

## What's in PROSE.md (read the relevant section before writing)

- **GitHub issues & PRs** — factual/neutral/professional tone, no niceties or preamble; acknowledge an external report the moment you start; never reply to a bot as if human; `Closes #N`/`Fixes #N` in a fix-PR body (`Refs #N` if it only relates); close issues with a brief factual comment, never silently; on release, comment the version + link on every shipped issue/PR.
- **Public-facing docs** — terse, code-first, no marketing fluff; show the thing working. Sentence/heading mechanics (never start a sentence or heading with inline code or a command), structure & density, honesty & restraint, description-field rules.
- **Blog & marketing** — open with the thing working; code blocks carry the argument; no walls of text; benchmarks as visuals; protective-refusals shown with real output; asides sparing.
- **Markdown mechanics** — never hard-wrap paragraphs; scannable over dense; release-notes shape; marquee-announcement shape (narrative arc, perf-multiplier framing, named attribution, signed milestone posts); migration / breaking-change entry shape.
- **Naming & capitalization**, and the **universal tone rules** (apply to every surface above).

## The through-line (if you read nothing else)

Factual, neutral, terse, scannable. State what is true; cut everything that does not add a fact. Never braggy, competitive, hyped, or over-promising. Build for a reader who skims. Then open PROSE.md for the specifics of the surface you're writing.
