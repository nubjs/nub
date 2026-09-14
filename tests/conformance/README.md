# Drop-in conformance harness

Proves that nub and real pnpm 12 agree on the same project, byte for byte, in both directions. Every fixture is staged as a **pnpm project** — `packageManager` pins the version nub embeds — so nub serves it under pnpm's own identity, which is the identity this parity claim is about. The round trip through **nub's** identity (`nub.lock`, `nub pm use nub` / `nub pm use pnpm`) is a different contract and lives in [`tests/lockfile-conformance/`](../lockfile-conformance/README.md).

## The two directions

**Direction A — nub READS pnpm's lockfile:** real pnpm writes `pnpm-lock.yaml`, then `nub install --frozen-lockfile` must install from it and leave every direct dependency in `node_modules`.

**Direction B — pnpm READS nub's lockfile:** nub writes `pnpm-lock.yaml`, then `pnpm install --frozen-lockfile` must accept it without rewriting a byte (`cmp`), and leave every direct dependency in `node_modules`.

Real pnpm is fetched through `npx` at the pinned version rather than taken off `PATH`. A `PATH` pnpm is whatever the box happens to carry, and a different major has a different store layout and a different settings home — so it judges a different product. The pin is `PNPM_PIN`, default `12.4.1`.

A `packageManager` pin naming any pnpm version **other** than the embedded one is a different path entirely: nub provisions that pnpm and delegates the command to it, so the harness would be testing pnpm against itself.

## Fixtures

| fixture | what it exercises |
| --- | --- |
| `simple` | plain registry deps with a transitive overlap — `debug` → `ms` and `ms` both direct, so version-range dedup is exercised |
| `scoped` | scoped packages, including scoped transitives |
| `peers` | peer dependencies — `react-dom@18` on `react@18` |
| `peer-meta` | optional peers declared through `peerDependenciesMeta` |
| `deep-graph` | a large transitive graph (`express`) |
| `alias` | an `npm:` aliased dependency pointing at a different version of the same package |
| `alias-scoped` | an `npm:` alias whose target is scoped |
| `file-dep` | a `file:` local dependency |
| `git-dep` | a git dependency pinned to a full commit SHA, plus a `github:owner/repo#ref` shorthand |
| `dist-tag-spec` | a `latest` dist-tag and a `*` wildcard as declared specs |
| `range-forms` | the four declared range forms isolated — exact, caret, tilde, compound |
| `optional-deps` | `optionalDependencies` with platform-conditional installs |
| `platform-optional` | an `optionalDependency` gated by `os`/`cpu` that does NOT install on the host |
| `postinstall` | a dependency with a real postinstall script (`@parcel/watcher`), decided `true` in `allowBuilds` so it passes the approve-builds gate |
| `workspace` | a multi-package `workspaces` monorepo with a `workspace:*` internal dep, so the lockfile is multi-importer |
| `workspace-dedup` | one dependency resolving to two different majors across members |
| `empty-root-importer` | a monorepo whose root package has no dependencies while its children do |
| `catalog` | a pnpm catalog (`ms: catalog:`) and the `catalogs:` lockfile section |
| `overrides-nested` | a scoped nested override (`debug>ms`) declared in `pnpm-workspace.yaml` |
| `overrides-ref` | a `pnpm.overrides` `$`-ref recorded resolved in the lockfile but literal in `package.json` |
| `patched-deps` | `patchedDependencies` — a real patch of `is-odd@3.0.1`, and the hash/path patch map |
| `patched-deps-no-newline` | a patch whose final hunk context line is the file's last line with no trailing newline, where pnpm omits the `\ No newline at end of file` marker; GNU `patch` and pnpm apply it, a strict byte-exact applier rejects it |
| `injected-deps` | a workspace consuming a sibling via `workspace:*` plus `dependenciesMeta.injected`. The hard-copy-vs-symlink layout is config-sensitive and outside the lockfile, so it is not asserted |

**`overrides-ref` passes in both directions now, and the reason is a pnpm 12 change.** pnpm 12 no longer reads the `pnpm` field in `package.json` at all — it warns and ignores it — so neither side honors the `pnpm.overrides` block and both write the same override-free lockfile. Under pnpm 10 this fixture's Direction B was a permanent skip, because pnpm honored the block and nub did not.

**`injected-deps` Direction B is skip-by-design**, an ecosystem impossibility rather than a nub bug: pnpm cannot round-trip `dependenciesMeta.injected` under `--frozen-lockfile` even from its OWN lockfile. It writes an importer block that omits the `dependenciesMeta` field, then on frozen-verify demands it back and self-rejects. nub's lockfile is byte-identical here, so no lockfile nub could write would frozen-pass. Direction A is the meaningful guard and runs.

**Features deliberately NOT given a fixture:** bun's `minimumReleaseAge` and bun scoped-registry URLs were each probed empirically and found to be resolver-time config that never reaches any lockfile, so a differential fixture would be a no-op gate.

## How to run

```sh
tests/conformance/run.sh                             # target/release/nub, every fixture
tests/conformance/run.sh target/debug/nub            # explicit binary
tests/conformance/run.sh target/release/nub peers    # a single fixture
DIRECTIONS=A tests/conformance/run.sh                # one direction
PNPM_PIN=12.4.0 tests/conformance/run.sh             # judge against another pnpm
KEEP=1 tests/conformance/run.sh                      # keep the sandbox for forensics
SANDBOX_ROOT=/tmp/my-sandbox tests/conformance/run.sh
```

Requirements: `node` and `npx` on `PATH`, and network access to the npm registry — these are real installs.

The sandbox redirects `HOME` and every `XDG_*` dir to a fresh temp root, so no dev-box `.npmrc`, cache or store leaks in and nothing leaks out. It also unsets every `npm_config_*` / `PNPM_*` variable the environment carries, since either could steer an install. On failure the sandbox is kept, with the per-leg log at `logs/<fixture>--<dir>.log` and the staged project at `runs/<fixture>--<dir>/`.

Known-red scenarios live in `expected-failures.txt` as `<fixture> <dir> <reason>`. The list must SHRINK: a listed scenario that passes fails the run (XPASS-STALE), so the fix and the entry's deletion land together.

## Relation to the sibling harnesses

- [`tests/lockfile-conformance/`](../lockfile-conformance/README.md) — nub's OWN identity: `nub.lock`, and the `pm use nub` / `pm use pnpm` round trip. Direction B only.
- [`tests/mutation/`](../mutation/README.md) — the write path: `nub add` / `remove` / `update`, judged for semantic graph equality against real pnpm's answer to the same mutation.
- `cmdflag/`, `frontdoor/` and `registry/` in this directory are separate harnesses about the CLI surface rather than the lockfile.

Direction A lives only here. A lockfile nub cannot READ is as broken as one nobody else can read, and nothing else in the repo covers it.
