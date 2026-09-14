# Native-dependency builds harness

Tests nub's approve-builds gate end to end against real packages that run native build scripts — the surface that neither unit tests nor the brand sweep exercise. A nub project records which dependencies may run install scripts in `package.json` `allowScripts`, and nub runs exactly those, the way pnpm 12 runs `allowBuilds`.

## What this tests

| Case | Packages | Expected outcome |
| --- | --- | --- |
| Approved builds | `esbuild@0.28.0` (its postinstall checks for or downloads the platform binary) and `better-sqlite3@11.10.0` (fetches a prebuilt N-API addon or compiles one with node-gyp), both decided `true` | The install exits 0, both scripts run, both modules load |
| Frozen install into a fresh store | The same `package.json` and `nub.lock`, in a new HOME and store | Both scripts run again and both modules load |
| Undecided build | `core-js@3.40.0` with no decision | The install fails with `ERR_NUB_IGNORED_BUILDS`, naming `core-js@3.40.0` and `nub approve-builds`, as pnpm 12 fails |
| Denied build | `core-js@3.40.0` decided `false` | The script is skipped and the install exits 0 |

Every project runs with its own HOME and XDG directories. A store shared between projects would already hold the first install's finished builds, and a later install would link them instead of building.

## Prerequisites

`node-gyp` requires a C++ compiler and Python 3 to compile `better-sqlite3` where no prebuilt addon matches the Node version. On most CI runners these are pre-installed. Locally:

- **macOS**: `xcode-select --install`
- **Ubuntu**: `apt-get install -y build-essential python3`

## The loop

```sh
# Build nub first
cargo build -p nub-cli

# Run the harness
tests/native-deps/run.sh target/debug/nub

# Inspect the sandbox on failure
KEEP=1 tests/native-deps/run.sh target/debug/nub
```

`SANDBOX_ROOT=<dir>` pins the sandbox directory (implies `KEEP=1`).

## CI gating

The `native-deps` CI job (`.github/workflows/native-deps.yml`) runs on ubuntu-latest on every push touching `crates/`, `runtime/`, or the harness itself. It is a single-shard ubuntu job because the gate is not OS-specific, and the build artifacts (the esbuild binary, the better-sqlite3 addon) are resolved for the platform at install time.

**Why a separate workflow:** the harness performs real registry installs (esbuild and better-sqlite3 are non-trivial — better-sqlite3 may compile native code). That makes the job slow and network-dependent. Keeping it in a separate workflow with a path filter ensures it only fires when the PM engine or native-build path changed, not on every unrelated commit.
