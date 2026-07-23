---
name: md-toc
description: Navigate large markdown files efficiently — get a complete heading TOC with exact line ranges, then Read only the section you need instead of loading the whole file.
version: 1.0.0
metadata:
  internal: true
---

# md-toc — markdown navigation via line ranges

When a markdown file is large (>200 lines), don't Read the whole thing. Run the TOC script first to see every heading and its section's line range, then use `Read` with `offset` + `limit` to jump straight to the section you need.

## Invocation

The executable is a repository tool, not a resource nested under this skill directory. Resolve the checkout root explicitly before invoking it so the command works even when the skill itself was discovered under `.claude/skills/`:

```bash
repo_root="$(git rev-parse --show-toplevel)"
node "$repo_root/scripts/md-toc/index.mjs" <file.md>
# or equivalently:
nub "$repo_root/scripts/md-toc/index.mjs" <file.md>
```

Resolve `<file.md>` from the task's working directory. Do not look for `.claude/skills/md-toc/scripts/md-toc/index.mjs`; that nested path does not exist.

## Output format

One line per heading, indented by depth, with the section's line range:

```
L1-402    # Nub — Final-Polish Orchestration Tracker
L152-192    ## Status board
L282-367    ## Item cards
L284-292      ### A — Settle/verify env-plumbing  ·  todo
```

The range `L152-192` means the "Status board" section occupies lines 152–192. To read it:

```
Read(file, offset=152, limit=41)   # limit = end - start + 1
```

A heading's section spans from its own line to just before the next heading of equal or higher level (or EOF). `#` lines inside fenced code blocks are correctly ignored (uses a real markdown AST parser, not regex).

## When to use

- Any file >200 lines where you need one section, not the whole file.
- HANDOFF/STATUS trackers, AGENTS.md, architecture docs, long epics.
- Before dispatching a sub-agent that needs a slice of a large doc — run the TOC yourself, embed the relevant offset/limit in the prompt so the agent Reads precisely.
