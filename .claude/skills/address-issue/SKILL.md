---
name: address-issue
description: >-
  End-to-end playbook for working a GitHub issue (or bug-fix PR) on nubjs/nub:
  triage → acknowledge an external report with an "Investigating" comment →
  reproduce and fix (via the fray methodology + the pre-push local-verification
  loop) → open a PR that references the issue with `Closes #N` → on merge,
  comment the resolution → on release, comment the version + release link. Invoke
  (via the Skill tool) whenever you pick up an issue to work, or are asked to fix
  a reported bug. Encodes the maintainer-hygiene conventions in AGENTS.md so the
  reporter is acknowledged, the issue is auto-closed by the merge, and the loop
  is closed when the fix ships.
---

# Addressing a nub issue end-to-end

This is the playbook for taking a GitHub issue from "reported" to "shipped and the reporter told." It exists because maintainer responsiveness on a PUBLIC repo is a visible health signal: a reporter who gets an acknowledgement, sees their issue auto-close on merge, and gets a link to the release it shipped in has a completely different experience from one whose issue goes silent. The hygiene rules here are mandatory, not courtesies (AGENTS.md "Git & GitHub maintainer hygiene").

**Guardrails (read first):**

- **Tone is always factual, neutral, professional — never braggy, competitive, or over-promising.** Every comment follows the project's tone bar (AGENTS.md "The repo is PUBLIC" + the commit-message tone rule; the cross-project GitHub-comment + prose guide is [`PROSE.md`](../../../PROSE.md)). Acknowledge sincerely; state what you found and what you did; don't promise a timeline you can't keep.
- **A fix lands via the PR-from-a-worktree flow, not directly on the shared `main` tree.** Substantive fixes are reviewable PRs opened from an isolated worktree (AGENTS.md "Default to a PR flow"). Trivial doc/typo fixes are the documented exception.
- **Verify the fix end-to-end before pushing — don't outsource verification to CI.** Run the pre-push local-verification loop (AGENTS.md "VERIFY LOCALLY BEFORE PUSHING"). A green test suite with a stubbed fix is worse than an unchecked one.
- **Don't autonomously land a change to a default / security posture / product behavior / API-config-env surface.** Those are recommend-only until the maintainer signs off (AGENTS.md). A mechanical, clearly-a-bug fix may land; a behavior decision routes back as a question.

---

## Step 1 — Triage: read the issue, the comments, and reproduce

```bash
gh issue view <n> --repo nubjs/nub --comments
```

- **Read the COMMENTS and the resolution history, not just the body.** The body says what someone wanted; the thread says what's true. A "feature request" can be a deliberate prior rejection; a "bug" can already be fixed on `main`. (AGENTS.md "Probing methodology" — read the comments.)
- **Classify it.** Is it a genuine bug, a usage question, a feature request, a duplicate, or already-fixed? Is the fix mechanical (clearly-a-bug, land it) or does it touch a default / behavior / API-config-env surface the maintainer owns (propose, get sign-off)?
- **Reproduce empirically before reading source or deciding.** Build a minimal differential fixture — the behavior under nub vs. the reference tool it claims parity with (npm/pnpm/yarn/bun/node) on identical input — in a throwaway `/tmp` dir. A divergence IS the finding. Test what it actually does; don't infer from code (AGENTS.md "Probing methodology — differential fixtures").

## Step 2 — Acknowledge an external report immediately

If the issue is from an EXTERNAL reporter (not a maintainer / not self-filed), post a brief acknowledgement the moment you start work, so they know it's seen:

```bash
gh issue comment <n> --repo nubjs/nub --body "Investigating — thanks for the report. Will follow up here."
```

Keep it short and sincere; state that you're looking into it, not when it'll be fixed. (Internal / self-filed issues don't need this.)

If triage shows it's NOT a bug (working as intended, a usage question, a duplicate, won't-fix), say so factually with the reason and close it per Step 6 — don't leave it hanging or sink time into a non-fix.

## Step 3 — Fix it (fray + the pre-push loop)

For anything beyond a one-line fix, drive the work with the **fray methodology** (the globally-installed `fray` plugin skill — load it by name): you orchestrate, dispatch model-tiered sub-agents as instruments (Opus for the fix that lands, Sonnet for supporting work, Haiku for scripted harvest), and a substantive fix gets a SEPARATE self-review pass before it's marked done. Scale the review to the blast radius — a far-reaching change (registry/lockfile/config/security/default) gets a multi-lens reviewer fleet.

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
| Triage | `gh issue view <n> --comments` · read the thread · reproduce with a differential fixture |
| Acknowledge | `gh issue comment <n> --body "Investigating — thanks for the report…"` (external only) |
| Fix | fray-driven, in a worktree off `origin/main`; self-review; pre-push loop green; add a regression test |
| Docs | Update `site/content/docs/` if behavior changed — same PR as the fix |
| PR | `gh pr create` with `Closes #<n>` in the body; report the URL; don't self-merge |
| On merge | `gh issue comment <n> --body "Fixed in #<pr>…"` (or `gh issue close --comment` if not auto-closed) |
| On release | `gh issue comment <n> --body "Shipped in v<ver>: <release URL>"` (via the `release` skill, across the whole changeset) |

Invoke via the Skill tool whenever you pick up a GitHub issue to work or are asked to fix a reported bug.
