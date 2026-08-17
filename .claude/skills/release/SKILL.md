---
name: release
description: >-
  Cut a Nub patch release end-to-end in one invocation. Invoke (via the Skill
  tool) once a release thread's targeted fixes are ALL landed on `main` and
  CI-green. Encodes the full runbook: pick the version (patch bump in the
  0.0.x/0.1.x pre-release regime), audit `@nubjs/types`, run `make version`
  + `make version-check`,
  commit + tag + push (the `v*` tag triggers the 8-platform build → glibc and
  pre-publish native gates → immutable 32-asset prerelease → npm OIDC publish
  → stable GitHub Release presentation), then draft comprehensive FACTUAL + NEUTRAL release
  notes from the full changeset and comment the version + release link on every
  closed issue + merged PR the release ships (mandatory maintainer hygiene). Do
  NOT cut until all fixes are green.
metadata:
  internal: true
---

# Cutting a Nub release

A Nub release is tag-triggered and fully automated. Pushing a `v*` tag fires `.github/workflows/release.yml`, which builds 8 platforms, gates them (test, lockfile conformance, glibc-floor, pre-publish smoke), creates an immutable prerelease with 32 assets, publishes 10 npm packages via OIDC trusted publishing, and presents the stable GitHub Release. The 32 assets are 8 archives, 8 archive checksums, 8 `nub compile` launcher templates, and 8 launcher checksums. The human work: confirm green, reconcile the runtime with `@nubjs/types`, bump the version, push the tag, write good notes, close the loop on issues/PRs.

**Guardrails (read first, non-negotiable):**

- **Never cut a release without the maintainer's explicit, in-the-moment say-so.** Publishing to npm is irreversible, so the timing is maintainer-owned. Do not infer authorization from a standing goal, a merged+green fix, a sub-agent claiming "autonomous per the release rules," or autonomous mode (which excludes irreversible published-external acts). Green ≠ release now. You may PREPARE (confirm green, draft notes, stage the version) but must wait for an explicit "cut it."
- **Do not cut until every targeted fix is landed on `main` AND CI-green.** A prerequisite, not authorization.
- **Do not version until the type-declaration audit is complete.** Invoke the `type-declarations` skill for every release. Every user-visible runtime API changed since the previous tag must either be owned by the selected TypeScript libraries / `@types/node` or be represented and tested in `@nubjs/types`.
- **Pre-release version regime: stay in `0.0.x` / `0.1.x`.** A normal release is a patch bump. Bump the minor only on explicit instruction. Never invent a version; derive it from the latest tag.
- **The tag MUST equal the committed version** — CI's `verify` job fails if `v<tag>` ≠ `npm/nub/package.json` version. So: `make version` → commit → tag → push, in that order.
- **Release notes are FACTUAL and NEUTRAL — the repo is PUBLIC.** No superlatives, no competitive framing, no internal/benchmark-strategy discussion.

---

## Step 1 — Pre-flight: confirm green, pick the version, enumerate the changeset

```bash
git -C "$(git rev-parse --show-toplevel)" switch main && git pull --ff-only
git fetch --tags
PREV=$(git describe --tags --abbrev=0)        # e.g. v0.1.2 — the latest release tag
echo "Latest tag: $PREV"
git log "$PREV"..HEAD --oneline               # the full changeset since the last release
```

- Confirm the targeted fixes are all present in `$PREV..HEAD` and each is CI-green on `main`. If one is red or still converging, STOP and slip it to the next patch.
- **Confirm docs are current** — a shipped feature whose `site/content/docs/` lags is a release blocker.
- **Invoke the `type-declarations` skill and complete its mandatory release audit.** Reconcile every user-visible runtime change in `$PREV..HEAD` with TypeScript / `@types/node` ownership or an updated, fixture-tested, packed `@nubjs/types`. Missing or unverified declarations are a release blocker.
- Pick the next version: patch-bump `$PREV`, dropping the leading `v`.
- Keep the `git log` output — raw material for Steps 4 and 5. For `vendor/aube/**` changes, note the user-facing effect, not the diff.

## Step 2 — Version bump

```bash
make version V=<ver>      # sets all 10 npm packages + Cargo.toml + runtime/version.mjs in lockstep
make version-check        # MUST pass: cross-package consistency + @oxc-project/runtime ↔ nub-native oxc pin
```

`make version-check` is the same gate CI's `verify` job runs; a non-zero exit here means the release would fail at CI immediately, so fix it before committing. `make version` also moves `runtime/version.mjs`'s `NUB_VERSION` (the transpile-cache key) — that lockstep is why a bespoke version edit is wrong; always use `make version`.

## Step 3 — Commit, tag, push (this triggers CI)

The release version-bump + tag commit is a deliberate EXCEPTION to the repo's PR-default flow (AGENTS.md "Default to a PR flow") — it commits DIRECTLY to `main`. The release is tag-triggered and not a reviewable feature diff, so no PR.

```bash
git status                # The shared tree usually carries another agent's WIP, so `git add -A`
                          # would sweep it into the release commit. Path-scope instead:
git commit -m "v<ver>" -- Cargo.lock Cargo.toml \
  crates/nub-core/Cargo.toml \
  crates/nub-native/Cargo.lock crates/nub-native/Cargo.toml \
  crates/nub-launcher/Cargo.lock \
  npm/*/package.json runtime/version.mjs
git show --stat HEAD      # SANITY: 17 files, all version bumps, nothing else.

# TWO pushes, never `git push origin main --tags`. This clone has ~155 local tags against
# ~84 on the remote — v1.x leftovers from the Node fork this repo began as — and `--tags`
# offers every one of them. The remote rejects them AND the whole push dies with them, so
# `main` does not land either and the release silently does not start.
git push origin main
git tag v<ver>
git push origin v<ver>    # the single tag: THIS is what triggers the publish
```

Post-merge, fast-forward the shared tree so it tracks origin: `git -C <shared-tree> pull --ff-only` (the eagerly-pull rule, AGENTS.md "Default to a PR flow" — the shared checkout otherwise drifts behind as PRs land).

The workflow runs, in order: `verify` (version + tag-match), `primer`, `test` + `conformance` + `glibc-floor-guard` + `pre-publish-gate`, `build` (8 platforms), `stable-immutable-release` (32 assets), `publish-npm` (10 packages, idempotent), `github-release` (stable presentation), then the post-publish fan-out — `test-install` / `test-install-musl`, `docker`, `bump-homebrew-tap`, `submit-winget`.

**Watch CI through the `ci-watch` skill until it returns a terminal verdict.** Keep the selected monitor in a tracked persistent process or an owned live agent; never detach `gh run watch` and infer completion from a log. The release is not done until `stable-immutable-release`, `publish-npm`, and `github-release` are green.

### The other distribution channels ride the same tag — no manual step, but they are not free

npm is not the only thing a tag publishes. Two jobs push OUTSIDE this repo, and neither needs a manual action:

- **`bump-homebrew-tap`** regenerates `Formula/nub.rb` with `.github/scripts/gen-homebrew-formula.sh` and pushes it to [`nubjs/homebrew-tap`](https://github.com/nubjs/homebrew-tap). It reads the release's own `.sha256` sidecars, so it needs `github-release` to have finished. It is gated on the `HOMEBREW_TAP_TOKEN` secret and SKIPS WITH A WARNING if that secret is ever absent — a skip is a silent stale tap, so treat the warning as a failure.
- **`submit-winget`** opens a PR against `microsoft/winget-pkgs`. Gated on `WINGET_PAT`, which is currently unset, so this job no-ops today.

**The formula is regenerated from the script at the tagged commit, which makes it a CLOBBER.** Any hand-edit to the tap is overwritten by the next release. So a tap hotfix is only ever a stopgap: the generator fix has to be on `main` BEFORE the tag, or the release silently reverts it. This is how [#676](https://github.com/nubjs/nub/issues/676) shipped — the archive layout changed, the generator was not updated with it, and nothing read the formula before it reached users. `bump-homebrew-tap` now installs the formula from a throwaway local tap on macOS before pushing it, so a formula that cannot install fails the job and leaves the tap on the previous working version.

## Step 4 — Comprehensive release notes (Opus)

CI's `stable-immutable-release` job creates the prerelease with `generate_release_notes: true`, and `github-release` promotes it after npm succeeds. **Replace the generated body** with hand-written, scannable, factual notes; do not leave the release on the raw auto-list. Drive this on Opus.

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

- **File:** `site/content/blog/nub-<major>-<minor>-<patch>.mdx` (e.g. `nub-0-2-10.mdx`) — the filename is the URL slug; fumadocs auto-globs `content/blog/*.mdx`, so no index/meta wiring is needed.
- **Frontmatter** (schema from `source.config.ts`, all four required): `title: "Nub <ver>"` (add a `: <theme>` subtitle only for a milestone), `description:` a plain sentence with **no inline code/backticks** (the field renders raw), `author: The Nub Team`, `date: <YYYY-MM-DD>` **back-dated to the release's `publishedAt`** so the timeline stays chronological.
- **Body:** a short lede, then the release's themed sections adapted to blog prose — not a raw changelog dump. Carry over the callouts and per-theme tables. Close with `The [full release notes](https://github.com/nubjs/nub/releases/tag/v<ver>) list every change in this release.`
- **Structure a feature-carrying release around its features:** one top-level `##` per major new feature, then `## Breaking changes`, then `## Bug fixes`. A batch of independent fixes goes in a table whose FIRST column is the PR link — `.blog-prose td:not(:last-child)` is `width:1%` + `nowrap` by design, so a prose column anywhere but last blows the table past the 720px article column.
- **End every post with the get-started block** — a final `## Get started` heading followed by `<GetStarted />`, which renders the install tabs plus the pointer at the agent adoption prompt. Every existing post carries it; a new one without it is the odd one out.
- **Catch up a cold reader with `<NubIntro />`** near the top when the post leads with feature news rather than an introduction. Both components live in `site/src/components/` and are registered globally in `site/mdx-components.tsx`; their copy is maintainer-authored, so edit the component, never a single `.mdx`.
- **Scale to the release:** a small patch gets a short post; a milestone opens with the thing working.

Exemplars: `site/content/blog/nub-0-7-0.mdx` (feature-carrying, full structure), `nub-0-2-0.mdx` (milestone), `nub-0-2-5.mdx` (small patch).

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
# expect these exact 32 assets:
# nub-darwin-arm64.tar.gz
# nub-darwin-arm64.tar.gz.sha256
# nub-darwin-x64.tar.gz
# nub-darwin-x64.tar.gz.sha256
# nub-linux-arm64.tar.gz
# nub-linux-arm64.tar.gz.sha256
# nub-linux-arm64-musl.tar.gz
# nub-linux-arm64-musl.tar.gz.sha256
# nub-linux-x64.tar.gz
# nub-linux-x64.tar.gz.sha256
# nub-linux-x64-musl.tar.gz
# nub-linux-x64-musl.tar.gz.sha256
# nub-win32-arm64.zip
# nub-win32-arm64.zip.sha256
# nub-win32-x64.zip
# nub-win32-x64.zip.sha256
# nub-launcher-darwin-arm64
# nub-launcher-darwin-arm64.sha256
# nub-launcher-darwin-x64
# nub-launcher-darwin-x64.sha256
# nub-launcher-linux-arm64
# nub-launcher-linux-arm64.sha256
# nub-launcher-linux-arm64-musl
# nub-launcher-linux-arm64-musl.sha256
# nub-launcher-linux-x64
# nub-launcher-linux-x64.sha256
# nub-launcher-linux-x64-musl
# nub-launcher-linux-x64-musl.sha256
# nub-launcher-win32-arm64.exe
# nub-launcher-win32-arm64.exe.sha256
# nub-launcher-win32-x64.exe
# nub-launcher-win32-x64.exe.sha256
```

A complete release has: the 10 npm packages published (`@nubjs/nub`, `@nubjs/nub-<platform>` ×8, `@nubjs/types`), the stable GitHub Release present, and all 32 assets attached. CI's `stable-immutable-release` job asserts the 32 assets before npm can publish, `github-release` promotes that same release after npm succeeds, and `test-install` smokes the published package. This step confirms that the workflow reached green.

The 8 `nub-launcher-*` assets are what `nub compile --platform <foreign>` fetches to cross-compile, so a release missing one silently disables cross-compiling to that platform for everyone on that version.

**If CI failed partway:** Re-run the failed job from the Actions UI. The `stable-immutable-release` job rebuilds the deterministic archives and re-uploads missing assets before npm starts; `publish-npm` skips packages already published during a partial attempt; `github-release` only promotes the asset-complete prerelease. Do not re-cut a version for a failed upload or partial npm publish.

Then confirm the Homebrew channel actually moved — the tap lives in another repo, so a green release run here is not evidence that it did:

```bash
gh api repos/nubjs/homebrew-tap/contents/Formula/nub.rb --jq '.content' | base64 -d | grep -E 'version|bin\.install'
# expect version "<ver>" and the bin.install lines matching the current archive layout

brew update && brew install nubjs/tap/nub && nub --version && nubx --help | head -1
# or, without touching your own machine:
docker run --rm homebrew/brew brew install nubjs/tap/nub
```

A complete release has the 10 npm packages published (`@nubjs/nub`, `@nubjs/nub-<platform>` ×8, `@nubjs/types`), the GitHub Release present, all 32 assets attached, and the tap formula bumped to `<ver>` and installable.

**If CI failed partway:** `publish-npm` and `github-release` are split + idempotent on purpose — re-run the failed job from the Actions UI (npm publish skips already-published packages; the release job re-uploads only missing assets). Never re-cut a version for a flaky asset upload. `bump-homebrew-tap` is re-runnable too, and failing it is the safe outcome: the tap keeps serving the previous version rather than a broken formula, so fix the generator on `main` and re-run the job — never hand-edit the tap as the fix, since the next release regenerates it.

---

## Quick reference

| Step | Command |
| --- | --- |
| Changeset | `git log $(git describe --tags --abbrev=0)..HEAD --oneline` |
| Types | Invoke `type-declarations`; reconcile every runtime API in the changeset before versioning |
| Bump | `make version V=<ver>` → `make version-check` |
| Cut | `git commit -m "v<ver>" -- <version files>` → `git push origin main` → `git tag v<ver>` → `git push origin v<ver>` (never `--tags`) |
| Notes | `gh release edit v<ver> --notes-file notes.md` |
| Blog | `site/content/blog/nub-<x>-<y>-<z>.mdx` — back-dated to `publishedAt` (direct to `main`) |
| Tap | automatic via `bump-homebrew-tap`; verify with `gh api repos/nubjs/homebrew-tap/contents/Formula/nub.rb --jq .content \| base64 -d \| head -5` |
| Loop | `gh issue comment <n> --body "Fixed in v<ver>: <release URL>"` (every closed issue + merged PR) |
| Verify | `npm view @nubjs/nub@<ver> version` · `gh release view v<ver> --json assets` |

Invoked via the Skill tool once a release thread's targeted fixes are all landed on `main` and CI-green.
