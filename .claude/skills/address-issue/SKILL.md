---
name: address-issue
description: >-
  End-to-end playbook for working a GitHub issue (or bug-fix PR) on nubjs/nub:
  triage → acknowledge an external report with an "Investigating" comment →
  reproduce it yourself → SIZE it → fix it at that size (most issues are a small
  fix you just write — no investigation spike, no sub-agent fleet, no top-tier
  model) → open a PR that references the issue with `Closes #N` → on merge,
  comment the resolution → on release, comment the version + release link. Invoke
  (via the Skill tool) whenever you pick up an issue to work, or are asked to fix
  a reported bug. Encodes both the maintainer-hygiene conventions in AGENTS.md
  (so the reporter is acknowledged, the issue auto-closes on merge, and the loop
  closes when the fix ships) and the proportionality gate that keeps an ordinary
  bug from being worked like an audit.
metadata:
  internal: true
---

# Addressing a nub issue end-to-end

This is the playbook for taking a GitHub issue from "reported" to "shipped and the reporter told." It exists because maintainer responsiveness on a PUBLIC repo is a visible health signal: a reporter who gets an acknowledgement, sees their issue auto-close on merge, and gets a link to the release it shipped in has a completely different experience from one whose issue goes silent. The hygiene rules here are mandatory, not courtesies (AGENTS.md "Git & GitHub maintainer hygiene").

**Guardrails (read first):**

- **Tone is always factual, neutral, professional — never braggy, competitive, or over-promising.** Every comment follows the project's tone bar (AGENTS.md "The repo is PUBLIC" + the commit-message tone rule; the cross-project GitHub-comment + prose guide is [`PROSE.md`](../../../PROSE.md)). Acknowledge sincerely; state what you found and what you did; don't promise a timeline you can't keep.
- **A fix lands via the PR-from-a-worktree flow, not directly on the shared `main` tree.** Substantive fixes are reviewable PRs opened from an isolated worktree (AGENTS.md "Default to a PR flow"). Trivial doc/typo fixes are the documented exception.
- **Verify the fix end-to-end before pushing — don't outsource verification to CI.** Run the pre-push local-verification loop (AGENTS.md "VERIFY LOCALLY BEFORE PUSHING"). A green test suite with a stubbed fix is worse than an unchecked one.
- **Don't autonomously land a change to a default / security posture / product behavior / API-config-env surface.** Those are recommend-only until the maintainer signs off (AGENTS.md). A mechanical, clearly-a-bug fix may land; a behavior decision routes back as a question.
- **Work the issue at its ACTUAL size — the default is that you just fix it yourself (HIGH PRIORITY).** Most issues are one clear cause and a small diff. They do not get an investigation spike, a sub-agent fleet, or a top-tier model, and reaching for that shape by reflex is the single most common way this playbook is run wrong. Size it first (Step 1b); escalate only on evidence you actually found.

---

## Step 1 — Triage: read the issue, the comments, and reproduce

```bash
gh issue view <n> --repo nubjs/nub --comments
```

- **Read the COMMENTS and the resolution history, not just the body.** The body says what someone wanted; the thread says what's true. A "feature request" can be a deliberate prior rejection; a "bug" can already be fixed on `main`. (AGENTS.md "Probing methodology" — read the comments.)
- **Classify it.** Is it a genuine bug, a usage question, a feature request, a duplicate, or already-fixed? Is the fix mechanical (clearly-a-bug, land it) or does it touch a default / behavior / API-config-env surface the maintainer owns (propose, get sign-off)?
- **Reproduce empirically before reading source or deciding.** Build a minimal differential fixture — the behavior under nub vs. the reference tool it claims parity with (npm/pnpm/yarn/bun/node) on identical input — in a throwaway `/tmp` dir. A divergence IS the finding. Test what it actually does; don't infer from code (AGENTS.md "Probing methodology — differential fixtures").
- **Do the reproduction YOURSELF.** It is a fixture and a couple of commands. Handing it to a sub-agent costs more than running it, and a second-hand repro is a lead you then have to re-verify anyway (AGENTS.md "A sub-agent's load-bearing claim is a LEAD, not a fact").

## Step 1b — Size it, and let the size pick the machinery

Once you can reproduce it you know its real shape. Pick the row and work it that way. **Default to the top of this table and move down only on evidence** — a cause you traced that turns out to be cross-cutting, a fix that turns out to touch a maintainer-owned surface. The issue's tone, its label, or how alarming the title sounds are not evidence.

| Class | What it looks like | How you work it |
| --- | --- | --- |
| **Answer it** | usage question, duplicate, already fixed on `main`, working-as-intended, a docs or typo fix | Reply and close (Step 6). No worktree, no sub-agent. |
| **Small fix** — *the common case* | one clear cause; a few lines across one or two files; no design call | Fix it yourself in a worktree. No sub-agent. Your own read of the diff plus the pre-push loop IS the review. |
| **Real change** | several files, or logic whose knock-on effects you can't hold in your head, or a new test harness | Do it yourself or hand it to ONE implementation agent. Add ONE reviewer over the diff if the logic isn't self-evident. |
| **Big** | a default / security posture / public surface, the resolver / lockfile / registry / CAS, cross-platform behavior, or a genuine multi-prong audit | The full shape earns its keep here: an owning agent, a multi-lens review split by dimension, top-tier models. |

**Three or four Opus-high sub-agents on a small fix is a defect in how you ran the playbook, not thoroughness.** It costs more than the fix is worth, it is slower than doing it, and every extra hop is another second-hand claim you have to re-verify. If you are about to dispatch, name which row above you're in and what specifically you found that puts you there — if you can't, you're in "Small fix" and you should just fix it.

The escalation mandates elsewhere in the repo — the fan-out rule and the mandatory multi-lens self-review in AGENTS.local.md, the L1/L2 shape in the `implementation-thread` skill — describe the **Big** row. They are not a floor under every issue.

## Step 2 — Acknowledge an external report immediately

If the issue is from an EXTERNAL reporter (not a maintainer / not self-filed), post the acknowledgement the moment you start work, so they know it's seen. The ack is EXACTLY one word — nothing else:

```bash
gh issue comment <n> --repo nubjs/nub --body "Investigating."
```

**Just "Investigating." — full stop.** No "thanks for the report," no "will follow up," no preview of your hypothesis, no diagnosis, no timeline. A longer ack leaks half-formed theories (which are often wrong) and reads as noise. Say nothing but that it's being looked at. (Internal / self-filed issues don't need this.)

**Every SUBSTANTIVE comment (Steps 5–6, or any reply) is terse + factual per PROSE.md — never a verbose essay.** State what you found and what you did, in the fewest words that carry the facts. No preamble, no editorializing, no walls of text. NEVER write "previous comments were wrong" or similar meta-commentary — prior comments are often an automated bot's, not a human's. The `gh-comment-guard` PreToolUse hook BLOCKS an over-long `gh issue/pr comment` / `gh pr create` body; trim it, or set `NUB_ALLOW_LONG_COMMENT=1` only for a genuinely-needed longer body (e.g. a detailed release note).

If triage shows it's NOT a bug (working as intended, a usage question, a duplicate, won't-fix), say so factually with the reason and close it per Step 6 — don't leave it hanging or sink time into a non-fix.

## Step 3 — Fix it, at the size you sized it

Work it at the size you settled on in Step 1b.

- **Small fix (most issues):** write it. The pre-push loop below is the verification, and your own read of your own diff is the review. Don't dispatch anything.
- **Real change:** one implementation agent, or yourself. One reviewer over the diff if the logic isn't self-evident to you.
- **Big:** now the **fray methodology** earns its keep (the globally-installed `fray` plugin skill — load it by name): you orchestrate, dispatch model-tiered sub-agents as instruments (Opus for the fix that lands, Sonnet for supporting work, Haiku for scripted harvest), and the fix gets a separate multi-lens self-review scaled to the blast radius — registry/lockfile/config/security/default changes get reviewers split by dimension.

Tier every agent you do dispatch by the judgment its task actually needs, not by the fact that it's a bug fix (AGENTS.local.md "Model tiering"). A repro, a harvest, a doc update, or a CI watch is Sonnet or Haiku work; Opus is for the fix that lands and for verdicts you'll act on.

Work in an isolated worktree off `origin/main` (AGENTS.md "Default to a PR flow"):

```bash
git worktree add /tmp/nub-fix-<n> -b fix-issue-<n> origin/main   # vendor/aube comes along (plain in-tree files)
cd /tmp/nub-fix-<n> && export CARGO_TARGET_DIR=/tmp/nub-fix-<n>-target
```

Before pushing, run the **pre-push local-verification loop** (AGENTS.md "VERIFY LOCALLY BEFORE PUSHING"): incremental build → the exact CI gates (`cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, scoped `cargo test`) → an e2e tmp-fixture run of the specific behavior the issue is about → Docker for anything touching the global cache/config → and promote a durable check into the test suite where reasonable (a regression test for this bug). Get it green locally and push ONCE.

## Step 3b — Update docs if the fix changes user-facing behavior

If the fix changes something a user observes — a flag's effect, a default, an error message, a formerly-broken feature that now works, a new workaround that's now unnecessary — update the relevant page in `site/content/docs/` as part of the same effort. The fix is not done until the docs reflect it. Land the doc update in the same PR as the code fix.

## Step 4 — Open the PR, referencing the issue

The PR body MUST reference the issue. Use a closing keyword for a bug the PR resolves so the merge auto-closes it; use `Refs #N` for a related-but-not-resolving PR.

```bash
git push -u origin fix-issue-<n>
gh pr create --repo nubjs/nub \
  --title "<concise factual title>" \
  --body "$(cat <<'EOF'
<What the bug was and what the fix does, factually.>

Closes #<n>

<Verification: the fixture/command you ran and its result.>

https://claude.ai/code/session_<id>
EOF
)"
```

Report the PR URL. Do NOT merge your own PR — the maintainer reviews and merges (AGENTS.md).

## Step 5 — On merge, comment the resolution

When the PR merges, the `Closes #N` auto-closes the issue. Add a brief factual comment stating what fixed it (the auto-close is silent otherwise):

```bash
gh issue comment <n> --repo nubjs/nub --body "Fixed in #<pr> (merged to main). Will ship in the next release."
```

If for some reason the issue did NOT auto-close (no closing keyword, or it was a non-fix resolution), close it explicitly with a comment — never silently:

```bash
gh issue close <n> --repo nubjs/nub --comment "<what fixed it, or why no code fix is needed>"
```

## Step 6 — On release, comment the version + release link (mandatory)

A fix merged is not a fix shipped. When the release that carries this fix goes out, comment the version and a link to the release on the issue AND on the merged PR. This is mandatory maintainer hygiene done on every release (AGENTS.md "Git & GitHub maintainer hygiene"; the `release` skill's Step 5 does this in bulk across the whole changeset):

```bash
REL="https://github.com/nubjs/nub/releases/tag/v<ver>"
gh issue comment <n> --repo nubjs/nub --body "Shipped in v<ver>: $REL"
gh pr comment   <pr> --repo nubjs/nub --body "Shipped in v<ver>: $REL"
```

In practice you don't run this per-issue at release time — the `release` skill enumerates every closed issue + merged PR in the changeset and comments them all. This step documents the contract for a single issue; the release skill is where it's executed across the release.

---

## Quick reference

| Step | Action |
| --- | --- |
| Triage | `gh issue view <n> --comments` · read the thread · reproduce it yourself with a differential fixture |
| Size | Answer it · Small fix (default — just fix it, no sub-agents) · Real change (≤1 agent + ≤1 reviewer) · Big (full fray shape) |
| Acknowledge | `gh issue comment <n> --body "Investigating."` — exactly that, nothing more (external only) |
| Fix | in a worktree off `origin/main`; machinery scaled to the size; pre-push loop green; add a regression test |
| Docs | Update `site/content/docs/` if behavior changed — same PR as the fix |
| PR | `gh pr create` with `Closes #<n>` in the body; report the URL; don't self-merge |
| On merge | `gh issue comment <n> --body "Fixed in #<pr>…"` (or `gh issue close --comment` if not auto-closed) |
| On release | `gh issue comment <n> --body "Shipped in v<ver>: <release URL>"` (via the `release` skill, across the whole changeset) |

Invoke via the Skill tool whenever you pick up a GitHub issue to work or are asked to fix a reported bug.
