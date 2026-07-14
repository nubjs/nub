---
name: git-archaeology
description: Fast recipes for answering when/what/why a feature, flag, API, file, or string was added, removed, unflagged, or renamed in git history.
metadata:
  internal: true
---

# git-archaeology

Instant playbook for "when did X land / leave / change?" questions. The goal: skip the meander, get the answer in one command.

---

## Recipes

### 1. Pickaxe — when a string/symbol entered or left history

```bash
# All commits that changed the COUNT of <string> in a path:
git log --oneline -S'<string>' -- <path>

# First introduction (oldest commit):
git log --oneline --reverse -S'<string>' -- <path> | head -1

# Most recent change:
git log --oneline -S'<string>' -- <path> | head -1

# Include all branches:
git log --oneline --all -S'<string>' -- <path>
```

Use when: finding the commit that added or removed an exact symbol, flag string, function name, or config key. `-S` counts occurrences — it fires when the count changes.

---

### 2. Pickaxe by regex / diff content

```bash
git log --oneline -G'<regex>' -- <path>
```

Use when: `-S` misses a change because the string appears in both old and new (count unchanged), or when you want to match a pattern across added/removed lines (e.g. `-G'--experimental-shadow-realm'` to find every commit that touched that flag in any form).

---

### 3. When a file was DELETED

```bash
# Find the commit that deleted a specific path:
git log --oneline --diff-filter=D -- <path>

# Find deletion of any file matching a name glob (across all branches):
git log --oneline --diff-filter=D --all -- '**/<name>'

# Cross-check the surrounding context:
git show <sha> -- <path>
```

Use when: a file exists in memory or docs but is absent from the tree — find when it was dropped and what the commit message says.

---

### 4. Follow renames

```bash
git log --follow --oneline -- <path>
```

Use when: a file was renamed and `git log -- <path>` shows a short history that obviously predates the file's true age.

---

### 5. Line-range history

```bash
# History of lines matching a pattern (N lines from match):
git log -L'/<pattern>/',+<N>:<file>

# History of a fixed line range:
git log -L<start>,<end>:<file>
```

Use when: tracing exactly when a specific function body, feature flag block, or config stanza was introduced or changed — avoids reading the whole file log.

---

### 6. What a specific commit changed (scoped)

```bash
# Full diff scoped to a path:
git show <sha> -- <path>

# Summary of what the commit touched:
git show --stat <sha>
```

Use when: you have a SHA from a pickaxe result and need to see the exact diff.

---

### 7. Who/when a line came in (blame)

```bash
# Blame a line range:
git blame -L<start>,<end> <file>

# Blame + full patch context (expensive but thorough):
git log -p -L<start>,<end>:<file>
```

Use when: you need the author + date for specific lines, or want the full patch history for a code block.

---

### 8. Date and subject of a commit

```bash
git log -1 --format='%h %cs %s' <sha>
```

Use when: a pickaxe returned a SHA and you need the short date (`%cs` = YYYY-MM-DD) and subject without extra noise.

---

### Combo: "when did X land in code vs site?"

```bash
# In implementation code (crates/, runtime/):
git log --oneline --reverse -S'<string>' -- crates/ | head -1

# In marketing/docs (site/, README):
git log --oneline --reverse -S'<string>' -- site/ README.md | head -1
```

A gap between these two dates = the feature was advertised before (or after) it shipped. The code date is the authoritative answer to "when did it land."

---

## Methodology — lessons that cost time

### Pickaxe the implementation, not the marketing surface

`site/`, `README.md`, and docs may list a feature aspirationally from the initial commit. Searching there answers "when was it promised," not "when did it ship." To answer "is X implemented / when did it land?", pickaxe:

- `crates/` — Rust implementation
- `crates/nub-core/src/node/flags.rs` and `spawn.rs` — flag injection (the authoritative source for "is a Node experimental flag active by default")
- the feature matrix or capability table in code, not in docs

Concrete burn: most homepage API names traced to "Initial commit" — all misleading, answered intent not state.

### A feature on a marketing surface may have been removed later

A positive pickaxe hit on `site/` does not mean the feature ships today. Always run the delete check and a grep for "deferred"/"removed"/"dropped" before concluding it's live:

```bash
git log --oneline --diff-filter=D -- '<feature-file>'
git log --oneline --all --grep='deferred\|removed\|dropped' -- '<area>'
```

Concrete burn: `connect()` appeared on the homepage but `runtime/connect-sockets.mjs` was deleted 2026-05-26 and the feature deferred.

### For "what was recently unflagged?" — target the flag-injection layer

```bash
# Find all commits that touched experimental-flag injection:
git log --oneline -G'--experimental-' -- crates/nub-core/src/node/flags.rs crates/nub-core/src/node/spawn.rs

# Pickaxe a specific flag:
git log --oneline -S'--experimental-shadow-realm' -- crates/
```

Cross-reference `.fray/*-unflag.md` / audit threads for the decision record. This makes "what was recently unflagged?" instant rather than a multi-file meander.

Concrete answer this should make instant: shadow-realm + wasm-modules were unflagged together in PR #31.

### Distinguish three states — they are not the same

| State | How to verify |
|---|---|
| On the homepage / in docs | Pickaxe `site/`, README |
| Implemented (code exists) | Pickaxe `crates/`, `runtime/` |
| Unflagged / default-on | Pickaxe `flags.rs`, `spawn.rs` for the flag string |

Always check which state is actually being asked about before searching.

### Always consider `--all` for deleted or branch-resident content

Deleted files and features that were developed on a branch and later dropped may only appear in non-main history. Add `--all` to any pickaxe or `--diff-filter=D` search when the expected result isn't turning up on `main`.

History rewrites change SHAs but not content — pickaxe by string still works after a force-push because it searches diff content, not commit identity.
