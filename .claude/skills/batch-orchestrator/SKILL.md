---
name: batch-orchestrator
description: Land a BATCH of code changes on one branch by fanning out sub-agents to edit in parallel, holding a barrier, doing ONE build yourself, then re-steering those SAME agents to verify against that single artifact. Invoke (via the Skill tool) whenever you have several decided code changes for the same branch — a run of small fixes, a set of grants, a comment sweep, a group of review findings. The rule it exists to carry: EDITS ARE CHEAP AND PARALLELISE, BUILDS ARE EXPENSIVE AND DO NOT — so agents edit but never build, and the orchestrator owns the single build. Re-steer the same agents rather than dispatching fresh ones, because the agent that wrote a change holds the richest context for testing it. Auto-triggers on "a bunch of fixes", "batch these changes", "work through the change list", or noticing several cargo target dirs building at once. Pairs with `remote-build` (the uncontended build), `ad-hoc-test` (the verification loop), `cpu-reduction` (clearing residue a violation creates), `rust-build-hygiene` (not creating it).
---

# batch-orchestrator

**Agents edit in parallel. Nobody builds but you. Then the same agents verify.**

The instinct when handed ten changes is ten agents that each edit, build, and test. That multiplies
the one thing that does not parallelise. In a Rust workspace an edit is minutes and a cold build is
tens of minutes, and N concurrent builds do not overlap usefully — they contend for CPU, for disk,
and on a shared target dir for cargo's lock.

Measured on this repo's dev host: **seven concurrent cargo target dirs drove load to 104 on a
10-core box**, and one build died with `No space left on device` because the parallel target dirs
filled the volume. Every lane got slower, including the ones that were "parallel".

## The four phases

### 1. EDIT — fan out, with one hard constraint

Dispatch one agent per change, all into the **same worktree** on the same branch. Each prompt must
carry, in these words or better:

> **This is an edit-only phase. Do NOT build, do NOT test, do NOT run cargo at all** — not even to
> check your work. Other agents are editing this same worktree concurrently and I run a single
> shared build after everyone finishes. Concurrent cargo invocations serialise on one target lock,
> so each of you would wait for the others and the batch would take longer than doing it serially.
> Make your edits, make them careful, and stop. Report the files you touched.

Say **why**, not just what. A diligent agent will build "just to be sure" unless it understands the
cost, and that single build blocks every sibling.

**Assign file ownership explicitly.** The file tools enforce read-before-write so concurrent edits
do not silently clobber, but two agents editing one file is a merge you did not need. Tell each
agent which files are its own and to STOP and report rather than touching anything outside them.

**Keep the file→agent map.** You need it in phase 3.

### 2. BARRIER — wait for all of them

Do not start the build while an edit is outstanding. A half-applied tree produces failures that
belong to nobody and cost a whole cycle to attribute.

If an agent dies mid-edit (session limits, transient API errors), **resume it with `SendMessage`**
rather than restarting — its partial work is in the worktree and its reasoning is in its transcript.

### 3. BUILD — once, yours, on an uncontended machine

Use `remote-build`: `nub scripts/remote-build.ts --job clippy` / `--job test` runs the
byte-identical CI invocation on an ephemeral GCE spot VM for a few cents. Reach for it whenever the
local host is contended, which on a machine hosting several agent sessions is most of the time.

**Read the tool's own exit code.** Writing `cargo … ; echo EXIT=$? ; tail` makes the *shell* exit 0
regardless — a branch was pushed and reported clean here while cargo had exited 101.

Gate on what the batch actually touches:

- `cargo clippy --all-targets --all-features` — a scoped `-p` without `--all-targets` misses
  test-code lints.
- **`cd vendor/aube && cargo check --workspace --all-targets`**, own `CARGO_TARGET_DIR`, if anything
  reaches aube. A dependent's `--all-targets` never builds a path dependency's test targets, so every
  nub-side gate can pass while `vendor/aube` will not compile. That gap already let a broken merge
  through.
- For `#[cfg(windows)]` code on a Mac, `cargo check -p <crate> --target x86_64-pc-windows-gnu`.
  Confirm from the log that the crate compiled *for that target* — otherwise the cfg'd code was
  skipped and you proved nothing.

**"ONE build" is really TWO artifact universes — plan for it (verified against `.github/workflows/ci.yml`).**
CI's check and clippy jobs run `--profile fast` (ci.yml:182, :220); `cargo test` runs on the DEFAULT
`dev` profile (ci.yml:286, :790). Those are different target subdirectories, so **clippy artifacts do
not serve `cargo test` and vice versa** — gating a batch on both means two dependency builds, not one.
Budget for that rather than discovering it mid-run, and never "helpfully" put `cargo test` on
`--profile fast` to unify them: that diverges from what CI actually runs, which is the one thing this
loop exists to reproduce. The reverse mistake is worse — dropping `--profile fast` from clippy drives
a second full dependency build under `dev` and leaves ~26 GB of duplicated `target/debug` +
`target/fast` on the volume. `fast` inherits `dev` (identical debug-assertions, overflow checks and
opt-level; only debuginfo differs, which no lint reads), so the split costs nothing in fidelity.

**Do not `export CARGO_TARGET_DIR` — for yourself or in any dispatch prompt.** `scripts/rust-build.sh`
picks the target dir AND CoW-seeds a fresh private one from a warm shared bucket (~14s); an explicit
variable silently opts out and buys a ~40-minute cold build. Check the BRANCH first, though: the
wrapper's seeding landed in #589, so a long-lived branch cut before it has the wrapper with NO seeding,
and telling a lane "use the wrapper for a warm start" there is simply wrong. `grep -c seed
scripts/rust-build.sh` settles it in one command.

**Never pipe a build or gate through `tail`/`head`.** Two separate failures: you get the PIPE's exit
status rather than cargo's — which hid three real failures in one session here — and `| head -N` closes
the pipe after N lines, SIGPIPE-killing tee and cargo outright. A lane lost a build to exactly that,
then found its target dir "cold" ten minutes later and `rm -rf`'d it. Redirect to a file and grep it.

**Triage failures against the phase-1 file map.** A shared build means a compile error is not
attributable by default. If an error spans two agents' files, that is a real interface disagreement:
warm-resume BOTH with the error rather than guessing which is wrong.

### 4. VERIFY — re-steer the SAME agents

**Verification means RUNNING THE BUILT BINARY, and it must stay compilation-free.** This is the
part most easily misread back into the problem: in a Rust workspace `cargo test` is a BUILD — it
compiles test targets plus dev-dependencies, often a heavier one than `cargo build` — and `cargo
clippy` compiles too. So an agent told to "just run the targeted tests for your change" is doing
the exact expensive serial thing the pattern exists to prevent, N times over. **Agents in this
phase run the binary you already built against fixtures, per `ad-hoc-test`, and invoke no cargo
subcommand at all.** `cargo test` and `cargo clippy` are the ORCHESTRATOR's, folded into the single
phase-3 build. Say this explicitly in the re-steer, the same way phase 1 says why not to build —
an agent that reads "verify your change" will reach for `cargo test` unless told the binary path
and told not to.

If a change genuinely can only be proven by a Rust-level unit test, that test is an EDIT: the agent
writes it in phase 1 and it compiles in the orchestrator's next single build. It does not get its
own `cargo test` invocation in a lane.

`SendMessage` each agent to test its own change against the artifact you just built. **Do not
dispatch fresh agents for this.** The agent that wrote the change knows why each choice was made,
what it rejected, and where the risk sits; a fresh context re-derives all of that badly. This is the
same reasoning as nesting a reviewer under an implementer.

Give them the built binary's path and a real fixture to exercise, per `ad-hoc-test`. This is the
phase where breadth genuinely pays — and it now runs against **one warm build instead of N cold
ones**, which is the whole point.

## What belongs in this pattern, and what does not

**Use it for:** several decided changes on one branch, file-disjoint, each statable in a line or two.

**Do not use it for:** changes needing different branch topology (those are separate branches);
long exploratory work whose shape changes as it learns (that is a lane, not a batch item); or a
single change (just make it).

**Research and adversarial probing stay ordinary sub-agent work** — they need breadth you cannot
hold and they do not build. Only the edit/build/verify cycle is what this pattern reshapes.

## The artifact that survives compaction

**The task list is the batch.** Keep it live — one entry per change, deleted when landed, added when
discovered. It plus the session's grounding document are what tell the next turn where the batch
stands; a mega-doc is not required and tends to rot.
