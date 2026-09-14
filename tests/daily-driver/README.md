# Daily-driver smoke harness

Exercises the full nub surface against a real Vite + React + TypeScript project — the scenarios that pass on synthetic fixtures but can regress silently on real-world projects.

## Why this harness exists

Unit tests and synthetic fixtures validate individual code paths in isolation. But nub is a runtime augmenter: its value is that it handles real, messy projects without breaking. A real Vite project exercises:

- `nub install` with a genuine pnpm lockfile and real registry packages
- `nub run <script>` driving Vite (complex build tool, not a hello-world script)
- `nub run <script>` driving tsc and vitest
- `nub <file.ts>` with a real `node_modules` import resolved through nub's augmentation layer
- `nub --node <file.ts>` as the negative control: augmentation off → .ts syntax → parse error

## The scenarios

| Scenario | What breaks if this regresses |
| --- | --- |
| `install` | PM engine, pnpm lockfile write, real registry |
| `type-check` | `nub run` + tsc finding its config |
| `build` | `nub run` + Vite build (TS + JSX, not transpiled by nub) |
| `test` | `nub run` + vitest (imports node_modules at runtime) |
| `ts-run` | `nub <file.ts>` with a CJS dep import (nub's transpile + CJS-from-ESM path) |
| `node-off` | `nub --node` disables augmentation; .ts must be a parse error |

## The loop

```sh
# Build nub
cargo build -p nub-cli

# Run the harness (scaffolds the fixture on first run, installs from registry)
tests/daily-driver/run.sh target/debug/nub

# Reuse an existing fixture (skip the scaffold + install for iteration)
FIXTURE=/tmp/nub-daily-driver tests/daily-driver/run.sh target/debug/nub

# Keep the sandbox on success for inspection
KEEP=1 tests/daily-driver/run.sh target/debug/nub
```

`SANDBOX_ROOT=<dir>` pins the sandbox directory for `HOME`/`XDG_*` isolation.

## CI gating

The `daily-driver` CI job (`.github/workflows/daily-driver.yml`) runs on ubuntu-latest on every push touching `crates/`, `runtime/`, or the harness. It builds nub from source, scaffolds the fixture, installs from the real registry, and runs all six scenarios.

**Not in ci.yml:** the job performs real `npm` registry installs (Vite + React + vitest = ~300 packages). That's too heavy for the main CI matrix. The separate workflow + path filter means it only fires when nub itself changed, not on every doc or wiki edit.
