// @ts-check
/**
 * fray — the SHARED, type-safe config + vocab module. Every fray hook
 * (.claude/hooks/*.mjs) and the board tool (scripts/fray/index.mjs) import from
 * here, so there is exactly ONE source of truth for: the config schema + parse,
 * and the thread-status vocabulary.
 *
 * Dependency-free by design (no `yaml` package): Node ships no built-in YAML
 * parser, and fray must stay portable + runnable by bare `node` with zero install.
 * We hand-parse the SMALL, FLAT shape of `.fray/config.yml` (top-level scalars
 * plus the one nested `state:` block) — not a general YAML parser, just enough.
 */

import { readFileSync } from 'node:fs';
import { join } from 'node:path';

/**
 * The thread-status vocabulary.
 * - `todo` — not started; no agent dispatched, nothing blocking it.
 * - `enqueued` — READY to run (work fully scoped + decided) but deliberately held
 *   until a NAMED in-flight agent/thread completes — a sequencing dependency
 *   (same-file serialization, or it needs the prior agent's output). Distinct from
 *   `blocked`: an `enqueued` thread has a concrete auto-trigger (agent X returns →
 *   dispatch it), it is NOT waiting on a human/decision. The thread's `## Next step`
 *   must name the agent/thread it is waiting on. PREFER messaging the in-flight
 *   agent to fold the work in over enqueuing-then-dispatching, when the work fits
 *   that agent's scope (see the fray skill — steer-in-flight beats spawn-fresh).
 * - `blocked` — cannot proceed; waiting on a human decision, an answer, or an
 *   external event with no in-session auto-trigger.
 * - `needs-decision` — surfaced a question the human owns; recommend-only until answered.
 * - `planned` — scoped AND **deliberately DEFERRED** (a human/orchestrator chose
 *   "not now"). NOT a dumping ground for decided-ready work: the `## Next step` MUST
 *   state WHY it's deferred and what un-defers it (e.g. "on hold per Colin, pick up
 *   post-v0.1.1"). Distinct from `todo` ("could start now, just hasn't") and
 *   `needs-decision` (gated on a human call). THE INVARIANT: a thread leaving
 *   `needs-decision` (just decided) transitions to `active` (dispatch this turn) or
 *   `enqueued` (`depends_on` a blocker) — NEVER `planned`, unless deliberately
 *   deferred WITH a stated reason. "Decided-and-ready" is never `planned`.
 * - `done` / `dismissed` — TERMINAL (completed / decided-against): kept, never
 *   deleted, excluded from the active board's pending views.
 * @type {readonly string[]}
 */
export const STATUS = ['todo', 'planned', 'enqueued', 'active', 'blocked', 'needs-decision', 'done', 'dismissed'];

/**
 * The terminal subset of {@link STATUS}: completed OR decided-against. Both are
 * kept on disk and both are excluded from the pending/board views.
 * @type {readonly string[]}
 */
export const TERMINAL = ['done', 'dismissed'];

/**
 * @typedef {Object} FrayConfig
 * @property {boolean} enabled       Master kill-switch. `false` makes all fray hooks no-op. Default `true` (fail-safe — a botched config never silently disables orchestration).
 * @property {boolean} autonomousMode  Whether autonomous mode is on. Default `false`.
 * @property {Record<string, string>} state  The `state:` block — cross-cutting "what's true now" globals. Default `{}`.
 */

/**
 * The type-safe DEFAULTS, returned when `.fray/config.yml` is absent. Individual
 * malformed lines are simply skipped (we keep whatever parsed), so a partially
 * broken file still yields a fully-populated config.
 * @returns {FrayConfig}
 */
function defaults() {
  return { enabled: true, autonomousMode: false, state: {} };
}

/**
 * Coerce a YAML-ish scalar to a boolean. Accepts the YAML 1.1 truthy/falsey
 * spellings fray actually uses (`true`/`false`, `on`/`off`, `yes`/`no`).
 * Anything else returns `fallback` so an unparseable value can't flip a default.
 * @param {string} raw
 * @param {boolean} fallback
 * @returns {boolean}
 */
function toBool(raw, fallback) {
  const v = raw.trim().toLowerCase();
  if (v === 'true' || v === 'on' || v === 'yes') return true;
  if (v === 'false' || v === 'off' || v === 'no') return false;
  return fallback;
}

/**
 * Strip surrounding single/double quotes and trailing inline `# …` comments.
 * @param {string} raw
 * @returns {string}
 */
function scalar(raw) {
  // Drop an inline comment only when the `#` is preceded by whitespace (so a `#`
  // inside a quoted value or a bare token isn't clobbered). Then trim + unquote.
  let v = raw.replace(/\s+#.*$/, '').trim();
  return v.replace(/^["']|["']$/g, '');
}

/**
 * Read + parse `.fray/config.yml` from `projectDir` into a fully-populated,
 * type-safe {@link FrayConfig}. The file is absent/unreadable → DEFAULTS.
 * A single malformed line → that line is skipped; everything else still parses.
 *
 * SESSION-LOCAL OVERRIDE (checked first, before any file read): `process.env.FRAY`
 * lets a session opt in/out at launch time without touching `config.yml`:
 *   - `FRAY=0` / `FRAY=false` → fray disabled this session (enabled: false).
 *   - `FRAY=1` / `FRAY=true`  → fray enabled this session (enabled: true).
 *   - unset / any other value → fall back to `.fray/config.yml` (today's behavior).
 * Set it when launching claude: `FRAY=0 claude`. CC hooks inherit the claude process
 * env, so the setting is session-wide and independent per terminal / session.
 * NOTE: a mid-session toggle is NOT supported via tool calls — Bash env changes
 * don't persist across tool invocations and don't reach hook processes. A mid-session
 * toggle would require a session_id-keyed sentinel file; that is a possible future add-on.
 *
 * Parser shape (intentionally narrow — matches fray's flat config, NOT general YAML):
 *   - `key: value`         top-level scalar (e.g. `enabled: true`, `autonomous_mode: off`)
 *   - `state:`             opens the one nested block
 *     `  key: "value"`     two-space-indented entries become `state[key] = value`
 *   - `# …` lines + blanks are ignored.
 *
 * @param {string} projectDir  The repo root (e.g. `process.env.CLAUDE_PROJECT_DIR`).
 * @returns {FrayConfig}
 */
export function loadConfig(projectDir) {
  const cfg = defaults();

  // SESSION-LOCAL ENV GATE — takes precedence over any config file.
  const frayEnv = (process.env.FRAY ?? '').trim().toLowerCase();
  if (frayEnv === '0' || frayEnv === 'false') {
    cfg.enabled = false;
    return cfg;
  }
  if (frayEnv === '1' || frayEnv === 'true') {
    cfg.enabled = true;
    // Don't return early — still parse the file to pick up autonomousMode + state.
  }

  let src;
  try {
    src = readFileSync(join(projectDir, '.fray', 'config.yml'), 'utf8');
  } catch {
    return cfg; // absent / unreadable → type-safe defaults
  }

  let inState = false;
  for (const line of src.split('\n')) {
    if (!line.trim() || line.trim().startsWith('#')) continue; // blank / comment

    // A nested `state:` entry: two-or-more leading spaces + `key: value`.
    const nested = line.match(/^[ \t]+([\w-]+):\s*(.*)$/);
    if (inState && nested) {
      cfg.state[nested[1]] = scalar(nested[2]);
      continue;
    }

    // A top-level `key: value` (or bare `key:` opening a block).
    const top = line.match(/^([\w-]+):\s*(.*)$/);
    if (!top) continue; // malformed → skip this line, keep parsing

    const key = top[1];
    const val = top[2];

    if (key === 'state') {
      inState = true; // open the nested block; `val` is empty for `state:`
      continue;
    }
    inState = false; // any other top-level key closes the state block

    // scalar() FIRST — strip any trailing inline `# …` comment before coercing,
    // else `autonomous_mode: on  # note` reads as garbage → silently falls back to
    // the default. (Bug found 2026-06-14: an inline comment flipped autonomous mode
    // back off. The nested `state:` entries already go through scalar(); the
    // top-level bools must too.)
    // When FRAY=1/true is set at launch, the session-local gate already fixed `enabled`;
    // don't let the config file override it. autonomousMode + state still come from the file.
    if (key === 'enabled' && frayEnv !== '1' && frayEnv !== 'true') cfg.enabled = toBool(scalar(val), cfg.enabled);
    else if (key === 'autonomous_mode') cfg.autonomousMode = toBool(scalar(val), cfg.autonomousMode);
    // unrecognized top-level keys are ignored by design (forward-compatible)
  }

  return cfg;
}
