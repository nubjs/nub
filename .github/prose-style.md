# Prose & tone style guide

A single, self-contained guide to writing prose for a software project: GitHub issue/PR replies, public-facing documentation, blog and marketing copy, release notes, and chat. It is deliberately project-agnostic — the rules below apply to any codebase. Project-specific copy rules (brand vocabulary, which source files a claim must trace to, product framing) live alongside the project, not here, and layer on top of this guide.

The through-line across every surface: **factual, neutral, terse, scannable.** State what is true; cut everything that does not add a fact. Build for a reader who skims.

## How to use this guide

- A **general** copy rule applies everywhere, not just at the one spot it was raised. When a writing rule is general (not a one-off fix to a single sentence), sweep every doc, the homepage, and the blog where it applies, and integrate the rule — don't fix it in one place and leave the rest inconsistent.
- The four sections below are: GitHub issues & PRs, public-facing docs, blog & marketing, and markdown mechanics. The markdown-mechanics rules and the universal tone rules apply to all of the others.

---

## GitHub issues & PRs

Maintainer responsiveness on a public repo is a visible signal of the project's health. These are mandatory hygiene, not optional courtesies. Every comment follows the universal tone bar — factual, neutral, professional, never braggy, competitive, or over-promising.

- **Tone is factual, neutral, professional — terse, no niceties or preamble.** State what you found and what you did. Acknowledge sincerely; never editorialize, never hype, never promise a timeline you can't keep.
- **Acknowledge an external report the moment you start work.** When you begin on an issue filed by someone outside the project (not a maintainer, not self-filed), post a brief acknowledgement — a short "Investigating, thanks for the report" — so the reporter knows it's seen. Keep it short and sincere; say you're looking into it, not when it'll be fixed. Internal / self-filed issues don't need the acknowledgement.
- **Never reply to an automated bot as if it were a human.** CI bots, review bots, dependency bots, and similar automation are not people. Don't thank them, don't address them conversationally, don't acknowledge their comments as if a person wrote them. Act on what the automation reports; don't converse with it.
- **Reference the associated issue(s) in every PR body.** A PR that resolves a bug uses a closing keyword — `Closes #N` / `Fixes #N` — so the merge auto-closes the issue. A PR that merely relates to an issue (touches the area, partial work, follow-up) uses `Refs #N`. Never land a fix-PR that leaves its issue unlinked.
- **Close issues with a brief factual comment — never silently.** State what fixed it, or why no code fix is needed (working as intended, a usage question, a duplicate, won't-fix). Keep the comment as short as possible while carrying that one fact. Don't leave a non-fix issue hanging, and don't close without a word.
- **On release, comment the version + release link on every closed issue and merged PR that shipped in it.** A fix being merged is not the same as it being on a published binary; this comment closes that gap for the reporter. For each issue closed and PR merged since the previous release, post the version it shipped in and a link to the release.

---

## Public-facing docs

Register: terse, code-first, no marketing fluff inside docs pages. Show the thing working; don't sell it.

### Sentences and headings

- **Never start a sentence with inline code. Absolute — applies to docs, blog, captions, and chat.** A sentence may never open with a code-formatted token: not a command, not a flag, not a field name, not a filename. Lead with prose words and put the code inside the sentence — write "The installer reads the lockfile…" or "Run the install command and it reads…", never open with the bare token. In JSX/TSX prose, put an explicit `{' '}` before an inline-code component when line breaks would otherwise swallow the space. If a draft sentence begins with backticks, rewrite it before shipping. No exceptions, not even "but it's the subject of the sentence."
- **Never start a sentence with a lowercase letter.** Capitalize the first word even when it would naturally be lowercase (a command name, a package, an identifier). Reword so a capitalized prose word leads — this also satisfies the rule above.
- **Section headings are the command/flag/field spelling, not prose — wherever a section maps cleanly onto one.** A section about a flag is headed by that flag; a section about a subcommand is headed by that subcommand — not an English paraphrase. Prose headings are for sections with no command surface (concepts, behaviors). Nest sub-syntax as a child heading under the owning flag.
- **A heading that IS a code token is rendered as inline code — backtick the whole token.** Covers flags, subcommands, config/property fields, file names, env vars, and API identifiers. (The "never START a heading with inline code" rule governs PROSE headings only — don't open a prose heading with a backtick. A heading that is *purely* a code token backticks the entire token, and that is correct. A prose heading with a token mid-phrase backticks just that token.)
- **Keep register consistent within a sibling-heading group, and never make a heading the place for pedantic correctness.**
  - **Don't mix registers across sibling headings.** A run of headings that flips between English, a syntax token, and a flag is unacceptable — pick one register for the group. For feature/concept sections, English wins (a syntax token becomes the English name of the concept). A code-token heading is for a group that is genuinely command/flag/field reference; a lone feature never gets a code-token heading just because it happens to have a syntax.
  - **Never cram exact syntax into a heading for correctness** — the body carries the precise spelling. The heading names the topic cleanly in English; pedantic accuracy lives in the prose.

### Structure and density

- **Reach a block-level element fast — a page never opens with two dense paragraphs.** Never lead with two back-to-back multi-sentence prose paragraphs. Open with one short framing sentence or two, then a block-level element — a code example is ideal ("show the cool thing") — and fold the rest of the prose in after.
- **Prioritize visual interest over prose density.** Use sections, tables, lists, callouts, and code blocks to carry the content. Two dense paragraphs in a row anywhere is the smell to avoid.
- **No inline-code pileups.** A sentence or paragraph that strings together a pile of back-to-back code chips — a flag list, a command list, a config-field list — reads as noise. Move the enumeration into a fenced code block (one item per line, `#` comments where they help), or a table if it's genuinely tabular; default to a code block. Keep any surrounding explanatory prose — a single sentence naming a difference is fine; it's the *list* that must leave the paragraph.
- **Precedence / source / ordering lists are just the names.** A resolution or precedence list (config-file order, a detection chain, pin sources) is a bullet list of the file/field names with at most a 2–3-word inline qualifier — never a full sentence per bullet, which just makes it look complicated. Long caveats, edge-case sources that technically win but never matter in practice, and error-on-ambiguity behavior go in a callout, not inline in the list. Only list a source the code actually consults — verify against the implementation before adding a bullet.
- **Don't narrative-ize — state the fact and cut the scaffolding.** Docs are terse, not a walkthrough. Don't add a paragraph that restates what an adjacent table, list, or code block already shows ("That one decision drives the table below…", "A declaration always beats a stray lockfile, so…"). Don't add a qualifier that the document's own structure already implies — most sharply in an **ordered/precedence list**, where position carries "only when the earlier signals are absent," so an item must not restate it ("lockfile on disk", not "lockfile on disk — consulted only when no declaration names a manager"). Keep every distinct fact; drop the connective prose, the recaps, and the implied qualifiers around it.

### Honesty and restraint

- **Terminal mockups show real captured output only — never invented lines.** Capture from the actual built binary or ground the exact string in the source; never invent example output, and never show output for a flag or command that doesn't exist.
- **Never address a concern nobody has — cut defensive editorializing.** Lines that pre-empt an imagined worry ("no minimum version", "this is the correct, conservative behavior", "rather than silently misread") answer a question the reader never asked. If a reader wasn't going to ask it, delete the sentence. State what the feature does; don't defend against objections nobody raised.
- **Don't sprinkle a cross-cutting flag's asides across unrelated pages.** A flag that cuts across many features (an escape hatch, a compatibility toggle) gets introduced once, on its own surface — or as a documented flag of the command a page is about. Don't tack a tangential "and `--flag` turns this off" note onto an unrelated feature page that never introduces the flag; it just confuses people.

### Description fields

- **Description fields carry NO inline code — they render raw and ugly.** Every frontmatter `description:` and every card/feature description-style prop renders as plain text, where inline code does not format — backticks show up literally. Write each as a regular sentence and de-emphasize the code tokens: replace API names, globals, flags, file names, and config fields with proper nouns or plain language. The precise token names live in the page body, where inline code renders. When in doubt, strip the token and describe the capability in English.

---

## Blog & marketing

These govern launch posts and long-form marketing prose. The homepage is the canonical register — when a passage or code block already exists there, reuse it rather than rewriting it. Everything in the docs section above (sentence/heading rules, no inline-code pileups, real output only) applies here too.

- **Open with the thing working; compatibility and parity come later.** The first code block lands within a sentence or two of the section heading — show the tool doing the cool thing before any feature/parity enumeration, or the reader is lost. Compatibility and parity notes go at the back of the section. Don't get into the weeds on internals the reader didn't ask about.
- **Code blocks carry the argument.** Every feature claim wants a code block; reuse vetted blocks before writing new ones. A bullet list restating what a code block already shows is strictly worse than the code block.
- **No walls of text.** Never open a section with a long multi-sentence paragraph. Two or more dense paragraphs in a row anywhere is a smell; 1–3 short paragraphs, then code. Don't end a section on a lone one-line closer paragraph — fold closers into the prose above.
- **Cut clever-but-empty lines.** A sentence that sounds tight but states nothing must go. Every sentence adds a fact.
- **No flowery phrasing.** Aphorisms and metaphors get rewritten plain. Internal shorthand is never user-facing. Concrete product comparisons are fine and encouraged; flourish for its own sake is not.
- **Headings must look like headings.** If a rendered heading level sits near body-text size, fix the styling or stop using a heading. Small repeated items read better as bold mdash lead-ins over their code block, which also keeps the table of contents clean.
- **Benchmarks are visuals, not tables.** Use a bar treatment. Put each benchmark in the section it measures. Present noise honestly — a 1 ms difference across two runs is a statistical tie, not a win.
- **Curate, don't enumerate.** Keep only the genuinely interesting FAQs in the post; link the rest to their anchors (verify the anchors against the rendered page; don't slug-guess).
- **Sell the implicit default, not the command surface.** Frame the automatic behavior as the product and explicit commands as the rare escape hatch.
- **Bailout commands are sub-section asides, not top-level sections.** Surfaces that exist for completeness get a short child heading under the section whose behavior they back up, never their own top-level section.
- **Asides are styled and sparing.** Default blockquote styling is rarely acceptable on a marketing surface; style asides deliberately, and use one per post at most.
- **Showcase protective refusals as a feature — with real failure output.** Wherever the tool eagerly refuses an unsound or unsupported operation, show it: the failing command, the real captured error, and (where useful) the exit code, then one tight framing sentence — never a paragraph apologizing for it. Mark failure/unsupported lines distinctly (a red ❌ in the inline comment) and successful lines with a check, so the asymmetry reads at a glance. Real captured output only; never invent it.

---

## Markdown mechanics

These apply to every markdown surface above — docs, blog, release notes, and any tracked `.md`.

### Never hard-wrap paragraphs

**Every paragraph is one long line.** Editors soft-wrap; hard line breaks inside a paragraph are forbidden. This applies to prose, blockquotes, list items, and table cells. Only code blocks, list-item boundaries, and headers introduce new lines. If you find yourself wrapping at column 72/80/100, stop — write the whole paragraph as one line and move on.

### Scannable over dense

Governs every user-facing body — release notes, changelogs, docs pages, blog/marketing prose. The reader skims; build for the skim.

- **Lead with what changed, then let structure carry it.** Use separate sections per major change (not one generic Fixes/Internal bucket).
- **Reach for the right block element.** A **table** when several independent items share a theme; a **list** when order doesn't matter; a **callout** (`> [!IMPORTANT]` / `> [!NOTE]`) for any heads-up or migration item — never bury those in a paragraph.
- **Link every item to its source** — the commit, PR, or issue — so a reader can jump to the change.
- **Readability is not a license to hype.** Visual interest comes from structure, never from marketing language. Stay factual and neutral.

### Release notes shape

A release note is the scannable rules applied to a changeset:

- **A one-line intro** stating the dominant theme.
- **A heads-up callout** (`> [!IMPORTANT]`) for anything a user should know before upgrading — omit it if there's nothing.
- **Themed sections, not generic buckets.** Group by what the changes touch. Each change gets a short titled blurb or a table row, never a multi-sentence paragraph.
- **A table for a batch of independent fixes** that share a theme — it reads far faster than a bullet wall.
- **Per-item links** to the commit and/or PR; issue refs link too.
- **A "Commits in this release" section at the bottom** — every commit as a bullet with its message and link — as the full audit trail beyond the themed sections.
- **Tone: factual and neutral.** Each line states what changed. No superlatives, no competitive framing, no editorializing — the same bar as commit messages.

---

## Universal tone rules

These hold everywhere — every surface above, plus commit messages and chat.

- **Factual, neutral, professional.** State what is true and what changed. Never braggy, competitive, or editorializing; no superlatives, no "fastest"/"beats X", nothing a skeptic could screenshot. For a public repo, assume everything you write is world-readable forever.
- **Commit messages state what changed, not how great it is.** Name the change (a new run, an updated component, a fixed behavior), never the verdict.
- **No emojis** in prose, comments, or chat — except where a convention deliberately uses a glyph as a status marker (e.g. ❌/✓ in a failure-demo code block).
- **Chat replies: minimum words that convey every point.** A terse subject-matter expert. Sentence fragments and conversational shorthand are fine; no preamble, no recap, no hedging, no restating the question. State conclusions. Concision is not omission — keep every distinct point, just strip the words around it. (This governs chat only; docs and code keep their normal rigor.)
- **Never quote a person's own words back to them** in any document — paraphrase the decision or fact in neutral third person instead.
