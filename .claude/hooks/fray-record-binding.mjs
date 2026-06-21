#!/usr/bin/env node
// @ts-check
/**
 * fray — PostToolUse hook on the `Agent` tool. Auto-records the agent→thread BINDING so the
 * orchestrator never hand-writes `agents:` frontmatter again (the last hand-maintained bit of
 * the derive-don't-store system).
 *
 * WHEN it fires: PostToolUse on `Agent`. For a `run_in_background:true` dispatch the tool
 * RESULT is the synchronous "Async agent launched successfully…" LAUNCH ACK — i.e. PostToolUse
 * fires at DISPATCH time, and the launched agent's id IS in that ack. EMPIRICALLY VERIFIED
 * against real session transcripts (2026-06-21): the Agent tool_use at T+0ms and its
 * tool_result (carrying `agentId: <id>` + `output_file: …/tasks/<id>.output`) at T+118ms — the
 * ack is immediate, NOT deferred to completion. (The docs don't document the Agent tool_response
 * shape, so this was confirmed from transcripts, not assumed.) So {thread (from prompt), agentId
 * (from result), label (from description)} are all knowable in this one event — no SubagentStart
 * correlation or transcript-read needed.
 *
 * WHAT it does: if `tool_input.prompt` carries a `THREAD: <slug>` tag and the result yields an
 * agentId, two in-place frontmatter edits on `${CLAUDE_PROJECT_DIR}/.fray/<slug>.md`:
 *   1. APPEND `{id, label}` to the `agents:` array (create the array if absent; DEDUPE by id).
 *      label = the dispatch `description`. NO per-agent `status:` is written — binding only, per
 *      derive-don't-store (agent liveness is DERIVED from output-file mtime + thread status).
 *   2. SET the thread `status: active` — a fresh THREAD-tagged dispatch means an agent is now
 *      working it, and `active` is the invariant the liveness derivation flags on. (Active-only
 *      flagging: a stale agent is surfaced iff the thread is `active`.) This auto-flip ONLY
 *      happens on a fresh THREAD-tagged Agent dispatch — NOT on a warm-resume (SendMessage) and
 *      NOT on an untagged dispatch; for those the orchestrator must set `active` by hand (see the
 *      fray SKILL.md labeling discipline).
 *
 * CONCURRENCY: read-modify-write of the thread .md. Single hook process per dispatch; the only
 * other writer of `agents:`/`status:` is a human/agent editing the same file rarely — each edit
 * re-reads right before writing and rewrites only its own single line (the `agents:` line and the
 * `status:` line), so a duplicate can't be introduced and a concurrent body edit isn't clobbered.
 *
 * FAIL-OPEN, ABSOLUTELY: any parse/IO error → no-op (emit {}/exit 0), NEVER block or alter the
 * dispatch. A binding-recorder must never disrupt orchestration.
 */
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';

/** No-op allow + exit. @returns {never} */
function done() {
  process.stdout.write('{}');
  process.exit(0);
}

/**
 * Extract the agentId from an Agent tool result. The result may be a string, or a content
 * array of `{type:'text', text}` parts (the real shape). We search the combined text for
 * `agentId: <id>` first, then fall back to `tasks/<id>.output`. Returns null if none.
 * @param {unknown} result
 * @returns {string|null}
 */
function extractAgentId(result) {
  let text = '';
  if (typeof result === 'string') text = result;
  else if (Array.isArray(result)) {
    for (const part of result) {
      if (part && typeof part === 'object' && typeof (/** @type {any} */ (part).text) === 'string') {
        text += (/** @type {any} */ (part).text) + '\n';
      }
    }
  } else if (result && typeof result === 'object') {
    // Some shapes nest under {content:[…]} — handle defensively.
    const c = /** @type {any} */ (result).content;
    if (typeof c === 'string') text = c;
    else if (Array.isArray(c)) {
      for (const part of c) {
        if (part && typeof part === 'object' && typeof part.text === 'string') text += part.text + '\n';
      }
    } else {
      text = JSON.stringify(result);
    }
  }
  if (!text) return null;
  // Prefer the explicit `agentId: <id>` line; fall back to the output_file path.
  const m = text.match(/agentId:\s*([A-Za-z0-9][A-Za-z0-9_-]{6,})/) || text.match(/tasks\/([A-Za-z0-9][A-Za-z0-9_-]{6,})\.output/);
  return m ? m[1] : null;
}

/**
 * Append `{id, label}` to the thread file's single-line `agents:` array, deduping by id. The
 * board/hook parser (`parseAgents`) expects a single-line `agents: [ {id: X, label: "Y"}, … ]`,
 * so we keep it single-line. If no `agents:` line exists, insert one inside the frontmatter
 * (right after the `status:`/`status_text:` block, before the closing `---`). Returns true if a
 * write happened.
 * @param {string} path
 * @param {string} id
 * @param {string} label
 * @returns {boolean}
 */
function appendBinding(path, id, label) {
  let src;
  try {
    src = readFileSync(path, 'utf8');
  } catch {
    return false;
  }
  // Only operate within the leading frontmatter block.
  const fm = src.match(/^---\n([\s\S]*?)\n---/);
  if (!fm) return false;

  const escLabel = `"${String(label).replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`;
  const obj = `{id: ${id}, label: ${escLabel}}`;

  const agentsLine = src.match(/^agents:\s*\[([\s\S]*?)\]\s*$/m);
  if (agentsLine) {
    // DEDUPE: if this id is already bound, no-op.
    if (new RegExp(`\\bid:\\s*${id}\\b`).test(agentsLine[0]) || new RegExp(`(^|[\\[,\\s])${id}([,\\]\\s])`).test(agentsLine[1])) {
      return false;
    }
    const inner = agentsLine[1].trim();
    const next = inner ? `agents: [${inner}, ${obj}]` : `agents: [${obj}]`;
    const out = src.replace(agentsLine[0], next);
    if (out === src) return false;
    writeFileSync(path, out);
    return true;
  }

  // No agents: line — insert one as the last frontmatter field (before closing ---).
  const fmEnd = src.indexOf('\n---', 3); // first closing fence after the opening ---
  if (fmEnd < 0) return false;
  const out = src.slice(0, fmEnd) + `\nagents: [${obj}]` + src.slice(fmEnd);
  writeFileSync(path, out);
  return true;
}

/**
 * Set the thread's frontmatter `status: active` (in-flight agent work ⟹ active). Edits only the
 * single `status:` line within the leading frontmatter block; no-op if already `active` or if the
 * thread is already terminal (`done`/`dismissed` — a fresh dispatch onto a terminal thread is
 * unusual, but we don't want to silently reopen one without an explicit decision). Returns true if
 * a write happened. Fail-soft: re-reads right before writing so a concurrent body edit isn't lost.
 * @param {string} path
 * @returns {boolean}
 */
function setThreadActive(path) {
  let src;
  try {
    src = readFileSync(path, 'utf8');
  } catch {
    return false;
  }
  // Only operate within the leading frontmatter block.
  const fm = src.match(/^---\n([\s\S]*?)\n---/);
  if (!fm) return false;
  const statusLine = fm[1].match(/^status:\s*(\S+).*$/m);
  if (!statusLine) return false; // no status field → leave it for the validator to flag
  const cur = statusLine[1];
  if (cur === 'active') return false; // already active → no-op
  if (cur === 'done' || cur === 'dismissed') return false; // don't silently reopen a terminal thread
  // Replace only the status line's value, preserving any trailing comment/text after it.
  const replaced = statusLine[0].replace(/^status:\s*\S+/, 'status: active');
  const out = src.replace(statusLine[0], replaced);
  if (out === src) return false;
  writeFileSync(path, out);
  return true;
}

try {
  let input = {};
  try {
    const raw = readFileSync(0, 'utf8');
    if (raw.trim()) input = JSON.parse(raw);
  } catch {
    done();
  }

  // Only the Agent tool. (settings.json scopes this, but double-check defensively.)
  const toolName = input.tool_name ?? input.toolName ?? '';
  if (toolName && toolName !== 'Agent') done();

  const ti = input.tool_input ?? input.toolInput ?? {};
  const prompt = typeof ti.prompt === 'string' ? ti.prompt : '';
  const m = prompt.match(/^THREAD:\s*([\w./-]+)/m);
  if (!m) done(); // no THREAD tag → nothing to bind
  const thread = m[1].replace(/^\.fray\//, '').replace(/\.md$/, '');

  const result = input.tool_result ?? input.toolResult ?? input.tool_response ?? input.toolResponse;
  const agentId = extractAgentId(result);
  if (!agentId) done(); // can't determine the id → fail-open (no binding)

  const label = (typeof ti.description === 'string' && ti.description.trim()) || (typeof ti.subagent_type === 'string' && ti.subagent_type) || 'sub-agent';

  const dir = process.env.CLAUDE_PROJECT_DIR || process.cwd();
  const path = join(dir, '.fray', `${thread}.md`);
  if (!existsSync(path)) done(); // thread file gone → fail-open

  appendBinding(path, agentId, label); // best-effort; ignore false (already bound / no write)
  setThreadActive(path); // a fresh THREAD-tagged dispatch ⟹ an agent is now working it
  done();
} catch {
  done(); // fail-open — never disrupt the dispatch
}
