---
name: lat-md
description: Search, read and maintain nub's knowledge graph under wiki/ with the `lat` CLI — the design and research corpus, cross-linked and checked. Invoke (via the Skill tool) before designing or changing anything non-trivial, to find the decision that already governs it instead of re-deriving it; and after any change that alters architecture, behavior or test coverage, because `lat check` is a CI gate and a stale wiki link now fails the build. Carries the commands, the section-id syntax, the two rules that make a section valid, and the three nub-specific traps — never run bare `lat init`, never create `.agents/skills/`, and Rust symbols inside a `mod` block cannot be linked.
version: 1.1.0
metadata:
  internal: true
---

# lat.md — nub's knowledge graph

The design and research corpus in `wiki/` is a [lat.md](https://github.com/1st1/lat.md) graph: cross-linked markdown, with `lat check` enforcing that every link and code reference still resolves. The repo root carries a `lat.md` symlink pointing at `wiki/`, because `lat` finds its graph by that directory name.

The package scripts fetch an exact Lat version outside the root dependency tree. `nub run lat:check` runs the same graph gate as CI; `nub run lat <command>` exposes the full CLI without a global install. The pin in `package.json` is checked against both MCP configurations by `scripts/lat.test.mjs`.

**Do not invoke bare `lat`.** It is not installed globally. Run commands from the repository root with `nub run lat`, or use the MCP tools. With only Node/npm installed, `npm run --silent lat -- <command>` is equivalent.

## Local setup and recovery

Run `nub run lat:index` once in each new checkout or worktree. It invokes `reindex --local --yes`, builds the index, and records a local-backend preference for that checkout. This is an explicit local-machine setup operation, not a Nub build. Initial indexing can take several minutes; warm searches only embed new or changed sections.

- **Storage:** the ignored `wiki/.cache/vectors.db` contains section text and local MiniLM embeddings. Only the graph is indexed, not all source files or `internal/`. Do not commit the cache or move private documents into the public wiki to make them searchable.
- **Offline operation:** the first package fetch needs network access. Once cached, embedding and search need no network or API key. The stored local model ignores hosted keys; the per-checkout preference survives deletion of the vector cache. A new checkout has a new path and needs its own bootstrap.
- **Recovery:** run `nub run lat:index` after cache corruption or a backend mismatch. Do not run it before every query: normal `search` already refreshes changed sections. Run `nub run lat config` to find the user-level preference file; there is no API key to configure for local search.
- **Code-reference scans:** install ripgrep (`rg`) on PATH. Lat uses it for `check`, `refs`, and `section`; its fallback can traverse large nested checkouts and is much slower. GitHub's Ubuntu runner already supplies it.
- **Agent tools:** `.mcp.json` configures Claude; `.codex/config.toml` configures Codex. Both launch the pinned Lat MCP server and expose `lat_search`, `lat_section`, `lat_locate`, `lat_refs`, `lat_expand`, and `lat_check`. Restart the agent after config changes and approve the project MCP server if prompted. The CLI fallback works in sessions that have not reloaded.
- **Prompt reminder:** both agents run the same dependency-free `scripts/lat-prompt.mjs` hook. It directs nontrivial tasks to the graph without fetching packages or embedding anything during prompt submission. Do not enable Lat's generated stop hook: it counts `lat.md/` diffs rather than this repo's tracked `wiki/` paths and can attribute unrelated shared-tree changes to the current task.

## Use it before you design, and after you change

Read the graph first. A grep over `crates/` tells you what the code does; the graph tells you **why**, and what was already tried and rejected. Both matter, and the second is the one you cannot recover by reading source.

CI executes the navigation examples below against this graph. A separate small-fixture test builds local embeddings, searches, refreshes edited content, and calls the MCP server; it does not rebuild the entire wiki index. Section ids are real; substitute your own.

```bash
nub run lat search "why is the user's Node spawned instead of embedded"   # semantic search
nub run lat locate "Two tiers"                                            # find a section by name
nub run lat section "architecture#Architecture#Turning it off"            # print a section with its links
nub run lat refs "architecture#Architecture#Composition"                  # incoming references
nub run lat expand "fix [[compat-mode-tests]]"                            # resolve references in a prompt
nub run lat check                                                         # the graph gate
```

Three gates run it for you, cheapest first: `.githooks/pre-commit` when the staged changes touch `wiki/` or this skill, `.githooks/pre-push` unconditionally (so a symbol rename that orphans a doc link is caught even though no doc was edited), and the `lat-check` job on pull requests to `main`. Both hooks warn rather than block if the checker cannot run, and both take `NUB_SKIP_LAT_CHECK=1`.

After a change that alters architecture, behavior, or test coverage, update the graph in the same commit and run `nub run lat:check`. CI runs on main pushes and on the `ci` label for pull requests targeting `main`; opening or pushing a PR alone does not request a run. Stacked PRs based on another branch do not run this workflow.

## Section ids and links

A section id is the file path with the `.md` dropped, then each heading — `design/architecture#Architecture#Composition`. A bare filename works when it is unique: `architecture#Architecture#Composition`. The path is relative to the graph directory under the name lat resolves it by, which is `lat.md`, so the full form lat prints in its own diagnostics is `lat.md/design/architecture#…`. The on-disk name is not interchangeable: `research/cold-start#…` and `lat.md/research/cold-start#…` both exit 0, while `wiki/research/cold-start#…` exits 1.

- **Wiki link:** `[[target]]` or `[[target|alias]]`, pointing at a section or at a source symbol.
- **Source link:** `[[crates/nub-core/src/node/spawn.rs#PATH_SHIM_PREFIX]]` — repo-root-relative, unlike a section id, and `lat check` verifies the symbol exists. That example is deliberately one the graph already uses (`wiki/design/architecture.md` links it), so renaming the constant fails the gate rather than rotting this file.
- **Code reference:** `// @lat: [[section-id]]` in Rust, TypeScript or JavaScript; `# @lat: [[section-id]]` in Python. It ties an implementation or a test back to the section that specifies it.

**What the gate cannot see:** a path written as plain text (`wiki/foo.md` in a comment) and an ordinary `[text](foo.md)` markdown link. Only `[[wiki links]]` and `@lat:` references are validated. That blind spot is how 36 dead `wiki/` paths accumulated in this repo, 21 citation sites of which had to be swept out of Rust comments by hand. If you want a reference to stay true, write it in one of the two checked forms.

Keep `@lat:` comments to places where the link earns its line — a subsystem entry point, or a test that covers a named spec. They are subject to nub's ordinary comment discipline: sparse and dense, never narration.

## The two rules that make a section valid

1. **Every heading needs a leading paragraph** — one or more sentences immediately after the heading, before any child heading, list, table, or code block.
2. **That paragraph is 250 characters or fewer**, excluding text inside `[[wiki links]]`. It is the summary that `lat search` and `lat section` print, so put the substance in it and the detail in the paragraphs below.

Every directory also needs an index file named after it — `wiki/research/research.md` lists every document in `wiki/research/`, and `lat check` fails if one is missing.

## Three nub-specific traps

- **Never run bare `lat init`, and lat will ask you to twice.** A passing run still prints `Warning: No init version recorded — run lat init to set up agent hooks and configuration.` — expected here, and safe to ignore. A checkout where the root `lat.md` symlink did not materialise fails instead with `No lat.md directory found` / ``Run `lat init` to create one.`` and exits 1; the fix is restoring the symlink (`git checkout -- lat.md`, or `git config core.symlinks true` on Windows), never `lat init`. Running it writes an instruction block into both `AGENTS.md` and `CLAUDE.md`, and in this repo `CLAUDE.md` is a symlink to `AGENTS.md` — Node writes through a symlink, so the second write lands on top of the first inside the tracked, public, Codex-shared `AGENTS.md`. Edit that file by hand instead.
- **Never let anything create `.agents/skills/`.** `lat init` puts its own skill there, and `.githooks/pre-push` refuses any push with a `SKILL.md` under that path, because a rival skill tree once drifted for weeks. This file is the skill; `.claude/skills/` is the only skills directory.
- **A Rust symbol inside a `mod` block cannot be linked.** lat's Rust extractor walks only top-level items, so `[[…rs#some_unit_test]]` fails for the 972 `#[test]` functions that live in `#[cfg(test)] mod tests`, and for any item in an inline `mod`. Top-level functions, structs, enums, traits, consts, type aliases and `impl` methods all resolve. `@lat:` comments are a plain comment scan and work anywhere, including inside `mod tests` — so test specs are unaffected.
