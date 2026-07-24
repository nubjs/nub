# Native-dependency floor harness

Tests nub's default-trust floor policy end-to-end against real packages that run native build scripts — the surface that neither unit tests nor the brand sweep exercise.

## What this tests

| Package | Build type | Expected outcome |
| --- | --- | --- |
| `esbuild@0.28.0` | Downloads a platform binary in postinstall | Floor-allowed: build runs, `defaultTrust` disclosure emitted, binary materialized |
| `better-sqlite3@11.10.0` | Compiles a C++ N-API addon via node-gyp | Floor-allowed: build runs, addon loadable, disclosed in same warning |
| `core-js@3.40.0` | Runs build scripts not on the floor allowlist | Floor-denied: build blocked, `WARN_NUB_IGNORED_BUILD_SCRIPTS` emitted naming core-js |

The three-part pass condition for floor-allowed builds is:
1. **Allowed** — `nub install` exits 0 and the module materializes.
2. **Disclosed** — the `defaultTrust` warning names the package. The floor is not a silent allow path.
3. **Loadable** — `node -e require(...)` succeeds — the native artifact actually runs.

## Prerequisites

`node-gyp` requires a C++ compiler and Python 3 to compile `better-sqlite3`. On most CI runners this is pre-installed. Locally:

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

The `native-deps` CI job (`.github/workflows/native-deps.yml`) runs on ubuntu-latest on every push touching `crates/`, `runtime/`, `vendor/aube`, or the harness itself. It is a single-shard ubuntu job because the floor behavior is not OS-specific — the deny/allow logic is pure Rust, and the binary artifacts (esbuild binary, better-sqlite3 addon) are platform-resolved at install time.

**Why separate workflow:** the harness performs a real `npm registry` install (esbuild + better-sqlite3 are non-trivial — better-sqlite3 compiles native code). That makes the job slow and network-dependent. Keeping it in a separate workflow with a path filter ensures it only fires when the PM engine or native-build path changed, not on every unrelated commit.
