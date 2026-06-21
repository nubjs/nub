#!/usr/bin/env node
// @ts-check
/**
 * fray — the board + validator. There is NO stored board file: the board/status
 * view is COMPUTED ON DEMAND from the independent per-thread `.fray/<slug>.md`
 * files (the filename slug IS the thread id — the filesystem guarantees uniqueness,
 * so there is no `id` frontmatter field and nothing to dedupe) plus `.fray/config.yml`
 * (globals). Each thread's frontmatter is validated against the schema; the
 * `fray-reminder` hook runs `--validate` every turn so malformed frontmatter surfaces
 * to the orchestrator immediately.
 *
 * Usage:
 *   node scripts/fray/index.mjs               # print the LIVE board (active/enqueued/blocked/needs-decision only)
 *   node scripts/fray/index.mjs --all         # print all threads (every status)
 *   node scripts/fray/index.mjs --status todo # print only threads in one status
 *   node scripts/fray/index.mjs --validate    # print ONLY validation errors; exit 1 if any (for the hook / CI). --check is an alias.
 *   node scripts/fray/index.mjs --json        # machine-readable {config, threads, errors} — ALWAYS complete, never filtered
 *
 * Thread DEPENDENCIES are expressed entirely in per-thread frontmatter — an optional
 * `depends_on: [slug, ...]` array naming OTHER THREAD SLUGS (the same files the board
 * already scans; NOT an external registry). When every target is terminal (done/
 * dismissed) the board prints `▶ READY — dependencies clear, dispatch now`; otherwise
 * it lists the outstanding blockers. Computed on demand from the scanned statuses —
 * there is no stored dependency graph.
 */

import { readFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadConfig, STATUS, TERMINAL } from './config.mjs';
import { parseAgents } from '../../.claude/hooks/fray-agent-liveness.mjs';
import { deriveAgentState, findAgentOutputAge } from '../../.claude/hooks/fray-agent-status.mjs';

const PROJECT_DIR = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const FRAY_DIR = join(PROJECT_DIR, '.fray');

// STATUS/TERMINAL are imported from ./config.mjs — the single shared source the hooks
// also use, so the vocab can never drift between the tool and the reminder hook.
const REQUIRED = ['title', 'status']; // created / last_update are optional.

/**
 * Parse a YAML inline-array value (`[a, b, c]` or empty `[]`) into a string list.
 * Bare scalars (`a` / `"a"`) are tolerated and wrapped as a single-element list.
 * Self-contained by design: each entry is a THREAD SLUG that the board already
 * scans — `depends_on` references other thread files, never an external registry.
 * @param {string | undefined} raw
 * @returns {string[]}
 */
function parseList(raw) {
  if (!raw) return [];
  const inner = raw.trim().replace(/^\[|\]$/g, '');
  return inner
    .split(',')
    .map((s) => s.trim().replace(/^["']|["']$/g, ''))
    .filter(Boolean);
}

/**
 * Parse a top-of-file `--- … ---` YAML frontmatter block (flat `key: value` only).
 * @param {string} src
 * @returns {Record<string, string> | null}
 */
function frontmatter(src) {
  const m = src.match(/^---\n([\s\S]*?)\n---/);
  if (!m) return null; // no frontmatter at all
  /** @type {Record<string, string>} */
  const out = {};
  for (const line of m[1].split('\n')) {
    const kv = line.match(/^(\w[\w-]*):\s*(.*)$/);
    if (kv) out[kv[1]] = kv[2].trim().replace(/^["']|["']$/g, '');
  }
  return out;
}

/**
 * First non-blank line under `## Next step`, collapsed to one cell.
 * @param {string} src
 * @returns {string}
 */
function nextStep(src) {
  const lines = src.split('\n');
  const i = lines.findIndex((l) => /^##\s+Next step\s*$/i.test(l));
  if (i === -1) return '';
  for (let j = i + 1; j < lines.length; j++) {
    if (/^#{1,6}\s/.test(lines[j])) break;
    if (lines[j].trim()) return lines[j].trim();
  }
  return '';
}

/**
 * The full body text under a `## <heading>` section, up to the next heading of the
 * same-or-higher level. Matching is case-insensitive on the heading text. Returns ''
 * if the section is absent. Used by the stall-suspect check to ask "does this thread
 * have DECIDED content?" (a non-empty `## Decisions` that isn't just a placeholder).
 * @param {string} src
 * @param {string} heading  e.g. "Decisions" — matches `## Decisions` (and any deeper
 *   `### …` it contains; stops at the next `##`).
 * @returns {string}
 */
function section(src, heading) {
  const lines = src.split('\n');
  const re = new RegExp(`^##\\s+${heading}\\b`, 'i');
  const i = lines.findIndex((l) => re.test(l));
  if (i === -1) return '';
  const body = [];
  for (let j = i + 1; j < lines.length; j++) {
    if (/^##\s/.test(lines[j])) break; // next `##` section
    body.push(lines[j]);
  }
  return body.join('\n').trim();
}

/**
 * Does a `## Decisions` body carry REAL settled content vs an empty placeholder?
 * The fray convention is "none yet"/"none" for an empty Decisions section, so we
 * treat anything substantive beyond that placeholder as decided.
 * @param {string} body  the `## Decisions` section text
 * @returns {boolean}
 */
function hasDecidedContent(body) {
  if (!body) return false;
  const stripped = body.replace(/[*_`>#-]/g, '').trim().toLowerCase();
  if (!stripped) return false;
  // Pure placeholders the convention uses for an empty Decisions section.
  return !/^(none|none yet|n\/a|tbd)\.?$/.test(stripped);
}

/**
 * Does a thread's `## Next step` (its one crisp line) state a DEFER-REASON or a
 * BLOCKER — i.e. an explicit "why this isn't being dispatched right now"? This is
 * the false-positive guard: a legitimately-deferred `planned` thread (e.g.
 * security-scanner: "on hold per Colin, pick up post-v0.1.1") MUST NOT be flagged
 * as a drop-risk. Conservative by design — we look for the vocabulary of a stated
 * deferral/gate, NOT for the mere absence of a dispatch. Better to miss a real
 * drop-risk than to cry-wolf on a thread that says why it's parked.
 * @param {string} next  the `## Next step` line
 * @returns {boolean}
 */
function statesDeferOrBlocker(next) {
  if (!next) return false;
  return /\b(on hold|hold(ing)?|deferr?(ed|ing)?|defer|parked?|park|not now|later|post-v|pick up|picked up|awaiting|await|blocked|block(ing|ed)? on|waiting on|wait on|needs?[- ]decision|pending|until|once|after .+ (returns?|lands?|merges?|completes?)|colin|human)\b/i.test(next);
}

// .fray/config.yml globals — parsed by the shared, type-safe loadConfig.
// loadConfig() honors the FRAY env gate first (see config.mjs).
const cfg = loadConfig(PROJECT_DIR);

// When fray is disabled via FRAY=0 env, exit cleanly — this session opted out.
// (The board is rarely run with FRAY=0 in practice; the gate mainly governs hooks.)
if (!cfg.enabled) {
  console.log('fray: disabled this session (FRAY=0 in env — or enabled:false in .fray/config.yml)');
  process.exit(0);
}

const threads = readdirSync(FRAY_DIR)
  .filter((f) => f.endsWith('.md') && !f.startsWith('_')) // `_`-prefixed = non-thread meta (e.g. a stray _board.md)
  .sort()
  .map((f) => {
    const id = f.replace(/\.md$/, ''); // the filename slug IS the id
    const src = readFileSync(join(FRAY_DIR, f), 'utf8');
    const fm = frontmatter(src);
    /** @type {string[]} */
    const errors = [];
    if (!fm) {
      errors.push('no YAML frontmatter');
    } else {
      for (const k of REQUIRED) if (!fm[k]) errors.push(`missing required field: ${k}`);
      if (fm.status && !STATUS.includes(fm.status))
        errors.push(`invalid status "${fm.status}" (expected one of: ${STATUS.join(', ')})`);
    }
    const dependsOn = parseList(fm?.depends_on);
    const next = nextStep(src);
    const threadStatus = fm?.status ?? ''; // ACTIVE-ONLY flagging in deriveAgentState: flagged iff `active`
    // Agent liveness is DERIVED, never read from a stored per-agent flag: the binding
    // carries only immutable `{id, label}`; state comes from output-file age (ground
    // truth) + the thread's own status, via the SAME derivation the Stop hook uses.
    const agents = parseAgents(src).map((a) => ({
      ...a,
      state: deriveAgentState({ ageMin: findAgentOutputAge(a.id), threadStatus }),
    }));
    return {
      id,
      title: fm?.title ?? '',
      status: fm?.status ?? '?',
      statusText: fm?.status_text ?? '',
      next,
      dependsOn,
      agents,
      text: src,
      errors,
      /** @type {string[]} */ warnings: [],
      decided: hasDecidedContent(section(src, 'Decisions')),
      nextDefers: statesDeferOrBlocker(next),
    };
  });

// `depends_on` references other THREAD SLUGS — validate they resolve. A dangling
// slug (no matching `.fray/<slug>.md`) is a warning, surfaced like any frontmatter
// error so the orchestrator notices the stale dependency. Everything is COMPUTED
// from the scanned set; there is no external registry to consult.
const slugs = new Set(threads.map((t) => t.id));
const statusOf = new Map(threads.map((t) => [t.id, t.status]));
for (const t of threads) {
  for (const dep of t.dependsOn) {
    if (!slugs.has(dep)) t.errors.push(`depends_on references unknown thread "${dep}"`);
  }
}

/**
 * A thread's blockers: the subset of its `depends_on` targets not yet terminal.
 * Empty ⇒ all dependencies clear. Unknown slugs are skipped here (already an error).
 * @param {{ dependsOn: string[] }} t
 * @returns {string[]}
 */
function blockers(t) {
  return t.dependsOn.filter((dep) => slugs.has(dep) && !TERMINAL.includes(statusOf.get(dep) ?? '?'));
}

// ── Stall-suspect WARNINGS (drop-risk heuristics) ───────────────────────────────
// These are CONSERVATIVE warnings, NOT hard errors — they never fail `--validate`'s
// exit code (that stays gated on real frontmatter errors so the per-turn hook + CI
// don't break on a heuristic). They exist because a decided-and-ready thread was once
// parked as `planned` with no dispatch + no `depends_on` and silently DROPPED. The
// guard against crying-wolf: we fire only when there is NO stated defer-reason/blocker
// (so a legitimately-deferred thread like security-scanner — "on hold per Colin, pick
// up post-v0.1.1" — is NOT flagged). Self-contained: every signal is read off the
// thread's own frontmatter + section text; no external state.
for (const t of threads) {
  if (TERMINAL.includes(t.status)) continue; // terminal threads are done — never a drop-risk

  // (1) DROP-RISK: a `planned` thread that is DECIDED (has real `## Decisions` content)
  //     AND has no `depends_on` blocker AND whose `## Next step` states no defer-reason
  //     /blocker. That is the exact shape of the dropped thread — "decided but parked
  //     with nothing to un-defer it." Keyed on the ABSENCE of a defer-reason, NOT merely
  //     planned+decided, so a deliberately-held thread that SAYS why is exempt.
  if (t.status === 'planned' && t.decided && t.dependsOn.length === 0 && !t.nextDefers) {
    t.warnings.push('decided but not queued (active/enqueued?) — drop risk: `planned` + has Decisions, no depends_on, and Next step names no defer-reason/blocker');
  }

  // status_text is a 1-2 sentence English status note (frontmatter); flag overlong ones —
  // anything past ~2 sentences belongs in the body, not the at-a-glance board field.
  if (t.statusText && t.statusText.length > 280) {
    t.warnings.push(`status_text is ${t.statusText.length} chars — keep it to 1-2 sentences; move detail into the body`);
  }

  // Soft warning: non-terminal threads without a status_text have no at-a-glance board note.
  if (!t.statusText && t.id !== 'backlog') {
    t.warnings.push('no status_text — add a 1-2 sentence gloss of the current state (shown on the board as the » line)');
  }

  // (2) An EMPTY `## Next step` on a non-terminal thread — the board's "→" cell goes
  //     blank, so the thread has no stated next action and is easy to lose track of.
  //     `backlog` is the documented parking-lot (a curated list, not a single-effort
  //     thread), so it legitimately has no `## Next step` — exempt it.
  if (!t.next && t.id !== 'backlog') {
    t.warnings.push('empty `## Next step` — no stated next action (the board "→" cell is blank)');
  }
}

const allErrors = threads.filter((t) => t.errors.length).map((t) => `  ${t.id}.md: ${t.errors.join('; ')}`);
const allWarnings = threads.filter((t) => t.warnings.length).map((t) => `  ${t.id}.md: ${t.warnings.join('; ')}`);

if (process.argv.includes('--validate') || process.argv.includes('--check')) {
  // Warnings print but DO NOT affect the exit code — they're conservative drop-risk
  // heuristics, not schema errors. Only real frontmatter errors fail the hook/CI.
  if (allWarnings.length) console.error(`fray drop-risk WARNINGS (advisory, non-fatal):\n${allWarnings.join('\n')}`);
  if (allErrors.length) {
    console.error(`fray frontmatter validation FAILED:\n${allErrors.join('\n')}`);
    process.exit(1);
  }
  console.log(`fray frontmatter OK${allWarnings.length ? ` (${allWarnings.length} drop-risk warning${allWarnings.length === 1 ? '' : 's'} above)` : ''}`);
  process.exit(0);
}

if (process.argv.includes('--json')) {
  const dump = threads.map(({ text, ...t }) => {
    const b = blockers(t);
    return { ...t, blockers: b, ready: t.dependsOn.length > 0 && b.length === 0 };
  });
  console.log(JSON.stringify({ config: cfg, threads: dump, errors: allErrors, warnings: allWarnings }, null, 2));
  process.exit(0);
}

// Substring search across id + title + body — find a thread when you can't recall its slug.
const qi = process.argv.indexOf('--search');
if (qi !== -1) {
  const q = (process.argv[qi + 1] ?? '').toLowerCase();
  const hits = threads.filter((t) => `${t.id} ${t.title} ${t.text}`.toLowerCase().includes(q));
  console.log(
    hits.length
      ? hits.map((t) => `${t.id} [${t.status}] — ${t.title}`).join('\n')
      : `no threads match "${q}"`,
  );
  process.exit(0);
}

// Default: the board. `--status <s>` narrows to one status. `--all` shows everything.
const si = process.argv.indexOf('--status');
const only = si !== -1 ? process.argv[si + 1] : null;
const showAll = process.argv.includes('--all');
if (only && !STATUS.includes(only)) {
  console.error(`unknown status "${only}" (expected one of: ${STATUS.join(', ')})`);
  process.exit(2);
}

// Statuses hidden from the default board (non-actionable). Read defensively from
// config.mjs STATUS so we're correct regardless of a `planned`→`todo` rename.
const HIDDEN_BY_DEFAULT = new Set(['todo', 'planned', 'done', 'dismissed'].filter((s) => STATUS.includes(s)));

// When `--all` or `--status <s>` is given, show the requested set; otherwise show
// only the live/actionable statuses.
const showStatuses = only
  ? [only]
  : showAll
    ? STATUS
    : STATUS.filter((s) => !HIDDEN_BY_DEFAULT.has(s));

const out = [];
out.push(`fray board — autonomous_mode: ${cfg.autonomousMode ? 'on' : 'off'}${only ? ` — status:${only}` : showAll ? ' — all' : ' — live'}`);
if (allErrors.length) out.push(`\n⚠ VALIDATION ERRORS:\n${allErrors.join('\n')}`);
if (allWarnings.length) out.push(`\n⚠ DROP-RISK WARNINGS (advisory):\n${allWarnings.join('\n')}`);
for (const s of showStatuses) {
  const group = threads.filter((t) => t.status === s);
  if (!group.length) continue;
  out.push(`\n## ${s} (${group.length})`);
  for (const t of group) {
    out.push(`- ${t.id} — ${t.title}`);
    if (t.statusText) out.push(`    » ${t.statusText}`);
    out.push(`    → ${t.next}`);
    if (t.dependsOn.length) {
      const b = blockers(t);
      out.push(b.length
        ? `    ⏳ blocked on: ${b.join(', ')}`
        : `    ▶ READY — dependencies clear, dispatch now`);
    }
    // Dispatched-agent liveness — DERIVED (output-file age + thread status), never stored.
    for (const a of t.agents) {
      if (a.state === 'fresh' || a.state === 'terminal' || a.state === 'unknown') continue;
      const who = `${a.label ? `${a.label} ` : ''}[${a.id.slice(0, 9)}]`;
      out.push(a.state === 'unreconciled'
        ? `    ⚠ UNRECONCILED agent ${who} — output stale, thread non-terminal; fold + reconcile`
        : `    ⚠ IDLE agent ${who} — no recent output; poke or confirm mid-build`);
    }
    for (const w of t.warnings) out.push(`    ⚠ ${w}`);
  }
}
const unknown = threads.filter((t) => !STATUS.includes(t.status));
if (unknown.length) out.push(`\n## (invalid status) (${unknown.length})\n${unknown.map((t) => `- ${t.id} [${t.status}]`).join('\n')}`);

// Footer: when threads are hidden in the default view, tell the user how many.
if (!only && !showAll) {
  const hiddenCount = threads.filter((t) => HIDDEN_BY_DEFAULT.has(t.status)).length;
  if (hiddenCount > 0) {
    const hiddenLabels = [...HIDDEN_BY_DEFAULT].filter((s) => threads.some((t) => t.status === s)).join('/');
    out.push(`\n… ${hiddenCount} hidden (${hiddenLabels}) — \`--all\` to show`);
  }
}

console.log(out.join('\n'));
