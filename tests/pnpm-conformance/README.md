# pnpm conformance harness — run pnpm's OWN test suite against nub

This harness runs pnpm's own black-box CLI test suite against the nub binary. It is the widest-net way to verify nub's pnpm-compatibility claim: instead of writing nub-authored parity tests, it points the *incumbent's* suite at nub and treats every divergence as a candidate finding. It is the PM-CLI analog of the Node-test-suite leverage harness (`tests/node-suite/`).

## The seam

pnpm's front-door package — the package literally named `pnpm` inside the [pnpm/pnpm](https://github.com/pnpm/pnpm) monorepo — ships ~64 test files in `pnpm/test/*.ts`. 63 of them spawn the real binary through ONE seam in `pnpm/test/utils/execPnpm.ts`:

```ts
export const binDir = path.join(__dirname, '../..', isWindows() ? 'dist' : 'bin')
export const pnpmBinLocation = path.join(binDir, 'pnpm.cjs')   // .mjs on newer pnpm
crossSpawn.spawn(process.execPath, [pnpmBinLocation, ...args], { env, stdio })
```

Every assertion is on stdout / stderr / exit-code / lockfile / node_modules state — exactly nub's drop-in parity surface. The harness replaces `bin/pnpm.cjs` with a shim (`nub-pnpm-shim.cjs`) that re-execs the nub binary with `argv0: 'pnpm'`, so nub adopts the pnpm identity (nub picks its package-manager role from argv[0]'s basename — `Argv0::detect` in `crates/nub-cli/src/cli.rs`). The whole suite then exercises nub.

npm's own suite is NOT usable this way: it constructs npm's internal JS `Npm` class in-process (tmock + nock), never spawning a binary (~3% black-box). The harness is pnpm-only by design.

## Why a pinned clone (not a vendored subset)

The reference checkout `.repos/pnpm` tracks pnpm's `main`, which drifts from the version nub spoofs. The harness clones pnpm at the EXACT version nub reports on `pnpm --version` (`v11.3.0` == `PNPM_PARITY_VERSION` in `crates/nub-cli/src/pm_engine/mod.rs`) so the suite's version-string assertions match the behavior nub targets. A vendored subset was the documented fallback if bootstrap proved too flaky; the pinned-clone bootstrap proved tractable (one `pnpm install` + a lean compile), so we use it — it never rots against the version under test.

## Files

| file | role |
| --- | --- |
| `run.sh` | the harness: clone → bootstrap → seam-swap → jest → classify |
| `nub-pnpm-shim.cjs` | the seam replacement (re-execs nub as pnpm; `__NUB_BIN__` is baked at swap time) |
| `classify.mjs` | parses jest `--json` output; classifies each failure against the allowlist |
| `allowlist.txt` | known-OK failures: intended divergences + tracked bugs (GENERATED — see below) |
| `gen-allowlist.mjs` | regenerates `allowlist.txt` from a run's results JSON, bucketed by category |

## Running it locally

```bash
# Build nub first.
cargo build -p nub-cli

# A real pnpm must be on PATH — NOT for the commands under test (those go to nub
# via the seam), but for the suite's registry mock, which launches verdaccio via
# `pnpm --use-node-version=20.x`. Install one separate from any nub shim:
npm install -g pnpm@11.3.0

# Full suite (clones to a temp dir, pins v11.3.0):
tests/pnpm-conformance/run.sh target/debug/nub

# A single test file (fast iteration; stale-allowlist check is skipped on subsets):
tests/pnpm-conformance/run.sh target/debug/nub v11.3.0 test/root.ts

# Reuse a clone across runs (skips the slow bootstrap):
PNPM_CLONE_DIR=/tmp/pnpm-conf KEEP_CLONE=1 tests/pnpm-conformance/run.sh target/debug/nub
```

Exit 0 iff no failing test is a SURPRISE (an un-allowlisted divergence). A stale allowlist entry is reported but does NOT fail the run — a known failure that starts passing is an improvement, not a regression.

## Bootstrap, step by step

1. **Clone** pnpm/pnpm at the pinned tag (`git clone --depth 1 --branch v11.3.0`).
2. **Install** the monorepo deps (`corepack pnpm install --frozen-lockfile`, ~30 s). Corepack runs the repo's own pinned pnpm.
3. **Compile** only the `pnpm` front-door package — `tsc --build` then `bundle` (produces `pnpm/dist/pnpm.cjs`) plus the runtime-asset copies. We deliberately SKIP the full `compile-only` script: it also typechecks + lints the entire monorepo (many minutes, irrelevant to running the suite).
4. **Swap the seam**: detect whether the suite spawns `bin/pnpm.cjs` or `bin/pnpm.mjs` (version-dependent), back it up, and write the shim with the absolute nub path baked in. (Baked, not env-passed: the suite's `createEnv()` rebuilds a clean env keeping only `PATH`/`COLORTERM`/`APPDATA`, so an exported `NUB_BIN` would be stripped before the shim runs.)
5. **Run jest** scoped to `pnpm/test/` from inside the `pnpm/` package (so its `@pnpm/jest-config/with-registry` preset — which boots the registry mock — is active).
6. **Classify** the jest `--json` output against the allowlist.

## The allowlist (generated)

The suite has a large, stable set of known failures — intended/out-of-scope divergences (pnpm version-switching, per-package Node provisioning, `pnpm server`/`self-update`, the global-install layout, patch/configurational-dependency hooks, supportedArchitectures), harness-infrastructure brittleness (the registry mock not honored on a flow, an `execPnpm.ts` helper that crashes on empty output, multi-minute timeouts on unimplemented features), and genuine nub↔pnpm divergences worth fixing. The maintainer call (2026-06-30) is to SKIP all of them so the nightly is green on this known reality and reds ONLY on a genuine NEW regression.

So `allowlist.txt` is GENERATED, not hand-maintained, from a real full-suite run:

```bash
node tests/pnpm-conformance/gen-allowlist.mjs <results.json> > tests/pnpm-conformance/allowlist.txt
```

Every currently-failing test becomes an EXACT-fullName entry (matched as a substring against a failing test's full name), bucketed into commented category sections. The **genuine-divergence** bucket is thread-referenced (`pnpm-conformance-divergences` D1–D7, `pnpm-conformance-99`) so real bugs are skipped-but-not-buried: fix the bug, then drop its line (or regenerate after the next run). Regenerate after any deliberate suite/pin change; the diff shows exactly which failures appeared or disappeared.

The classifier reports three things; only one reds the run:

- **SURPRISE** (FATAL) — a failing test matching NO allowlist entry: a genuinely new divergence / regression. This is the whole value of the gate.
- **STALE-ALLOW** (non-fatal) — an allowlist entry that matched no failure (a known failure that now passes or was renamed): reported loudly for pruning, but it does NOT red the run — an improvement is not a regression.
- **KNOWN-FAILING** — a failure matching an allowlist entry: expected, skipped.

## Flake sources (and mitigations)

- **Registry mock (verdaccio).** The jest preset boots verdaccio under Node 20 via a real pnpm. Needs a real pnpm on PATH and (first run) a Node-20 download. This is the main flake/cost source; the CI job allows 90 minutes and uploads the results JSON as an artifact.
- **Self-update banner (B3).** nub's/aube's self-update check can print an "Update available" box to stdout. The harness exports `NUB_NO_UPDATE=1` and `AUBE_NO_UPDATE_CHECK=1` to suppress it; an `Update available` entry stays in the allowlist defensively.
- **Network.** The pinned clone and the monorepo install need network; this is not an offline harness.

## CI

`.github/workflows/pnpm-conformance.yml` runs this NIGHTLY (08:00 UTC) and on-demand (`workflow_dispatch`, with optional `pnpm_tag` / `jest_args` inputs). It is intentionally NOT a per-PR gate: it is a new external-suite surface with real flake sources, so it must not block the trunk. Promote it to a per-PR gate only after it proves stable across several nightly runs.

## Baseline (2026-06-30, local, pnpm v11.3.0)

Full front-door suite (`pnpm/test/`, 50 suites / 337 tests, ~45–60 min including the slow `server.ts`/`recursive` suites). With the generated allowlist the classifier reports `SURPRISE: 0` → exit 0. The known-failing set is bucketed in `allowlist.txt`; the genuine-divergence bucket is tracked in `pnpm-conformance-divergences` (D1–D7) and `pnpm-conformance-99`, to be fixed on their own threads. Regenerate the baseline after a deliberate suite/pin change.

## Keeping the pin in sync

When nub's spoofed pnpm version (`PNPM_PARITY_VERSION` in `crates/nub-cli/src/pm_engine/mod.rs`) changes, update `PNPM_TAG`/`PNPM_PIN` in the workflow + the version references here to the SAME version, then **regenerate `allowlist.txt`** from a fresh run (`gen-allowlist.mjs`) — a version change shifts the failure set, and the generated allowlist must match. (`lockfile-roundtrip.yml` pins its own version for lockfile-FORMAT coverage, a separate axis — it no longer tracks this pin.)
