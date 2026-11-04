#!/usr/bin/env node
// PreCompact hook: steer WHAT SURVIVES a context compaction.
//
// WHY: a long session compacts down to a summary a fraction of its size, and the first thing lost is
// the HIGH-LEVEL approach — the plan, the alternatives already ruled out, the reasoning — leaving a
// session that remembers which files it edited but not why, and that re-litigates settled decisions
// or re-walks dead ends. The compaction summary is the only thing carried across, so it is worth
// spending words on. This hook supplies the editorial brief for it.
//
// THE CHANNEL — unusual, so do not "fix" this to emit JSON: a PreCompact hook's PLAIN STDOUT becomes
// the compaction instructions. Claude Code collects the trimmed stdout of every succeeding PreCompact
// hook, joins them, and splices the result into the summarization prompt as a literal
// `Additional Instructions:` section. Emitting `hookSpecificOutput` JSON (the shape most other hook
// events want) would hand the summarizer a blob of JSON as its brief.
//
// WORD_TARGET is measured, and the response to it is NON-MONOTONIC — do not raise it without
// re-measuring. Four branches forked from one identical 345,898-token session, each compacted:
//
//   no hook (control)                    post-compaction context  4,060 tokens
//   qualitative "prefer a long summary"                           4,429   (+9%, i.e. noise)
//   explicit ">= 15,000 words"                                   20,339   (5.0x the control)
//   explicit ">= 35,000 words"                                    5,182   (WORSE, slower, dearer)
//
// Adjectives do nothing; a concrete number does the work; a number the model cannot reach backfires
// and degrades compliance. The cost is latency — the 15,000-word run's summarization took 8.6 min. On
// an `auto` trigger that is largely hidden when precomputed compaction is on (the summary is generated
// in a background sidecar before the limit is reached); a manual /compact pays it up front.
//
// CEILING: the summary is ONE model response, so it is bounded by max output tokens (~48k words). No
// prompt gets a summary materially larger than that.
//
// TONE: an instruction that reads like injected prompt-hijacking gets REFUSED — a probe that demanded
// a sentinel token produced a summary that declined and called its own instructions fake. Keep this
// legible as an ordinary editorial brief about what to retain.
import { readFileSync } from "node:fs";

const WORD_TARGET = 15000;

let input = {};
try {
  input = JSON.parse(readFileSync(0, "utf8"));
} catch {
  // No stdin / not JSON — fall through and emit the brief anyway. A missing brief is the bad outcome.
}

// Sub-agents compact independently; leave those alone rather than steering a child's summary with the
// parent's brief.
if (input.agent_id ?? input.agentId) process.exit(0);

// When a dispatching harness supplies its own compaction brief (it sets FRAY_UI_THREAD and registers
// its own PreCompact hook), stay inert. Both hooks' stdout would otherwise be CONCATENATED into one
// `Additional Instructions:` section, giving the summarizer two overlapping and possibly conflicting
// word targets.
if ((process.env.FRAY_UI_THREAD ?? "").trim()) process.exit(0);

const brief = [
  "Three things matter more in this summary than brevity does, because they are what actually gets",
  "lost in compaction:",
  "",
  `1. LENGTH. Write an EXHAUSTIVE summary, not a condensed one: target at least ${WORD_TARGET.toLocaleString("en-US")} words.`,
  "There is ample room for it, so do not compress. A short summary is the failure mode here — omitting",
  "something load-bearing is far worse than including something redundant.",
  "",
  "2. THE HIGH-LEVEL APPROACH, AT HIGH FIDELITY. Preserve the shape of the work, not just its",
  "mechanics: what problem is being solved, the approach chosen, the approaches considered and REJECTED",
  "and why, and the reasoning that led there. Reproduce that in full rather than condensing it to a",
  "sentence — a summary that lists edits but loses the plan leaves the next turn unable to judge",
  "whether a step is still the right one, and prone to re-walking dead ends already ruled out. Include",
  "substantial verbatim excerpts of the code, commands, and output that matter rather than describing",
  "them from memory.",
  "",
  "Carry forward, in as much detail as you can:",
  "- The task list and its state — every item, marked done / in progress / not started / blocked.",
  "- Decisions made and the rationale for each, especially any the maintainer made, approved, or reversed.",
  "- Constraints, conventions, and explicit maintainer instructions or corrections — in their own",
  "  wording where you can, since paraphrase is where intent gets lost.",
  "- Paths of every file created, read, or modified, and what changed in each.",
  "- Any issue and PR numbers in play, what each covers, and exactly where each change sits: not yet",
  "  opened / PR open awaiting review / merged to main / on a published release. Those are different",
  "  states and conflating them produces false completion claims.",
  "- Commands run (build, fmt, clippy, test, benchmarks) and their OBSERVED results — not the results",
  "  you expected.",
  "- What has been VERIFIED by actually running it LOCALLY versus what is merely believed to work or",
  "  was left to CI. Keep that distinction explicit; it is routinely lost in compaction and its loss",
  "  is what causes a change to be reported as done when it was never exercised.",
  "- Open questions, known failures, dead ends already ruled out, and anything left unfinished.",
  "",
  '3. RE-GROUNDING. End the summary with a final section headed exactly "Re-grounding before',
  'continuing:" that says what the next turn must re-establish BEFORE it asserts anything or resumes',
  "editing. Make it concrete and specific to this session, not generic advice:",
  "- Name the specific files to re-read before describing or changing them, rather than relying on",
  "  remembered contents.",
  "- Name any verification that was in flight and should be re-run rather than assumed.",
  "- Restate the issue/PR numbers involved and what still has to be true before each can land.",
  "- Flag anything believed but never confirmed, so it is not carried forward as established fact.",
  "- State the single next action to take.",
].join("\n");

process.stdout.write(`${brief}\n`);
