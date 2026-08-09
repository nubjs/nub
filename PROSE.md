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

- **Design the whole-page scaffold first — order sections by the reader's task, push internals to the back.** Before writing sections, decide the page's top-level shape: what a reader needs first (get it working), then each capability with its example, then interop, then internals and meta (how it's built, conformance, caveats). Don't front-load mechanism or spec-conformance — a reader wants to use the thing before reading how it's made. Group a capability's concept and its supporting data with that capability, not in a catch-all section — "what can cross a worker boundary" belongs under messaging, not dangling off an "API" heading. When a section feels misplaced, re-outline the whole page and move it; a good scaffold is a deliberate act, not the order the sections happened to get written.
- **No orphan subsections.** A single H3 under an H2 reads as a stray in the table of contents. Either promote it to its own H2, fold it back into the parent's prose, or give the parent a second sibling H3 — a subsection level earns its existence only when there are at least two of them.
- **Reach a block-level element fast — a page never opens with two dense paragraphs.** Never lead with two back-to-back multi-sentence prose paragraphs. Open with one short framing sentence or two, then a block-level element — a code example is ideal ("show the cool thing") — and fold the rest of the prose in after.
- **Prioritize visual interest over prose density.** Use sections, tables, lists, callouts, and code blocks to carry the content. Two dense paragraphs in a row anywhere is the smell to avoid.
- **The default rhythm is short paragraph → code snippet, and the snippet's comments carry the explanation.** Model the register on zod.dev: one or two sentences framing the idea, then a fenced example whose inline `// comments` explain each line — not a multi-sentence paragraph that describes in prose what an annotated snippet would just show. When a passage runs past two sentences without reaching a code block, that's the smell; break it with an example. Favor several short paragraph-plus-snippet beats over one long expository block.
- **Finish the explanation before the example.** A code block is the payoff and usually the end of its beat. Move ordinary explanation, consequences, and setup above it; after it, keep only a genuinely important caveat or callout. Delete lone closer sentences that merely restate what the example showed.
- **An API reference is an annotated code block, not a bulleted signature list.** Show each member in use in one example — `const code = await worker.terminate(); // stops it, resolves to the exit code` — never a list of `**method(args)** — sentence` entries, which reads as a spec dump and hides the usage. Head such a section idiomatically ("API", not "API surface"). An orthogonal options/inputs *matrix* stays a table; a member *surface* is a code block.
- **A callout is a paragraph, not a block-level break** (the bold-lead rule's sibling). A `<Callout>` or blockquote aside wedged between two prose paragraphs does NOT break the wall — paragraph, wordy callout, paragraph is still three heavy blocks in a row. Reserve a callout for a single genuine heads-up, keep it to a sentence or two, and carry ordinary explanation as short-paragraph-plus-snippet instead.
- **No inline-code pileups.** A sentence or paragraph that strings together a pile of back-to-back code chips — a flag list, a command list, a config-field list — reads as noise. Move the enumeration into a fenced code block (one item per line, `#` comments where they help), or a table if it's genuinely tabular; default to a code block. Keep any surrounding explanatory prose — a single sentence naming a difference is fine; it's the *list* that must leave the paragraph.
- **Precedence / source / ordering lists are just the names.** A resolution or precedence list (config-file order, a detection chain, pin sources) is a bullet list of the file/field names with at most a 2–3-word inline qualifier — never a full sentence per bullet, which just makes it look complicated. Long caveats, edge-case sources that technically win but never matter in practice, and error-on-ambiguity behavior go in a callout, not inline in the list. Only list a source the code actually consults — verify against the implementation before adding a bullet.
- **A config example carries the MINIMUM that makes its point — nothing else.** A snippet illustrating one field shows that field, not a plausible-looking file around it. Every extra key is a question the reader has to answer ("is this required? does it interact?") and the answer is almost always no. The tell that you have overreached: you added a field because it was *interesting*, or because you had just learned something about it, rather than because the section cannot be understood without it. A second field earns its place only when the point IS the interaction between the two — and then that interaction is the section's subject, not a bonus. (Burned 2026-07-28: an "adding export conditions" section shipped a `preload` entry alongside `conditions` because the author had just verified how the two interact. The maintainer: "we're talking about export conditions, but you inexplicably have a preload in there. Why? Is it necessary?")
- **Annotate a config example with lowercase `//` fragments; longer notes stack above the field.** House style for an annotated config block: lead a fragment with `// ...` so the reader knows it is an excerpt, put a brief note inline after the field, and stack multiple single-line `//` comments directly above a field when it needs more. Comments are **all lowercase**, concise, and prefer **sentence fragments** over full sentences — they annotate, they do not narrate. Never a block comment, never a capitalized sentence with a period.
  ```jsonc
  {
    // ...
    "conditions": ["development"]  // brief explanation inline

    // longer explanations can go above the actual field as needed
    // multiple simple inline double slash comments
    // all lowercase, concise, prefer sentence fragments
    "conditions": ["asdf"]
  }
  ```
- **Don't narrative-ize — state the fact and cut the scaffolding.** Docs are terse, not a walkthrough. Don't add a paragraph that restates what an adjacent table, list, or code block already shows ("That one decision drives the table below…", "A declaration always beats a stray lockfile, so…"). Don't add a qualifier that the document's own structure already implies — most sharply in an **ordered/precedence list**, where position carries "only when the earlier signals are absent," so an item must not restate it ("lockfile on disk", not "lockfile on disk — consulted only when no declaration names a manager"). Keep every distinct fact; drop the connective prose, the recaps, and the implied qualifiers around it.

- **Never invent a category noun, and never use a connector that connects nothing.** Two habits that read as writing while saying nothing. (1) A made-up class name the reader has no definition for — "typed project settings", "typed fields", "the neutral surface", "Nub's own axis". If a reader cannot say what is and isn't in the category, name the actual things instead: "four fields: `linker`, `publicHoist`, `minimumReleaseAge`, `minimumReleaseAgeExclude`". (2) A contrastive or reassuring connector — *instead*, *rather*, *the same*, *whether or not* — whose two sides were never established, which at best is noise and at worst asserts a relationship that does not exist. Test each one by asking **instead of what? the same as what?** and delete the sentence if there is no answer in the surrounding text. (Burned 2026-08-07 twice on one page: "Typed project settings live in the `nub.jsonc` install block **instead**" implied that block was an alternative spelling for the `.npmrc` keys above it, which it is not; and "A project carrying these fields **resolves the same** whether or not Nub is the tool that wrote them" had no second term on either side. The maintainer, both times: "What the fuck does this even mean?")

### Honesty and restraint

- **Terminal mockups show real captured output only — never invented lines.** Capture from the actual built binary or ground the exact string in the source; never invent example output, and never show output for a flag or command that doesn't exist.
- **Never address a concern nobody has — cut defensive editorializing.** Lines that pre-empt an imagined worry ("no minimum version", "this is the correct, conservative behavior", "rather than silently misread") answer a question the reader never asked. If a reader wasn't going to ask it, delete the sentence. State what the feature does; don't defend against objections nobody raised.
- **Don't inventory the exceptions — omission is usually the better edit.** A reader is served by the rule, not by a catalog of where it bends; cutting a true-but-peripheral clause is not a loss of accuracy, it IS the edit. The test: does the reader's next action change if they never learn it? Two tells — a sentence whose only job is an asymmetry between two things that behave the same in the common case, and a caveat restating what an earlier line already established. An exception that genuinely bites belongs in the section where a reader would hit it, not stacked into an intro.
- **Don't sprinkle a cross-cutting flag's asides across unrelated pages.** A flag that cuts across many features (an escape hatch, a compatibility toggle) gets introduced once, on its own surface — or as a documented flag of the command a page is about. Don't tack a tangential "and `--flag` turns this off" note onto an unrelated feature page that never introduces the flag; it just confuses people.

### Description fields

- **Description fields carry NO inline code — they render raw and ugly.** Every frontmatter `description:` and every card/feature description-style prop renders as plain text, where inline code does not format — backticks show up literally. Write each as a regular sentence and de-emphasize the code tokens: replace API names, globals, flags, file names, and config fields with proper nouns or plain language. The precise token names live in the page body, where inline code renders. When in doubt, strip the token and describe the capability in English.

---

## Blog & marketing

These govern launch posts and long-form marketing prose. The homepage is the canonical register — when a passage or code block already exists there, reuse it rather than rewriting it. Everything in the docs section above (sentence/heading rules, no inline-code pileups, real output only) applies here too.

- **Open with the thing working; compatibility and parity come later.** The first code block lands within a sentence or two of the section heading — show the tool doing the cool thing before any feature/parity enumeration, or the reader is lost. Compatibility and parity notes go at the back of the section. Don't get into the weeds on internals the reader didn't ask about.
- **Code blocks carry the argument.** Every feature claim wants a code block; reuse vetted blocks before writing new ones. A bullet list restating what a code block already shows is strictly worse than the code block.
- **No walls of text.** Never open a section with a long multi-sentence paragraph. Two or more dense paragraphs in a row anywhere is a smell; 1–3 short paragraphs, then code. Don't end a section on a lone one-line closer paragraph — fold closers into the prose above.

### Long-form narrative (blog posts, essays, announcements)

The rules above govern every marketing surface; these govern the SHAPE of a long-form piece specifically. The reader is a smart engineer — possibly from another language ecosystem entirely — deciding within the first screen whether to keep reading. Structure the piece for that reader, and judge every edit against the flow of the whole document, not just the paragraph it touches.

- **The opening paragraph is exactly one sentence.** The first paragraph is a single sentence that stands alone as the hook; elaboration starts in paragraph two.
- **Never multiple one-sentence paragraphs in a row.** Consecutive single-sentence paragraphs read as staccato fragments — merge them, or grow one into a real paragraph. (The one-sentence opener stands alone; the paragraph after it must not also be a single sentence.)
- **Ration em dashes.** At most one em-dash construction per paragraph (a paired parenthetical aside counts as one). Two dash-connectors in a sentence or paragraph is the tell of unedited model prose; rewrite with a period, colon, or comma.
- **Structure the piece as an arc: setup → tension → reveal → payoff.** Open with the universally-familiar setup, build tension with the obvious-but-never-achieved idea (with linked receipts — real PRs and issues), name the culprit only at the reveal moment, and place the "we solved this" line immediately before the section that delivers the solution. Never leak the reveal early: a locally-fine sentence that spoils the ending is wrong even when true. Big claims ride the arc; the benchmark payoff comes last, under its own closing section.
- **Each fact appears exactly once, where it does argumentative work.** Restating what a code block, diagram, or earlier sentence already established detracts from the point being made now. When the same fact shows up twice, keep the instance that carries the argument and cut the other.
- **Cut implementation detail that doesn't serve the thesis.** Internal mechanics — naming schemes, hashing, patch-site specifics — either advance the argument or go. A vague-but-honest gesture ("patch-package style") beats a precise-but-distracting digression. The tell: if a detail makes the reader ask "wait, what's that?" without moving the thesis, cut it.
- **Write for the smart outsider.** Assume no ecosystem-insider knowledge: explain the platform mechanic the argument depends on in one or two plain sentences at first use (e.g. how Node resolves imports), anchor with cross-ecosystem analogies (Cargo, uv), and never lean on unexplained insider jargon or abstract coinages where a plain phrase exists.
- **Examples use famous packages and tools.** The reader should recognize every name instantly; an obscure package makes them take the claim on faith. If the true motivating case is obscure, find a famous instance of the same class — and verify the claim actually holds for it before it ships.
- **Triumphant, never sheepish.** Frame the work as eliminating the obstacle, not shipping despite it: no "anyway", no defensive hedging, no autopsies of your own defects. Honesty still binds every number and claim (see the docs honesty rules) — the register is confident, the facts stay exactly true.
- **Diagram layouts as minimal tree blocks.** Filesystem-layout discussion gets a squint-friendly `tree`-style block — elide with `…`, mark symlinks with `->` (the `ls -l` convention), annotate with `←` comments — never a transcript of readlink/ls commands. The block lands immediately after the sentence that introduces it; elaboration resumes after the block.
- **Config snippets name their file.** A bare config block is ambiguous about where the lines go; make the first line a comment naming the destination (`# .npmrc — …`).
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
- **A bold lead-in is NOT a block-level break.** Successive paragraphs that each open with a `**bold lead.**` are still a wall of text — bolding the first sentence does not satisfy the "break up two-plus paragraphs" rule. When you have three or more such items in a row, convert them to a real unordered list, table, or callout. Reserve the bold-mdash lead-in form for small items that each sit over their own code block (per the headings rule); a run of bare bold-led prose paragraphs is the smell, not the fix.
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

### Marquee announcements (a major/milestone post, not a routine patch note)

A signed milestone post — a major version, a launch — earns a different register from a patch release note. It is still factual and code-first, but it has a narrative arc and a small, controlled measure of personality.

- **Arc: context → why-now → numbers → features → migration at the back.** Open with a one-line "it's here" plus brief context (why this version exists, what ceiling it clears), lead the body with the strongest quantified wins, then features, then breaking changes/migration last. Migration is redirected to its own page, not front-loaded — don't lead a celebratory post with "this breaks."
- **Lead a performance claim with the multiplier, then back it with raw numbers.** State the ratio first and precisely ("~57% smaller (2.3x)", "14x faster"), then give the underlying measurement (ms, bytes, run count) so the ratio is auditable. The multiplier is the headline; the raw number is the proof. A before/after table is the default presentation.
- **A short evaluative aside is allowed here — and ONLY here.** A signed milestone post may carry a controlled beat of voice: a two-word reaction after a number ("That's good!"), a flagged surprise ("This was an unexpected one."), one em-dash aside. This is the narrow exception to the "cut clever-but-empty lines" rule — it is licensed by the signed-announcement register and nowhere else (never in docs, patch notes, or GitHub comments). One or two per post, not a habit; the asides punctuate the facts, they don't replace them.
- **Credit contributors and sponsors by name, specifically, with a link.** Attribution is prominent and warm — name the person/org, link them directly, say what they did. Vague "thanks to everyone who helped" is weaker than a named, linked acknowledgement.
- **Sign it.** A milestone post closes with a brief personal sign-off from the author. (A routine patch note does not — it stays unsigned and impersonal.)

### Migration / breaking-change entries

A migration guide is a flat list of changes optimized for a reader scanning for the one that bit them.

- **Lead each entry with a one-line summary as its heading, then the code.** The heading states what changed in a sentence ("Replaces the message param with error"); the before/after blocks follow. Don't make the reader parse code to learn what the entry is about.
- **Label before/after blocks explicitly** with the version each belongs to, and use inline ✓/❌ on supported-vs-removed lines where a side-by-side is overkill.
- **Tag the change TYPE in plain words.** "has been deprecated in favor of", "has been removed", "is no longer supported", "now behaves differently" — the reader sorts by severity at a glance. Removals get blunt, terse treatment; behavior changes get the old-state-then-new-state explanation they need.
- **Explain WHY only when the why adds something.** Lean toward WHAT changed. A rationale earns its sentence when it prevents a "wait, why?" ("integers outside this range caused rounding errors") — otherwise state the change and move on. Don't manufacture a justification for every removal.
- **Group related changes under a category heading** rather than listing every sub-change serially — collect a surface's changes under that surface's name.

---

## Naming and capitalization

**A project or product name is a proper noun in prose; its lowercase form belongs only inside monospace.** The test: if the word is in running prose — a sentence, a heading, a table cell description — it takes the proper-noun capitalization. Inside backticks, a fenced code block, a `<code>`/`<pre>`, a URL, a file path, or a package name it keeps its literal lowercase spelling. Compound modifiers follow the same rule — the proper noun in prose, the code spelling in backticks. All-caps is never correct unless the name genuinely is an acronym. Which form is the proper noun and which is the executable is a project fact: it lives in the project's own agent instructions, not here.

---

## Universal tone rules

These hold everywhere — every surface above, plus commit messages and chat.

- **Factual, neutral, professional.** State what is true and what changed. Never braggy, competitive, or editorializing; no superlatives, no "fastest"/"beats X", nothing a skeptic could screenshot. For a public repo, assume everything you write is world-readable forever.
- **Commit messages state what changed, not how great it is.** Name the change (a new run, an updated component, a fixed behavior), never the verdict.
- **No emojis** in prose, comments, or chat — except where a convention deliberately uses a glyph as a status marker (e.g. ❌/✓ in a failure-demo code block).
- **Chat replies: minimum words that convey every point.** A terse subject-matter expert. Sentence fragments and conversational shorthand are fine; no preamble, no recap, no hedging, no restating the question. State conclusions. Concision is not omission — keep every distinct point, just strip the words around it. (This governs chat only; docs and code keep their normal rigor.)
- **Write like a terse senior engineer — lead with the claim, let evidence carry it, cut the connective tissue.** This is the register for substantive prose where you're making an argument: GitHub issues, PR descriptions, review comments, design rationale, and longer chat answers. Lead with the conclusion, then let a repro / data / a concrete example do the persuading instead of adjectives. Concede a counterpoint in a clause, not a paragraph — "Legitimate, but erroring on every key is a blunt fix for a narrow problem," never "Fair as to origin, but it's been stable for years that an ecosystem now depends on; behavior relied upon at that scale is usually formalized rather than hard-broken, regardless of how it originated." Kill the AI-slop tells: throat-clearing openers ("I'd like to", "It's worth noting", "It's important to"), trailing summary sentences that restate the point just made, qualifier sprawl ("fully", "genuinely", "actually", "simply", "quite"), paired hedges ("valuable, and fully achievable without…"), and sub-bullets where one sentence is tighter. One load-bearing idea per sentence; if a line adds no fact and moves no argument, delete it. A section heading is a label, not a thesis statement.
- **Name the concrete thing, not an abstract frame.** "Layout is its own axis" gives the reader nothing to act on; "how packages get linked into `node_modules` is configured only in the project's config file" does. An abstraction standing in for the subject — *axis*, *surface*, *dimension*, *space* — is the tell that the sentence has not been written yet. Say what the thing is, where it is set, and what the reader does about it.
- **Never quote a person's own words back to them** in any document — paraphrase the decision or fact in neutral third person instead.
