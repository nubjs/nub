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

A nub release is tag-triggered and fully automated. Pushing a `v*` tag fires `.github/workflows/release.yml`, which builds 8 platforms, gates them (test, lockfile conformance, glibc-floor, pre-publish smoke), publishes 10 npm packages via OIDC trusted publishing, and creates a GitHub Release with 16 assets. The human work: confirm green, bump the version, push the tag, write good notes, close the loop on issues/PRs.

## Guardrails

- **Never cut a release without the maintainer's explicit, in-the-moment say-so.** Publishing to npm is irreversible, so the timing is maintainer-owned. Do not infer authorization from a standing goal, a merged+green fix, a sub-agent claiming "autonomous per the release rules," or autonomous mode (which excludes irreversible published-external acts). Green ≠ release now. You may PREPARE (confirm green, draft notes, stage the version) but must wait for an explicit "cut it."
- **Do not cut until every targeted fix is landed on `main` AND CI-green.** A prerequisite, not authorization.
- **Pre-release version regime: stay in `0.0.x` / `0.1.x`.** A normal release is a patch bump. Bump the minor only on explicit instruction. Never invent a version; derive it from the latest tag.
- **The tag MUST equal the committed version** — CI's `verify` job fails if `v<tag>` ≠ `npm/nub/package.json` version. So: `make version` → commit → tag → push, in that order.
- **Release notes are FACTUAL and NEUTRAL — the repo is PUBLIC.** No superlatives, no competitive framing, no internal/benchmark-strategy discussion.

---

## Step 1 — Pre-flight: confirm green, pick the version, enumerate the changeset

```bash
git -C "$(git rev-parse --show-toplevel)" switch main
# NOT `pull --ff-only`: pull.rebase=true makes pull abort on any dirty file, and the shared tree
# always carries some agent's WIP. `merge --ff-only` has no clean-tree precondition.
git fetch origin && git merge --ff-only origin/main
git fetch --tags
PREV=$(git describe --tags --abbrev=0)        # e.g. v0.1.2 — the latest release tag
echo "Latest tag: $PREV"
git log "$PREV"..HEAD --oneline               # the full changeset since the last release
```

- Confirm the targeted fixes are all present in `$PREV..HEAD` and each is CI-green on `main`. If one is red or still converging, STOP and slip it to the next patch.
- **Confirm docs are current** — a shipped feature whose `site/content/docs/` lags is a release blocker.
- Pick the next version: patch-bump `$PREV`, dropping the leading `v`.
- Keep the `git log` output — raw material for Steps 4 and 5. For `vendor/aube/**` changes, note the user-facing effect, not the diff.

## Step 2 — Version bump

```bash
make version V=<ver>      # sets all 9 npm packages + Cargo.toml + runtime/version.mjs in lockstep
make version-check        # MUST pass: cross-package consistency + @oxc-project/runtime ↔ nub-native oxc pin
```

`make version-check` is the same gate CI's `verify` job runs. `make version` also moves `runtime/version.mjs`'s `NUB_VERSION` (the transpile-cache key) — that lockstep is why a bespoke edit is wrong.

## Step 3 — Commit, tag, push (this triggers CI)

The release commit is a deliberate exception to the PR-default flow — direct to `main`.

```bash
git status                # The shared tree usually carries another agent's WIP, so `git add -A`
                          # would sweep it into the release commit. Path-scope instead:
git commit -m "v<ver>" -- Cargo.lock Cargo.toml \
  crates/nub-native/Cargo.lock crates/nub-native/Cargo.toml \
  npm/*/package.json runtime/version.mjs
git show --stat HEAD      # SANITY: 15 files, all version bumps, nothing else.

# TWO pushes, never `git push origin main --tags`. This clone has ~155 local tags against
# ~84 on the remote — v1.x leftovers from the Node fork this repo began as — and `--tags`
# offers every one of them. The remote rejects them AND the whole push dies with them, so
# `main` does not land either and the release silently does not start.
git push origin main
git tag v<ver>
git push origin v<ver>    # the single tag: THIS is what triggers the publish
```

Then fast-forward the shared tree: `git -C <shared-tree> fetch origin && git -C <shared-tree> merge --ff-only origin/main` (never `pull --ff-only` — it aborts on the shared tree's ever-present WIP).

The workflow runs, in order: `verify` (version + tag-match), `primer`, `test` + `conformance` + `glibc-floor-guard` + `pre-publish-gate`, `build` (8 platforms), `publish-npm` (10 packages, idempotent), `github-release` (release + 16 assets, independently re-runnable), then the post-publish fan-out — `test-install` / `test-install-musl`, `docker`, `bump-homebrew-tap`, `submit-winget`.

**Watch CI, but never block the foreground on it.** Dispatch a background watcher and report the log path. The release is not done until `publish-npm` + `github-release` are green.

### The other distribution channels ride the same tag — no manual step, but they are not free

npm is not the only thing a tag publishes. Two jobs push OUTSIDE this repo, and neither needs a manual action:

- **`bump-homebrew-tap`** regenerates `Formula/nub.rb` with `.github/scripts/gen-homebrew-formula.sh` and pushes it to [`nubjs/homebrew-tap`](https://github.com/nubjs/homebrew-tap). It reads the release's own `.sha256` sidecars, so it needs `github-release` to have finished. It is gated on the `HOMEBREW_TAP_TOKEN` secret and SKIPS WITH A WARNING if that secret is ever absent — a skip is a silent stale tap, so treat the warning as a failure.
- **`submit-winget`** opens a PR against `microsoft/winget-pkgs`. Gated on `WINGET_PAT`, which is currently unset, so this job no-ops today.

**The formula is regenerated from the script at the tagged commit, which makes it a CLOBBER.** Any hand-edit to the tap is overwritten by the next release. So a tap hotfix is only ever a stopgap: the generator fix has to be on `main` BEFORE the tag, or the release silently reverts it. This is how [#676](https://github.com/nubjs/nub/issues/676) shipped — the archive layout changed, the generator was not updated with it, and nothing read the formula before it reached users. `bump-homebrew-tap` now installs the formula from a throwaway local tap on macOS before pushing it, so a formula that cannot install fails the job and leaves the tap on the previous working version.

## Step 4 — Comprehensive release notes (Opus)

CI creates the release with `generate_release_notes: true`. **Replace that** with hand-written, scannable, factual notes built from the **full** `git log "$PREV"..HEAD` changeset, not just the headline fixes. Follow [`PROSE.md`](../../../PROSE.md). Rules:

- **One-line intro** stating the dominant theme.
- **Themed `##` sections, not generic buckets** — group by what changes *touch* ("Lockfile compatibility", "Performance", "Runtime fixes"), not Fixes/Compatibility/Internal. Short titled blurbs or table rows, never multi-sentence paragraphs.
- **A table for a batch of independent fixes** — `| Area | What changed | Commit |`.
- **A callout for heads-up / migration items** — `> [!IMPORTANT]` or `> [!NOTE]`, never buried in a bullet.
- **Per-item links** — commit (`[`abc1234`](https://github.com/nubjs/nub/commit/<full-sha>)`), PR (`[#17](https://github.com/nubjs/nub/pull/17)`), issue.
- **An auto-generated `## What's Changed` section at the BOTTOM (mandatory)** — the exhaustive PR list + `**Full Changelog**` compare link, appended verbatim under a `---` separator below the curated narrative.
- **Tone: factual + neutral.** Visual interest comes from structure, never marketing language.

Template (adapt the section names to the actual changeset):

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

Generate the bottom section mechanically, then publish:

```bash
# PR-level list + New Contributors + Full Changelog compare link — append verbatim below the curated narrative
gh api repos/nubjs/nub/releases/generate-notes \
  -f tag_name=v<ver> -f previous_tag_name=$PREV --jq '.body'

# Edit a notes file, then:
gh release edit v<ver> --notes-file <path-to-notes.md>
gh release view v<ver> --repo nubjs/nub --json body -q .body   # verify it rendered
```

The v0.1.4 and v0.1.3 release bodies are the reference exemplars.

## Step 4b — Publish the notes as a blog post (mandatory, every release)

Same content-to-`main` exception as docs (commit directly, no PR). Invoke the `prose-writing` skill first.

- **File:** `site/content/blog/nub-<major>-<minor>-<patch>.mdx` (e.g. `nub-0-2-10.mdx`) — the filename is the URL slug; fumadocs auto-globs `content/blog/*.mdx`, so no index/meta wiring is needed.
- **Frontmatter** (schema from `source.config.ts`, all four required): `title: "Nub <ver>"` (add a `: <theme>` subtitle only for a milestone), `description:` a plain sentence with **no inline code/backticks** (the field renders raw), `author: The Nub Team`, `date: <YYYY-MM-DD>` **back-dated to the release's `publishedAt`** so the timeline stays chronological.
- **Body:** a short lede, then the release's themed sections adapted to blog prose — not a raw changelog dump. Carry over the callouts and per-theme tables. Close with `The [full release notes](https://github.com/nubjs/nub/releases/tag/v<ver>) list every change in this release.`
- **Scale to the release:** a small patch gets a short post; a milestone opens with the thing working.

Exemplars: `site/content/blog/nub-0-2-0.mdx` (milestone), `nub-0-2-5.mdx` (small patch).

## Step 5 — Close the loop on issues + PRs (mandatory, every release)

Comment the version and release link on **every closed issue and every merged PR that shipped** — not just the headline fixes. Users see "fixed" when an issue closes, but the fix is not on a published binary until the tag publishes.

Release URL: `https://github.com/nubjs/nub/releases/tag/v<ver>`.

**Enumerate the targets mechanically — never a hand-typed list**, which silently misses issues still open at cut time or closed after the cut. Drive the set from the union of:

```bash
# 1. Every issue a shipped PR auto-closes (closingIssuesReferences) + any Closes/Fixes/Resolves #N in a PR body:
gh pr list --repo nubjs/nub --state merged --search "merged:<PREV-date>..<cut-date>" \
  --json number,body,closingIssuesReferences --limit 200 \
  --jq '.[] | {pr:.number, closes:[.closingIssuesReferences[].number], refs:([.body|scan("(?i)(?:clos|fix|resolv)\\w*\\s+#(\\d+)")]|flatten)}'
# 2. Every issue closed in the release window (catches issues closed without a linked PR):
gh issue list --repo nubjs/nub --state closed --search "closed:<PREV-date>..<cut-date+1>" \
  --json number,title,stateReason --limit 200
```

For each, check whether it already carries the comment before posting; skip a `NOT_PLANNED` issue with no shipped fix. **Re-run this pass for any issue closed AFTER the cut.**

```bash
gh issue view <n> --repo nubjs/nub --json comments --jq '[.comments[].body|select(test("Shipped in v<ver>"))]|length'

REL="https://github.com/nubjs/nub/releases/tag/v<ver>"
gh issue comment <n> --body "Fixed in v<ver> (now published): $REL"
gh pr comment <n>    --body "Shipped in v<ver>: $REL"
```

Hit every issue and PR the union surfaces. Do not skip one for being "minor," do not fall back to the release thread's targeted-fix list (it under-counts), and do not comment on unrelated issues.

## Step 6 — Post-release verify

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

Then confirm the Homebrew channel actually moved — the tap lives in another repo, so a green release run here is not evidence that it did:

```bash
gh api repos/nubjs/homebrew-tap/contents/Formula/nub.rb --jq '.content' | base64 -d | grep -E 'version|bin\.install'
# expect version "<ver>" and the bin.install lines matching the current archive layout

brew update && brew install nubjs/tap/nub && nub --version && nubx --help | head -1
# or, without touching your own machine:
docker run --rm homebrew/brew brew install nubjs/tap/nub
```

A complete release has the 10 npm packages published (`@nubjs/nub`, `@nubjs/nub-<platform>` ×8, `@nubjs/types`), the GitHub Release present, all 16 assets attached, and the tap formula bumped to `<ver>` and installable.

**If CI failed partway:** `publish-npm` and `github-release` are split + idempotent on purpose — re-run the failed job from the Actions UI (npm publish skips already-published packages; the release job re-uploads only missing assets). Never re-cut a version for a flaky asset upload. `bump-homebrew-tap` is re-runnable too, and failing it is the safe outcome: the tap keeps serving the previous version rather than a broken formula, so fix the generator on `main` and re-run the job — never hand-edit the tap as the fix, since the next release regenerates it.

---

## Quick reference

| Step | Command |
| --- | --- |
| Changeset | `git log $(git describe --tags --abbrev=0)..HEAD --oneline` |
| Bump | `make version V=<ver>` → `make version-check` |
| Cut | `git commit -m "v<ver>" -- <version files>` → `git push origin main` → `git tag v<ver>` → `git push origin v<ver>` (never `--tags`) |
| Notes | `gh release edit v<ver> --notes-file notes.md` |
| Blog | `site/content/blog/nub-<x>-<y>-<z>.mdx` — back-dated to `publishedAt` (direct to `main`) |
| Tap | automatic via `bump-homebrew-tap`; verify with `gh api repos/nubjs/homebrew-tap/contents/Formula/nub.rb --jq .content \| base64 -d \| head -5` |
| Loop | `gh issue comment <n> --body "Fixed in v<ver>: <release URL>"` (every closed issue + merged PR) |
| Verify | `npm view @nubjs/nub@<ver> version` · `gh release view v<ver> --json assets` |
