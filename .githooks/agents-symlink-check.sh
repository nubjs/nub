#!/bin/sh
# AGENTS.md must stay a symlink to wiki/agents.md — the thing that puts the
# entry-point document inside the knowledge graph.
#
# Shared by pre-commit and pre-push, and mirrored by a step in lat-check.yml.
# One file rather than three copies, because the three gates exist to say the
# same thing at different costs and a drifting copy would defeat that.
#
# WHY IT NEEDS GUARDING: the symlink makes AGENTS.md a regular-file/symlink pair
# for anything branched before the move, and git cannot auto-merge those types.
# A concurrent PR editing it hits `CONFLICT (distinct types)`, and the resolution
# has two failure directions. Restoring a regular file is the one a check can
# see, and it silently removes the entry point from the graph. The other —
# keeping the symlink and dropping the hunk — is invisible to every gate,
# because `lat check` is green when the content is simply absent; that one is
# covered by the resolution instruction in wiki/agents.md.
#
# Costs nothing: two filesystem calls, no npm, no network. Deliberately NOT
# covered by NUB_SKIP_LAT_CHECK, which exists to skip the ~6s graph scan — there
# is no cost here to opt out of. `git commit/push --no-verify` still skips it.

root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
[ -n "$root" ] || exit 0
[ -e "$root/wiki/agents.md" ] || exit 0   # not this repo's layout; nothing to assert

if [ ! -L "$root/AGENTS.md" ]; then
  echo "AGENTS.md is not a symlink." >&2
  echo "  It must stay a symlink to wiki/agents.md, or the entry-point document" >&2
  echo "  leaves the knowledge graph and stops being checked." >&2
  echo "  Resolving an AGENTS.md conflict? Keep the symlink and put the edit in" >&2
  echo "  wiki/agents.md — see the resolution note there." >&2
  exit 1
fi

target=$(readlink "$root/AGENTS.md")
if [ "$target" != "wiki/agents.md" ]; then
  echo "AGENTS.md points at '$target', expected 'wiki/agents.md'." >&2
  exit 1
fi

exit 0
