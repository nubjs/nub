---
name: ad-hoc-test
description: >-
  Verify new nub functionality end-to-end by building the dev binary and
  exercising it against a real throwaway fixture. Invoke (via the Skill tool)
  after implementing or changing a subcommand/flag/behavior, to confirm the
  feature ACTUALLY works (not just that tests pass). The loop: create a fixture
  in a tmp dir, build the dev `nub`, run the subcommand you implemented against
  the fixture, verify it had the intended effect, then run command variants to
  probe edge cases. Ad-hoc e2e is a valid verification method on its own; this
  skill also covers when to promote a durable check into the committed test
  suite. Pairs with the `dev-loop` build skill and AGENTS.md's pre-push loop.
metadata:
  internal: true
---

# Ad-hoc end-to-end testing of nub

A green `cargo test` is necessary but not sufficient — it does not prove the *feature works when a user runs it*. The highest-confidence cheap check is to build the dev `nub` and run the actual subcommand against a real fixture on disk. This is a **valid, first-class way to verify new functionality** (it is the implementer's half of the pre-push verification loop). It does not REPLACE the test suite — durable behaviors should also become committed tests — but it is how you confirm a change is real before you trust it.

This is also the highest-yield way to find correctness bugs: a **differential fixture** — one minimal fixture isolating ONE behavior, run against `nub` AND the reference tool it claims parity with (npm/pnpm/yarn/bun/node) on identical input — turns "nub does X" into a verified divergence or match. Always compare against the thing you assert parity with.

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
