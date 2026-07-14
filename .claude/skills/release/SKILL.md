---
name: release
description: >-
  Cut a nub patch release end-to-end in one invocation. Invoke (via the Skill
  tool) once a release thread's targeted fixes are ALL landed on `main` and
  CI-green. Encodes the full runbook: pick the version (patch bump in the
  0.0.x/0.1.x pre-release regime), `make version` + `make version-check`,
  commit + tag + push (the `v*` tag triggers the 8-platform CI build → npm OIDC
  publish → GitHub Release), then draft comprehensive FACTUAL + NEUTRAL release
  notes from the full changeset and comment the version + release link on every
  closed issue + merged PR the release ships (mandatory maintainer hygiene). Do
  NOT cut until all fixes are green.
metadata:
  internal: true
---

# Cutting a nub release

A nub release is **tag-triggered and fully automated**. Pushing a `v*` tag fires `.github/workflows/release.yml`, which builds 8 platforms, gates them (test, lockfile conformance, glibc-floor, pre-publish smoke), publishes 10 npm packages via OIDC trusted publishing (no secrets), and creates a GitHub Release with 16 attached assets. The human-side work is: confirm the fixes are green, bump the version, push the tag, then write good notes and close the loop on issues/PRs.

**Guardrails (read first, non-negotiable):**

- **NEVER cut a release without the maintainer's EXPLICIT, IN-THE-MOMENT say-so (HIGH PRIORITY — #1 gate).** A release publishes to npm and cannot be un-published — it is a published-external, irreversible act, which makes the *timing* maintainer-owned, FULL STOP. Do NOT infer authorization from a standing goal ("land X so I can upgrade"), from a fix being merged + green, from a sub-agent reporting "autonomous per the release rules," or from autonomous-mode being on (autonomous mode explicitly excludes irreversible/published-external acts). "Green and ready to release" ≠ "release now" — the maintainer routinely wants to batch more in first. The orchestrator may PREPARE (confirm green, draft notes, stage the version) but MUST WAIT for an explicit "cut it" / "ship it" / "release now" before `make version` + tag + push. When work is green and a release *could* go, SURFACE it as a recommendation and ask — never tag. (Burned 2026-06-27: cut v0.2.6 off a sub-agent's "proceed autonomously" claim; the maintainer wanted to batch more first.)
- **Do NOT cut until every targeted fix is landed on `main` AND CI-green.** A release is the point of no return — you cannot un-publish an npm version. The whole reason this is a deliberate, tag-triggered flow. (This is a prerequisite, NOT authorization — the explicit say-so gate above still applies even when everything is green.)
- **Pre-release version regime: stay in `0.0.x` / `0.1.x`.** A normal release is a **patch bump** (`0.1.2 → 0.1.3`). Bump the minor to `0.2.0`/`0.1.0` only on explicit instruction (reserved for a polished public launch — whitepaper + benchmarks + install experience ready). Never invent a version; derive it from the latest tag.
- **The tag MUST equal the committed version.** CI's `verify` job fails the release if `v<tag>` ≠ `npm/nub/package.json` version. So: `make version` → commit → tag → push, in that order, with the tag matching exactly.
- **Release notes are FACTUAL and NEUTRAL — the repo is PUBLIC.** No braggy, competitive, or superlative framing; no "fastest", no "beats X", nothing a skeptic could screenshot. State WHAT CHANGED. This is the same rule as commit messages (see AGENTS.md "The repo is PUBLIC" + the commit-message rule). Also: no internal/benchmark-strategy/competitive discussion in the notes.

---

## Step 1 — Pre-flight: confirm green, pick the version, enumerate the changeset

```bash
git -C "$(git rev-parse --show-toplevel)" switch main && git pull --ff-only
git fetch --tags
PREV=$(git describe --tags --abbrev=0)        # e.g. v0.1.2 — the latest release tag
echo "Latest tag: $PREV"
git log "$PREV"..HEAD --oneline               # the full changeset since the last release
```

- Confirm the targeted fixes (from the release thread's "Fixes targeted for …" list) are **all present** in `$PREV..HEAD` and each is **CI-green on `main`**. If a fix is still converging or its CI is red, STOP — the release is blocked (the thread's `depends_on` gate). Slip it to the next patch rather than cutting early.
- **Confirm docs are current.** For every user-facing feature or behavior change in the changeset, verify that `site/content/docs/` already reflects it. A shipped feature whose docs lag is a release blocker — land the doc update on `main` before cutting the tag (not after).
- Pick the next version: patch-bump `$PREV` (drop the leading `v`). `v0.1.2` → `0.1.3`.
- Keep the `git log "$PREV"..HEAD` output — it is the raw material for both the release notes (Step 4) and the issue/PR loop (Step 5). Note any `vendor/aube/**` changes in the range (vendored PM-engine changes ship fork engine behavior; mention the user-facing effect, not the diff).

## Step 2 — Version bump

```bash
make version V=<ver>      # sets all 9 npm packages + Cargo.toml + runtime/version.mjs in lockstep
make version-check        # MUST pass: cross-package consistency + @oxc-project/runtime ↔ nub-native oxc pin
```

`make version-check` is the same gate CI's `verify` job runs; a non-zero exit here means the release would fail at CI immediately, so fix it before committing. `make version` also moves `runtime/version.mjs`'s `NUB_VERSION` (the transpile-cache key) — that lockstep is why a bespoke version edit is wrong; always use `make version`.

## Step 3 — Commit, tag, push (this triggers CI)

The release version-bump + tag commit is a deliberate EXCEPTION to the repo's PR-default flow (AGENTS.md "Default to a PR flow") — it commits DIRECTLY to `main`. The release is tag-triggered and not a reviewable feature diff, so no PR.

```bash
git add -A
git status                # SANITY: commit ONLY the version-bump files. If unrelated WIP is in the
                          # tree, stage just the touched version files (see the v0.1.2 precedent:
                          # the release commit kept in-flight site/.claude WIP out of it).
git commit -m "v<ver>"
git tag v<ver>
git push origin main --tags
```

Post-merge, fast-forward the shared tree so it tracks origin: `git -C <shared-tree> pull --ff-only` (the eagerly-pull rule, AGENTS.md "Default to a PR flow" — the shared checkout otherwise drifts behind as PRs land).

Pushing the `v<ver>` tag fires the release workflow. It runs, in order: `verify` (version + tag-match), `primer` (metadata primer generation), `test` + `conformance` + `glibc-floor-guard` + `pre-publish-gate` (the publish gates), `build` (8 platforms), then `publish-npm` (10 packages, idempotent), `github-release` (release + 16 assets, independently re-runnable), and `test-install` / `test-install-musl` (post-publish smoke of the published package).

**Watch CI, but never block the foreground on it.** Dispatch a background watcher (a sub-agent or a detached `gh run watch` writing to a log path) and report the log path; do not poll in the foreground. The release is not "done" until `publish-npm` + `github-release` are green.

## Step 4 — Comprehensive release notes (Opus)

CI's `github-release` job creates the release with `generate_release_notes: true` (GitHub's auto commit/PR list). **Replace that** with hand-written, scannable, factual notes — do not leave the release on the raw auto-list. Drive this on Opus.

Build the notes from the **full** `git log "$PREV"..HEAD` changeset (Step 1), not just the headline fixes — every user-affecting change ships.

**Notes must be SCANNABLE, not paragraph-dense.** A reader skims headings, tables, and the heads-up callout and gets the whole release at a glance — they should never have to read a run-on paragraph to find what changed. The cross-project prose/tone guide for all public-facing copy — including the release-notes shape — is [`PROSE.md`](../../../PROSE.md). The concrete rules:

- **One-line intro** stating what the release is about (the dominant theme).
- **Themed `##` sections, not generic buckets.** Group by what the changes *touch* — e.g. "Lockfile compatibility" / "Performance" / "Runtime fixes" / "Documentation" / "Testing & internals" — not by Fixes/Compatibility/Internal abstractions. Each major change gets a short titled blurb or a table row, never a multi-sentence paragraph.
- **A table for a batch of independent fixes.** When several small fixes share a theme (a run of lockfile fixes), put them in a table — `| Area | What changed | Commit |` — tables read far faster than a bullet wall.
- **A callout for heads-up / migration items.** Anything a user should know before upgrading (a cache-schema re-warm, a behavior change) goes in a GitHub-flavored alert: `> [!IMPORTANT]` (or `> [!NOTE]`), not buried in a bullet.
- **Per-item links.** Every fix/change links to its commit (`[`abc1234`](https://github.com/nubjs/nub/commit/<full-sha>)`) and/or PR (`[#17](https://github.com/nubjs/nub/pull/17)`). Issue refs link too (`[#16](https://github.com/nubjs/nub/issues/16)`).
- **An auto-generated `## What's Changed` section at the BOTTOM (MANDATORY) — this is what makes "lists every change" literally true.** GitHub's PR-level breakdown (every merged PR + author + New Contributors) plus the `**Full Changelog**: <PREV>...v<ver>` compare link, from `gh api …/releases/generate-notes` (command below). Append it verbatim under a `---` separator below the curated narrative — the curated themes stay on top, the exhaustive PR list goes underneath.
- **Tone: factual + neutral.** Readability ≠ hype. Each line states what changed. No superlatives, no competitive framing, no editorializing. (Same bar as commit messages — AGENTS.md.) Visual interest comes from structure (sections, tables, callouts), never from marketing language.

**Template** (adapt the section names to the actual changeset):

```markdown
<One-line intro: what this release is about.>

> [!IMPORTANT]
> **<Heads-up title>.** <The one thing to know before upgrading. Omit the callout if there's nothing.>

## <Theme A, e.g. Lockfile compatibility>

<Optional one-line lead.>

| Area | What changed | Commit |
| --- | --- | --- |
| <area> | <what changed, one clause> | [`<sha7>`](https://github.com/nubjs/nub/commit/<full-sha>) |

## <Theme B, e.g. Performance>

<Short blurb with the PR link inline.> ([#17](https://github.com/nubjs/nub/pull/17))

## Testing & internals

- <Bullet> ([`<sha7>`](https://github.com/nubjs/nub/commit/<full-sha>)).

---

## What's Changed

<!-- appended verbatim from `gh api …/releases/generate-notes` — the PR list, New Contributors, and Full Changelog link -->
* <PR title> by @<author> in https://github.com/nubjs/nub/pull/<n>

**Full Changelog**: https://github.com/nubjs/nub/compare/<PREV>...v<ver>
```

Generate the bottom `## What's Changed` breakdown mechanically so every merged PR is listed:

```bash
# PR-level list + New Contributors + Full Changelog compare link — append verbatim below the curated narrative
gh api repos/nubjs/nub/releases/generate-notes \
  -f tag_name=v<ver> -f previous_tag_name=$PREV --jq '.body'
```

Append that block under a `---` separator below the curated sections, then `gh release edit`. The curated narrative stays on top; this exhaustive PR list goes underneath.

Update the release body:

```bash
# Edit a notes file, then:
gh release edit v<ver> --notes-file <path-to-notes.md>
gh release view v<ver> --repo nubjs/nub --json body -q .body   # verify it rendered
```

The v0.1.4 and v0.1.3 release bodies are the reference exemplars of this structure.

## Step 4b — Publish the notes as a blog post (MANDATORY — every release)

Every release also ships as a blog post under `site/content/blog/`. This is a standard release step, done on every version — the same content/presentation-to-`main` exception as docs (commit directly to `main`, no PR). Before writing, invoke the `prose-writing` skill and follow PROSE.md (blog copy: routine patch notes stay factual, neutral, unsigned, scannable — no hype, no personality; a milestone version gets a fuller treatment but the same neutral bar).

- **File:** `site/content/blog/nub-<major>-<minor>-<patch>.mdx` (e.g. `nub-0-2-10.mdx`) — the filename is the URL slug (`/blog/nub-0-2-10`); fumadocs auto-globs `content/blog/*.mdx`, so no index/meta wiring is needed.
- **Frontmatter** (schema from `source.config.ts` — all four required): `title: "Nub <ver>"` (add a `: <theme>` subtitle only for a milestone/single-theme release), `description:` a plain sentence stating the theme **with NO inline code/backticks** (the field renders raw — de-emphasize code tokens to plain words), `author: The Nub Team`, `date: <YYYY-MM-DD>` **back-dated to the release's `publishedAt`** so the blog timeline stays chronological.
- **Body:** a short lede (the dominant theme), then the release's themed sections adapted to blog prose — NOT the raw "Commits in this release" changelog dump. Carry over the heads-up `> [!IMPORTANT]` / `> [!NOTE]` callouts and the per-theme tables; keep the PR/issue links that matter. Close with a link to the full release notes: `The [full release notes](https://github.com/nubjs/nub/releases/tag/v<ver>) list every change in this release.`
- **Scale to the release:** a small patch gets a short post (a few sections); a milestone (a minor bump, a headline feature) gets a fuller one that opens with the thing working (a code block within a sentence or two of the heading, per the blog rules).

Reference exemplars: any `site/content/blog/nub-0-2-*.mdx` post (`nub-0-2-0.mdx` for a milestone, `nub-0-2-5.mdx` for a small patch).

## Step 5 — Close the loop on issues + PRs (MANDATORY — always, no matter what)

Comment a brief factual note carrying **the version and a link to the release** on **EVERY closed issue and EVERY merged PR that shipped in this release** — not just the headline fixes. This is mandatory maintainer hygiene (AGENTS.md "Git & GitHub maintainer hygiene"); do it on every release without exception. Users see "fixed" the moment an issue closes, but the fix is not on the released binary until the tag publishes — this comment closes that credibility gap and gives the reporter a link to the exact release.

The release URL is `https://github.com/nubjs/nub/releases/tag/v<ver>`. Every comment includes both the version and that link, e.g. `Shipped in v<ver>: <release URL>`.

**Enumerate the targets MECHANICALLY — never a hand-typed list.** A hand-enumerated pass silently misses any issue still open at cut time or closed AFTER the cut (this happened on v0.3.0). Drive the set from the union of three queries:

```bash
# 1. Every issue a shipped PR auto-closes (closingIssuesReferences) + any Closes/Fixes/Resolves #N in a PR body:
gh pr list --repo nubjs/nub --state merged --search "merged:<PREV-date>..<cut-date>" \
  --json number,body,closingIssuesReferences --limit 200 \
  --jq '.[] | {pr:.number, closes:[.closingIssuesReferences[].number], refs:([.body|scan("(?i)(?:clos|fix|resolv)\\w*\\s+#(\\d+)")]|flatten)}'
# 2. Every issue closed in the release window (catches issues closed without a linked PR):
gh issue list --repo nubjs/nub --state closed --search "closed:<PREV-date>..<cut-date+1>" \
  --json number,title,stateReason --limit 200
```

For each issue/PR in the union, check whether it ALREADY carries the comment before posting (`gh issue view <n> --repo nubjs/nub --json comments --jq '[.comments[].body|select(test("Shipped in v<ver>"))]|length'`) — skip a `NOT_PLANNED` issue with no shipped fix. **Re-run this pass for any issue closed AFTER the cut** — a late-closing issue does not appear in the first sweep.

Then comment (short, factual — what fixed it + the version and release link, no fluff):

```bash
REL="https://github.com/nubjs/nub/releases/tag/v<ver>"
gh issue comment <n> --body "Fixed in v<ver> (now published): $REL"
gh pr comment <n>    --body "Shipped in v<ver>: $REL"
```

Hit **every** issue and PR the mechanical union above surfaces — not just the headline fixes. This is non-optional; do not skip an issue because it was "minor," and do not fall back to the release thread's targeted-fix list as the source of truth (it under-counts). Do not comment on issues unrelated to the release.

## Step 6 — Post-release verify

Confirm the automated publish actually landed:

```bash
npm view @nubjs/nub@<ver> version            # the root package is on the registry
npm view @nubjs/nub@<ver> dist.tarball        # sanity: published artifact exists
gh release view v<ver> --json assets --jq '.assets[].name' | sort
# expect 16 assets: 8 platforms × {archive, .sha256}
#   nub-darwin-arm64.tar.gz(.sha256), nub-darwin-x64.tar.gz(.sha256),
#   nub-linux-x64.tar.gz(.sha256), nub-linux-x64-musl.tar.gz(.sha256),
#   nub-linux-arm64.tar.gz(.sha256), nub-linux-arm64-musl.tar.gz(.sha256),
#   nub-win32-x64.zip(.sha256), nub-win32-arm64.zip(.sha256)
```

A complete release has: the 10 npm packages published (`@nubjs/nub`, `@nubjs/nub-<platform>` ×8, `@nubjs/types`), the GitHub Release present, and all 16 assets attached. CI's own `github-release` job already asserts the 16 assets and `test-install` smokes the published package — this step is the human confirmation that the workflow reached green.

**If CI failed partway:** `publish-npm` and `github-release` are split + idempotent on purpose — re-run the failed job from the Actions UI (npm publish skips already-published packages; the release job re-uploads only missing assets). A version is never re-cut for a flaky asset upload; just re-run the job.

---

## Quick reference

| Step | Command |
| --- | --- |
| Changeset | `git log $(git describe --tags --abbrev=0)..HEAD --oneline` |
| Bump | `make version V=<ver>` → `make version-check` |
| Cut | `git commit -m "v<ver>"` → `git tag v<ver>` → `git push origin main --tags` |
| Notes | `gh release edit v<ver> --notes-file notes.md` |
| Blog | `site/content/blog/nub-<x>-<y>-<z>.mdx` — publish the notes as a post, back-dated to `publishedAt` (direct to `main`) |
| Loop | `gh issue comment <n> --body "Fixed in v<ver>: <release URL>"` (every closed issue + merged PR — mandatory) |
| Verify | `npm view @nubjs/nub@<ver> version` · `gh release view v<ver> --json assets` |

Invoked via the Skill tool once a release thread's targeted fixes are all landed on `main` and CI-green.
