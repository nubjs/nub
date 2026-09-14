# pnpm conformance harness

This harness runs pnpm's own black-box CLI test suite against the Nub binary. In a pnpm project Nub claims to behave exactly like pnpm 12.4.1, so instead of writing Nub-authored parity tests it points the incumbent's suite at Nub and treats every divergence as a candidate finding. [`tests/conformance/`](../conformance/) is the complementary harness: it diffs Nub against a real pnpm 12.4.1 on lockfiles and `node_modules` for a fixed fixture set.

## What pnpm 12 ships

pnpm 12 is the Rust `pnpm-cli` crate in the [pnpm/pnpm](https://github.com/pnpm/pnpm) monorepo (`pnpm/crates/cli`, published as the `pnpm` npm package from `pnpm/npm/pnpm`). Its end-to-end suite is a single integration-test binary, `pnpm/crates/cli/tests/suite/`, of roughly 2,000 tests run with `cargo nextest`. The TypeScript suite under `pnpm11/pnpm/test/` in the same tree belongs to the pnpm 11 line and does not test pnpm 12.

## The seam

Every test in the suite reaches the CLI through assert_cmd's `Command::cargo_bin("pnpm")`, which resolves `CARGO_BIN_EXE_pnpm`, the path of the `pnpm` binary Cargo built into `<clone>/target/debug/pnpm`. nextest sets that variable itself when it starts each test, so exporting a different value does nothing; the file at that path is the seam.

`run.sh` builds the suite, records nextest's build metadata (`cargo nextest list --list-type binaries-only` and `cargo metadata`), replaces `target/debug/pnpm` with `nub-as-pnpm.sh`, and runs the recorded binaries with `--binaries-metadata`/`--cargo-metadata`, so nextest never asks Cargo to rebuild and restore the original. Two controls bracket the swap: the seam must report `12.4.1` before it and Nub's own version after it. The shim logs each invocation, and a run in which Nub was never spawned fails.

The shim execs `nub` under its own name. Nub's argv0 `pnpm` is the package-manager shim, which hands the command to a real pnpm, so a seam that invoked Nub as `pnpm` would test pnpm against itself.

## Which tests are pnpm projects

Nub picks its identity per project: a directory with a `pnpm-lock.yaml`, a `pnpm-workspace.yaml`, or a `packageManager`/`devEngines` pin naming pnpm is a pnpm project, and anything else is a Nub project with its own lockfile and error codes. Most tests set up the mocked registry with `CommandTempCwd::add_mocked_registry`, which writes a `pnpm-workspace.yaml`, so they run as pnpm projects. Tests that call `CommandTempCwd::init()` alone and never write a pnpm marker run as Nub projects; a failure there that follows from Nub's identity is a legitimate divergence, and the allowlist names it as such rather than as a bug. A test that pins a `packageManager` version other than 12.4.1 is delegated to that pnpm.

Several tests also spawn a bare `pnpm` from `PATH` and compare its output with the binary under test. The harness installs `pnpm@12.4.1` for them and checks its version.

## Files

| file | role |
| --- | --- |
| `run.sh` | clone at the tag, install tools, build, swap the seam, run, classify |
| `nub-as-pnpm.sh` | the seam replacement; `__NUB_BIN__` and `__SHIM_LOG__` are substituted at swap time |
| `junit.mjs` | readers for nextest's JUnit report and the allowlist |
| `classify.mjs` | classifies each failing test against the allowlist |
| `gen-allowlist.mjs` | regenerates `allowlist.txt` from a report, one reason per entry |
| `allowlist.txt` | known failures with their category and reason (generated) |

## Running it

```bash
# Needs cargo (rustup; the clone pins its own toolchain), node, npm, git and curl.
tests/pnpm-conformance/run.sh target/debug/nub

# A subset, passed through to nextest (stale entries are not reported):
tests/pnpm-conformance/run.sh target/debug/nub -E 'test(/^root::/)'

# Keep the clone and build between runs:
CONF_WORK_DIR=/tmp/pnpm-conf tests/pnpm-conformance/run.sh target/debug/nub
```

The first run clones pnpm/pnpm, installs its root workspace, and compiles the suite, which is a large Rust build; run it on a Linux builder rather than a laptop. The run strips every `npm_*`, `NPM_CONFIG_*`, `pnpm_config_*` and `PNPM_*` variable and gives the tools a private `HOME` and XDG tree under the work directory.

## The allowlist

`allowlist.txt` is generated from a full run, never edited by hand. Generating it takes three files from the work directory: the report of the run against Nub, the seam's invocation log (one line per call: test name, the project identity Nub picks for the cwd, the cwd, the arguments), and a baseline report from the same suite run against pnpm itself with `CONF_BASELINE=1`:

```bash
CONF_WORK_DIR=/tmp/pnpm-conf CONF_BASELINE=1 tests/pnpm-conformance/run.sh target/debug/nub
CONF_WORK_DIR=/tmp/pnpm-conf tests/pnpm-conformance/run.sh target/debug/nub
node tests/pnpm-conformance/gen-allowlist.mjs /tmp/pnpm-conf/junit.xml \
  /tmp/pnpm-conf/shim-invocations.log /tmp/pnpm-conf/junit-baseline.xml > tests/pnpm-conformance/allowlist.txt
```

Each line is `<module>::<test>  # <category>: <reason>`. `gen-allowlist.mjs` assigns every failure to the first rule that accepts it, and exits non-zero if any failure matches no rule, so a new kind of failure needs a written reason before it can be allowlisted. The categories:

| category | meaning |
| --- | --- |
| `environment` | the test also fails against pnpm itself on the same runner |
| `bug/…` | a divergence in a pnpm project with no design reason; a product defect |
| `divergence/…` | Nub answers with its own command or version in a pnpm project, by its current routing |
| `identity/nub-project` | the fixture has no pnpm marker, so Nub's own identity applies |
| `seam/…` | behavior the seam cannot show, such as pnpm's shims that copy its own executable |

The classifier reports three outcomes:

- **SURPRISE** (fails the run): a failing test with no entry.
- **STALE-ALLOW** (reported only): an entry whose test now passes or no longer exists.
- **KNOWN**: a failing test with an entry.

Regenerate after a pnpm version change or a Nub change that moves the failure set; the diff shows which failures appeared or disappeared.

## CI

`.github/workflows/pnpm-conformance.yml` runs the harness nightly and on demand. It is not a pull-request gate: it clones an external repository, compiles a large workspace, and its mocked registry proxies real npm packages from the public registry.

## Updating the pin

When Nub's pnpm target version changes, change `PNPM_TAG` in `run.sh` and the workflow, run the full suite, and regenerate the allowlist.
