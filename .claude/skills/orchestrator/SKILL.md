---
name: orchestrator
description: Drive a multi-part effort to completion by dispatching, steering and verifying sub-agents — one cohesive epic, a batch of decided changes, or a coverage campaign over a population. Invoke (via the Skill tool) whenever you hold a goal set larger than one context and will delegate pieces of it: "drive this epic", "work through these fixes", "batch these changes", "measure all of X", or being handed a large multi-unit implementation. Carries the three dispatch PATTERNS (phased batch, worktree fan-out, relay) and the rule that picks between them — EDITS ARE CHEAP AND PARALLELISE, BUILDS ARE EXPENSIVE AND DO NOT. Supersedes the separate `batch-orchestrator` and `dynamic-orchestrator` skills. Pairs with `remote-build` (uncontended builds), `ad-hoc-test` (verification), `cpu-reduction` (clearing residue), `rust-build-hygiene` (not creating it).
---

# orchestrator

You hold the goal set, own the durable record, and dispatch sub-agents to execute pieces — reviewing, steering, spot-checking, integrating. **The patterns below are different shapes of that one job**; pick by the work's dependency structure, not by habit.

**Not a board of independent efforts.** Here every piece belongs to ONE goal. It reuses whatever machinery your harness already gives you — dispatch profiles, sub-agent messaging, worktrees, a merge queue — rather than defining its own.

**Only a top-level session orchestrates.** If you are yourself a dispatched sub-agent, you are one of the pieces, not the holder of the goal set — the repo-wide depth cap in `AGENTS.local.md` applies and you do not dispatch. Execute your scoped task inline and return; if it turns out to be a whole campaign, say so in your return and let your dispatcher run it.

## The record you own

One living doc **is** the effort — goals, architecture, resolved ambiguities with rationale, the to-do, status. Keep it behavior-level; a symbol-pinned to-do rots. Sub-agents get scoped tasks and **do not edit it**; you reflect each landed piece yourself. (They still merge scoped progress into the thread's own scratch notes — that is the standing exception.)

**DIRECTION FROM THE HUMAN GOES INTO THE RECORD IN THE SAME TURN IT ARRIVES.** Conversational memory is not a record: it dies at the next compaction and you revert to instinct. Measured — "stop chasing root causes, just grant what they need" was given, acted on for one turn, never written down, and reverted to within hours.

### §0 CANON — a persistent, non-editable core at the top

A long effort buries load-bearing *direction* under accreted findings, and compaction loses it. Give the doc a `§0 — CANON` section, marked persistent and non-editable.

- **In it:** the vision only — which products exist and how they differ, non-negotiable properties, the decided mechanism per platform *and the requirement that selected it*, banned patterns and why, standing invariants, the definition of done. **~100 lines**; too long to re-read every cycle means it will not be re-read.
- **Not in it:** findings, measurements, status, to-dos, history.
- **State these rules inside the section** so they outlive this skill: nothing in §0 changes except on explicit human instruction; if anything below contradicts §0, **§0 wins**; re-read §0 after every compaction and before every dispatch; a dispatch prompt whose premise contradicts §0 is a defect in the prompt, not a decision.

**The tell you have lost canon:** you are about to state something about the design and your confidence comes from recall rather than from having just read it.

## Pick the pattern

| pattern | when | shape |
|---|---|---|
| **Phased batch** | several small file-disjoint changes, one branch | N agents edit in one worktree → barrier → **you** build once → re-steer the same agents to verify |
| **Worktree fan-out** | long-lived independent lanes, different branch topology | each lane gets its own worktree + target; you merge and reconcile |
| **Relay** | one non-trivial piece | plan → implement → review → test, you verifying between stages |

**The rule that decides most of it: EDITS ARE CHEAP AND PARALLELISE; BUILDS ARE EXPENSIVE AND DO NOT.** N concurrent builds contend for CPU, disk, and cargo's target lock. Measured here: seven concurrent target dirs drove load to 104 on a 10-core box and one build died `No space left on device`; separately, lanes each building their own target took free disk from 42 GiB to 14 GiB in an afternoon. Every lane got slower, including the "parallel" ones.

**Serialize only the final assembly, never the editing.** The file tools reject stale edits, so concurrent edits to one worktree do not silently clobber. What collides is BUILDING (one target lock) and raw destructive git ops (`reset`/`checkout`/`stash`), which no batch agent should run.

### The phased batch, in full

1. **EDIT (parallel).** N agents, one worktree, disjoint file sets. Every prompt carries, in these words or better:

   > **This is an edit-only phase. Do NOT build, do NOT test, do NOT run cargo at all** — not even to check your work. Other agents are editing this same worktree and I run a single shared build after everyone finishes. Concurrent cargo invocations serialise on one target lock, so each of you would wait for the others. Make your edits, make them careful, and stop. Report the files you touched.

   Say **why**, not just what — a diligent agent builds "just to be sure" unless it understands the cost. The ban is on the target lock: `rustfmt --check` and anything avoiding cargo is fine.
   - **Assign file ownership explicitly**; tell each agent to stop and report rather than touching anything outside its own files.
   - **You are one of the editors — do not dispatch into a worktree you are editing yourself.**
   - **Never `git add -A` in a shared tree** — stage only your own paths, or a mid-edit sweep commits another agent's half-written files under your message.
   - **Keep the file→agent map.** You need it in phase 3.
2. **BARRIER.** A half-applied tree produces failures that belong to nobody. If an agent dies mid-edit, **resume it with `SendMessage`** — its partial work is in the worktree and its reasoning is in its transcript. Run `cargo fmt` yourself here.
3. **BUILD (once, yours).** Prefer `remote-build` — an ephemeral GCE spot VM runs the byte-identical CI invocation for cents, and takes the load off a contended host entirely. **Read the tool's own exit code**; `cargo … ; echo EXIT=$? ; tail` makes the *shell* exit 0 regardless. Gate on what the batch touched: `NUB_ALLOW_INCOMPLETE_RUNTIME=1 clippy --all-targets --all-features` (a scoped `-p` without `--all-targets` misses test-code lints; the env var is what CI's clippy job sets, since `--all-features` otherwise panics on an incompletely staged `runtime/`); `cd vendor/aube && cargo check --workspace --all-targets` with its own target dir if anything reaches aube (a dependent's `--all-targets` never builds a path dependency's test targets, and that gap has let a broken merge through); `cargo check -p <crate> --target x86_64-pc-windows-gnu` for `cfg(windows)` code, **confirming from the log that the crate compiled for that target**. Read `rust-build` — clippy and `cargo test` run on different profiles, so "one build" is two artifact universes, and neither leaves a runnable binary. **Triage failures against the phase-1 file map**; an error spanning two agents' files is a real interface disagreement — resume BOTH.
4. **VERIFY (parallel, warm-resumed).** **Verification means running the built binary and must stay compilation-free.** `cargo test` and `cargo clippy` are BUILDS. Give each agent the binary's path and a real fixture per `ad-hoc-test`, and say cargo is yours. If a change can only be proven by a Rust unit test, that test is an EDIT — written in phase 1. **Re-steer the SAME agents, never fresh ones:** the agent that wrote the change knows what it rejected and where the risk sits.

## Coverage campaigns — the failure that kills them

When the job is to COVER a population (measure N packages, audit N files, migrate N call sites), the thing that kills it is not difficulty. **Each unit of coverage surfaces something interesting, and interesting things get investigated.** Measured: 20 commits landed after a coverage-measuring harness was built and **2 of them added coverage** — the other 18 went to mechanisms, schemas, docs and controls, every one individually defensible.

- **A FINDING NEVER GETS A LANE.** It gets an entry in whatever the campaign produces — a catalog row, a fix, a backlog line — and you move on. **Lanes are for coverage.** Two exceptions: work that UNBLOCKS coverage (the harness cannot run on a platform), and a case whose failure indicts the tool rather than the subject.
- **Before dispatching anything, name the coverage number it moves.** No number, no lane.
- **Track that number where you will see it and report it FIRST.** If it did not move since the last check-in, that is the headline — not what was discovered.
- **Build the terminal fallback EARLY.** Where every unit can end with a known-good disposition (grant the whole disk, mark unsupported, accept the default), no finding *requires* investigation. It converts "this one is strange" from a research prompt into a one-line disposition, and it is what lets a campaign actually finish.

## The loop

1. **Read the record** — reload the goal set and dependency structure.
2. **Pick the next piece(s).** Judge serialize-vs-parallelize by the dependency structure *at this moment*: foundational/shared-surface work serializes, disjoint pieces parallelize. No hardcoded concurrency — making that call well is the whole point.
3. **Dispatch scoped sub-agents**, tiered to the right model/effort, with four standing instructions in every prompt:
   - run the quality loop: **build → ad-hoc fixture test → adversarially probe → distill durable checks into committed tests**;
   - **pause and surface real ambiguity upward** rather than guessing on a genuine fork;
   - comments sparse + dense; **name any skill it must load** (`prose-writing`, `impact-analysis`, `ad-hoc-test` — skills are NOT inherited);
   - **run builds/tests/waits in the FOREGROUND to completion — never background-a-build-and-rest.** A rested agent is not reliably re-woken by its own background task, so it strands itself with work uncommitted.
4. **Review every return like you mean it.** A sub-agent's "done" is a claim to verify — spot-check it. If work is incomplete, stubbed or off-goal, warm-resume and re-steer rather than accepting it.
5. **Facts you assert in a dispatch prompt need the same grounding as facts you tell the human.** A fresh-context agent cannot tell a checked claim from a remembered one and will act on both.
6. **Update the record and repeat.**

## Yours vs delegated

- **Yours:** the goal view, serialize/parallelize calls, the record, reviewing + spot-checking, steering, resolving/escalating ambiguities, integration.
- **Delegated:** implementation, breadth of ad-hoc testing, focused research, self-review lenses.

**Give a sub-agent an OUTCOME + context, not a prescribed internal structure** — an agent close to the material decomposes better than you guessing from outside. A sketched "one agent per X" from the human is a shape suggestion, not a spec. **Let results anneal** — iterate until new passes stop yielding signal. **React, don't batch-and-wait**: a return is an immediate input to the next decision.

### implement → TEST → self-review

The implementer runs the gates, then verifies by **RUNNING it** — an ad-hoc fixture sweep against a built binary, built to FALSIFY — then **reads its own diff in-thread**. In-thread because the implementer knows why each choice was made; a fresh reviewer re-derives that badly. A spawned reviewer is an ESCALATION for a change that earns it (wide blast radius, security posture, serialized format, memory/UB), not a default leg.

**Never let a review round substitute for the sweep.** Reviewers read code and hypothesize, so they miss the silent wrong answer — the resolution that returns a different module with no error — reachable only by executing and checking *which* file answered.

**Research and adversarial probing stay ordinary sub-agent work** — they need breadth you cannot hold and they do not build. Only the edit/build/verify cycle is what the batch pattern reshapes.

## Quality discipline

- A piece is not done until **build → ad-hoc test → probe → distilled tests** has run.
- **For an OS-level, runtime or UI feature, "verified end-to-end" means a REAL environment** — a VM (`gcloud-vm`) for OS-privilege/kernel/installer behavior, a real browser (`visual-review`) for UI. Capture evidence via `SendUserFile`; for a privilege-escalation or first-run flow, the screenshots ARE the deliverable.
- **Investigate open questions empirically.** Pull the actual corpus, build a prototype, run the real differential. High-fidelity empirical checks are cheap relative to shipping a wrong answer.

## Long-run hygiene

- **Reuse ONE warm target across serial builds.** A fresh private `CARGO_TARGET_DIR` pays a full cold dependency build (~40 min contended). Serial builds self-serialize on cargo's lock; only concurrent builds need separate targets. Let `scripts/rust-build.sh` pick the dir rather than exporting `CARGO_TARGET_DIR`.
- **Sweep build/test residue periodically** — a rising load floor with nothing building, a build hanging on `Blocking waiting for file lock`, or `~/.cache` filling: invoke `cpu-reduction`, and follow `rust-build-hygiene` so you stop creating it. **Pruning worktree CHECKOUTS reclaims almost nothing** (CoW clones); the `<name>-target` dirs are the consumers. Judge every reclaim by `df`, never `du`.
- **A "wait-looping" build agent is usually LOCK CONTENTION.** Check for `Blocking waiting for file lock on artifact directory` and `ps aux | grep rustc | grep <target>`. Usual cause: two builds on one target, often a stopped agent's DETACHED build that `TaskStop` did not reap. `pkill -f '<target-dir>'` (artifacts stay warm), then hand it to ONE fresh foreground agent.
- **Watch dev-loop durations.** A long duration you accept once, you pay every iteration. Shard across parallel workers, reuse a warm target/image, scope the build down to what changed, cross-compile on the host and scp rather than building on a VM.
- **Arm a fallback heartbeat when the human steps away** (`ScheduleWakeup`, re-armed each turn) so a hung sub-agent cannot end the thrust. It is a BACKSTOP — the primary wake is sub-agent completions (a live `Agent`-tool sub-agent; **never `spawn_thread`**, which reports to a sibling and never returns to you). **You can always see a result WITHOUT a notification:** reconcile directly against `git log`/`git status`, the agent's `tasks/<id>.output` mtime and bounded tail, and `ps`. Git state and output files are the truth.
- **A long-idle lane is a red flag, not progress — and check its EXTERNAL dependency yourself.** A lane waiting on CI is fine; a lane sitting on a run that already finished is stuck. Re-poll rather than trusting one earlier reading, and **if the artifact exists, recover it yourself** rather than waiting for narration.
- Don't block the foreground on long work.

### When blocked, UNBLOCK

A block is a problem to solve, not a wall to wait behind. Infra you control → fix or recreate it. A tool or host down → route around it (Docker, `ci-adhoc-test`, a fresh cloud box). A held PR → advance everything it does not block. **The test:** *what do I have the power to do about this right now?* "Nothing" is a rare answer and requires having tried the levers.

### The reconciliation law — `fleet-empty ≠ effort-done`

**Never conclude the effort is done off an empty in-flight fleet.** The trigger for checking completeness is a full re-read of the record's OPEN-ITEMS ledger, reconciled item-by-item against the codebase — never your memory of what you dispatched.

- **The record MUST carry an explicit OPEN-ITEMS ledger** — in-flight, decided-not-dispatched, needs-a-human-decision, done-gate, housekeeping. Tick an item only when verified closed-in-code-and-tested or explicitly human-gated.
- **Each cycle, reconcile the ledger against the tree.** `git log`/`git grep` for the symbol or commit that would prove an item closed; a landed commit is not a closed item until you have confirmed a later commit did not regress it.
- **Your harness's board will mislead you here.** It tracks the *dispatched fleet*, not the goal set. An empty fleet is the moment of maximum danger for a false "done".

### Sub-agent branch discipline

- **Sub-agents commit to the EFFORT branch, or a branch forked from it that merges back** — never a separate off-main stack, and never an independent `gh pr create`. Every prompt names the branch: "commit to `<branch>` or a branch off it; do NOT open an independent PR; report the sha for me to integrate."
- **Before dispatching, reconcile against ALL related work** — `gh pr list --state open` and `git branch -a --list '<prefix>*'`. Reconciling only the feature branch misses a piece already built elsewhere.
- **Date-check every document; defer to the latest decision.**

## Home / promotion

Written as a nub-local skill. If it proves general across projects, promote it into the harness plugin's own source as a sibling mode — reference that machinery, don't fork it.
