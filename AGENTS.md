# AGENTS.md — agent orientation for the Nub repo

This file is the entry point for AI coding agents working in this repository. It mirrors the [`AGENTS.md` convention](https://agents.md) used by other AI tools. [`CLAUDE.md`](CLAUDE.md) is a symlink to this file — Claude Code and the `AGENTS.md` convention read identical content.

## Read this first

- This file (orientation, brand-boundary rules, testing philosophy, write-style rules).
- [`wiki/architecture.md`](wiki/architecture.md) — **the single most load-bearing doc.** Nub is a Rust CLI that augments the user's Node via extension surfaces. It is not a fork. Includes the `--node` / `NODE_COMPAT=1` compat-mode contract. Includes the toolchain section. Read this before editing anything under `wiki/`.
- [`wiki/philosophy.md`](wiki/philosophy.md) — the design principles: additivity, brand boundary, reversibility.
- [`wiki/PLAN.md`](wiki/PLAN.md) — canonical design plan; index of all per-feature docs.
- [`wiki/whitepaper.md`](wiki/whitepaper.md) — the framing / pitch / launch post. The current ship-scope intent in user-facing form.
- [`epics/v0.1/todo.md`](epics/v0.1/todo.md) — the v0.1 implementation epic; the to-do list driving the build. Each epic lives in its own subdirectory under `epics/` (the version dimension is in the directory name, not the filename), with a `prompt.md` (framing + how-to-execute) alongside `todo.md` (the phase-by-phase work). Successor epics (`epics/v0.2/`, `epics/v0.1-edits/`, etc.) land as sibling directories.

## Work directly on `main` — no branches, no stashes, no worktrees

**All work happens on `main`.** This is a pre-release product with multiple agents often running in parallel, and the trunk is `main`. Do **not** `git checkout -b`, do **not** `git stash`, do **not** switch branches, and do **not** create git worktrees for isolation. Those operations on a shared working tree cause parallel agents to clobber each other's uncommitted work and orphan commits onto dead branches — this actually happened (2026-05-29: two efforts ended up split across `v0.1-launch-fixes` / `feat/supported-version-expansion-verified` with WIP stranded in a stash, and three stale worktrees locked by a day-old session). The rule that prevents it: **commit small and often, directly to `main`.** Committed work cannot be clobbered; frequent commits shrink the window where loose uncommitted work can be lost to near-zero. If you need to preserve WIP, *commit* it — never stash it. Releases are tag-triggered and deliberate (see [Releasing](#releasing)), so a busy `main` with in-progress work is expected and fine.

## What Nub is, briefly

- **A Rust CLI that orchestrates the user's installed Node.** v0 is a fast script runner plus a TypeScript-just-works runtime. Mechanism: Node's own extension surfaces — `module.registerHooks()`, `--import` preload, `--require`, env vars, N-API addons, V8 flag injection.
- **TypeScript-first developer supertool.** Own transpiler (oxc-based). TS, JSX, non-erasable syntax, `emitDecoratorMetadata` all supported.
- **Compatibility is paramount.** Code targeting Node must run on Nub byte-for-byte. The contract: **default mode = Nub runtime augmentation active; `NODE_COMPAT=1` or `--node` = augmentation disabled, plain Node behavior**.
- **Behavioral additivity** is the rule for augmentation — [`wiki/philosophy.md#additivity`](wiki/philosophy.md#additivity) for the canonical statement. "Additive" means doesn't break code written for vanilla Node; mechanism is restricted to Node's own extension surfaces (not source patches).

## The augmenter / fork distinction is load-bearing

A previous direction (2026-05-17) positioned Nub as a soft fork of Node, with patches to Node source on the table. **That direction was reversed 2026-05-18.** Nub is now a Rust CLI augmenter via Node's extension surfaces only. Source patches are off the table.

If you're editing a plan doc and find "soft fork" or "patched Node" language, it predates 2026-05-18 and should be updated. The authoritative statement of the current direction is [`wiki/architecture.md#augmenter-not-fork`](wiki/architecture.md#augmenter-not-fork).

The reason the distinction is load-bearing for agents specifically: implementation approaches that *sound* clean — "just patch `lib/internal/modules/esm/resolve.js` to do X" — were valid under the old posture and are no longer. The current mechanism rule is **"would a user on plain Node, plus the corresponding `module.register()` / `--import` / npm-addon, get the same result?"** If yes, in scope. If no, find a different mechanism or scope the feature out.

## Design philosophy — read before suggesting features

Before proposing any new feature, API, env var, package, or config surface, read these in order. They are short and load-bearing; do not skip them.

- This file's brand-boundary rules below.
- [`wiki/architecture.md#augmenter-not-fork`](wiki/architecture.md#augmenter-not-fork) — the "would a user on plain Node + the corresponding `module.register()` / `--import` / npm-addon get the same result?" test.
- [`wiki/philosophy.md#additivity`](wiki/philosophy.md#additivity) — Nub adds behavior via Node's own extension surfaces; it never modifies existing Node semantics.
- [`wiki/PLAN.md`](wiki/PLAN.md) — the v0.1 manifest, Phase 1 / Phase 2 split, reversibility filter.
- [`wiki/whitepaper.md`](wiki/whitepaper.md) §"Zero Nub-specific APIs, zero lock-in" — the brand-boundary rule in user-visible form.

### Non-negotiable rules (the brand boundary)

The Nub brand stops at the binary boundary. These rules are absolute. Do not suggest, scaffold, or write code that violates any of them; do not propose them "behind a flag" or "for internal use only."

- **No `globalThis.nub`.** Nub never injects a global named after itself. Equivalents of `Bun.*`-style APIs, if they exist at all, go in importable modules. *No internal-only markers either* — a `globalThis.__nub_*` sentinel is the same brand leak in a worse disguise.
- **No `nub:*` module namespace.** Nub never registers synthetic specifiers like `nub:test`, `nub:sqlite`, `nub:serve`. The `node:*` namespace is owned by upstream Node and is closed; Nub does not squat in any sibling.
- **No `@nub/*` npm scope** for user-facing packages or APIs — the bare `@nub` scope is unused entirely. The CLI ships under the project's own `@nubjs` org: `@nubjs/nub` (the package users `npm install -g`) plus its `@nubjs/nub-<platform>` `optionalDependencies`, which carry the per-platform binary + N-API addon and are install-time plumbing never imported by user code (same shape as `@biomejs/biome` + `@biomejs/cli-*`, `@rollup/rollup-*`, `@esbuild/*`). The only `nub`-named package user code may reference is `@types/nub` on DefinitelyTyped (the `@types` scope is a TypeScript-ecosystem convention, not a runtime-vendor claim). `@nubjs` is deliberately distinct from the prohibited bare `@nub`. See [`wiki/philosophy.md#the-brand-boundary`](wiki/philosophy.md#the-brand-boundary) for the full rationale.
- **No `NUB_*` environment variables.** Ever. Including internal-only install-script hints, detection sentinels, debug toggles, and "users aren't supposed to set this" backchannels. Anything in `printenv` leaks into CI logs, shell history, and process-inspection tools, then becomes a thing users depend on. Use sentinel files, PM-prefix path heuristics, or piggyback on `NODE_*` (where Node doesn't claim the name itself, e.g. `NODE_COMPAT`) and ecosystem-neutral conventions (`NO_COLOR`, `FORCE_COLOR`, `PORT`, `HOST`, `NODE_ENV`).
- **No `"nub"` field in `package.json`.** Nub does not consume a `package.json`-level config field. Config (if any) lives in dedicated files Nub reads, not in the `package.json` namespace.
- **No vendored Node patches.** Nub does not patch Node source, ship a custom-built Node binary, or embed `libnode`. Every augmentation rides on Node's public extension surfaces. If a feature can't be expressed through those, find a different feature.

### `nub`-named npm packages, explained

Two families of `nub`-named packages exist on npm. Neither is the prohibited bare `@nub/*` scope, and neither is user-facing application code:

1. **`@types/nub`** — DefinitelyTyped publishes ambient TypeScript declarations under `@types/<runtime-or-package-name>` (`@types/node`, `@types/bun`, `@types/deno`). The `@types` scope is a TypeScript-ecosystem convention owned by DefinitelyTyped, not a runtime-vendor namespace claim. If Nub ships ambient types for something user code references — `import.meta.hot` for hot-mode consumers is the obvious candidate — `@types/nub` is structurally the only correct home.

2. **`@nubjs/nub` + `@nubjs/nub-<platform>`** — the CLI itself and its platform-specific binary distribution packages (`@nubjs/nub-darwin-arm64`, `@nubjs/nub-linux-x64`, … — 8 platforms total). `@nubjs/nub` is the package users `npm install -g`; the `@nubjs/nub-<platform>` packages are its `optionalDependencies`, each containing the compiled Rust binary + N-API addon for one platform, selected at install time by npm's `os`/`cpu` filters and copied into place by `postinstall.js`. They are install-time plumbing — never `import`ed, never referenced in application source, never a thing users type. Same distribution pattern as `@biomejs/biome` + `@biomejs/cli-*`, `@oxlint/cli-*`, `@rollup/rollup-*`, `@esbuild/*`. The org scope is `@nubjs` (the project's own npm org), deliberately distinct from the prohibited bare `@nub`.

### Naming

- **"Nub"** — proper noun, capitalized, in prose.
- **`nub`** — lowercase, only for the executable, CLI invocations, package name, and code identifiers.

### v0 verb surface

`nub <file>`, `nub run`, `nub watch`, `nubx`, `nub upgrade`. No `nub install` / `add` / `remove` / etc. — package management defers to the user's existing pnpm / npm / yarn / bun. `nub watch` ships in restart-mode, on top of Node's `--watch` engine.

**No `nub node` passthrough verb, no `NODE_COMPAT=1` env var.** `nub node <file>` (run-a-file-on-plain-Node) stays dismissed — the `nub node` keyword is the version-management namespace (`install`/`ls`/`uninstall`/`pin`, see [`wiki/commands/node-versions.md`](wiki/commands/node-versions.md)), so `nub node <file>` is an error. `NODE_COMPAT=1` stays dismissed too (no compat env var — brand boundary). **But the `--node` *flag* is live** (revived 2026-06-01): on the top-level file run (`nub --node <file>`) and on `nub run` / `nubx`. It runs with zero augmentation while keeping version provisioning on — "the project's *pinned* Node, vanilla," plus differential debugging. The top-level spelling was dismissed 2026-05-25 (premise: "plain Node is just `node`") and revived once version management gave it a use the shell can't reproduce: `node script.js` runs your *shell's* Node, not the project's pin. Full decision record: [`wiki/commands/node.md`](wiki/commands/node.md).

## Testing philosophy

**Minimum number of tests, comprehensively covering the API surface.** This is the load-bearing rule for the v0.1 implementation, and the antidote to AI-generated test bloat. Quality of coverage matters; volume of tests does not.

- **Comprehensive coverage, not exhaustive coverage.** Each behavior gets tested once, well, against the cases that matter: golden path, the one or two failure modes that have user-facing implications, the boundary condition that's easy to get wrong. Stop there. Don't write `it("should not throw when input is null")` after you've already written `it("rejects null input")`.
- **Hand-crafted feel.** Tests must read as if a careful engineer wrote them, not as if an agent enumerated possibilities. Concrete signs of agent bloat to avoid: identical assertions across multiple `describe` blocks with different setup; per-input parametrization where one assertion would do; test names that paraphrase the implementation instead of describing the contract; descriptions like "should handle X" without naming what "handle" means.
- **Carefully considered abstractions.** Test helpers and fixtures are part of the contract. Don't bury behavior in clever shared setup; don't copy-paste either. Pick the shape that lets a reader skim a test file in 30 seconds and understand what's being verified. If a test file is over 300 lines, you're probably testing too many things in one place or each test is too verbose — split or trim.
- **Some things are untestable, and that's fine.** Perf-shaped behavior, OS-specific corners, race conditions that require infrastructure we don't have, behaviors that depend on Node's internal scheduling. Leaving them untested is honest; writing ceremonial fake tests for them is sloppification. Document the gap in a comment if it'll surprise a future reader.
- **Pull from upstream where it makes sense.** Node ships an extensive test suite at `nodejs/node/test/`. The executable-level subset (tests that just spawn `node` and assert behavior) is the strongest possible compat-validation against Nub. We run it black-box in two modes: `nub --node` (expected ~100% pass — passthrough mode) and `nub` augmented (expected near-100% with documented divergences). See `wiki/research/node-test-suite-leverage.md` for the harness design and CI cadence.
- **Test names describe the contract, not the implementation.** `test("emits ECONNRESET when peer closes mid-write")` is good; `test("emits ECONNRESET")` is too short to mean anything; `test("calls socket.destroy() with the EconnReset constructor when..."` is paraphrasing the code.
- **Failure messages must be self-debugging.** When a test fails in CI, the assertion message should make the cause obvious without rerunning locally. Asserting `expect(result).toEqual({ ok: true })` is fine when it passes and useless when it fails ("expected {ok: true}, got {ok: false}"). Use `expect(result.ok).toBe(true)` or write a custom message.

## Docker is available — use it instead of declaring things "untestable"

**Docker Desktop is installed and running on the dev machine** (`docker version` → 28.3.3). Before you write off a behavior as "hard to test" or "can't verify locally," check whether a container closes the gap — several things the testing philosophy lists as untestable corners actually *are* reachable through Docker:

- **A clean, dependency-free environment.** A fresh `node:22.15-slim` (or no-Node `debian:slim`) container with no `~/.cache/nub`, no global Node, no dev-box state — the honest way to test first-run install, the curl `install.sh` flow, `nub upgrade`'s `~/.nub` channel, provisioning a Node from `nodejs.org`, and "works on a machine that isn't mine."
- **A specific / floor Node version.** Pin `node:22.15` to exercise the **floor-only** defects the dev box (Node 24/26) masks — `using`/`await using` down-leveling, the Temporal `toTemporalInstant` path, version-gated polyfills, the async-tier `module.register` loader. This is the local complement to the Node-22.15 CI leg.
- **Linux-specific paths.** musl-vs-glibc detection (`node:22-alpine` vs `node:22-slim`), arch matching, signal/`PATH` behavior on Linux.

**What Docker on this host does NOT give you:** it runs **Linux containers only** (the macOS host → `linux/aarch64` by default, `--platform linux/amd64` via slow QEMU emulation). **Windows containers require a Windows host**, so cmd.exe behavior, the `--shell-emulator`, `.cmd`/`.bat` resolution, and `nub.exe` vs `bin/nub` still can only be verified on the **`windows-latest` CI leg** — Docker is not a substitute there. When an item's *Verify* says "on Windows," it means Windows CI; do not claim a Docker run verified it.

Keep containers ephemeral (`docker run --rm`), mount the repo read-only where you only need to read it, and never leave long-running containers behind.

## Chrome DevTools is available — use it eagerly to evaluate UI

**The `chrome-devtools` MCP server is connected, and the marketing site dev server runs on `localhost:3000`.** Whenever you touch the site (`site/`) — any layout, spacing, color, typography, or copy change that renders — do not declare it done by reading the JSX. Drive the actual browser and look: navigate/reload the page, take a screenshot, and visually confirm the change is correct. For anything alignment- or spacing-sensitive, don't trust the eyeball alone — measure with `evaluate_script` (`getBoundingClientRect`, `getComputedStyle`) so the assessment is grounded in real numbers, not a guess. Geometry checks and visual checks catch different bugs: a box can be mathematically centered while serif font metrics make the ink ride visually high, so use both. Also check `list_console_messages` for runtime errors after a change — a green HTTP 200 is not proof the page didn't throw. The cost of a screenshot is trivial; shipping a UI change you never looked at is not.

## Implementation quality discipline

**Quality over velocity. Always.** Don't move fast, check boxes, and ship stubs as complete implementations. The specific failure modes:

- **Never mark a task `[x]` without verifying the behavior end-to-end.** Running `cargo test` is necessary but not sufficient. If the task says "implement per-line stream prefixing matching pnpm," you must run `nub -r --stream run build` on a real fixture and compare the output to `pnpm -r --stream run build`. A green test suite with a stubbed implementation is worse than an unchecked task.
- **Never claim "parity" without evidence.** "Complete workspace parity with pnpm" means every flag, every edge case, every output format. If you haven't tested a flag, don't mark it done. If the implementation is a simplified version, say so explicitly in the task note — don't use language like "implemented" for something that's "scaffolded."
- **Name what you actually built.** If you shipped `Stdio::inherit()` with a header line, that's not "stream prefixing." If you partitioned into fixed batches, that's not "work-stealing." Use precise language so the next reader knows the actual state.
- **If you're completing 10 tasks per hour, you're not testing them.** A thoughtfully implemented and tested task takes 15-30 minutes. If a phase has 12 tasks and you completed it in 20 minutes, audit yourself.

## Research eagerly

For non-trivial technical or strategic questions, spawn a research sub-agent rather than guessing. Each meaningful research thread produces a write-up under `wiki/research/<topic>.md`.

**Research docs are canonical living documents.** When findings update, supersede earlier conclusions, or reverse on new evidence, edit the doc in place — but record the change at the bottom in a `## Changelog` section as a dated bullet. Format: `- YYYY-MM-DD — <what changed and why>`. The first entry on a new doc is `- YYYY-MM-DD — Initial write-up.` Major reversals get a leading `**REVERSAL:**` marker so future agents skimming the changelog see the shift immediately. Don't blow away prior conclusions silently — note that the doc previously said X and now says Y, and what evidence drove the update.

This matters because the codebase has gone through posture shifts (soft-fork-on, soft-fork-off, etc.). The git history of the research doc plus its changelog tell you the *shape of the evolution*; the doc body always reflects the current best understanding. When two research docs cover overlapping territory, prefer merging into a single canonical and superseding the older — record the merge in the surviving doc's changelog and note the supersession at the top of the doc being absorbed before deleting it (or just delete and reference the merge in the survivor).

## Chat responses: few words, expert tone

Colin wants the **minimum words that convey every point**. Tone: a brilliant subject-matter expert who is terse by nature. Sentence fragments, conversational shorthand, no preamble, no recap, no hedging. State conclusions; cut throat-clearing and restatements of the question. This governs chat replies only — docs/code keep their normal rigor. (Concision ≠ omission: keep every distinct point, just strip the words around it.)

## Never hard-wrap markdown paragraphs

**Every paragraph in every `.md` file in this repo is one long line.** Editors do soft-wrap. Hard line breaks inside a paragraph are forbidden. This applies to prose, blockquotes, list items, and table cells. Only code blocks, list-item boundaries, and headers introduce new lines.

If you find yourself wrapping at column 72/80/100, stop. Write the whole paragraph as one line and move on. There is a fixer at `wiki/scripts/unwrap-md.py` if you need to repair an existing document; do not invoke it as a substitute for not making the mistake.

## Doc indexer — start here for a roadmap overview

Every doc under `wiki/runtime/` and `wiki/commands/` carries a YAML front-matter block describing its status, mechanism, and dependencies. **Run the indexer before opening files individually:**

```sh
node wiki/scripts/index.mjs            # grouped table: kind → status → docs
node wiki/scripts/index.mjs --all      # full per-doc front-matter dump
node wiki/scripts/index.mjs --json     # machine-readable
node wiki/scripts/index.mjs --check    # lint: non-zero exit if any doc lacks valid front matter
node wiki/scripts/index.mjs --kind runtime --status v0.1
```

Front-matter schema:

```yaml
---
id: <basename without .md>
kind: runtime | command
title: <human title>
status: v0.1 | v0.x | v1.x | decision-only | dismissed
tags: [<free-form>]
mechanism: <short one-line "how" — e.g. "--import preload", "module.registerHooks", "PATH shim", "decision record">
depends_on: [<other doc ids>]
research: [<research doc ids from wiki/research/>]
last_decision: <YYYY-MM-DD or "">
---
```

Status values:

- **v0.1** — in the v0.1 ship scope.
- **v0.x** — planned for later in v0.
- **v1.x** — deferred to v1 or beyond.
- **decision-only** — file documents a *non-implementation* choice (defer / do-not / decision record).
- **dismissed** — explicitly not pursued.

When adding a new doc under `wiki/runtime/` or `wiki/commands/`, include the front matter and re-run `node wiki/scripts/index.mjs --check` before committing.

## Releasing

Nub is published to npm as `@nubjs/nub` with 8 platform-specific binary packages. The release is fully automated via GitHub Actions.

```bash
make version V=0.0.6          # sets version in all 9 npm packages + Cargo.toml
make version-check             # verify consistency
git add -A && git commit -m "v0.0.6"
git tag v0.0.6
git push origin main --tags    # CI builds 8 platforms, publishes to npm, creates GitHub release
```

**Makefile targets:**
- `make version V=<ver>` — set version across all packages atomically.
- `make version-check` — verify all package versions are consistent.
- `make npm-build` — build + package for the current platform (local testing).
- `make npm-publish` — publish all packages manually (prefer CI).
- `make npm-publish-dry` — dry-run publish.

**CI release workflow** (`.github/workflows/release.yml`): triggers on `v*` tags. Builds for darwin-arm64, darwin-x64, linux-x64, linux-x64-musl, linux-arm64, linux-arm64-musl, win32-x64, win32-arm64. Publishes via npm OIDC trusted publishing (no secrets needed). Creates GitHub Release with binary artifacts.

**Version regime:** stay in `0.0.x` until ready for public launch. Bump to `0.1.0` only when the whitepaper, benchmarks, and install experience are polished.

## Where things live

- **Agent orientation** (this file) — `AGENTS.md` (with `CLAUDE.md` as a symlink).
- **Architectural distinctions and toolchain** (augmenter-not-fork, compat mode, the Rust crates Nub is built on) — `wiki/architecture.md`.
- **Design philosophy** (additivity, brand boundary, reversibility) — `wiki/philosophy.md`.
- **Canonical plan + v0.1 manifest** — `wiki/PLAN.md`.
- **User-facing framing / launch post** — `wiki/whitepaper.md`.
- **Per-version implementation epics** — `epics/<version>/`. Each epic directory contains `prompt.md` (the framing + how-to-execute, drives the Ralph loop) and `todo.md` (the actual phase-by-phase work). The version dimension lives in the directory name; the filenames inside are stable across epics. Today: `epics/v0.1/` (the original v0.1 epic) and `epics/v0.1-edits/` (drift catalog + open-questions consolidation for v0.1).
- **Per-runtime-feature plans** — `wiki/runtime/<feature>.md`.
- **Per-subcommand plans** — `wiki/commands/<cmd>.md`.
- **Research write-ups** (canonical living docs with changelogs at the bottom) — `wiki/research/<topic>.md`.
- **Scripts** — `wiki/scripts/`.
- **Implementation** — the Rust CLI tree (lands during the implementation epic).
- **Reference checkouts of other runtimes and tools** (Node, Bun, tsx, pnpm — pnpm now includes pacquet's Rust crates in-tree) for reading only — `.repos/<name>/` (gitignored, local-only).

Never put plans, research, or new architecture docs at the repo root. The only repo-root markdown files are agent orientation (this file + `CLAUDE.md` symlink) and `README.md`; the implementation epics live under `epics/<version>/`.

## Repo layout

- `wiki/` — the entire design corpus (plans, runtime/command docs, research, philosophy, architecture). Tracked.
- `.repos/` — local-only reference checkouts of other runtimes and tools (`node/`, `bun/`, `tsx/`, `pnpm/`) for reading, not editing. Gitignored. Use `Read`/`Grep` against `.repos/node/lib/...`, `.repos/pnpm/exec/commands/src/runRecursive.ts`, etc. when you need to consult the source; never modify anything under `.repos/`.
- `.scripts/`, `.claude/` — local-only tooling. Gitignored.
- Implementation tree — TBD (Rust CLI). When it lands it goes in a tracked top-level dir (e.g. `crates/`, `src/`), not under `.repos/`.

The repo was previously a Node.js fork; the Node source has been moved into `.repos/node/` so it remains available for reference without being part of Nub's tree. There is no upstream-Node remote and no history shared with `nodejs/node`.

## Reference checkouts under `.repos/`

`.repos/` holds local-only clones of other runtimes and tools used as reading material: `.repos/node/` (the Node source — previously the contents of this repo's root, since Nub was originally a Node fork), `.repos/bun/`, `.repos/tsx/`, and `.repos/pnpm/` (the pnpm monorepo, which now absorbs pacquet's in-progress Rust port at `.repos/pnpm/pacquet/crates/`). They are gitignored and never modified. When a research question requires reading the actual source ("how does Node's `lib/internal/modules/esm/resolve.js` handle X?"; "how does `pnpm -r` implement `--resume-from`?"), read from the corresponding `.repos/<name>/` directory. The Nub repo itself contains no vendored Node source, no Node patches, and no upstream-Node remote — see [`wiki/architecture.md#augmenter-not-fork`](wiki/architecture.md#augmenter-not-fork).

When citing pnpm's implementation in epic todos or research docs, link to the specific file in `.repos/pnpm/` (e.g. [`.repos/pnpm/exec/commands/src/runRecursive.ts`](.repos/pnpm/exec/commands/src/runRecursive.ts)). The canonical workflow for pnpm-parity work is: read the source first, then write a fixture against real pnpm, then verify nub matches.

## Per-feature granularity

Per Colin's 2026-05-18 directive: feature docs should be granular — each meaningful augmentation gets its own doc with status, priority, decisions, open questions, and research links. Don't combine TS transpilation + JSX + non-erasable syntax + source maps into one doc; each is its own concern with its own decision record. Splitting is preferred to merging. When a feature doc grows past ~500 lines, look for a natural seam to split.
