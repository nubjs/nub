#!/usr/bin/env node
// @ts-check
// PreToolUse hook on the `Agent` tool. Two jobs in one place:
//   1) ENFORCE background dispatch — deny any Agent call lacking run_in_background:true
//      (a foreground agent blocks the orchestrator turn; a human interjection orphans it).
//   2) AUTO-APPEND an ORCHESTRATION EPILOGUE to every backgrounded sub-agent's prompt, so
//      sub-agents always hand back the next links in the chain (follow-ups / self-review /
//      push-to-CI / next-step). This is the multi-agent chaining pattern (2026-06-13:
//      the orchestrator often loses track of a sub-agent's role in a broader implementational plan).
// Run directly with node (no transpiler). Supersedes agent-must-be-background.sh.
// FAIL OPEN: any parse error → allow unmodified. A broken dispatch hook must never halt
// orchestration (the overnight heartbeat itself dispatches through here).
import { readFileSync, appendFileSync, existsSync } from 'node:fs';
import { loadConfig } from '../../scripts/fray/config.mjs';

const EPILOGUE = `

---
[ORCHESTRATION EPILOGUE — auto-appended by the dispatch hook] FULL DISPATCHED-SUB-AGENT CONTRACT: \`.github/subagent-guide.md\` in this repo — read it for your complete role (it covers everything below in depth). The few NON-NEGOTIABLES are inlined here in case you do not open it. Your final message IS the handoff — make it orchestration-ready: verdict/status; what you did; changed files/artifacts/clone-path/commit SHA when applicable; verification commands + their results; caveats/risks; one concrete next action. A bare "done" or progress-only final message is an INCOMPLETE handoff (a bug), not success.
THREAD WRITE-OWNERSHIP: if a \`THREAD: <slug>\` tag is at the top of this prompt, you OWN \`.fray/<slug>.md\` — edit it IN PLACE (the Edit tool) to reflect what you did: update \`## Status\` / \`## Decisions\` / \`## Next step\` / \`## Steps\`, keeping the single-voice current-truth discipline (no full-file rewrite, no changelog append — git holds the past). Write detailed artifacts (long traces/tables/write-ups) to a \`.fray/<slug>.findings/<id>.md\` sidecar. Edit ONLY your own dispatched thread, never another thread's \`.md\` or \`config.yml\`.
YOU OWN YOUR THREAD'S \`status:\` — SET IT YOURSELF to a VALID value; the orchestrator does NOT clean up after you. The ONLY allowed statuses are EXACTLY: \`todo · planned · enqueued · active · blocked · needs-decision · done · dismissed\` (any other word — "ready"/"landing"/"investigated"/"root-caused"/"complete" — is INVALID and breaks the board validator). When your work is genuinely complete, set \`status: done\` (or \`dismissed\` if decided-not-to-pursue); if it now needs a human call, \`needs-decision\`; if blocked on another in-flight thread, \`enqueued\` + \`depends_on:\`. Also update the 1-line \`status_text:\` to the current truth. Do NOT leave the thread \`active\` when you've finished — that strands it. (Per-agent liveness is DERIVED by the board from output freshness + the THREAD status; do NOT hand-maintain any per-agent \`status\` field in \`agents:\` — just set the THREAD status correctly.)
End your final report with a \`## Follow-ups\` section so the orchestrator can chain the next steps:
1. Concrete FOLLOW-UP work your findings/changes imply.
2. If you implemented something substantial → recommend a SELF-REVIEW pass (a fresh adversarial sub-agent reviewing your diff for correctness/regressions).
3. If you added/changed code or tests CI should exercise → recommend cutting a push to \`main\` + a CI-watch follow-up to confirm green.
4. The single most important NEXT STEP, and whether it needs maintainer sign-off (a default/security/product/brand/API-config-env call → recommend-only) or can proceed autonomously.
Your FINAL MESSAGE is your whole report to the orchestrator — there is no mid-run channel back to it, so put everything it needs to chain the next step in that final message.
If you COMMITTED: verify the tree COMPILES at your commit (a parallel agent may share a file — build before committing so you don't ship a broken HEAD). If there are no follow-ups, write "Follow-ups: none."
PUSH-THEN-EXIT (NO IN-AGENT CI WAITS, EVER): if you create a commit, \`git push\` it IMMEDIATELY (safe pre-CI — CI runs on the pushed commit), the instant it exists, BEFORE any verification wait or rest. NEVER arm a CI watcher / poll loop / \`sleep\`-on-CI / "wait for the notification" on CI: in-agent CI waits strand work and die at caps. Run any local verification in the FOREGROUND to completion, then push. If your deliverable is a PR push/merge, after pushing append one JSON line to \`.fray/merge-queue.jsonl\` (\`{"pr","sha","branch","thread","enqueued_at"}\`), report "pushed <sha>, awaiting CI", and EXIT — the orchestrator's heartbeat owns poll-CI → merge-on-green. Do NOT hold the agent open watching CI.`;

/**
 * Write the hook decision object and exit.
 * @param {unknown} obj
 * @returns {never}
 */
function emit(obj) {
  process.stdout.write(JSON.stringify(obj));
  process.exit(0);
}

try {
  const input = JSON.parse(readFileSync(0, 'utf8'));
  const ti = input.tool_input ?? {};
  const dir = process.env.CLAUDE_PROJECT_DIR ?? '.';

  // fray kill-switch — if disabled, allow the dispatch unmodified (no bg-enforce / epilogue / ledger).
  // Missing/unparseable config → loadConfig defaults to enabled (fail-safe).
  if (loadConfig(dir).enabled === false) emit({});

  if (ti.run_in_background !== true) {
    emit({
      hookSpecificOutput: {
        hookEventName: 'PreToolUse',
        permissionDecision: 'deny',
        permissionDecisionReason:
          'fray mode (hook-enforced): Agent sub-agents MUST be dispatched with run_in_background:true — never foreground/blocking. A foreground agent blocks the orchestrator turn and a human interjection orphans its work. Re-send this Agent call with run_in_background:true.',
      },
    });
  }

  const prompt = typeof ti.prompt === 'string' ? ti.prompt : '';

  // fray pointer-back: if the dispatch names a THREAD (a `THREAD: <name>` line the
  // orchestrator puts at the top of the prompt), log it to the dispatch ledger so the
  // orchestrator has a durable record of which thread each agent serves — survives
  // compaction. Fail open: a ledger error must never block a dispatch.
  const m = prompt.match(/^THREAD:\s*([\w./-]+)/m);
  const thread = m ? m[1].replace(/^\.fray\//, '').replace(/\.md$/, '') : null;
  if (thread) {
    // BULLETPROOF: a THREAD:-tagged dispatch whose .fray/<slug>.md does NOT exist is DENIED.
    // The thread file must be created FIRST (with current context) before any agent runs for it —
    // every new/split-off effort gets its file first, or it gets forgotten (2026-06-14).
    // (A genuine one-shot with no thread should carry no THREAD: tag.)
    if (!existsSync(`${dir}/.fray/${thread}.md`)) {
      emit({
        hookSpecificOutput: {
          hookEventName: 'PreToolUse',
          permissionDecision: 'deny',
          permissionDecisionReason:
            `fray (hook-enforced): dispatch is tagged \`THREAD: ${thread}\` but \`.fray/${thread}.md\` does NOT exist. CREATE THE THREAD FILE FIRST — write \`.fray/${thread}.md\` with all current context (Goal · Status · Decisions · Open questions · Steps · Next step), THEN re-send this dispatch. Every new or split-off effort gets its file BEFORE any agent runs for it. (If this is a true one-shot needing no thread, remove the \`THREAD:\` line from the prompt.)`,
        },
      });
    }
    try {
      appendFileSync(
        `${dir}/.fray/.dispatch-ledger.jsonl`,
        JSON.stringify({ ts: new Date().toISOString(), agent_type: ti.subagent_type ?? '', thread, reconciled: false }) + '\n',
      );
    } catch {
      /* fail open — never block a dispatch on the ledger */
    }
  }

  const updatedInput = prompt.includes('[ORCHESTRATION EPILOGUE') ? ti : { ...ti, prompt: prompt + EPILOGUE };

  emit({
    hookSpecificOutput: {
      hookEventName: 'PreToolUse',
      permissionDecision: 'allow',
      updatedInput,
    },
  });
} catch {
  emit({}); // fail open — allow unmodified
}
