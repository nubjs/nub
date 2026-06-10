# Lockfile conformance harness — real PMs judge nub's lockfiles

nub's PM surface is aube's command layer embedded as an engine, and its lockfile story is "write the *foreign* format in place" — `pnpm-lock.yaml` by default, `package-lock.json` / `bun.lock` when the project (or `defaultLockfileFormat`) says so. The only honest judge of those files is the real package manager that owns the format, so this harness has nub produce each lockfile in a hermetic sandbox and then makes the real, pinned PM accept it. It exists because byte-plausible lockfiles can still be semantically wrong — the founding example is the P1 blocker where `hoist_auto_installed_peers` wrote auto-installed peers (`react`) as root importer specifiers absent from `package.json`, which real pnpm rejects with `ERR_PNPM_OUTDATED_LOCKFILE`.

## The gates

- **pnpm** — nub writes `pnpm-lock.yaml`; real pnpm (pinned, via `npx pnpm@<pin>`) must accept it with `install --frozen-lockfile`, and a follow-up *mutable* `pnpm install` must leave the lockfile byte-identical (zero churn — pnpm already considers it canonical).
- **npm** — nub writes `package-lock.json`; real `npm ci` (pinned via `npx npm@<pin>`) must accept it, and a follow-up `npm install` must leave it byte-identical.
- **bun** — nub writes `bun.lock`; real `bun install --frozen-lockfile` must accept it.
- **yarn** — inverted: nub's write gate must *refuse* to mutate a detected `yarn.lock` (yarn write fidelity is unproven in the engine, so refusal is the contract). The leg seeds a classic `yarn.lock`, attempts `nub add`, and passes only on non-zero exit + an error that names `yarn.lock` + a byte-identical `yarn.lock` + no stray lockfile of another format.

Every nub invocation's captured output is additionally swept for the string `aube` — the conformance projects double as brand-leak canaries (the systematic guard is `tests/brand-sweep/run.sh`; this is the free local complement on real-install output).

## The corpus

| fixture | what it exercises | nub command |
| --- | --- | --- |
| `simple` | plain registry deps, direct + transitive overlap (`debug` → `ms`) | `install` |
| `workspace` | 3-member workspace, `workspace:*` and `workspace:^` protocol, scoped member names, both `pnpm-workspace.yaml` and the `workspaces` field | `install` |
| `peer-heavy` | the blocker's exact repro — clean project, `nub add react-dom@18.3.1 chokidar@3.6.0`, plus `react-redux` for `@types/react`-style *optional* peers (`peerDependenciesMeta`) | `add` |
| `overrides` | npm-style top-level `overrides` + `pnpm.overrides` pinning a transitive (`supports-color`) outside its requested range | `install` |
| `platform-optional` | platform-conditional optionals: `esbuild`'s per-platform optional deps + direct `fsevents` (darwin-only) | `install` |
| `scoped` | scoped packages, including scoped transitives (`@babel/code-frame`) | `install` |
| `git-dep` | a git dependency pinned to a tag (`github:vercel/ms#2.1.3`) | `install` |

## Red list

[`expected-failures.txt`](expected-failures.txt) holds the scenarios that are red *on purpose* — same discipline as `tests/aube-bats/known-gaps.txt`: each line is `<fixture> <format> <reason>`, the list must shrink, and a listed scenario that starts passing fails the run as `XPASS-STALE` so the fix and the entry deletion land in the same commit. Every entry is a fork-fixable lockfile-writer bug; the founding one is `peer-heavy pnpm` (the P1 phantom-peer-specifier blocker, fixed fork-side in task P; the vendor pin bump flips it green). Scenarios the *ecosystem* makes impossible are a different category: they live in `skip_reason()` in `run.sh` and report `SKIP (by design)` — today that's `workspace × npm`, because npm errors `EUNSUPPORTEDPROTOCOL` parsing `workspace:` specs in the member manifests themselves, so no lockfile nub writes can ever satisfy `npm ci` there.

## Running locally

```sh
cargo build -p nub-cli
tests/aube-conformance/run.sh target/debug/nub                 # full matrix
tests/aube-conformance/run.sh target/debug/nub peer-heavy      # one fixture
FORMATS="pnpm yarn" tests/aube-conformance/run.sh target/debug/nub   # subset of legs
SANDBOX_ROOT=/abs/path KEEP=1 tests/aube-conformance/run.sh target/debug/nub  # keep evidence
```

Requirements: network access to registry.npmjs.org and github.com (these are real installs — that is the point), `node`/`npx` on `PATH`, and `bun` on `PATH` for the bun leg. pnpm and npm are exact-pinned inside `run.sh` (`PNPM_PIN`, `NPM_PIN`) and fetched per-run via npx into the sandbox; bun is pinned in CI via `oven-sh/setup-bun` and only soft-checked locally. The sandbox redirects `HOME` and all `XDG_*` dirs to absolute paths under a fresh temp root (aube mis-handles relative `XDG_DATA_HOME`), so nothing from the dev box leaks in and nothing leaks out; on failure the sandbox is kept and each scenario's full log (`logs/<fixture>--<format>.log`) plus the staged project (`runs/<fixture>--<format>/`) are available for forensics.

CI: the `aube-conformance` job in [`.github/workflows/aube-parity.yml`](../../.github/workflows/aube-parity.yml), one ubuntu shard, gated on the vendor gitlink + this harness's paths (same cost rationale as the parity job — the dominant variable in the verdict is the engine pin; nub-side `pm_engine` changes that need a conformance check can always re-run it locally or touch the harness).
