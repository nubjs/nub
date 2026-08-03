#!/usr/bin/env node
// PreToolUse hook: guard `gh` comment/create verbs against verbose bodies.
//
// WHY: dispatched sub-agents do NOT inherit AGENTS.md / PROSE.md, so a prompt
// telling them "comments must be terse" is unenforced and routinely ignored —
// the #118/#120 fix-agents posted verbose, unreasonable essays AS the maintainer.
// Prompts demonstrably fail; this guard is the DETERMINISTIC floor. It blocks an
// over-long body on `gh issue comment` / `gh pr comment` / `gh issue create` /
// `gh pr create` and points the caller at the rule: an initial issue ack is
// EXACTLY "Investigating."; substantive comments are terse per PROSE.md. An
// explicit NUB_ALLOW_LONG_COMMENT=1 override prevents false-positive lockout for
// a genuinely-needed longer body (e.g. a detailed release note).
//
// Scope: ONLY the four comment/create verbs are guarded; any other `gh` call
// (and any non-gh Bash) passes through untouched. Crash-safe — on any parse
// problem it allows (fail-open), since a guard that blocks unrelated work is
// worse than a missed essay.

// Threshold: 700 chars. A terse substantive comment (a few sentences of facts,
// a short repro, a one-line resolution) fits comfortably under this; an essay
// does not. Picked as the midpoint of the ~600-800 band — high enough to never
// trip a normal factual comment, low enough to catch the multi-paragraph slop.
const MAX_BODY_CHARS = 700;

const REMINDER = [
  "GitHub comment tone (PROSE.md): factual, neutral, terse. No preamble, no essays,",
  "no editorializing. Initial ack of an external issue = EXACTLY \"Investigating.\".",
  "Never claim \"previous comments were wrong\" (they may be a bot's).",
].join(" ");

function readStdin() {
  return new Promise((resolve) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (c) => (data += c));
    process.stdin.on("end", () => resolve(data));
    process.stdin.on("error", () => resolve(""));
  });
}

// Allow the tool call (optionally with a non-blocking reminder on stderr).
function allow(reminder) {
  if (reminder) process.stderr.write(reminder + "\n");
  process.exit(0);
}

// Block with a deny decision + reason shown to the agent.
function deny(reason) {
  process.stdout.write(
    JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: reason,
      },
    }) + "\n",
  );
  process.exit(0);
}

// Does the command invoke one of the guarded gh verbs? Robust to flags/spacing.
function isGuardedGhComment(cmd) {
  // gh <issue|pr> <comment|create>  — tokens may be separated by any whitespace.
  return /\bgh\b[\s\S]*?\b(issue|pr)\b[\s\S]*?\b(comment|create)\b/.test(cmd)
    && /\bgh\b\s+(issue|pr)\s+(comment|create)\b/.test(cmd.replace(/\s+/g, " "));
}

// Extract the body text from the command, covering --body/-b, --body-file/-F,
// and heredocs. Returns { body, fromFile } — fromFile means the length can't be
// statically known from the command string.
function extractBody(cmd) {
  // --body-file / -F <path>  → body lives in a file; we can't measure it here.
  if (/(--body-file|(?<![\w-])-F)(\s+|=)/.test(cmd)) {
    return { body: null, fromFile: true };
  }

  // Collect EVERY body candidate, not just the first — gh uses the LAST --body
  // when several are passed, so a short-then-long pair (`--body hi --body <essay>`)
  // would slip past a first-match-only check. Measure the longest of all forms.
  const candidates = [];

  // --body="..." / --body '...' / -b "..."  (quoted) — all occurrences.
  for (const m of cmd.matchAll(
    /(?:--body|(?<![\w-])-b)(?:\s+|=)(['"])([\s\S]*?)\1/g,
  )) {
    candidates.push(m[2]);
  }

  // --body=unquoted-token / -b=token  (single shell word) — all occurrences.
  for (const m of cmd.matchAll(/(?:--body|(?<![\w-])-b)=(\S+)/g)) {
    candidates.push(m[1]);
  }

  // heredoc: --body "$(cat <<'EOF' ... EOF)" or a bare <<EOF ... EOF block.
  for (const m of cmd.matchAll(
    /<<-?\s*(['"]?)(\w+)\1\s*\n([\s\S]*?)\n\s*\2\b/g,
  )) {
    candidates.push(m[3]);
  }

  if (candidates.length === 0) return { body: null, fromFile: false };

  // Return the LONGEST candidate — whichever body would trip the limit.
  let longest = "";
  for (const c of candidates) if (c.length > longest.length) longest = c;
  return { body: longest, fromFile: false };
}

async function main() {
  const raw = await readStdin();
  let payload;
  try {
    payload = JSON.parse(raw);
  } catch {
    allow(); // unparseable input — fail open
    return;
  }

  const cmd = payload?.tool_input?.command;
  if (typeof cmd !== "string" || !cmd.trim()) {
    allow();
    return;
  }

  if (!isGuardedGhComment(cmd)) {
    allow(); // unrelated gh / non-gh command — untouched
    return;
  }

  // From here on it IS a guarded gh comment/create verb.
  if (process.env.NUB_ALLOW_LONG_COMMENT === "1") {
    allow(REMINDER + " [NUB_ALLOW_LONG_COMMENT=1 — length check bypassed]");
    return;
  }

  const { body, fromFile } = extractBody(cmd);

  // Can't measure a --body-file / piped body statically: surface the reminder
  // but don't block (the body may well be terse; blocking blind is over-reach).
  if (fromFile || body == null) {
    allow(REMINDER);
    return;
  }

  if (body.length > MAX_BODY_CHARS) {
    deny(
      `Blocked: GitHub comment body is ${body.length} chars (limit ${MAX_BODY_CHARS}). ` +
        REMINDER +
        " Rewrite it to be as MINIMAL as possible — the fewest words that carry the facts. Do NOT just trim to under the limit; cut until nothing remains that isn't a fact. Set NUB_ALLOW_LONG_COMMENT=1 only if a genuinely longer body is warranted (e.g. a detailed release note).",
    );
    return;
  }

  allow(REMINDER);
}

main();
