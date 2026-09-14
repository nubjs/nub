# Lockfile conformance harness — real pnpm judges nub's lockfiles

nub's package manager is pnpm 12's engine. A pnpm project gets `pnpm-lock.yaml`, and a nub project gets `nub.lock`, the same v9 format under nub's name. A lockfile can look right and still be wrong, and the honest judge of either file is real pnpm, so this harness has nub write each lockfile in a hermetic sandbox and then makes the pinned pnpm accept it. The founding example: auto-installed peers (`react`) written as root importer specifiers absent from `package.json`, which real pnpm rejects with `ERR_PNPM_OUTDATED_LOCKFILE`.

## The legs

- **pnpm**: the fixture declares `packageManager: pnpm@<pin>`. nub writes `pnpm-lock.yaml`; real pnpm must accept it with `install --frozen-lockfile`, and a follow-up mutable `pnpm install` must leave it byte-identical.
- **nub**: the fixture declares nothing and carries no pnpm-named file. nub writes `nub.lock` and nothing pnpm-named, and a frozen `nub install` must work from it. The same lockfile, renamed to `pnpm-lock.yaml` in a copy of the project that declares pnpm, must then pass the pnpm leg's checks.
- **switch**: the pnpm leg's mutation, then `nub pm use nub` (a `nub.lock`, no pnpm-named file, a working frozen install), then `nub pm use pnpm@<pin>`, after which real pnpm judges the result as in the pnpm leg.

Every nub step in a nub project is also swept for `ERR_PNPM_` and `WARN_PNPM_`. nub reports the engine's codes under its own prefix there, so either spelling is a leak. A pnpm project keeps pnpm's own codes, as pnpm 12 prints them.

## The corpus

| fixture | what it exercises | nub command |
| --- | --- | --- |
| `simple` | plain registry deps, direct + transitive overlap (`debug` → `ms`) | `install` |
| `workspace` | 3-member workspace, `workspace:*` and `workspace:^` protocol, scoped member names; the pnpm project reads `pnpm-workspace.yaml`, the nub project the `workspaces` field | `install` |
| `peer-heavy` | a clean project, `nub add react-dom@18.3.1 chokidar@3.6.0`, plus `react-redux` for `@types/react`-style optional peers (`peerDependenciesMeta`) | `add` |
| `overrides` | a transitive (`supports-color`) pinned outside its requested range; the nub project reads the top-level `overrides` field, the pnpm project `overrides` in `pnpm-workspace.yaml` | `install` |
| `platform-optional` | platform-conditional optionals: `esbuild`'s per-platform optional deps + direct `fsevents` (darwin-only); esbuild's install script is decided `false`, in `allowScripts` for the nub project and `allowBuilds` for the pnpm project | `install` |
| `scoped` | scoped packages, including scoped transitives (`@babel/code-frame`) | `install` |
| `git-dep` | a git dependency pinned to a tag (`github:vercel/ms#2.1.3`) | `install` |
| `patched` | the full patch workflow — `nub patch ms@2.1.3` → edit → `nub patch-commit`, then the install must both pass and link the patched content | `install` + `patch` + `patch-commit` |

## Red list

[`expected-failures.txt`](expected-failures.txt) holds the scenarios that are red on purpose. Each line is `<fixture> <leg> <reason>`, the list must shrink, and a listed scenario that starts passing fails the run as `XPASS-STALE`, so the fix and the entry's deletion land in the same commit.

## Running locally

```sh
cargo build -p nub-cli
tests/lockfile-conformance/run.sh target/debug/nub                  # full matrix
tests/lockfile-conformance/run.sh target/debug/nub peer-heavy       # one fixture
LEGS="pnpm nub" tests/lockfile-conformance/run.sh target/debug/nub  # subset of legs
SANDBOX_ROOT=/abs/path KEEP=1 tests/lockfile-conformance/run.sh target/debug/nub  # keep evidence
```

Requirements: network access to registry.npmjs.org and github.com (these are real installs), and `node`/`npx` on `PATH`. pnpm is exact-pinned inside `run.sh` (`PNPM_PIN`) and fetched per run through npx into the sandbox. The sandbox points `HOME` and every `XDG_*` dir at absolute paths under a fresh temp root, so nothing from the machine leaks in and nothing leaks out. On failure the sandbox is kept, with each scenario's full log (`logs/<fixture>--<leg>.log`) and staged project (`runs/<fixture>--<leg>/`).

CI: the `conformance` job in [`.github/workflows/release.yml`](../../.github/workflows/release.yml), which `publish` needs.
