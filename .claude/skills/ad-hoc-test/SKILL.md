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

A green `cargo test` does not prove the *feature works when a user runs it*. Build the dev `nub` and run the actual subcommand against a real fixture on disk — a first-class verification method, and the implementer's half of the pre-push loop. It does not replace the test suite; durable behaviors should also become committed tests.

The highest-yield bug-finding shape is a **differential fixture**: one minimal fixture isolating ONE behavior, run against `nub` AND the reference tool it claims parity with (npm/pnpm/yarn/bun/node) on identical input. Always compare against the thing you assert parity with.

**Where it runs: the builder VM by default.** Any probe or sweep that needs no macOS-specific behavior goes to a spot VM — write the whole loop below as ONE script and dispatch it with `nub scripts/remote-build.ts --job adhoc --script <file> --detach`, then `--attach <vm-name>` (the `remote-build` skill). The script runs at the synced repo root with `NUB_BIN` naming a fresh `--profile fast` build of your tree, real addon staged; the image carries Node 26 + npm, and the script installs pnpm/bun itself when a differential needs them. Batch a sweep into one script, not one VM per fixture. The merge-base control build can ride a second dispatch with `--source <merge-base-worktree>`. Stay local only for the tight fix-and-rerun loop against a warm binary and for macOS-native behavior.

## Two directions — and the second is the one that gets skipped

**CONFIRM** is the loop below. **FALSIFY** is the sweep: before opening a PR on behavior, and again before calling a review round done, hunt across many fixtures for what your change broke *somewhere you were not looking*. Both are required; only the first is instinctive.

- **Confirming is not testing.** Fixtures built to demonstrate a fix all pass, because you chose them to pass. A sweep is built to FALSIFY: adversarial shapes, real registry packages, the full tier × module- format matrix, the layouts users actually have. If every fixture passed on the first run, you tested your intent, not your change.
- **"The gates are green and the reported case works" is the trap** — it satisfies the letter of the pre-push loop, so no alarm fires.
- **Review cannot substitute for it.** A reviewer reads code and hypothesizes; they cannot run it, so they systematically miss the SILENT WRONG ANSWER — the resolution that returns a different module with no error, the value that is quietly wrong. Those are reachable only by executing the thing and checking WHICH file answered, not merely that something did.
- **Schedule the sweep explicitly.** Answering someone else's findings always offers a next one; your own verification never gets scheduled by default.

The sweep decomposes into prongs that share nothing — adversarial boundary abuse, real installed packages, monorepo and symlink topologies, the tier × entry-kind × import-form matrix — so it parallelises across sub-agents well. Require every claim to carry its command and verbatim output, and verify each load-bearing finding yourself.

## The control decides whether the result means anything

A green result with no control is not evidence. Run each fixture three ways — plain `node`, a build of your branch's **merge-base**, and your build.

- **The control is the MERGE-BASE, never a shipped release.** `~/.nub/bin/nub` can trail `main` by dozens of unrelated commits, so a difference against it is not attributable to your change and every "pre-existing, not mine" verdict resting on it is unfounded.
  ```bash
  git worktree add /tmp/base $(git merge-base origin/main HEAD)
  # own CARGO_TARGET_DIR; build the addon before nub-cli
  ```
- **Bind the artifact to the change before trusting any run.** The N-API addon and the whole `runtime/` tree are EMBEDDED into the `nub` binary at build time, so a stale binary silently tests the old behavior and a working fix reads as broken. Rebuild in order (addon → `nub-cli`) and confirm from the build log that the crates you changed actually compiled.
- **Score with the SCORER, never by hand-summing its input.** A results file is often PRE-GATE: in the build-jail corpus, `verdict-table.json` holds raw rows while validity, scheduling, artifact-ratio and manufactured-pass all live in `aggregate.mjs`, so arithmetic over the tables silently skips every gate. Measured 2026-07-31: hand-summing gave 47 survive / 61 break where `node aggregate.mjs <root>` over the `report.json` set gives **45/63** — two `hook-installer` rows that read `DID-WORK-AND-SUCCEEDED` in BOTH arms are still breaks, because a granted shard-mate's project write satisfies an ungranted package's predicate. Run the project's own scorer over its own inputs; a number you computed yourself from an intermediate file is a different number.
- **Read the artifact you BUILT, not the first path matching its name.** Same root cause as the rule above — trusting the first plausible file instead of the final one. `--profile fast` writes `target/fast/`, `cargo test` writes `target/debug/`, and generated files live under a per-crate `build/<crate>-<hash>/out/`, so several plausible copies coexist and the stale one is often first alphabetically. Sort candidates by mtime and check the winner is newer than the build.
- **Vary exactly one thing, and check the variable is INDEPENDENT of the harness.** A control that moves two variables launders a wrong answer into a verified one; a variable that also feeds the measurement destroys attribution instead. Measured 2026-07-31: a Windows differential varied `LOCALAPPDATA`, but nub's store lives at `%LOCALAPPDATA%\nub\pm\store`, so the store moved outside every snapshot root and 6 packages read NOT-INSTALLED. Before running an experiment, ask whether the thing you are varying also feeds the harness. When a control agrees with what you expected, get suspicious rather than relieved.
- **Exit code is NOT a universal success condition — check what the script actually does first.** Some lifecycle scripts wrap `spawnSync` in `try/catch` and never propagate its status, so *every* arm exits 0 no matter what was denied (every hook installer measured — `lefthook`, `@arkweid/lefthook`, `@evilmartians/lefthook` — behaves this way). Where that happens, judge the ARTIFACT against a jail-off control instead. Exit code remains the right default; this is a real exception class.
- **A grant can MOVE a failure rather than close it.** Re-measure after every grant instead of assuming one is the whole job.
- **Cover the TIERS, not just your host Node.** The fast tier (22.15+) and compat tier (18.19–22.14) take different code paths and break differently.
- **A behavior fix landed AFTER your sweep un-verifies the sweep.** Narrowing changes feel safe but can withdraw a resolution real code depended on. Re-run at least the highest-value prong against the binary you intend to ship.
- **Read a tester's CAVEATS as closely as its findings** — the limitations section is where a report tells you which conclusions are not load-bearing yet.

## The loop

### 1. Create a fixture in a tmp dir

Minimal, isolating the ONE behavior you changed — not a whole app.

```bash
FIX=$(mktemp -d /tmp/nub-fix.XXXX)
cd "$FIX"
# write only what the behavior needs, e.g.:
cat > package.json <<'EOF'
{ "name": "fix", "scripts": { "build": "echo built" } }
EOF
# ...a lockfile, a workspace, a .npmrc, a tsconfig, an index.ts — whatever this behavior reads.
```

> **Give each fixture a UNIQUE package identity when the behavior touches install / linking /
> build-approval.** nub's global virtual store persists built package cells across runs, so a second
> fixture reusing a dependency's `name@version` can link the *first* run's already-built cell — a FALSE
> CONFIRMATION, because the run never exercised your package at all. Name the dependency uniquely per run
> (e.g. `dep$(date +%s)@9.9.9`) or wipe the store, and confirm `node_modules/<dep>/` contents are yours.

### 2. Build the dev `nub`

```bash
# from your worktree:
cargo build -p nub-cli --profile fast        # -> <worktree>/target/fast/nub
NUB=<worktree>/target/fast/nub
# or, if you ran `make install-dev`:  NUB=nub-dev
```

If the change touches the runtime/transpiler (the N-API addon), build the addon too: `make addon-fast` (or `make install-dev`, which does both).

### 3. Run the subcommand against the fixture

```bash
cd "$FIX"
"$NUB" <the-subcommand-and-flags-you-changed>
echo "exit: $?"
```

### 4. Verify the INTENDED effect

State explicitly what "worked" means before running, then check that exact thing — the effect on disk, the exit code, or the diff vs the reference tool.

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

### 5. Probe variants and edge cases

```bash
"$NUB" <cmd> --flag-variant        # each flag/alias you touched
"$NUB" <cmd>                        # the no-arg / default case
(cd empty-dir && "$NUB" <cmd>)      # missing input / empty project
"$NUB" <cmd> <malformed-input>      # the failure mode — should error clearly, not panic
```

For version-banded runtime behavior, drive nub onto a specific Node: `PATH="$HOME/.nvm/versions/node/v20.19.0/bin:$PATH" "$NUB" …`. Use Docker for clean-machine / global-cache / Node-floor behavior.

### 6. Clean up

```bash
rm -rf "$FIX"
```

## Then: promote durable checks into the suite

Ad-hoc verification proves *this* change; a committed test prevents the *next* regression.

- A behavior covered by a tmp-fixture check should become a committed integration test under `crates/nub-cli/tests/*.rs` (or a documented harness under `tests/<feature>/` for multi-version/Docker loops).
- Keep it a throwaway only for genuinely one-shot / environment-bound checks.
- Follow AGENTS.md's testing philosophy: minimum number of tests, comprehensive (not exhaustive) coverage, contract-describing names, self-debugging failure messages.

**Local host won't show the behavior?** OS/platform-gated probes — macOS Seatbelt / `sandbox-exec` / codesigning, Windows cmd.exe / `--script-shell` selection / `.cmd` resolution / Authenticode, musl/glibc, a Node floor — can't be reached here. Linux corners go to Docker; a real macOS or Windows behavior goes to the **`ci-adhoc-test` skill** (branch-scoped workflow, no PR required).

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
