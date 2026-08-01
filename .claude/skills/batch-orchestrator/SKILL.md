---
name: batch-orchestrator
description: Land a BATCH of code changes on one branch by fanning out sub-agents to edit in parallel, holding a barrier, doing ONE build yourself, then re-steering those SAME agents to verify against that single artifact. Invoke (via the Skill tool) whenever you have several decided code changes for the same branch — a run of small fixes, a set of grants, a comment sweep, a group of review findings. The rule it exists to carry: EDITS ARE CHEAP AND PARALLELISE, BUILDS ARE EXPENSIVE AND DO NOT — so agents edit but never build, and the orchestrator owns the single build. Re-steer the same agents rather than dispatching fresh ones, because the agent that wrote a change holds the richest context for testing it. Auto-triggers on "a bunch of fixes", "batch these changes", "work through the change list", or noticing several cargo target dirs building at once. Pairs with `remote-build` (the uncontended build), `ad-hoc-test` (the verification loop), `cpu-reduction` (clearing residue a violation creates), `rust-build-hygiene` (not creating it).
---

# batch-orchestrator

**Agents edit in parallel. Nobody builds but you. Then the same agents verify.**

The instinct when handed ten changes is ten agents that each edit, build, and test. That multiplies
the one thing that does not parallelise: N concurrent builds contend for CPU, for disk, and on a
shared target dir for cargo's lock. Measured here — seven concurrent target dirs drove load to 104 on
a 10-core box and one build died with `No space left on device`. Every lane got slower, including the
"parallel" ones.

## 1. EDIT — fan out, with one hard constraint

Dispatch one agent per change, all into the **same worktree** on the same branch. Each prompt must
carry, in these words or better:

> **This is an edit-only phase. Do NOT build, do NOT test, do NOT run cargo at all** — not even to
> check your work. Other agents are editing this same worktree concurrently and I run a single
> shared build after everyone finishes. Concurrent cargo invocations serialise on one target lock,
> so each of you would wait for the others and the batch would take longer than doing it serially.
> Make your edits, make them careful, and stop. Report the files you touched.

Say **why**, not just what. A diligent agent will build "just to be sure" unless it understands the
cost. The ban is on the target lock, not on verification — `rustfmt --check` and anything else that
avoids cargo are fine.

**Assign file ownership explicitly**, and tell each agent to STOP and report rather than touching
anything outside its own files. Two agents editing one file is a merge you did not need.

**You are one of the editors — do not dispatch into a worktree you are editing yourself.** Give the
agent its own, or stop touching yours until it returns. And in a shared tree **never `git add -A`**:
stage only your own paths. Both halves were violated at once (2026-07-31) — a docs agent was sent into
the orchestrator's live worktree, and a later `git add -A` swept three of its in-progress files under
the orchestrator's commit message. Nothing was lost, but authorship was silently misattributed across
two commits, and a mid-edit sweep can just as easily commit a half-written file that compiles.

**Facts you assert in a dispatch prompt need the same grounding as facts you tell the maintainer.** A
fresh-context agent cannot tell a checked claim from a remembered one and will act on both. The same
session shipped a brief asserting `wiki/` was gitignored and local-only; `git check-ignore` exits 1 and
`git ls-files` lists the files. The agent verified and corrected it, which is luck, not a process — an
agent that believed it would have written internal-register prose into a public tracked file.

**Fray's canonical scratchpad is the standing exception to deliverable file ownership.** Every lane
should re-read it and merge its own scoped progress, decisions, checks, and terminal status as it
works. Do not make the root its sole writer or lock the pad against child edits; root reconciles the
shared updates against the live fleet at the barrier.

**Keep the file→agent map.** You need it in phase 3.

## 2. BARRIER — wait for all of them

Do not start the build while an edit is outstanding: a half-applied tree produces failures that
belong to nobody.

If an agent dies mid-edit, **resume it with `SendMessage`** rather than restarting — its partial work
is in the worktree and its reasoning is in its transcript.

**Run `cargo fmt` yourself here, before the build.** It takes seconds and fixes the whole tree at
once, so a formatting slip never reaches the gate.

## 3. BUILD — once, yours, on an uncontended machine

Use `remote-build`: `nub scripts/remote-build.ts --job clippy` / `--job test` runs the
byte-identical CI invocation on an ephemeral GCE spot VM for a few cents. Reach for it whenever the
local host is contended, which on a machine hosting several agent sessions is most of the time.

**Read the tool's own exit code.** `cargo … ; echo EXIT=$? ; tail` makes the *shell* exit 0
regardless — a branch was pushed and reported clean here while cargo had exited 101.

Gate on what the batch actually touches:

- `cargo clippy --all-targets --all-features` — a scoped `-p` without `--all-targets` misses
  test-code lints.
- **`cd vendor/aube && cargo check --workspace --all-targets`**, own `CARGO_TARGET_DIR`, if anything
  reaches aube. A dependent's `--all-targets` never builds a path dependency's test targets, so every
  nub-side gate can pass while `vendor/aube` will not compile. That gap already let a broken merge
  through.
- For `#[cfg(windows)]` code on a Mac, `cargo check -p <crate> --target x86_64-pc-windows-gnu`.
  Confirm from the log that the crate compiled *for that target*, or the cfg'd code was skipped and
  you proved nothing.

**Read `rust-build` first** — it owns the cargo mechanics, and three change what you budget: clippy
and `cargo test` run on DIFFERENT profiles, so "one build" is two artifact universes; neither leaves
a runnable binary, so add an explicit `build` step if phase 4 needs one; and let
`scripts/rust-build.sh` pick the target dir rather than exporting `CARGO_TARGET_DIR`.

**Triage failures against the phase-1 file map.** An error spanning two agents' files is a real
interface disagreement — warm-resume BOTH with it rather than guessing which is wrong.

## 4. VERIFY — re-steer the SAME agents

**Verification means running the built binary, and it must stay compilation-free.** `cargo test` is a
BUILD — it compiles test targets plus dev-dependencies — and so is `cargo clippy`. An agent told to
"run the tests for your change" is doing the expensive serial thing this pattern exists to prevent, N
times over. Give each agent the binary's path and a real fixture per `ad-hoc-test`, and say
explicitly that cargo is yours, not theirs.

If a change can only be proven by a Rust-level unit test, that test is an EDIT — written in phase 1,
compiled in your next single build.

**Do not dispatch fresh agents here.** The agent that wrote the change knows what it rejected and
where the risk sits; a fresh context re-derives that badly.

## Scope

**Use it for:** several decided changes on one branch, file-disjoint, each statable in a line or two.

**Not for:** changes needing different branch topology (separate branches); long exploratory work
whose shape changes as it learns (a lane, not a batch item); or a single change (just make it).

**Research and adversarial probing stay ordinary sub-agent work** — they need breadth you cannot hold
and they do not build. Only the edit/build/verify cycle is what this reshapes.

## The artifact that survives compaction

**The task list is the batch.** One entry per change, deleted when landed, added when discovered. A
mega-doc is not required and tends to rot.
