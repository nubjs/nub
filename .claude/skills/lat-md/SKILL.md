---
name: lat-md
description: Search, read and maintain nub's knowledge graph under wiki/ with the `lat` CLI — the design and research corpus, cross-linked and checked. Invoke (via the Skill tool) before designing or changing anything non-trivial, to find the decision that already governs it instead of re-deriving it; and after any change that alters architecture, behavior or test coverage, because `lat check` is a CI gate and a stale wiki link now fails the build. Carries the commands, the section-id syntax, the two rules that make a section valid, and the three nub-specific traps — never run bare `lat init`, never create `.agents/skills/`, and Rust symbols inside a `mod` block cannot be linked.
version: 1.0.0
metadata:
  internal: true
---

# lat.md — nub's knowledge graph

The design and research corpus in `wiki/` is a [lat.md](https://github.com/1st1/lat.md) graph: cross-linked markdown, with `lat check` enforcing that every link and code reference still resolves. The repo root carries a `lat.md` symlink pointing at `wiki/`, because `lat` finds its graph by that directory name.

`npm run lat:check` runs the gate exactly as CI does, fetching the pinned checker through `npx`. It is deliberately NOT a root devDependency: it pulls ~185 transitive packages, and the root `npm ci` runs through a Socket Firewall shim in every `ci.yml` test leg, where that much extra install tripped the "assert root deps actually installed" guard across the matrix. For repeated local use, `npm i -g lat.md@0.12.2` puts `lat` on `PATH`.

## Use it before you design, and after you change

Read the graph first. A grep over `crates/` tells you what the code does; the graph tells you **why**, and what was already tried and rejected. Both matter, and the second is the one you cannot recover by reading source.

```bash
lat search "how does workspace root discovery work"   # semantic search, offline, no API key
lat locate "Verb dispatch"                            # find a section by name
lat section "research/cold-start#Where the time goes"  # print a section with its links
lat refs "design/architecture#Augmenter, not fork"     # what points AT this section
lat expand "fix [[cold-start]]"                       # resolve [[refs]] in a prompt
lat check                                              # the CI gate
```

After a change that alters architecture, behavior, or test coverage, update the graph in the same commit and run `lat check`. It runs on every pull request against `main`, so a doc naming a symbol you just renamed fails there rather than rotting quietly. It is not a *required* check until someone adds it to branch protection, and a stacked pull request based on another branch does not run it at all.

## Section ids and links

A section id is `<file>#<Heading>#<SubHeading>`, with the file path relative to the repo root and the `.md` dropped — `wiki/research/cold-start#Where the time goes`. A bare filename works when it is unique: `cold-start#Where the time goes`.

- **Wiki link:** `[[target]]` or `[[target|alias]]`, pointing at a section or at a source symbol.
- **Source link:** `[[crates/nub-cli/src/pm_engine/mod.rs#lookup_verb]]` — `lat check` verifies the symbol exists.
- **Code reference:** `// @lat: [[section-id]]` in Rust, TypeScript or JavaScript; `# @lat: [[section-id]]` in Python. It ties an implementation or a test back to the section that specifies it.

Keep `@lat:` comments to places where the link earns its line — a subsystem entry point, or a test that covers a named spec. They are subject to nub's ordinary comment discipline: sparse and dense, never narration.

## The two rules that make a section valid

1. **Every heading needs a leading paragraph** — one or more sentences immediately after the heading, before any child heading, list, table, or code block.
2. **That paragraph is 250 characters or fewer**, excluding text inside `[[wiki links]]`. It is the summary that `lat search` and `lat section` print, so put the substance in it and the detail in the paragraphs below.

Every directory also needs an index file named after it — `wiki/research/research.md` lists every document in `wiki/research/`, and `lat check` fails if one is missing.

## Three nub-specific traps

- **Never run bare `lat init`, and lat will ask you to twice.** A passing run still prints `Warning: No init version recorded — run lat init to set up agent hooks and configuration.` — expected here, and safe to ignore. A checkout where the root `lat.md` symlink did not materialise fails instead with `No lat.md directory found` / ``Run `lat init` to create one.`` and exits 1; the fix is restoring the symlink (`git checkout -- lat.md`, or `git config core.symlinks true` on Windows), never `lat init`. Running it writes an instruction block into both `AGENTS.md` and `CLAUDE.md`, and in this repo `CLAUDE.md` is a symlink to `AGENTS.md` — Node writes through a symlink, so the second write lands on top of the first inside the tracked, public, Codex-shared `AGENTS.md`. Edit that file by hand instead.
- **Never let anything create `.agents/skills/`.** `lat init` puts its own skill there, and `.githooks/pre-push` refuses any push with a `SKILL.md` under that path, because a rival skill tree once drifted for weeks. This file is the skill; `.claude/skills/` is the only skills directory.
- **A Rust symbol inside a `mod` block cannot be linked.** lat's Rust extractor walks only top-level items, so `[[…rs#some_unit_test]]` fails for the 972 `#[test]` functions that live in `#[cfg(test)] mod tests`, and for any item in an inline `mod`. Top-level functions, structs, enums, traits, consts, type aliases and `impl` methods all resolve. `@lat:` comments are a plain comment scan and work anywhere, including inside `mod tests` — so test specs are unaffected.
