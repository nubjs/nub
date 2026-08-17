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

Taking a GitHub issue from "reported" to "shipped and the reporter told." The hygiene rules here are mandatory, not courtesies — maintainer responsiveness on a public repo is a visible health signal.

## Guardrails

- **Tone is factual, neutral, professional** — never braggy, competitive, or over-promising. See [`PROSE.md`](../../../PROSE.md).
- **A fix lands via the PR-from-a-worktree flow**, not directly on the shared `main` tree. Trivial doc/typo fixes are the documented exception.
- **Verify end-to-end before pushing** — run the pre-push local-verification loop. A green suite with a stubbed fix is worse than an unchecked one.
- **Don't autonomously land a change to a default / security posture / product behavior / API-config-env surface.** Those are recommend-only until the maintainer signs off. A mechanical, clearly-a-bug fix may land; a behavior decision routes back as a question.
- **Work the issue at its ACTUAL size — the default is that you just fix it yourself.** Most issues are one clear cause and a small diff. Reaching for an investigation spike or a sub-agent fleet by reflex is the most common way this playbook is run wrong.

## Step 1 — Triage: read the issue, the comments, and reproduce

```bash
gh issue view <n> --repo nubjs/nub --comments
```

- **Read the COMMENTS and the resolution history, not just the body.** The body says what someone wanted; the thread says what's true. A "feature request" can be a deliberate prior rejection; a "bug" can already be fixed on `main`.
- **Classify it.** Genuine bug, usage question, feature request, duplicate, or already-fixed? Mechanical fix (land it) or a maintainer-owned surface (propose, get sign-off)?
- **Reproduce empirically before reading source or deciding.** Build a minimal differential fixture — the behavior under nub vs the reference tool it claims parity with — in a throwaway `/tmp` dir. A divergence IS the finding.
- **Do the reproduction YOURSELF.** It is a fixture and a couple of commands; a second-hand repro is a lead you then have to re-verify anyway.

## Step 1b — Size it, and let the size pick the machinery

**Default to the top row and move down only on evidence** — a cause you traced that turns out to be cross-cutting, a fix that touches a maintainer-owned surface. The issue's tone, label, or alarming title are not evidence.

| Class | What it looks like | How you work it |
| --- | --- | --- |
| **Answer it** | usage question, duplicate, already fixed on `main`, working-as-intended, a docs or typo fix | Reply and close (Step 6). No worktree, no sub-agent. |
| **Small fix** — *the common case* | one clear cause; a few lines across one or two files; no design call | Fix it yourself in a worktree. No sub-agent. Your own read of the diff plus the pre-push loop IS the review. |
| **Real change** | several files, or logic whose knock-on effects you can't hold in your head, or a new test harness | Do it yourself or hand it to ONE implementation agent. Add ONE reviewer over the diff if the logic isn't self-evident. |
| **Big** | a default / security posture / public surface, the resolver / lockfile / registry / CAS, cross-platform behavior, or a genuine multi-prong audit | The full shape earns its keep: an owning agent, a multi-lens review split by dimension, top-tier models. |

**Three or four Opus-high sub-agents on a small fix is a defect in how you ran the playbook, not thoroughness.** If you are about to dispatch, name which row you're in and what you found that puts you there — if you can't, you're in "Small fix" and you should just fix it.

The escalation mandates elsewhere in the repo — the fan-out rule and multi-lens self-review in AGENTS.local.md, the L1/L2 shape in `implementation-thread` — describe the **Big** row. They are not a floor under every issue.

## Step 2 — Acknowledge an external report immediately

For an EXTERNAL reporter (not a maintainer, not self-filed), post the acknowledgement the moment you start work. It is exactly one word:

```bash
gh issue comment <n> --repo nubjs/nub --body "Investigating."
```

No "thanks for the report," no timeline, no preview of your hypothesis — a longer ack leaks half-formed theories and reads as noise. Internal/self-filed issues don't need this.

**Every substantive comment is terse + factual per PROSE.md.** State what you found and what you did, in the fewest words that carry the facts. Never write "previous comments were wrong" or similar meta-commentary — prior comments are often a bot's. The `gh-comment-guard` PreToolUse hook blocks an over-long `gh issue/pr comment` / `gh pr create` body; trim it, or set `NUB_ALLOW_LONG_COMMENT=1` only for a genuinely-needed longer body.

If triage shows it's not a bug, say so factually with the reason and close it per Step 6.

## Step 3 — Fix it, at the size you sized it

- **Small fix (most issues):** write it. The pre-push loop is the verification; your own read of your diff is the review. Don't dispatch anything.
- **Real change:** one implementation agent, or yourself. One reviewer if the logic isn't self-evident.
- **Big:** the orchestration methodology earns its keep (load the `orchestrator` skill by name) — you hold the goal set, dispatch model-tiered sub-agents, and the fix gets a multi-lens self-review scaled to blast radius.

Tier every agent you do dispatch by the judgment its task needs. A repro, a harvest, a doc update, or a CI watch is Sonnet or Haiku work; Opus is for the fix that lands.

Work in an isolated worktree off `origin/main`:

```bash
git worktree add /tmp/nub-fix-<n> -b fix-issue-<n> origin/main   # vendor/aube comes along (plain in-tree files)
cd /tmp/nub-fix-<n>
# Build through the wrapper; never export CARGO_TARGET_DIR yourself — that opts
# out of the CoW seeding that starts an isolated worktree warm (~14s) instead of
# cold (~40 min).
scripts/rust-build.sh build -p nub-cli --profile fast
```

Before pushing, run the pre-push local-verification loop: incremental build → the exact CI gates (`NUB_ALLOW_INCOMPLETE_RUNTIME=1 cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, scoped `cargo test`) → an e2e tmp-fixture run of the specific behavior → Docker for anything touching the global cache/config → promote a regression test for this bug into the suite. Get it green locally and push ONCE.

## Step 3b — Update docs if the fix changes user-facing behavior

If a user observes the change — a flag's effect, a default, an error message, a formerly-broken feature that now works — update `site/content/docs/` in the same PR. The fix is not done until docs reflect it.

## Step 4 — Open the PR, referencing the issue

The body MUST reference the issue: a closing keyword for a bug the PR resolves, `Refs #N` for related-but-not-resolving.

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

Report the PR URL. Do NOT merge your own PR.

## Step 5 — On merge, comment the resolution

`Closes #N` auto-closes the issue silently, so add a brief factual comment:

```bash
gh issue comment <n> --repo nubjs/nub --body "Fixed in #<pr> (merged to main). Will ship in the next release."
```

If it did NOT auto-close (no closing keyword, or a non-fix resolution), close it explicitly with a comment — never silently:

```bash
gh issue close <n> --repo nubjs/nub --comment "<what fixed it, or why no code fix is needed>"
```

## Step 6 — On release, comment the version + release link (mandatory)

A fix merged is not a fix shipped.

```bash
REL="https://github.com/nubjs/nub/releases/tag/v<ver>"
gh issue comment <n> --repo nubjs/nub --body "Shipped in v<ver>: $REL"
gh pr comment   <pr> --repo nubjs/nub --body "Shipped in v<ver>: $REL"
```

In practice the `release` skill's Step 5 executes this in bulk across the whole changeset; this documents the contract for a single issue.

---

## Quick reference

| Step | Action |
| --- | --- |
| Triage | `gh issue view <n> --comments` · read the thread · reproduce it yourself with a differential fixture |
| Size | Answer it · Small fix (default — just fix it, no sub-agents) · Real change (≤1 agent + ≤1 reviewer) · Big (full orchestrator shape) |
| Acknowledge | `gh issue comment <n> --body "Investigating."` — exactly that, nothing more (external only) |
| Fix | in a worktree off `origin/main`; machinery scaled to the size; pre-push loop green; add a regression test |
| Docs | Update `site/content/docs/` if behavior changed — same PR as the fix |
| PR | `gh pr create` with `Closes #<n>` in the body; report the URL; don't self-merge |
| On merge | `gh issue comment <n> --body "Fixed in #<pr>…"` (or `gh issue close --comment` if not auto-closed) |
| On release | `gh issue comment <n> --body "Shipped in v<ver>: <release URL>"` (via the `release` skill) |
