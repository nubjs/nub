# Foreign-lockfile conversion harness

A project arriving on npm, yarn or bun has to be able to leave. nub writes none of those three formats, so there is nothing to convert *to* them — the harness proves the two destinations that exist, with a real tool judging each one.

## The two destinations

**→ nub.** `nub pm migrate` converts the foreign lockfile to `nub.lock` and removes the source, once. `nub install --frozen-lockfile` must then install from it with every direct dependency materialized — including in every workspace member, which is why the assertion walks each `package.json` rather than only the root.

Then the claim that makes `nub.lock` portable is judged by something other than nub: `nub.lock` is pnpm v9 format, so the harness copies the project, renames the file to `pnpm-lock.yaml`, and requires real pnpm to `--frozen-lockfile` install from those same bytes. Without that step "pnpm v9 format" is a claim only nub's own reader ever tests.

The judge copy declares no package manager, and that is load-bearing rather than incidental. A manifest that pins pnpm makes pnpm demand its own env lockfile document — the `packageManagerDependencies` block it uses to provision itself — which `nub.lock` legitimately does not carry, because nub manages no package-manager versions. pnpm then refuses the frozen install before it ever reads the project graph, and the leg would be measuring the declaration rather than the lockfile.

The fixture's `pnpm-workspace.yaml`, where it has one, is removed for this destination: it is one of the markers that makes a project pnpm-incumbent, so leaving it in place means `pm migrate` correctly targets pnpm's format and never writes a `nub.lock` at all. The workspace still resolves, from the neutral `workspaces` field.

**→ pnpm.** `nub pm use pnpm@<pin>` converts the foreign lockfile to `pnpm-lock.yaml` and declares pnpm. Real pnpm must then `--frozen-lockfile` install from it.

Both destinations also assert the conversion left no stale lockfile behind. A source left in place recreates the ambiguity the conversion existed to remove, and the next tool to run would pick a different one.

## Sources and the judge

The sources are the three FOREIGN formats — `npm`, `yarn`, `bun` — driven off `PATH`, because the point is whatever lockfile a real project actually arrives carrying. A missing source prints a `NOTE:` line and its legs are skipped; the suite does not fail over an absent tool.

pnpm is not among them. A `pnpm-lock.yaml` is already the format `nub.lock` is, so `nub pm migrate` refuses it outright — "there is nothing to migrate" — and the pnpm-to-nub hand-over is `nub pm use nub`, which [`tests/lockfile-conformance/`](../lockfile-conformance/README.md) owns.

Real pnpm — the judge, and the `pm use` pin — is fetched through `npx` at `PNPM_PIN` (default `12.4.1`, the version nub embeds). A `PATH` pnpm is whatever the box happens to carry, and a different major has a different store layout and a different settings home, so it would judge a different product.

## Fixtures

| fixture | what it exercises |
| --- | --- |
| `simple` | plain registry deps with a transitive overlap |
| `peers` | peer dependencies, so the converted graph has to carry peer resolutions |
| `empty-root-importer` | a `workspaces` monorepo whose root package has no dependencies while its children do — the shape where a root-only `node_modules` assertion passes vacuously |

## How to run

```sh
tests/conversion/run.sh                          # target/release/nub, every fixture
tests/conversion/run.sh target/debug/nub         # explicit binary
tests/conversion/run.sh target/debug/nub simple  # a single fixture
TARGETS=nub tests/conversion/run.sh              # one destination
SKIP_YARN=1 SKIP_BUN=1 tests/conversion/run.sh   # npm and pnpm sources only
KEEP=1 tests/conversion/run.sh                   # keep the sandbox for forensics
```

Requirements: `node` and `npx` on `PATH`, network access to the npm registry, and whichever source package managers you want exercised.

The sandbox redirects `HOME` and every `XDG_*` dir to a fresh temp root, and unsets every `npm_config_*` / `PNPM_*` variable the environment carries. On failure the sandbox is kept, with the per-leg log at `logs/<fixture>--<source>--<target>.log` and the staged project at `runs/<fixture>--<source>--<target>/`.

Known-red scenarios live in `expected-failures.txt` as `<fixture> <source> <target> <reason>`. The list must SHRINK: a listed scenario that passes fails the run.

## Relation to the sibling harnesses

- [`tests/conformance/`](../conformance/README.md) — nub and real pnpm on the same pnpm project, both directions, 23 fixtures.
- [`tests/lockfile-conformance/`](../lockfile-conformance/README.md) — nub's own identity, and the `pm use nub` / `pm use pnpm` round trip.
- [`tests/mutation/`](../mutation/README.md) — the write path: `nub add` / `remove` / `update` judged for semantic graph equality against real pnpm.
