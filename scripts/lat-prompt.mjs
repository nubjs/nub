// Both agents accept this UserPromptSubmit shape. Keep the hook dependency-free:
// installing packages or embedding the graph must not delay a user's prompt.
process.stdout.write(JSON.stringify({
  hookSpecificOutput: {
    hookEventName: "UserPromptSubmit",
    additionalContext: [
      "For nontrivial Nub work, read .claude/skills/lat-md/SKILL.md and search the knowledge graph before designing or editing.",
      "Use the Lat MCP tools lat_search and lat_section. If unavailable, run `nub run lat search \"query\"` and `nub run lat section \"section-id\"` from the repository root; do not assume bare `lat` is installed.",
      "On a fresh checkout, run `nub run lat:index` once before searching to select local embeddings and build the index. Search refreshes changed sections afterward.",
      "After meaningful behavior, architecture, or test changes, update the relevant wiki sections and run `nub run lat:check`. Keep private plans and research in internal/; Lat searches wiki/ only.",
    ].join("\n"),
  },
}) + "\n");
