# Provenance

`SKILL.md` in this directory is **vendored, not ours**. It is a byte-verbatim copy of `skills/productivity/grilling/SKILL.md` from [mattpocock/skills](https://github.com/mattpocock/skills), at commit [`068b6e0c62393147daf03530149cdce209c93da8`](https://github.com/mattpocock/skills/tree/068b6e0c62393147daf03530149cdce209c93da8) (2026-08-15).

MIT License, Copyright (c) 2026 Matt Pocock. Full text: <https://github.com/mattpocock/skills/blob/main/LICENSE>.

## Do not edit SKILL.md in place

Keeping the file byte-identical to upstream is the whole point: it is what lets anyone diff this copy against the source and see, in one command, whether it has drifted.

```bash
# Should print nothing.
curl -sL https://raw.githubusercontent.com/mattpocock/skills/068b6e0c62393147daf03530149cdce209c93da8/skills/productivity/grilling/SKILL.md \
  | diff - .claude/skills/grilling/SKILL.md
```

To take a newer upstream revision, re-copy the file, update the commit pin above, and re-run that diff. Local adaptations do not go here — they go in the skill that *calls* this one, which is where the `implementation-thread` additions live.

## Why it is vendored rather than installed

The skill was first installed as a personal skill under `~/.claude/skills/`, and `implementation-thread` pointed at it there. That is unsound for a public repo: a reference to a skill that exists only on one maintainer's machine is a broken reference for every contributor. It lives in-tree so a clone is sufficient.

A personal copy may still exist at `~/.agents/skills/grilling/` alongside a `grill-me` wrapper, which makes the interview available in repos other than this one. That copy and this one are independent — updating one does not update the other.

## What calls it

`implementation-thread` loads it as phase 1, the interview that settles a brief before any design work starts. Upstream also ships `grill-me` (a one-line, user-typed wrapper around this skill) and `grill-with-docs` (the same interview, plus `CONTEXT.md` and ADRs, which additionally needs `domain-modeling`). Neither is vendored here.
