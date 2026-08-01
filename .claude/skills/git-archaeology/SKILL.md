---
name: git-archaeology
description: Fast recipes for answering when/what/why a feature, flag, API, file, or string was added, removed, unflagged, or renamed in git history.
metadata:
  internal: true
---

# git-archaeology

Playbook for "when did X land / leave / change?" — get the answer in one command.

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

For an exact symbol, flag string, function name, or config key. `-S` fires when the occurrence count changes.

### 2. Pickaxe by regex / diff content

```bash
git log --oneline -G'<regex>' -- <path>
```

When `-S` misses because the string appears in both old and new (count unchanged), or to match a pattern across added/removed lines.

### 3. When a file was DELETED

```bash
# Find the commit that deleted a specific path:
git log --oneline --diff-filter=D -- <path>

# Find deletion of any file matching a name glob (across all branches):
git log --oneline --diff-filter=D --all -- '**/<name>'

# Cross-check the surrounding context:
git show <sha> -- <path>
```

### 4. Follow renames

```bash
git log --follow --oneline -- <path>
```

Use when `git log -- <path>` shows a history that obviously predates the file's true age.

### 5. Line-range history

```bash
# History of lines matching a pattern (N lines from match):
git log -L'/<pattern>/',+<N>:<file>

# History of a fixed line range:
git log -L<start>,<end>:<file>
```

Traces when a specific function body, flag block, or config stanza changed without reading the whole log.

### 6. What a specific commit changed (scoped)

```bash
# Full diff scoped to a path:
git show <sha> -- <path>

# Summary of what the commit touched:
git show --stat <sha>
```

### 7. Who/when a line came in (blame)

```bash
# Blame a line range:
git blame -L<start>,<end> <file>

# Blame + full patch context (expensive but thorough):
git log -p -L<start>,<end>:<file>
```

### 8. Date and subject of a commit

```bash
git log -1 --format='%h %cs %s' <sha>      # %cs = YYYY-MM-DD
```

### Combo: "when did X land in code vs site?"

```bash
# In implementation code (crates/, runtime/):
git log --oneline --reverse -S'<string>' -- crates/ | head -1

# In marketing/docs (site/, README):
git log --oneline --reverse -S'<string>' -- site/ README.md | head -1
```

A gap between the two dates = the feature was advertised before (or after) it shipped. The code date is the authoritative answer to "when did it land."

## Methodology

**Pickaxe the implementation, not the marketing surface.** `site/`, `README.md`, and docs may list a feature aspirationally from the initial commit — searching there answers "when was it promised." For "when did it ship," pickaxe `crates/`, `crates/nub-core/src/node/flags.rs` and `spawn.rs` (the authoritative source for "is a Node experimental flag active by default"), and the capability tables in code.

**A feature on a marketing surface may have been removed later.** A positive hit on `site/` does not mean it ships today — run the delete check before concluding it's live:

```bash
git log --oneline --diff-filter=D -- '<feature-file>'
git log --oneline --all --grep='deferred\|removed\|dropped' -- '<area>'
```

**For "what was recently unflagged?" target the flag-injection layer:**

```bash
# Find all commits that touched experimental-flag injection:
git log --oneline -G'--experimental-' -- crates/nub-core/src/node/flags.rs crates/nub-core/src/node/spawn.rs

# Pickaxe a specific flag:
git log --oneline -S'--experimental-shadow-realm' -- crates/
```

Cross-reference `.fray/*-unflag.md` / audit threads for the decision record.

**Distinguish three states — decide which one is being asked about before searching:**

| State | How to verify |
|---|---|
| On the homepage / in docs | Pickaxe `site/`, README |
| Implemented (code exists) | Pickaxe `crates/`, `runtime/` |
| Unflagged / default-on | Pickaxe `flags.rs`, `spawn.rs` for the flag string |

**Add `--all` when the expected result isn't on `main`** — deleted files and dropped branch-resident features may only appear in non-main history. History rewrites change SHAs but not content, so pickaxe still works after a force-push.
