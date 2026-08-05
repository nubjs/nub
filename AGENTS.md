# AGENTS.md — agent orientation for the Nub repo

Entry point for AI coding agents working in this repo, per the [`AGENTS.md` convention](https://agents.md). [`CLAUDE.md`](CLAUDE.md) is a symlink to this file. If you are OpenAI Codex, read [`CODEX.md`](CODEX.md) after this one — it is Codex-only and does not apply to other agents.

## Skills work for EVERY agent, not just Claude (Codex included)

Task playbooks ("skills") live as plain-markdown `SKILL.md` files under `.claude/skills/<name>/SKILL.md`, each with a frontmatter `description` naming when to use it. **That is the one and only skills directory** — despite the Claude-specific name, every agent reads it. There is no mirror and no symlink: a second copy under `.agents/` drifted silently for weeks and is exactly what `.githooks/pre-push` now refuses. Claude Code auto-discovers and auto-triggers them; **Codex and other agents do not** — run `ls .claude/skills/` and read the matching `SKILL.md` before improvising. A skill's steps are authoritative over your default approach for that task. The highest-value ones:

- **`gcloud-vm`** — provision/start/reach Google Cloud VMs (real Linux-kernel enforcement, real Windows/MSVC/AppContainer, a clean build box). You can CREATE a VM on demand, not just start the standing `nub-linux`/`nub-win`.
- **`dev-loop`** / **`rust-build`** / **`worktree`** — build & test nub in a worktree: the fast-profile loop, the shared-target-dir contamination hazard, the one-command worktree setup.
- **`remote-build`** — run a cold build, `clippy --all-targets --all-features`, a full `cargo test`, or a `release` build on an ephemeral GCE spot VM instead of the dev Mac; cross-compile `aarch64-apple-darwin` on Linux (no Apple SDK) and pull a signed binary back. Reach for it whenever the host is contended; the ~5s warm inner loop stays local.
- **`ci-adhoc-test`** — run a macOS/Windows/Linux-arch probe on real CI with no PR (branch-scoped workflow). **`ci-watch`** — block on CI correctly.
- **`release`** — cut a patch release end-to-end. **`address-issue`** — the full issue playbook. **`audit-thread`** / **`sandbox-pentest`** — parity-audit and adversarial-red-team methodology.
- **`prose-writing`** — required before writing any GitHub comment, doc, or release note. Also **`benchmarking`**, **`pm-perf-tracing`**, **`impact-analysis`**, **`aube-bump`**, **`git-archaeology`**, **`md-toc`**, **`soak`**.

`.claude/skills/*` is this repo's own agent tooling; the "agent-agnostic, never overfit to Claude" rule governs copy that ships to *users'* agents, not these playbooks.

Per-agent config is tracked for **every** agent, not just Claude: `.claude/settings.json` + `.claude/hooks/` (Claude Code), `.codex/config.toml` + `.codex/hooks.json` (Codex), and `.mcp.json` (the shared `chrome-devtools` MCP server). The two agents read different config files, so the `gh-comment-guard` hook script exists once per agent — `.githooks/pre-push` requires the copies stay byte-identical, and rejects a hook command that hardcodes an absolute home path. Codex sets no project-dir environment variable of its own — verified by dumping a live hook's environment: it received 79 vars, none of them `CODEX_*`, and no `CLAUDE_PROJECT_DIR` even though 11 unrelated `CLAUDE_*` vars had leaked in from the launching shell. It does run hooks with the working directory set to the project root and passes `cwd` in the stdin payload. So a Codex hook path uses `$(git rev-parse --show-toplevel)`, which additionally survives being invoked from a subdirectory.

When present, `AGENTS.local.md` (gitignored, not in a clean checkout) adds maintainer-local orientation — orchestration workflow, dispatch policy, pointers into local-only directories. It is an optional overlay; this file is self-sufficient without it.

## Non-negotiables

An index of the rules a doing-agent must not miss. Each links to its authoritative section.

- A PR that resolves an issue MUST carry `Closes #N` / `Fixes #N` / `Resolves #N` in its **body**. Verify before `gh pr create`. ([Git & GitHub maintainer hygiene](#git--github-maintainer-hygiene))
- The repo is public — never commit internal or competitive discussion to a tracked file or commit message. ([The repo is PUBLIC](#the-repo-is-public--never-commit-internal-discussion))
- Brand boundary: no public `globalThis.nub`, no `nub:*` import namespace, no `@nub/*` scope, no `"nub"` user-authored config field, no documented `NUB_*` user knob. Internals are exempt. ([The brand boundary](#the-brand-boundary--public-surfaces-only-internals-are-exempt))
- `vendor/aube` fork-discipline no longer applies (2026-07-29) — edit the vendored engine freely, defaults included. The separate rule against upstreaming is untouched. ([The aube vendoring + upstreaming](#the-aube-vendoring--upstreaming))
- No agent co-author trailers in commits; the commit-msg hook strips them.
- **Never cut a release without the maintainer's explicit, in-the-moment instruction.** Publishing to npm is irreversible, and the tag push is what triggers it. Same shape as the never-upstream rule. ([Releasing](#releasing) — and invoke the `release` skill, which carries the runbook and this gate.)

## Core design positions

The load-bearing architectural premises. They are easy to violate by reflex, so keep them resident: before designing or dispatching any CLI / PM / runtime change, re-state the relevant position and design to it.

- **What nub is:** a Rust CLI that AUGMENTS the user's installed Node through Node's own extension surfaces (`--import`/`--require` preload, `module.registerHooks`, env vars, N-API addons, V8-flag injection). It is not a fork, ships no patched Node, embeds no libnode. The test for any augmentation: *would a user on plain Node plus the corresponding `module.register()` / `--import` / npm-addon get the same result?* If no, find a different mechanism or scope the feature out. (A soft-fork direction was reversed 2026-05-18; "soft fork" or "patched Node" language in any doc is stale — the authoritative statement is [`wiki/architecture.md#augmenter-not-fork`](wiki/architecture.md#augmenter-not-fork).)
- **TypeScript-first developer supertool.** Own oxc-based transpiler: TS, JSX, non-erasable syntax, `emitDecoratorMetadata`.
- **Compatibility is paramount.** Code targeting Node must run on Nub byte-for-byte. Augmentation is *additive* — it never modifies existing Node semantics ([`wiki/philosophy.md#additivity`](wiki/philosophy.md#additivity)).
- **The PM CLI frontend mirrors pnpm's grammar, exclusively** — not npm, not yarn, not bun. When adding or checking a flag/alias/positional on a PM verb, the only question is *does real pnpm accept it?* No npm-isms (`--omit`, `--no-save`, `-S`/`--save`, npm's `-w <name>`/`--workspaces` selectors). Gotcha: pnpm `-w` is `--workspace-root` (boolean); member selection is `--filter`, recursion is `-r`.
  - **Deliberate exceptions (2026-06-20):** (1) the top-level file run `nub <file>` is nub's own surface; (2) `nub upgrade` is nub's SELF-UPDATE, not pnpm's `upgrade`→`update` alias — `nub upgrade <pkg>` does not route to `update`; (3) no implicit script shortcuts, ever — always explicit `nub run <script>`, never `nub test`/`nub start`/`nub t`; (4) `nub init` is nub's own TypeScript-first scaffold (2026-07-21), takes no positionals, and hints `nubx create-<template>` for templates (`wiki/commands/init.md`, `crates/nub-cli/src/init.rs`). Everything else mirrors pnpm.
  - **`nub pm <verb>` is nub's own PM-management namespace, not a pnpm mirror.** `which`/`pin`/`migrate`/`update`/`cache`/`shim`/`unshim` are nub-own verbs. pnpm's `pm` is a builtin-forcing prefix with no subcommands of its own, so no pnpm-compat obligation binds these — a compat audit must not flag them.
- **Lockfile-compatibility is a separate axis from CLI-compat.** nub is lockfile-compatible with whatever the project already uses (npm/pnpm/bun round-trip, yarn read-only) and imposes no format of its own. "pnpm-only" governs the CLI grammar; the lockfile axis is multi-PM.
- **Yarn berry write is IN scope (reversed 2026-06-27; [`.fray/pnp-node-linker.md`](.fray/pnp-node-linker.md) D1).** nub writes berry `yarn.lock` + `.yarn/cache/*.zip` + `.pnp.cjs` for the Yarn Plug'n'Play node-linker, accepting lockfile churn rather than byte-lossless `yarn install --immutable` parity (`wiki/research/pnp-write-fidelity.md`). Yarn-classic (v1) write stays out; `nub pm use yarn` (one-shot identity switch) writes a classic-v1 `yarn.lock` on conversion. The pnpm-only CLI-grammar axis is unaffected. Superseded: the former blanket refusal of every `yarn.lock` write.
- **The PM is the vendored `aube` engine, embedded as a Rust library** — path dep `vendor/aube`, called in-process, no subprocess. `vendor/aube` is plain tracked files in nub's history (not a submodule), so an aube edit is a normal nub commit/PR with no pin to bump. Upstream is `jdx/aube`. **Never upstream anything to `jdx/aube` — and never offer, propose, or label something an "upstream candidate" — without the maintainer's explicit, in-the-moment instruction.** Full detail: [The aube vendoring + upstreaming](#the-aube-vendoring--upstreaming).
- **Config identity model:** a **nub-identity** project consumes ONLY neutral cross-tool config (`overrides`, `resolutions`, `catalog`, `workspaces`) — never another PM's branded fields or files. A **compat** project (incumbent npm/pnpm/yarn/bun) is mirrored faithfully, branded fields included. pnpm-NAMED files are never read unless pnpm is the incumbent.
- **Project `nub.jsonc` is an intentional config surface and an explicit naming exception** — nub's canonical typed runtime/install/dlx configuration, separate from global `~/.config/nub/nub.jsonc`. Project discovery is live: never gate or disable the whole file to hold back one unfinished field. A key lands in the parser when its consumer does — the file carries no inert forward-compatibility keys, and a sandbox key arrives with the sandbox frontend that reads it.
- **Compat targets are per-major-version of each package manager (2026-06-20).** Config homes, lockfile formats, and behavior shift across majors — pnpm 10 reads scalar settings from `.npmrc`; pnpm 11 moved them to `pnpm-workspace.yaml`/global `config.yaml`. Distinct targets today: npm, pnpm 9/10/11, yarn 1 (classic), yarn 2/3/4 (berry), bun. When mirroring an incumbent, DETECT its major (`packageManager` → `devEngines` → installed-PM `--version` → lockfile-version signal) and mirror that one; never assume latest. PM known but version unknown → default to the dominant major (pnpm → the v10 `.npmrc` model). Architect it as a version→model MAP so a new major slots in cheaply — but split a major out only where behavior materially differs AND that version is in real use; lump near-identical majors; don't pre-populate historical or speculative ones.
- **Brand boundary (public surfaces only):** no public `globalThis.nub`, no `nub:*` module namespace, no `@nub/*` scope, no `"nub"` user-authored config field, no documented `NUB_*` user knob. Internal `NUB_*`/`__NUB_*` plumbing vars, error codes, and cache/dir names may carry the brand. Full rules: [The brand boundary](#the-brand-boundary--public-surfaces-only-internals-are-exempt).
- **`node` PATH-hijack contract (under active design — see [[node-hijack-compat-design]]):** an invocation of `node` through nub's shim respects node/PATH semantics; the hijack's legitimate job is version resolution and pinning. Augmentation belongs to the `nub`/`nubx` entrypoints.
- **v0 verb surface:** `nub <file>` · `run` · `watch` · `nubx` / `nub dlx` / `nub x` · `upgrade` · `init` · `nub node` (version management: `install`/`ls`/`uninstall`/`pin`, [`wiki/commands/node-versions.md`](wiki/commands/node-versions.md)) · the pnpm-compatible PM (`install`/`i`/`ci` plus `add`/`remove`/`update`/`import`/`dedupe`/`pm …`, engine-routed through `pm_engine::ENGINE_VERBS`; some families still filling in). `nub watch` ships in restart-mode on top of Node's `--watch`. There is no `nub node <file>` passthrough — `nub node` is the version-management namespace, so `nub node <file>` is an error.
- **`--node` and `NODE_COMPAT` are both live and compose** (either forces compat mode: zero augmentation, version provisioning stays on). `--node` works on the top-level file run, `nub run`, `nubx`, and the `node`-hijack path (`node --node <file>`). A truthy `NODE_COMPAT` (`1`/`true`/`yes`, case-insensitive) is the persistent, tree-wide equivalent, inherited by every descendant `node`/`nub`. Decision record: [`wiki/commands/node.md`](wiki/commands/node.md).

## The repo is PUBLIC — never commit internal discussion

`github.com/nubjs/nub` is a public repo, and everything committed survives in git history even after deletion. Never put internal product, marketing, or competitive discussion into anything git tracks — code, comments, commit messages, `tests/**` READMEs, any tracked markdown. Specifically banned:

- **Benchmark-presentation strategy** — which competitors or numbers to show vs. omit, competitive-band thresholds, anything about how results are framed for publication.
- **Candid hedges that read as gaming** — any sentence a competitor or skeptic could screenshot as "they juice their benchmarks."
- **Private attributions, internal quotes, first-person strategy** — "our number," "don't publish X," "reads as cherry-picking." Public copy states what the code DOES, never *why* a number was framed a certain way.

**Commit messages are factual and neutral.** State what changed, not how great it is or how it compares. No competitive bragging ("beats bun", "crushes pnpm", "fastest"), no superlatives. Write `site: update warm-install bench to 36-run numbers`, not `site: … (nub clearly beats bun)`. Benchmark commits name the change, never the verdict.

All internal deliberation lives in the gitignored `.fray/`, `epics/`, and `wiki/`. When in doubt, leave it out of the commit.

## Git & GitHub maintainer hygiene

This is a public project with real reporters watching, so maintainer responsiveness is mandatory hygiene, not courtesy. The end-to-end issue playbook is the `address-issue` skill; comment tone is [`PROSE.md`](PROSE.md) — factual, neutral, never braggy.

- **Acknowledge an EXTERNAL issue with exactly the comment `Investigating.`** — no summary, plan, timeline, or restatement of the bug. Internal/self-filed issues need no acknowledgement.
- **Substantive issue/PR comments are terse and factual.** State what you found and what you did, in the fewest words that carry the facts. Never write meta-commentary like "previous comments were wrong" — prior comments are often a bot's. The `gh-comment-guard` PreToolUse hook (`.claude/hooks/gh-comment-guard.mjs`, wired in `.claude/settings.json`) blocks an over-long `gh issue/pr comment` or `gh pr create` body; trim it, or set `NUB_ALLOW_LONG_COMMENT=1` when a longer body is genuinely needed (e.g. a detailed release note).
- **A PR that resolves an issue MUST carry a closing keyword (`Closes #N` / `Fixes #N` / `Resolves #N`) in its body before it is opened.** The keyword is what auto-closes the issue on merge; without it a resolved issue stays silently open. Verify as the last step before `gh pr create`. A PR that merely relates to an issue uses `Refs #N` instead.
- **On every release, comment the version + release link on every closed issue and merged PR that shipped in it** — e.g. `Shipped in v0.1.8: https://github.com/nubjs/nub/releases/tag/v0.1.8`. Merged is not shipped; this comment closes that gap for the reporter. (The `release` skill's Step 5 has the enumeration commands.)
- **`git commit -- <paths>` isolates files, not hunks.** On a shared tree, check `git status` first: if another agent already modified a file, your edit is not separable non-interactively — leave it uncommitted rather than committing their work. To undo, `git reset --soft HEAD~1` then `git restore --staged` (working tree untouched).
- **Close issues with a brief factual comment, never silently:** `gh issue close <n> --comment <text>`, stating what fixed it or why no code fix is needed.
- **No agent co-author trailers.** `.githooks/commit-msg` strips `Co-authored-by` trailers naming automated agents; `core.hooksPath` is already `.githooks` for this clone (a fresh clone needs `git config core.hooksPath .githooks`). CI's `no-agent-trailers` gate (`.github/workflows/commit-hygiene.yml`) is the backstop. Scope is UI-visible co-author trailers only — `Claude-Session:` URLs and other body lines are not flagged.

## Design philosophy — read before suggesting features

Before proposing any new feature, API, env var, package, or config surface, read:

- The brand-boundary rules below.
- [`wiki/architecture.md#augmenter-not-fork`](wiki/architecture.md#augmenter-not-fork) — the plain-Node-plus-extension-surface test.
- [`wiki/philosophy.md#additivity`](wiki/philosophy.md#additivity) — nub adds behavior; it never modifies existing Node semantics.
- [`wiki/PLAN.md`](wiki/PLAN.md) — the v0.1 manifest, Phase 1 / Phase 2 split, reversibility filter.
- [`wiki/whitepaper.md`](wiki/whitepaper.md) §"Zero Nub-specific APIs, zero lock-in".

### The brand boundary — public surfaces only (internals are exempt)

The boundary protects **public-facing surfaces only** — anything a user imports, installs, types, or authors as configuration, or any API they call by a documented name. **Internal mechanism is explicitly exempt:** error/warning codes, env vars nub sets for its own cross-process or shim plumbing, `globalThis.__nub*` sentinels, cache/dir names. Don't chase the brand out of internals. On the public surface the rules are absolute — no public nub-branded API even behind a flag.

- **No public `globalThis.nub`.** nub never injects a documented, user-callable global named after itself; `Bun.*`-style conveniences, if they exist at all, go in importable modules. Internal `globalThis.__nub*` sentinels are fine.
- **No `nub:*` module namespace.** nub never registers synthetic specifiers users would `import` (`nub:test`, `nub:sqlite`, `nub:serve`). The `node:*` namespace is upstream Node's and closed; nub does not squat a sibling.
- **No `@nub/*` npm scope** — the bare `@nub` scope is unused entirely. Everything ships under the project's own `@nubjs` org; see [`nub`-named npm packages](#nub-named-npm-packages-explained) and [`wiki/philosophy.md#the-brand-boundary`](wiki/philosophy.md#the-brand-boundary).
- **Internal `NUB_*` env vars are fine; never a brand env var as a documented USER knob.** nub may set `NUB_*`/`__NUB_*` for its own cross-process plumbing, detection sentinels, and debug toggles. For genuinely user-facing toggles prefer `NODE_*` where Node doesn't claim the name (`NODE_COMPAT`) and neutral conventions (`NO_COLOR`, `FORCE_COLOR`, `PORT`, `NODE_ENV`).
- **The nub runtime reads ZERO `AUBE_*` env vars.** Distinct from the exemption above: that covers vars nub *sets* for its children; this covers vars the runtime *reads* to decide behavior, which are user-facing knobs — and under nub the user sees nub, with aube an invisible implementation detail. aube exposes ~30 (`AUBE_DISABLE_CLONEDIR`, `AUBE_CONCURRENCY`, `AUBE_CACHE_DIR`, `AUBE_NO_UPDATE_CHECK`, the `AUBE_DISABLE_*`/`AUBE_DIAG_*`/`AUBE_SELF_*` families, …); none may be nub's surface. Preferred instead: a **neutral npmrc field** where one reads naturally (also reachable as `npm_config_<field>`), else a **`NUB_*`** env var or nub-namespaced config (`NUB_CACHE_DIR`, `NUB_PRIMER_TTL`). Two disciplines: keep the exposed set as SMALL as possible, and design each knob to GENERALIZE — a `NUB_PRIMER_TTL` *duration* (default unlimited), never a boolean `primer_evergreen`. The structural fix is an env-prefix / env-resolution hook on the Embedder profile, so standalone aube keeps `AUBE_*` while the NUB profile reads neutral/`npm_config_*`/`NUB_*`. Build-time `AUBE_*` knobs in aube's `build.rs` are out of scope for this runtime rule.
- **Prefer neutral names for USER-AUTHORED config, with one file exception.** nub reads a neutral `"tasks"` field rather than a `"nub"` field; do not add a `"nub"` package.json namespace or a `nub.toml`. The deliberate exception is project-root `nub.jsonc` — settled; do not reinterpret this preference as grounds to rename or remove it. nub may adopt another vendor-neutral, any-tool-could-implement-it field by decision record (currently `"tasks"`, [`wiki/commands/tasks.md`](wiki/commands/tasks.md)).
- **Sandbox config capabilities are per source scope, not repository-trust guesses.** Project `nub.jsonc` and root-authored `scriptsMeta` may use approved dynamic env values and credential-broker injection; dependency-controlled `dependenciesMeta` may not. Filesystem substitution is unconditional. A mixed project/script/dependency policy must preserve those per-scope capabilities rather than applying one global trusted/untrusted boolean, and nub never tries to infer whether a checkout or PR is trustworthy.
- **The boundary is SYMMETRIC.** Just as nub never *emits* its brand into your config, when nub is the active PM it never *consumes* another PM's branded config — `pnpm.overrides`, `pnpm.*`, yarn-namespaced config, Bun-branded fields are all ignored. A nub-identity project respects only `overrides`, `resolutions`, `catalog`, `workspaces`. (A compat project whose incumbent is npm/pnpm/yarn/bun is mirrored faithfully, branded fields included — the no-other-brand rule is for nub-identity projects.) Mechanism: `crates/nub-cli/src/pm_engine/config_scope.rs`.
- **pnpm-NAMED files and paths are never read unless pnpm is the incumbent PM.** No `pnpm-workspace.yaml`, `.pnpmfile.cjs`/`.pnpmfile.mjs`, `.pnpmrc`, `~/.config/pnpm/` (including `auth.ini`), or `pnpm.*` package.json field. The gate is the **name**, not the semantic effect — "auth-only / harmless / no resolution impact" exempts nothing. Crucial distinction: pnpm-NAMED ≠ pnpm-specific. A generically-named field that only pnpm happens to support (e.g. `overrides`) is a separate case-by-case decision and may be honored in a nub-identity project.
- **Error codes are internal — don't brand-sweep them.** `ERR_NUB_*` / `ERR_AUBE_*` / `WARN_*` identifiers are mechanism. Adding a missing code is good UX; rebranding an existing one for purity is not required.
- **No vendored Node patches.** nub does not patch Node source, ship a custom-built Node binary, or embed `libnode`. (An architecture rule, not a brand rule.)

### Web/runtime API compliance bar — spec and Cloudflare semantics; Bun parity is not the goal

Web-platform and runtime APIs (Worker, HTMLRewriter, fetch, streams, …) are implemented to WHATWG/TC39/WinterTC spec, or to Cloudflare Workers semantics where there is no formal spec (the documented HTMLRewriter exception). **Never implement a Bun-specific non-spec behavior for parity's sake.** If a user asks for one later it is considered individually, on its merits. Bun remains useful as a reference for what's possible and as a differential test oracle — not as a spec.

### `nub`-named npm packages, explained

Neither family is the prohibited bare `@nub/*` scope, and neither is user-facing application code:

1. **`@nubjs/types`** — nub's ambient TypeScript declarations, a types-only devDep under our own org. The correct home for any declaration user code references (`import.meta.hot`, nub-specific globals). Never a runtime import. (`@types/nub` on DefinitelyTyped is not viable — we don't own the npm package `nub`.)
2. **`@nubjs/nub` + `@nubjs/nub-<platform>`** — the CLI users `npm install -g`, plus its 8 platform `optionalDependencies` carrying the compiled binary + N-API addon, selected by npm's `os`/`cpu` filters and copied into place by `postinstall.js`. Install-time plumbing, never imported. Same pattern as `@biomejs/biome` + `@biomejs/cli-*`, `@rollup/rollup-*`, `@esbuild/*`.

### Naming

- **"Nub"** — proper noun, always capitalized in prose, on every surface: docs, release notes, blog, README, GitHub comments, commit bodies, code comments, chat. No "it's informal here" exception.
- **`nub`** — lowercase only as an identifier or part of a shell command: the executable, CLI invocations (`nub install`), package/file names (`@nubjs/nub`, `nub.lock`, `nub.jsonc`), code identifiers. Always in backticks in prose; a lowercase bare "nub" in running text is a style error.

## The aube vendoring + upstreaming

nub's package manager IS the vendored **aube** engine. Ground every aube claim here or in the code — never in memory.

- **Embed, not subprocess.** `vendor/aube` is a path dependency of plain in-tree files (not a submodule). nub has its own CLI (clap dispatch + `pm_engine::ENGINE_VERBS`) and calls `aube::commands::<verb>::run(typed_opts)` in-process. aube's own `cli_main` and tool subcommands are dead under nub. All engine output is rebranded through `crates/nub-cli/src/pm_engine/present.rs` (`ERR_AUBE_*`→`ERR_NUB_*`, `WARN_AUBE_*`→`WARN_NUB_*`, `aube`→`nub`, jdx URLs stripped). Consequence when asserting on output: grepping a log or test stderr for the raw `WARN_AUBE_*`/`ERR_AUBE_*` spelling silently finds nothing under nub. Match `(AUBE|NUB)`.
- **Fork vs upstream.** The vendored source originates from the fork `nubjs/aube`; upstream is `jdx/aube`. nub `main` is the source of truth. The `nubjs/aube` `nub-fork` branch is retained only as the upstream-PR staging area and historical record.
- **Never upstream to `jdx/aube` without the maintainer's explicit, in-the-moment instruction** — and never offer, propose, or label something an "upstream candidate" anywhere (PR bodies, commit messages, reports). The mechanics below stay dormant until asked.
- **When asked, contribution PRs target `jdx/aube`, never the fork.** A contribution PR is `jdx/aube:main ← nubjs/aube:<branch>`; a fork-internal `nubjs/aube ← nubjs/aube` PR upstreams nothing. Stacked PRs also base on `jdx/aube:main` (note the stack and which commit to review in the body; the diff auto-cleans once the parent merges). Create with `gh pr create --repo jdx/aube --base main --head nubjs:<branch>`.
- **What would upstream vs stays fork-only:** pluggability/additive changes that are no-op for standalone aube (the embedder profile, env-resolution hooks, the `prog()`/`cmd()` source-branding helpers, the exit-code sweep) → upstream. Material PM-behavior or nub-specific changes (primer-TTL, etc.) → fork-only, merge-synced on main, never rebased.
- **The embedder profile** is compile-time pluggability. nub sets its own (`cache_namespace` "nub/pm", `env_prefix`/`config_env_prefix`, banner-gating, pnpm-surface off, `read_branded_pnpm_config` gating) — this is what makes the brand and config boundary hold. Standalone aube keeps its `AUBE_*` behavior.
- **Fork-discipline retired 2026-07-29.** Standalone aube is no longer buildable from this tree, so a default-preserving opt-in shim protects nothing. Edit `vendor/aube` like any other nub source, defaults included. The Embedder-profile machinery stays where it already exists but is not required for a new change. A trustworthy-build/test deliverable still gets its own isolated clone + `CARGO_TARGET_DIR`.
- **Stale-doc warning:** `wiki/architecture.md`'s "no own package manager" line and the augmenter/fork section's Node-only framing predate the vendored PM. Trust this section and the code.

### The plain-vendoring workflow

Three recipes, all proven:

**(1) Make an aube change = a normal nub edit/PR.** Edit `vendor/aube/...`, commit, open a PR to nub `main` like any other file. The aube diff shows in the PR. No `nub-fork` branch, no pin, no ordering footgun.

**(2) Upstream selected aube changes to `jdx/aube`** (only on explicit instruction). Prefer (2b) for a few cherry-picked commits, (2a) for a larger contiguous slice. Either way the PR targets `jdx/aube:main` from a branch off it, so the upstream diff is only the change — never the whole fork delta.

```sh
# (2a) subtree split → cherry-pick onto a fresh branch off upstream
git subtree split --prefix=vendor/aube -b _aube_split   # re-roots paths to aube's layout, drops nub-only commits
git fetch upstream main                                  # upstream = jdx/aube
git checkout -b upstream-<topic> upstream/main
git cherry-pick <sha-from-_aube_split>...               # a MIXED commit brings ONLY its aube hunk across
git push fork upstream-<topic>
gh pr create --repo jdx/aube --base main --head nubjs:upstream-<topic>
```

```sh
# (2b) format-patch --relative (no split branch needed)
git format-patch <sha1>..<sha2> --relative=vendor/aube -o /tmp/aube-patches   # re-roots every diff path
git checkout -b upstream-<topic> upstream/main
git am --3way /tmp/aube-patches/*.patch
```

**(3) Sync FROM upstream `jdx/aube`** — infrequent and deliberate. Re-vendor the upstream tree and commit the delta as one reviewable nub commit. Keep the fork delta thin so this merge stays cheap.

```sh
git -C <clone-of-jdx-aube> archive main | tar -x -C vendor/aube
git add vendor/aube && git commit -m "aube: sync upstream <sha>"
# reconcile any nub-specific fork delta on top (cherry-pick / am from the prior vendored state)
```

## Testing philosophy

**Minimum number of tests, comprehensively covering the API surface.** Quality of coverage matters; volume does not. This is the antidote to AI-generated test bloat.

> Before pushing, run the [pre-push local verification loop](#implementation-quality-discipline). Start with `make verify` for the bounded host-local gate: root and native formatting, all-target/all-feature clippy, the native addon, the static brand lint, and the ordinary test suite. It does not reproduce platform matrices, Docker jobs, or change-specific end-to-end tests — run those separately when the change requires them, then promote durable checks into the suite. Get it green locally and push ONCE; fix-after-fix pushes starve the shared runner pool.

- **Comprehensive, not exhaustive.** Each behavior tested once, well: golden path, the one or two failure modes with user-facing implications, the boundary condition that's easy to get wrong. Stop there.
- **Hand-crafted feel.** Signs of agent bloat to avoid: identical assertions across `describe` blocks with different setup; per-input parametrization where one assertion would do; names that paraphrase the implementation; "should handle X" without naming what "handle" means.
- **Carefully considered abstractions.** Helpers and fixtures are part of the contract — don't bury behavior in clever shared setup, don't copy-paste either. A reader should skim a test file in 30 seconds and know what's verified. Past ~300 lines, split or trim.
- **Some things are untestable, and that's fine.** Perf-shaped behavior, OS corners, races needing infrastructure we lack, Node's internal scheduling. Leaving them untested is honest; ceremonial fake tests are not. Note the gap in a comment if it would surprise a future reader.
- **Pull from upstream where it makes sense.** Node's suite at `nodejs/node/test/` — the executable-level subset is the strongest compat validation available. Run black-box in two modes: `nub --node` (expect ~100%) and `nub` augmented (expect near-100% with documented divergences). Harness design and CI cadence: `wiki/research/node-test-suite-leverage.md`.
- **Test names describe the contract, not the implementation.** `test("emits ECONNRESET when peer closes mid-write")`, not `test("emits ECONNRESET")` and not a paraphrase of the code.
- **Failure messages must be self-debugging.** `expect(result).toEqual({ok: true})` is useless when it fails. Use `expect(result.ok).toBe(true)` or a custom message.
- **Flakes are hunted and killed at the SOURCE.** A test that passes on one run and fails on another is a P1 defect in the test. The cause is almost always shared mutable state or ordering coupling — a process global (`std::env`, `current_dir`, a `static`/`OnceCell`), a temp fixture/cache dir shared with a sibling, or reliance on filesystem/iteration ordering, leaking under cargo's multi-thread runner. Make the test hermetic (isolate env/cwd/fixture per test; serialize only as a last resort). **Banned as fixes:** `#[ignore]`, a retry wrapper, loosening the assertion, or "re-run until green" (a CI re-run is only ever to unblock an unrelated PR). One green run does not prove a flake dead — re-run repeatedly, varying `--test-threads` and order; for a platform-only flake use `ci-adhoc-test` across multiple runs. Windows-only surfacing is an ordering tell, not evidence of a platform parse bug.

## Docker is available — use it instead of declaring things "untestable"

Before writing a behavior off as unverifiable locally, check whether a container closes the gap:

- **A clean, dependency-free environment** — `node:22.15-slim` or no-Node `debian:slim` with no `~/.cache/nub`, no global Node: the honest way to test first-run install, the curl `install.sh` flow, `nub upgrade`'s `~/.nub` channel, provisioning a Node from nodejs.org.
- **A specific / floor Node version** — pin `node:22.15` for floor-only defects a modern host Node masks: `using`/`await using` down-leveling, the Temporal `toTemporalInstant` path, version-gated polyfills, the async-tier `module.register` loader.
- **Linux-specific paths** — musl-vs-glibc detection (`node:22-alpine` vs `node:22-slim`), arch matching, signal/`PATH` behavior.

**What Docker does NOT give you:** Linux containers only (on macOS/arm64 → `linux/aarch64`; `--platform linux/amd64` runs under slow QEMU). Windows containers need a Windows host, so cmd.exe behavior, `--script-shell` selection, `.cmd`/`.bat` resolution, and `nub.exe` vs `bin/nub` can only be verified on the `windows-latest` CI leg — never claim a Docker run verified those. For an ad-hoc macOS- or Windows-only probe use the `ci-adhoc-test` skill: a self-contained harness under `tests/<probe>/` driven by a branch-scoped workflow, no PR required.

**Need a macOS binary, not a container?** `nub scripts/mac-build.ts` builds nub and the N-API addon on a real macOS runner and pulls the signed artifact back, verifying checksums before staging — natively, so no cross-compilation stubs, with a correct deployment target (`minos 11.0`). See the `remote-build` skill.

Keep containers ephemeral (`docker run --rm`), mount the repo read-only where you only read it, and never leave long-running containers behind.

## Performance tracing the package manager

When a `nub install` or PM operation is mysteriously slow, do not hand-trace Rust — the `pm-perf-tracing` skill is the canonical method. The essentials:

- `RUST_LOG=debug nub install` emits `phase:<name> <elapsed>` lines (the coarse resolve/fetch/link split) and works under nub today.
- The per-file/per-strategy linker diagnostic (`aube_util::diag`; the `link_clonedir`/`link_macos_small_copy`/… tally) is LIVE under nub as `NUB_DIAG_FILE` — no source edit, no rebuild. `identity.rs` sets `diag_env_prefix: Some("NUB")`, carved out from `env_prefix: None` for exactly this. (`AUBE_DIAG_*` is not read under nub, and a former version of this bullet told you to patch `env_prefix` and rebuild for nothing.)
- Judge by the **strategy tally plus an A/B ratio** (the default `NodeLinker::Isolated` — see `vendor/aube/crates/aube-linker/src/lib.rs` — vs `--node-linker hoisted`), never a contended-host absolute. Always run a verified-clean warm `--offline` loop with rc=0.

## Verify the artifact, not something adjacent to it (HIGH PRIORITY)

Most wrong answers here trace to one habit: confirming that a thing EXISTS instead of confirming it is the thing you MEANT. A path that matches, a search that returns hits, a pipeline that exits 0, an authoritative-sounding doc comment, a filter that yields a tidy split, a results file one stage before the gates — each is a *plausible representation* of the truth, not the truth. **Name the artifact a claim rests on, and ask whether it is downstream of the question.**

**Before trusting a search, filter, or probe, run it against a case whose answer you already know.** If it misses the known case, the instrument is broken — fix it before reading any conclusion off it. This costs seconds and is the only check that catches a false NEGATIVE.

- **A passing test is not evidence until you have seen it fail for the right reason.** Three ways one passes while testing nothing, all observed in a single change: it asserts something is ABSENT on a platform that could never produce it; its PRECONDITION was silently already met (a test of "we bootstrap node-gyp" passed on a box that already had node-gyp on `PATH`, so the code never ran); or it asserts a non-zero exit that a DIFFERENT failure upstream produced. Before trusting a new test, break the thing it guards and watch it go red — and gate any absence assertion behind a positive control that proves the artifact appears when it is earned.
- **Enumeration undercounts by default.** State the expected count BEFORE searching and reconcile against it. A suspiciously round or small result means the pattern is too narrow, not that the surface is small.
- **Trace a symbol to its CONSUMERS before describing what it does.** Prose near a definition states intent; call sites state behavior. When they disagree, call sites win, and stale prose is worse than none.
- **A filter that produces a surprising split is more likely broken than insightful.** Check a row you can independently classify before believing the partition.
- **Confirm you are reading the artifact you just built.** Sort candidate paths by mtime — a stale `target/debug/` copy will report your new change absent.
- **A green wrapper is not a green job.** Any pipe makes `$?` the last stage's, including `| grep -v` used to suppress noise. Redirect to a file, capture `$?` on its own line, then read the log.
- **Symptoms tell you what HAPPENED; only code tells you what the system DOES.** Logs, error strings, reports and generated files are downstream of the answer. A cluster of similar-looking errors cannot tell you two failures share a cause — clustering Windows failures by the string `temp` merged two unrelated bugs and produced a wrong fix estimate. Any claim about MECHANISM, or about what a fix WILL change, is answered by reading the code that produces the behavior.
- **Check whether a board item is still true before acting on it.** Recorded facts go stale — an item called "the highest-value fix" had already been implemented for weeks; `#[cfg(not(windows))]` on one line said so. Same for dispatch briefs: a fresh agent cannot distinguish your checked claim from your remembered one.
- **Score with the project's scorer; never hand-sum its inputs.** Results files are often PRE-GATE, with the gates in a separate aggregator. Recomputing from them skips every gate and looks authoritative doing it.
- **Your own script's output is a claim, not data** — hold it to the same bar as a sub-agent's. A one-off classifier, `du`, or an ad-hoc grep filter has no more standing than an agent's assertion.
- **A number or superlative must carry its derivation.** Say "N, counted by X" or say "roughly". Every costly wrong claim here was quantified — the number is what made it actionable, and vagueness would have been harmless.

## Research the prior art — search the web early and often

**Before designing, implementing, or debugging anything non-trivial, spend the first few minutes finding out how it has already been solved.** Nearly everything nub does — a resolver rule, a lockfile shape, a linker layout, a concurrency default, a config-discovery order, an OS confinement primitive — some production tool has already shipped, and usually written down why. Map that space first, so you are CHOOSING among known approaches instead of inventing one and discovering its failure modes yourself. This is a reflex, not a phase: it fires when you pick up a work item, when a design fork appears, and when something behaves surprisingly.

- **Reach for `WebSearch` / `WebFetch` (or your harness's equivalent) eagerly and repeatedly.** One search is cheap and either hands you the answer or tells you you're first. There is no matching cost to over-searching, and the failure it prevents — a day spent on a problem someone already documented — is expensive.
- **Search the FILED BUG before you build a theory.** When a dependency, runtime, or API behaves in a way that surprises you, search its issue tracker, release notes, and the integrating projects' issues FIRST. Theorizing from local evidence alone produces confident, wrong stories that survive until the next piece of evidence kills them.
- **Read another tool's SOURCE locally, never one file at a time over HTTP.** `git clone --depth 1 <repo> .repos/<name>` (gitignored), then grep and read. A shallow clone takes seconds and gives you a whole consistent tree; check what `.repos/` already holds before cloning.
- **The highest-value output is what was TRIED AND ABANDONED** — recorded in issue threads, release notes, and RFCs, and invisible in the code. A knob that shrank across releases, or a feature that shipped and was reverted a release later, tells you more than the current implementation does.
- **Check whether their constraint is YOUR constraint before importing the lesson.** Another project's rejected approach may have been rejected for a reason nub does not share, which can make an option they closed off correct here. A survey that only tells you what to copy is half-read.
- **Name it in the dispatch prompt.** A fresh-context sub-agent does not inherit this habit and will reason from first principles unless told to survey.
- **Timebox it.** A high-level scan to map the space, not a research thread unless the work is one. A mechanical fix skips it.

## Probing methodology — differential fixtures, empirical over source

The highest-yield way to find correctness bugs in nub is a **differential fixture**: a minimal fixture isolating ONE behavior, run against nub AND the reference tools it claims parity with (npm / pnpm / bun / node) on identical inputs. The divergence is the finding. This beats happy-path app rounds by a wide margin. "nub does X" is unverified until "…and npm/pnpm/bun do Y on the same fixture" is in hand.

- **Test empirically before reading source, and before deciding.** A throwaway fixture answers "what does it actually do?" faster and more reliably than tracing Rust or Zig.
- **Read the COMMENTS and the resolution, not just the issue body.** A body says what someone wanted; the maintainer comments and why it was closed say what's true. An "open feature request" can be a deliberate rejection with the rationale in the thread.
- **Ground every claim about the system in code or an experiment — never memory.** Verify the actual surface (read `cli.rs`, or run it) before building against it.
- **Settle a language/stdlib semantics question with a probe, not reasoning.** A standalone `rustc` file needs no cargo and runs in seconds. `Path`/`OsStr`/`Components` are the repeat offenders — `Path` equality normalizes `.` away and `parent()` trims a trailing `.`, so paths that look distinct compare equal. If a diagnosis turns on what a std type does, run it; label it UNVERIFIED if you cannot.

**Reversing a decision on new evidence is correct, not flip-flopping.** Present the decision, keep probing, and let evidence move you rather than defending the first answer.

## Iterating across Node versions and tiers

nub's behavior splits by **tier** — the fast tier (Node 22.15+, sync `module.registerHooks`) and the compat tier (18.19–22.14, async loader-worker via `module.register`) take different code paths and break differently — so a green run on one modern Node routinely masks compat-tier and floor-only defects. nub discovers its Node from `PATH`, so the cheapest way to pin a version is `PATH="$HOME/.nvm/versions/node/v20.19.0/bin:$PATH" nub …`. Sweep several to cover both tiers, and use Docker for Linux + floor confirmation. **Do not claim cross-version support from one Node.**

**Always use the LATEST Node major (currently 26) as the recommended/example version** in docs, examples, Dockerfiles, `@types/node`, and marketing — never the tier floor. Version-FLOOR facts are different and should be stated where relevant (support floor 18.19, fast-tier classifier 22.15) — they are facts, not the version to put in a `FROM` line. Docs samples use the `{{NODE_MAJOR}}`/`{{NODE_VERSION}}` tokens, substituted by the `remark-node-version` plugin at build time; prefer those over a hardcoded number.

Feature-specific harnesses live under `tests/<feature>/` — e.g. `tests/pnp/` builds a fixture (`make-fixture.sh`), runs a scenario matrix across Node versions on the host (`run-pnp-matrix.sh`) and in per-version containers (`docker-matrix.sh`), and documents the loop in `tests/pnp/README.md`. When you build a system for iterating on a hard-to-unit-test feature, document it in place like that.

## Implementation quality discipline

**Quality over velocity.** Don't move fast, check boxes, and ship stubs as complete implementations.

- **Prefer the root-cause fix over a guardrail.** When a defect admits both a real fix and a cheaper mitigation (a warning, an error gate, a docs caveat, a bail-out), the real fix is the default and the recommendation out of the gate — not a follow-up. The amount of work is not a factor. A mitigation is acceptable only when the real fix is genuinely infeasible (external blocker, needs an upstream decision), and then it is framed as a stopgap with the real fix as the primary tracked deliverable.
- **Do not overengineer.** Solve the demonstrated problem at its actual scale. No generalized infrastructure, extra abstraction, speculative hardening, or adjacent cleanup without evidence the complexity earns its cost. "Not worth doing" is a valid and preferred verdict. (This is about not inventing scope — it never means picking a cheap mitigation over the correct fix.)
- **Never build a waiter for work the harness already tracks — and if you must poll, the predicate has to be able to FIRE.** After dispatch, continue useful non-overlapping work rather than arming a wait. Decide with this table before writing any wait:

  | What you are waiting on | What to do |
  | --- | --- |
  | A command YOU started with `run_in_background` | **Nothing.** You are re-invoked when it exits. Do not poll, `sleep`, or `until`-loop on its log. |
  | A sub-agent YOU dispatched (as root) | **Nothing.** Its completion notification re-invokes you. |
  | Your OWN backgrounded command, when you ARE a sub-agent | Don't background it. Block in the FOREGROUND to completion, looping foreground calls past the ~10-min cap. |
  | External state nothing reports (a CI run, a deploy, a remote queue) | Poll — capped iterations, and treat a timeout as an outcome you handle. |

  The harness's "use an until-loop" hint appears when it blocks a bare `sleep`; it applies to the last row only. Never arm a second waiter while the first is unfired.
- **A completed sub-agent becomes an immediate progress update to the user** — what it checked, what it found, what changed because of it, what remains open. Do this even when the finding is "no issue found."
- **Sub-agents never edit the root session's own working notes** — findings come back through the result channel. The one exception is fray's thread scratchpad (`.fray/threads/<session-id>/scratch.md`), which is shared coordination state: a child merges its OWN scoped progress there, re-reading first and never truncating or replacing the file.
- **Act on clear findings immediately.** If a test, review, or sub-agent exposes a concrete fix within scope, make it and verify it rather than listing it as follow-up. Ask the user only when the next step changes product behavior, security posture, public API, or another decision they own.
- **Never mark a task done without verifying the behavior end-to-end.** `cargo test` is necessary, not sufficient. If the task is "per-line stream prefixing matching pnpm," run `nub -r --stream run build` on a real fixture and diff against `pnpm -r --stream run build`.
- **Never claim parity without evidence.** If you haven't tested a flag, it isn't done. If the implementation is simplified, say so — don't write "implemented" for something scaffolded.
- **Name what you actually built.** `Stdio::inherit()` with a header line is not "stream prefixing"; fixed batches are not "work-stealing."
- **A task finished without running anything is not finished** — whatever its size. Match the verification to the change rather than to a clock: a typo needs a build, a behavior change needs a fixture. (A time floor would be the wrong rule — most issues really are a small fix you just write.)
- **A user-facing feature or behavior change is not done until `site/content/docs/` reflects it.** The code change and the doc update are the same effort.
- **Dialing in the visuals is part of implementing UI.** For any rendered surface, invoke the `visual-review` skill *before* calling it correct — always when placing an icon, glyph, emoji, badge, chip, or counter next to text. The skill carries the method; the part worth knowing up front is that **you are the first reviewer of your own screenshot** — capturing it is not reviewing it.
- **Verify locally before pushing — don't outsource verification to CI.** This protects shared CI capacity: every push fires ~5 workflows × the 8-platform matrix (~30–40 min each), and several PRs pushing fix-after-fix saturate the runner pool so head-commit jobs queue for hours. Get it green locally, push once. The loop, in your worktree:
  1. **Incremental build** through `scripts/rust-build.sh` — `scripts/rust-build.sh build -p nub-cli --profile fast` for the touched crates. **Never `export CARGO_TARGET_DIR` yourself:** the wrapper picks the target dir AND CoW-seeds a fresh private one from a warm shared bucket (~14s); an explicit variable silently opts out and costs a ~40-min cold build. This applies to a dispatch prompt as much as your own shell.
  2. **Run exactly what CI runs** for the cheap gates: `cargo clippy --all-targets --all-features --profile fast -- -D warnings` (a scoped `-p` without `--all-targets` misses test-code lints), `cargo fmt --check`, and the scoped `cargo test` for what changed. Match the invocations in `.github/workflows/ci.yml`. `--profile fast` on the lint gates is load-bearing, not a shortcut: it is what CI's check and clippy jobs run, and it keeps the gates in the same artifact universe as the dev loop instead of driving a second full dependency build under `dev`. `fast` inherits `dev`, so debug-assertions, overflow checks, and opt-level are identical — only debuginfo differs, which no lint reads. `cargo test` stays on the DEFAULT profile, matching CI's test jobs.
  2a. **The root workspace is not the whole repo.** `crates/nub-native` is its OWN workspace — a cdylib loaded into the user's Node process needs `panic = "unwind"`, which cannot coexist with the root release profile's `panic = "abort"` — so `-p nub-native` from the root fails with `package ID specification … did not match any packages`. That is the workspace boundary, not a typo. Run its gates from inside the crate, as CI does, and note `cargo fmt --check` is per-workspace too:
      - `cd crates/nub-native && cargo clippy --all-features --profile fast -- -D warnings`

      `crates/nub-launcher` is the SECOND such workspace and the one that gets forgotten — it ships inside every compiled artifact, and a root `clippy --all-targets --all-features` returning 0 says nothing about it. A field added to a struct it shares with `nub-core` left its test fixture behind and went red in CI while every local gate passed, twice. Its CI leg (`ci.yml:258`) is `cd crates/nub-launcher && cargo clippy --all-targets -- -D warnings && cargo build && cargo test`; run that before pushing anything that touches a shared type.

      Build the addon with `cd crates/nub-native && cargo build --release`, **never** `--manifest-path`: Cargo discovers `.cargo/config.toml` by walking up from the CWD, not from the manifest dir, so only the `cd` form picks up `crates/nub-native/.cargo/config.toml` and its `target-dir = "../../target"` that routes the addon into the repo-root `target/` the copy paths expect.

      **`vendor/aube` is the same story, and its failure mode is quieter.** It is a path DEPENDENCY, not a workspace member, so `cargo test -p aube-scripts` from the repo root refuses outright — `package 'aube-scripts' cannot be tested because it requires dev-dependencies and is not a member of the workspace`. Run aube's tests from inside it, with its own target dir: `cd vendor/aube && CARGO_TARGET_DIR=<somewhere-else> cargo test -p <crate>`. The consequence worth internalising is that **a test you add under `vendor/aube/` is invisible to every nub-side gate** — `--all-targets` from the root never builds a path dependency's test targets — so it neither runs nor protects anything until that gate exists (tracked as the aube-workspace gate). Verified 2026-08-04 by adding a test there and watching the root invocation refuse to run it.
  3. **Prefer running the heavy gates remotely.** `cargo clippy --all-targets --all-features` and a full `cargo test` are what saturate the dev host when many worktrees build at once. Run them on an ephemeral GCE spot VM: `nub scripts/remote-build.ts --job clippy --detach`, then `nub scripts/remote-build.ts --attach <vm-name>` to collect (the `remote-build` skill). Byte-identical CI invocation, a few cents each. **Use `--detach`/`--attach`, never the plain foreground form** — a foreground run is SIGKILLed at the agent harness's timeout, which no handler can catch, so cleanup is skipped and the builder leaks until its server-side TTL; `--attach` exits 75 meaning "still running, call again". The `--profile fast` inner loop stays local.
  4. **End-to-end test the specific functionality with a tmp fixture** — build the binary and exercise the actual feature against a throwaway fixture in `/tmp`, diffing against the reference tool where parity is claimed. "Tests pass" is not "the feature works."
  5. **Use Docker for behavior touching the global cache / config / a clean machine** — global `~/.npmrc`, config homes, the CAS store, first-run install, a Node floor — in an ephemeral `docker run --rm` container so host state can't mask or pollute the result. Linux only; Windows rides CI.
  6. **Incorporate the verification into the test suite where reasonable.** A tmp-fixture check that caught a real behavior should become a committed integration test or a documented harness under `tests/<feature>/`. A throwaway probe verifies *this* change; a committed test prevents the *next* regression.
  7. **A change touching `site/**` is gated by `.githooks/pre-push`,** which runs the same `next build` Vercel runs whenever the pushed range touches the site (~20s; a Rust-only push pays ~35ms and skips). It auto-installs site deps on first use in a worktree, and never blocks on infrastructure — only on a real build failure. For the inner loop use `pnpm run typecheck` in `site/` (~2s). This matters because `ci.yml` only fires on `push: main` / `pull_request: main`, so a long-lived feature branch gets NO CI and Vercel would otherwise be the first thing to type check the site — reporting by email, after the fact.
- **Inline comments are SPARSE and DENSE — design, invariant, provenance only.** The models we run over-comment by default: line-by-line narration restating what the code already says. Actively cut that; treat the bar to ADD a comment as high. Every comment must communicate one of: system DESIGN (how this fits the larger architecture), a non-obvious INVARIANT, or DECISION PROVENANCE (why this way, what was rejected, the gotcha that bit someone). Never narrate what the code says — no `// increment counter`, no step-by-step play-by-play. One dense comment at a subsystem or function boundary beats scattered narration. When in doubt, cut it; if the code needs a comment to be clear, improve the code first.

## Chat responses

**Plain language, colleague tone.** Use the minimum words that convey every point: direct, calm, collaborative, one technical colleague to another. Simplest accurate term; explain any necessary jargon at first use. State conclusions and tradeoffs; cut preamble, repetition, hedging, and ceremony. Concision never means omission — keep every fact needed for the decision. Full cross-surface tone bar: [`PROSE.md`](PROSE.md).

**The last message before you come to rest must stand alone.** It is the whole interface the reader gets: they see it in a queue, later, without your working context and without scrolling the transcript. So it accounts for everything you did since their last message — what changed and where, what you verified and how, what you decided and why, and what is still open. Never point at "the above", at a tool call, or at a file they have not read.

**Self-contained is a floor on content, not a licence for length.** Lead with the outcome, cut the chronology of how you got there, and drop anything that does not change what the reader now knows or does. A short list of what landed beats a retelling of the session.

**Prefer visual structure over dense prose.** A small status table, decision matrix, checklist, flow, or text diagram when it makes the answer easier to scan; for sequences show the path directly (`current → next → done`). Use a table when several items share fields, a list when order doesn't matter, a numbered list for a sequence, a callout for the one fact that could change the decision. Never a middot-separated or run-on inline list where real structure is clearer. Structure must clarify, not decorate.

Visual structure never replaces the host application's control syntax. In Fray, put every open human-owned decision in its own `question` fence with self-contained options, recommended option first — never hidden in a status table, prose list, or ordinary code fence.

**References must be clickable — every occurrence, not just the first.** The harness renders GFM with OSC-8 hyperlinks, so every issue/PR, file, fray thread, doc, or plan you mention is a markdown link, every time it appears: the fifth mention as much as the first, inside tables and lists as much as prose, and ones you linked in an earlier turn. There is no "already known" exception.

- **Issues/PRs:** `[#N](https://github.com/<owner>/<repo>/issues/N)` — the `/issues/N` form redirects to `/pull/N`, so it works for both. Never a bare `#N`.
- **Local files, threads, docs, plans:** link via the reader's IDE URI scheme — `[name](cursor://file/<absolute-path>)` or `[name](vscode://file/<absolute-path>)`, `:<line>` to jump.
- **Bare URLs:** wrap in `[text](url)`.

(This governs chat output only. In tracked files keep links relative and agent-agnostic — never a machine-specific absolute path or username.)

## User-facing copy: prose & tone

**[`PROSE.md`](PROSE.md) is the cross-project copywriting guide — read it before writing any GitHub comment, docs page, blog/marketing copy, or release note.** It is also the `prose-writing` skill, which auto-triggers on copy work and points back to PROSE.md as canonical. It owns register, sentence/heading mechanics, scannability, inline-code-pileup avoidance, description fields, real-output-only mockups, GitHub tone, markdown mechanics, and release-notes shape. The sections below carry only the nub-specific layers.

**Any general copy-style feedback gets applied everywhere it applies, not just where it was raised** — and gets recorded in `PROSE.md`, not here. Dispatch a sweep of all docs (plus homepage/blog where relevant), then add the rule to the shared guide.

**User-facing AGENT instructions must be coding-agent-agnostic.** Any copy an arbitrary coding agent reads and *executes* — `start.md`, the `nub agent skill` output, docs that tell an agent to do something — must accommodate every coding agent (Claude Code, Cursor, Codex/Copilot, Cline, …). Each stores standing instructions differently (`.claude/skills/<name>/SKILL.md`, `.cursor/rules/`, `AGENTS.md`, `.github/copilot-instructions.md`). Never hardcode a Claude-specific path as THE target; instruct the running agent to follow its own conventions and the repo's existing layout. A Claude path may appear only as one example among several. Keep `nub agent skill` / `https://nubjs.com/skill.md` as the agent-neutral source. (This does not govern this repo's own `.claude/skills/*`.)

### nub docs specifics (`site/content/docs/`)

- **Register:** zod.dev — to the point, code-first, no marketing fluff inside docs pages.
- **Page slugs are command-aligned:** `/docs/run`, `/docs/pm`; the command-less file runner is `/docs/files`.
- **Features relying on nub's own ambient TS declarations get a `<Callout>` at the top:** install `@nubjs/types` as a devDep alongside `@types/node`, and use `@types/node` 26. Only for pages whose types actually come from `@nubjs/types`/`nub-env.d.ts` (data-format imports, `Worker`, `import.meta.hot`) — word it to match what the feature needs.
- **The `--node` escape hatch is introduced once, on its own surface** (the runtime overview's `--node` section, the node-command page) or as a documented flag of the command a page is about (`nub run --node`, `nubx --node`) — never as a tangential aside tacked onto a PnP / TypeScript / web-storage behavior section.
- **Know the rendered HTML/CSS before hand-rolling a code block in a React/MDX component.** The site's global CSS styles a bare `<code>` as INLINE code, so a `<code>` inside a custom `<pre>` renders the whole block as an inline-code pill. A hand-built code block is a `<pre>` containing the text directly: `<pre>{`line1\nline2`}</pre>`. (A real MDX code fence is styled correctly by fumadocs — prefer it.) Never put `select-none` or per-span styling on parts of a code block; it makes them unselectable.

## Blog & marketing copy (`site/content/blog/`, homepage)

General structure rules live in [`PROSE.md`](PROSE.md). The homepage is the canonical register — reuse an existing passage or code block rather than rewriting it. The nub-specific layers:

- **Benchmarks use the homepage `<Bench>` component** (a registered global MDX component). The file-execution bench belongs to the file runner, not `nub run`.
- **Bailout commands are sub-section asides.** Surfaces that exist for completeness (`nub node`, `nub pm`) get a short `###` under the section whose implicit behavior they back up, never their own `##`.
- **Frame replacements as toolchain, not "npm packages."** What nub displaces is project-level tooling — `nvm`, `corepack`, the PM CLI. Keep the two compat axes distinct: pnpm-compatible CLI surface; lockfile-compatible with whatever the project uses. Concrete product comparisons are encouraged ("uv for Node").
- **Marketing asides use the styled treatment** (`.blog-prose blockquote`); default blockquote styling is unacceptable on the marketing site.
- **Version-gated claims must trace to code, not the wiki.** Every claim about a Node-version-gated feature maps to a named constant or function in `crates/nub-core/src/node/flags.rs` or `spawn.rs` — no symbol, no claim. The wiki's `status: v0.1` tracks intent, not implementation.
- **"Every supported Node version" is a banned phrase** unless the feature is `typeof`-feature-detected in the shared preload. Unflagged features are tier- and version-banded; state the floor per row.
- **Brand-boundary copy.** "The brand stops at the binary boundary" is wiki-internal shorthand, never user-facing. Absolute "the name nub never appears anywhere" claims are false (`~/.local/share/nub/store`, `~/.cache/nub`, internal `NUB_*` vars, error codes) — and do **not** claim "no `NUB_*` environment variables": three sanctioned user-facing PM knobs exist and are publicly documented (`NUB_CACHE_DIR`, `NUB_CONCURRENCY`, `NUB_PRIMER_TTL`). The correct public promise is narrower: no nub-specific imported/callable APIs, no `nub:*` namespace, no `@nub/*` scope, no `"nub"` config field — zero lock-in.
- **Protective-refusal demos use real nub output.** Capture from `target/release/nub` or ground the exact string in `crates/nub-cli/src/pm_engine/` — never invent.

## Markdown navigation — line-range TOC for large files

Before loading a large markdown file in full, run `node scripts/md-toc/index.mjs <file.md>` (or `nub scripts/md-toc/index.mjs <file.md>`) for a heading TOC with exact line ranges, then `Read` only the section you need via `offset`/`limit`. Full usage: `.claude/skills/md-toc/SKILL.md`.

## Releasing

Nub publishes to npm as `@nubjs/nub` plus 8 platform-specific binary packages, fully automated via GitHub Actions.

**The `v*` tag push IS the publish — it is irreversible and requires the maintainer's explicit, in-the-moment
say-so. Invoke the `release` skill rather than running the recipe below from memory**; it carries that gate,
the version-pick rules, and the mandatory post-release issue/PR comments.

```bash
make version V=0.0.6          # sets version in all 9 npm packages + Cargo.toml
make version-check             # verify consistency
git add -A && git commit -m "v0.0.6"
git tag v0.0.6
git push origin main --tags    # CI builds 8 platforms, publishes to npm, creates GitHub release
```

Other Makefile targets: `make npm-build` (build + package for the current platform), `make npm-publish` (manual publish — prefer CI), `make npm-publish-dry`.

**CI release workflow** (`.github/workflows/release.yml`) triggers on `v*` tags and builds darwin-arm64, darwin-x64, linux-x64, linux-x64-musl, linux-arm64, linux-arm64-musl, win32-x64, win32-arm64. Publishes via npm OIDC trusted publishing (no secrets), then creates the GitHub Release with binary artifacts.

**Version regime:** stay in `0.0.x` until public launch; bump to `0.1.0` only when the whitepaper, benchmarks, and install experience are polished.
