---
name: ad-hoc-test
description: >-
  Verify new nub functionality end-to-end by building the dev binary and
  exercising it against real throwaway fixtures. Invoke (via the Skill tool) in
  BOTH directions, and the second is the one that gets skipped. (1) CONFIRM —
  after implementing or changing a subcommand/flag/behavior, check the feature
  ACTUALLY works, not just that tests pass. (2) FALSIFY — before opening a PR on
  behavior, and again before calling a review round done, SELF-REVIEW by sweeping
  many adversarial fixtures for what the change BROKE somewhere you were not
  looking: boundary abuse, real installed registry packages, monorepo and symlink
  layouts, the tier x module-format matrix. Green gates plus a working
  reported-case is NOT that sweep. Reviewers read code and cannot run it, so they
  systematically miss the silent wrong answer — the resolution that returns a
  different module with no error — which only a fixture run can catch, by
  checking WHICH file answered rather than that something did. The loop: create
  fixtures in a tmp dir, build the dev `nub`, run against them, and diff every
  result three ways — plain node, a build of your branch's MERGE-BASE, and your
  build. Use the merge-base, NEVER the shipped release: the release can trail
  `main` by dozens of unrelated commits, so a difference against it is not
  attributable to your change and every "pre-existing, not mine" verdict resting
  on it is unfounded. Ad-hoc e2e is a valid verification method on its own; this
  skill also covers when to promote a durable check into the committed test
  suite. Pairs with the `dev-loop` build skill and AGENTS.md's pre-push loop.
metadata:
  internal: true
---

# Ad-hoc end-to-end testing of nub

A green `cargo test` is necessary but not sufficient — it does not prove the *feature works when a user runs it*. The highest-confidence cheap check is to build the dev `nub` and run the actual subcommand against a real fixture on disk. This is a **valid, first-class way to verify new functionality** (it is the implementer's half of the pre-push verification loop). It does not REPLACE the test suite — durable behaviors should also become committed tests — but it is how you confirm a change is real before you trust it.

This is also the highest-yield way to find correctness bugs: a **differential fixture** — one minimal fixture isolating ONE behavior, run against `nub` AND the reference tool it claims parity with (npm/pnpm/yarn/bun/node) on identical input — turns "nub does X" into a verified divergence or match. Always compare against the thing you assert parity with.

---

## Two directions — and the second is the one that gets skipped

**CONFIRM** is the loop below: you changed a behavior, so check it actually works when a user runs it. **FALSIFY** is the sweep: before opening a PR on behavior, and again before calling a review round done, hunt across many fixtures for what your change broke *somewhere you were not looking*. Both are required, and only the first one is instinctive.

- **Confirming is not testing.** Fixtures built to demonstrate a fix all pass, because you chose them to pass. A sweep has to be built to FALSIFY: adversarial shapes, real registry packages, the full tier × module-format matrix, the layouts users actually have. If every fixture you wrote passed on the first run, you tested your intent, not your change.
- **"The gates are green and the reported case works" is the trap.** It satisfies the letter of the pre-push loop, so no alarm fires.
- **Review cannot substitute for it.** A reviewer reads code and hypothesizes; they cannot run it, so they systematically miss the SILENT WRONG ANSWER — the resolution that returns a different module with no error, the value that is quietly wrong. Those are reachable only by executing the thing and checking WHICH file answered, not merely that something did. A run of good review findings is not coverage of this bug class.
- **Schedule the sweep explicitly.** Answering someone else's findings always offers a next one, and your own verification never gets scheduled by default.

The sweep decomposes into prongs that share nothing — adversarial boundary abuse, real installed packages, monorepo and symlink topologies, the tier × entry-kind × import-form matrix — so it parallelises across sub-agents well. Require every claim to carry its command and verbatim output, and verify each load-bearing finding yourself.

## The control decides whether the result means anything

A green result with no control is not evidence. Run each fixture three ways — plain `node`, a build of your branch's **merge-base**, and your build. Three columns settle "is this mine or pre-existing?" where argument settles nothing.

- **The control is the MERGE-BASE, never a shipped release.** `~/.nub/bin/nub` is convenient and wrong: it can trail `main` by dozens of unrelated commits, so a difference against it is not attributable to your change, and every "pre-existing, not mine" verdict resting on it is unfounded. Build the base once and diff against that:
  ```bash
  git worktree add /tmp/base $(git merge-base origin/main HEAD)
  # own CARGO_TARGET_DIR; build the addon before nub-cli
  ```
- **Bind the artifact to the change before trusting any run.** The N-API addon and the whole `runtime/` tree are EMBEDDED into the `nub` binary at build time, so a stale binary silently tests the old behavior and a working fix reads as broken. Rebuild in order (addon → `nub-cli`) and confirm from the build log that the crates you changed actually compiled.
- **Vary exactly one thing.** A control that moves two variables can launder a wrong answer into a verified one. When a control agrees with what you expected, get suspicious rather than relieved.
- **Cover the TIERS, not just your host Node.** The fast tier (22.15+) and compat tier (18.19–22.14) take different code paths and break differently, so a green run on one modern Node is not a green run.
- **A behavior fix landed AFTER your sweep un-verifies the sweep.** Narrowing changes feel safe — they only defer more to the reference implementation — but they can withdraw a resolution real code depended on. Re-run at least the highest-value prong against the binary you actually intend to ship.
- **Read a tester's CAVEATS as closely as its findings.** A sub-agent's limitations section is where a report tells you which of its conclusions are not load-bearing yet.

---

## The loop

### 1. Create a fixture in a tmp dir

A minimal directory isolating the ONE behavior you changed — not a whole app. Concretely:

```bash
FIX=$(mktemp -d /tmp/nub-fix.XXXX)
cd "$FIX"
# write only what the behavior needs, e.g.:
cat > package.json <<'EOF'
{ "name": "fix", "scripts": { "build": "echo built" } }
EOF
# ...a lockfile, a workspace, a .npmrc, a tsconfig, an index.ts — whatever this behavior reads.
```

Keep it minimal: the smaller the fixture, the clearer the signal.

> **Give each fixture a UNIQUE package identity when the behavior touches install / linking / build-approval.** nub's global virtual store persists built package cells across runs, so a second fixture that reuses a dependency's `name@version` can link the *first* run's already-built cell instead of building yours — and the behavior you're testing (does the build run? does the script fire?) silently reads the stale result. This produces a **false confirmation**: the run "proves" your hypothesis because it never exercised your package at all. Burned repeatedly verifying an approve-builds claim: a `file:./dirdep` dep named `dirdep@1.0.0` linked a cell built by an earlier same-named fixture, complete with a marker file the current fixture never wrote. Fix: name the dependency uniquely per run (e.g. `dep$(date +%s)@9.9.9`) — or wipe the store between runs — and confirm the linked `node_modules/<dep>/` contents are actually YOURS before trusting the result.

### 2. Build the dev `nub`

Use the `fast` profile from a worktree with a stable target dir (see the `dev-loop` skill). Either invoke the binary by path or via the `nub-dev` symlink:

```bash
# from your worktree:
cargo build -p nub-cli --profile fast        # -> <worktree>/target/fast/nub
NUB=<worktree>/target/fast/nub
# or, if you ran `make install-dev`:  NUB=nub-dev
```

If the change touches the runtime/transpiler (the N-API addon), build the addon too so the binary loads the new one: `make addon-fast` (or `make install-dev`, which does both).

### 3. Run the subcommand you implemented, against the fixture

```bash
cd "$FIX"
"$NUB" <the-subcommand-and-flags-you-changed>
echo "exit: $?"
```

### 4. Verify it had the INTENDED effect

Don't just eyeball stdout — assert the concrete effect. Depending on the feature:

```bash
# filesystem effect:
ls -la node_modules/.bin/ ; cat the-file-it-should-have-written
# lockfile/config it should have produced or respected:
cat nub-lock.yaml 2>/dev/null; cat package.json
# exit code on a refusal path:
"$NUB" <unsound-invocation>; echo "exit: $?"   # expect a non-zero + a clear error
# DIFFERENTIAL: run the reference tool on the SAME fixture and diff:
pnpm <equivalent> ; # compare output / node_modules / lockfile to nub's
```

State explicitly what "worked" means before you run it, then check that exact thing. "Tests pass" is not "the feature works"; the effect on disk / the exit code / the diff-vs-reference is.

### 5. Probe variants and edge cases

Run the neighbours of the happy path — the cases that are easy to get wrong:

```bash
"$NUB" <cmd> --flag-variant        # each flag/alias you touched
"$NUB" <cmd>                        # the no-arg / default case
(cd empty-dir && "$NUB" <cmd>)      # missing input / empty project
"$NUB" <cmd> <malformed-input>      # the failure mode — should error clearly, not panic
```

For version-banded runtime behavior, drive nub onto a specific Node: `PATH="$HOME/.nvm/versions/node/v20.19.0/bin:$PATH" "$NUB" …` — a green run on one modern Node masks compat-tier and floor-only defects (see AGENTS.md "Iterating across Node versions and tiers"). Use Docker for clean-machine / global-cache / Node-floor behavior.

### 6. Clean up

```bash
rm -rf "$FIX"
```

---

## Then: promote durable checks into the suite

Ad-hoc verification proves *this* change; a committed test prevents the *next* regression. When the behavior is stable and the fixture is reusable, turn the probe into a real test rather than discarding it:

- A behavior covered by a tmp-fixture e2e check should become a committed integration test under `crates/nub-cli/tests/*.rs` (or a documented harness under `tests/<feature>/` for multi-version/Docker loops).
- Keep it a throwaway only for genuinely one-shot / environment-bound checks.
- Follow the testing philosophy in AGENTS.md: minimum number of tests, comprehensive (not exhaustive) coverage, contract-describing names, self-debugging failure messages.

This is the final step of the [pre-push local verification loop in AGENTS.md](../../../AGENTS.md) — incorporate the verification into the suite where reasonable, so the reviewer's pass is cheap and the CI run is a formality.

**Local host won't show the behavior?** If the thing you need to probe is OS/platform-gated — macOS Seatbelt / `sandbox-exec` / codesigning, Windows cmd.exe / `--script-shell` shell selection / `.cmd` resolution / Authenticode, musl/glibc, a Node floor — this local loop can't reach it. Linux corners go to Docker (AGENTS.md's Docker section); a real **macOS or Windows** behavior goes to the **`ci-adhoc-test` skill** — a self-contained probe under `tests/<probe>/` run by a branch-scoped workflow with **no PR required**.

---

## Quick reference

```bash
FIX=$(mktemp -d /tmp/nub-fix.XXXX); cd "$FIX"        # 1. fixture
# ...write minimal package.json / lockfile / tsconfig / source...
cargo build -p nub-cli --profile fast                # 2. build dev nub
NUB=<worktree>/target/fast/nub
"$NUB" <subcommand>; echo "exit: $?"                 # 3. run it
cat the-effect; pnpm <equiv>                         # 4. verify effect (differential)
"$NUB" <variant>; "$NUB" <bad-input>                # 5. probe edges
rm -rf "$FIX"                                         # 6. clean up
# 7. promote a durable check into crates/nub-cli/tests/*.rs
```
