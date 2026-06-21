# Drop-in PM conformance harness

This harness proves nub is a true drop-in package manager in both directions. It exists because the only honest judge of lockfile compatibility is the real package manager that owns the format.

## The two directions

**Direction A — nub READS others:** the real PM writes its native lockfile, then `nub install --frozen-lockfile` must succeed without touching the lockfile and produce a correct `node_modules`.

**Direction B — others READ nub:** nub writes the lockfile, then the real PM must accept it with its frozen-install command (`pnpm install --frozen-lockfile` / `npm ci` / `bun install --frozen-lockfile`) without rewriting it (zero churn).

Both yarn directions are now the normal two-way check. Direction B for yarn writes a **classic (v1) yarn.lock** — the leg seeds resolution state with a real pnpm resolve, runs `nub pm use yarn` to convert it into a classic yarn.lock, then asserts real `yarn install --frozen-lockfile` accepts that file unchanged (zero churn) with a correct `node_modules`. This replaced the old "nub must refuse to write yarn.lock" contract: the classic writer was empirically proven against real yarn (1.13 and 1.22 both frozen-accept a nub-written yarn.lock from any source with zero churn — yarn v1's frozen check validates that the manifest is satisfiable by the lockfile, not byte-identity, so the writer's lossy bits — no `resolved` URL, resolved versions in place of declared ranges — are tolerated), so the write gate for classic yarn was lifted.

**Yarn Berry (v2+) write fidelity.** The Berry writer (`write_berry`) was made byte-faithful to yarn 4's on-disk layout (metadata `version: 10`, the root `<name>@workspace:.` block sorted in among the packages, unquoted `version`/`checksum`/bare-key scalars, quoted `resolution`/`npm:` values, headers carrying only the declared ranges). On a yarn→nub→yarn round-trip the output is **byte-identical** to real yarn 4 and passes `yarn install --immutable` with zero churn — including the keyed blake2b `checksum`, which round-trips verbatim (it is parsed from the source lock, never synthesized). Two caveats remain, both stricter than yarn v1 because Berry's `--immutable` is a strict re-serialize-and-compare: (1) **cross-format conversion to Berry churns** — pnpm/npm→Berry can't recover the keyed `checksum` or the original declared ranges, so yarn re-derives them; the proven Berry path is round-trip/same-flavor, not conversion. (2) **packages declaring a `bin:` lose it** — aube's lockfile model doesn't track package `bin` entries, so yarn re-adds the `bin:` section under `--immutable`. For these reasons the conformance harness exercises the *classic* writer (the host's `yarn` is v1); the Berry round-trip fidelity is covered by `aube-lockfile`'s `test_write_berry_output_matches_yarn4_layout` and the proptest round-trip.

## Tool/version matrix (as of 2026-06-11)

All 16 legs PASS on this host:

| fixture | dir | pm | pm-version | result |
| --- | --- | --- | --- | --- |
| simple | A | npm | 11.13.0 | PASS |
| simple | A | pnpm | 10.15.1 | PASS |
| simple | A | yarn | 1.13.0 | PASS |
| simple | A | bun | 1.3.14 | PASS |
| simple | B | npm | 11.13.0 | PASS |
| simple | B | pnpm | 10.15.1 | PASS |
| simple | B | yarn | 1.13.0 | PASS |
| simple | B | bun | 1.3.14 | PASS |
| peers | A | npm | 11.13.0 | PASS |
| peers | A | pnpm | 10.15.1 | PASS |
| peers | A | yarn | 1.13.0 | PASS |
| peers | A | bun | 1.3.14 | PASS |
| peers | B | npm | 11.13.0 | PASS |
| peers | B | pnpm | 10.15.1 | PASS |
| peers | B | yarn | 1.13.0 | PASS |
| peers | B | bun | 1.3.14 | PASS |

The pnpm-11 leg runs separately via `run-pnpm11.sh` (uses `npx pnpm@latest-11`). See the pnpm-11 matrix below.

## pnpm-11 matrix (as of 2026-06-11)

pnpm 11.6.0 via `npx pnpm@latest-11`. **Finding: pnpm 11 still uses `lockfileVersion: '9.0'` — the format version did not change between pnpm 10 and 11.** All 4 legs pass.

| fixture | dir | pm | pm-version | lockfileVersion | result |
| --- | --- | --- | --- | --- | --- |
| simple | A | pnpm | 11.6.0 | 9.0 | PASS |
| simple | B | pnpm | 11.6.0 | 9.0 | PASS |
| peers | A | pnpm | 11.6.0 | 9.0 | PASS |
| peers | B | pnpm | 11.6.0 | 9.0 | PASS |

Direction A: pnpm 11 writes `pnpm-lock.yaml` (lockfileVersion 9.0) → nub `--frozen-lockfile` installs from it — succeeds. Direction B: nub writes `pnpm-lock.yaml` (lockfileVersion 9.0, with an extra `time:` section pnpm 11 also accepts) → pnpm 11 `--frozen-lockfile` reports "Lockfile is up to date, resolution step is skipped" and zero-churns.

## Fixtures

| fixture | what it exercises |
| --- | --- |
| `simple` | plain registry deps, direct + transitive overlap (`debug` → `ms`), a devDep |
| `peers` | peer dependencies — `react-dom@18` with a `peerDep` on `react@18`, exercising peer resolution and auto-install |
| `has-install-script` | npm's per-package verbatim keys — Direction A asserts nub reads a real npm `hasInstallScript` (on `esbuild`) and re-emits it with zero churn; scoped to npm via `skip_reason()` since no other PM's lockfile encodes them. (The `deprecated`/`inBundle`/`hasShrinkwrap`/`bundleDependencies` siblings round-trip through the same verbatim path; `aube-lockfile`'s `test_roundtrip_preserves_npm_verbatim_meta_fields` covers all five.) |

Both are small by design; the goal is a fast, signal-dense suite. The aube-conformance harness (`tests/aube-conformance/`) covers larger fixtures (workspaces, overrides, platform-conditionals, patched deps, git deps) for the Direction B side; this harness adds Direction A and is the regression guard for the bidirectional contract.

### Differential feature fixtures (pnpm-only)

These exercise the bug-prone pnpm lockfile fields. Each is scoped to pnpm via `skip_reason()` — the field has no representation in an npm/bun/yarn lockfile, so running it against those PMs would test nothing.

| fixture | what it exercises | status |
| --- | --- | --- |
| `catalog` | a pnpm catalog (`ms: catalog:` resolved through the default catalog in `pnpm-workspace.yaml`); the `catalogs:` lockfile section | PASS A+B |
| `overrides-nested` | a scoped nested override (`debug>ms: 2.1.3` in `pnpm-workspace.yaml`); the `overrides:` lockfile block | PASS A+B |
| `patched-deps` | `patchedDependencies` (a real `is-odd@3.0.1` patch declared in `pnpm-workspace.yaml`); the hash/path patch map | PASS A+B |
| `patched-deps-no-newline` | issue #25: a `patchedDependencies` patch (a real `pnpm patch` of `is-odd@3.0.1`) whose final hunk context line is the patched file's last line with no trailing newline, where pnpm omits the `\ No newline at end of file` marker. `is-odd`'s `README.md` ships without a trailing newline. pnpm + GNU `patch` apply this; `git apply` and a strict byte-exact applier reject it (`error applying hunk #1`). Guards that nub applies it with pnpm's tolerance. | A: PASS |
| `overrides-ref` | a `pnpm.overrides` `$`-ref (`ms: $ms`) recorded resolved in the lockfile but literal in `package.json` | A: PASS (was #16); B: skip-by-design |
| `injected-deps` | a workspace consuming a sibling via `workspace:*` + `dependenciesMeta.injected`; guards that nub frozen-reads pnpm's injected-workspace lockfile and materializes the dep. The hard-copy-vs-symlink layout is config/version-sensitive and outside the lockfile, so it is not asserted. | A: PASS; B: skip-by-design |

`patched-deps` Direction A and B both pass. The write-side fix (#23) — nub now emits the `{hash, path}` map form pnpm 10.x requires — shipped in v0.1.9 (aube pin `855e113`). No gate remains in `expected-failures.txt`.

`overrides-ref` Direction A now passes — the `$`-ref read-as-drift bug (#16) was fixed by the vendor/aube pin bump `c948a38`; the fixture is now its regression guard. Direction B is an intended brand-boundary divergence, not a bug: this fixture carries a `pnpm.overrides` block in `package.json`, and a nub-identity project consumes only neutral cross-tool fields (`overrides`/`resolutions`), never another PM's branded config — so nub writes an override-free lockfile that real pnpm rejects with `ERR_PNPM_LOCKFILE_CONFIG_MISMATCH`. (The `overrides-nested` fixture sidesteps this by declaring the override in `pnpm-workspace.yaml`, where nub mirrors pnpm faithfully.)

`injected-deps` Direction B is skip-by-design, an ecosystem impossibility rather than a nub bug: pnpm 10.15.1 cannot round-trip `dependenciesMeta.injected` under `--frozen-lockfile` even from its OWN lockfile. It writes an importer block that omits the `dependenciesMeta` field, then on frozen-verify demands it back and self-rejects with `ERR_PNPM_OUTDATED_LOCKFILE` ("importer dependencies meta (undefined) doesn't match package manifest"). nub's lockfile is byte-identical to pnpm's here, so no nub-written lockfile could frozen-pass. Direction A — nub frozen-reads pnpm's injected-workspace lockfile and materializes the dep — is the meaningful guard and passes. (The migration side — `nub pm use nub` no longer refusing an injected project — is covered by the `use_nub_migrates_with_injected_deps_and_preserves_meta` unit test.)

**Features deliberately NOT given a fixture:** `peerDependenciesMeta` optional peers, bun's `minimumReleaseAge`, and bun scoped-registry URLs were each probed empirically and found to be **resolver-time config that is never encoded in any PM's lockfile** — the field never reaches the compared lockfile, so a differential fixture would be a no-op gate (it would only re-test plain install, already covered by `simple`/`scoped`). The optional-peer install path is covered by `peer-meta`.

## How to run

```sh
tests/conformance/run.sh                          # uses target/release/nub, runs all fixtures
tests/conformance/run.sh target/debug/nub         # explicit binary
tests/conformance/run.sh target/release/nub peers # single fixture
KEEP=1 tests/conformance/run.sh                   # keep sandbox for forensics
SANDBOX_ROOT=/tmp/my-sandbox tests/conformance/run.sh  # reuse/inspect a specific sandbox
SKIP_YARN=1 tests/conformance/run.sh              # skip yarn legs

# pnpm-11 leg (separate script — uses npx pnpm@latest-11, no global install needed)
tests/conformance/run-pnpm11.sh
tests/conformance/run-pnpm11.sh target/debug/nub
PNPM11_VERSION=11.6.0 tests/conformance/run-pnpm11.sh   # pin exact version
KEEP=1 tests/conformance/run-pnpm11.sh                  # keep sandbox
```

Requirements: network access to the npm registry (these are real installs), `node` on PATH, and each PM you want to exercise on PATH. Missing PMs print a `NOTE:` line and their legs are skipped — the suite does not fail on a missing tool.

`run-pnpm11.sh` requires `npm`/`npx` on PATH (for `npx pnpm@latest-11`). If npx is absent, set `PNPM11_BIN=<path>` to an explicit pnpm 11 binary. Docker mode (`PNPM11_USE_DOCKER=1`) is also supported as a last resort but requires Docker Desktop running.

The sandbox redirects `HOME` and all `XDG_*` dirs to a fresh temp root so no dev-box `.npmrc`, caches, or PM stores leak in and nothing leaks out. On failure the sandbox is kept and the per-leg log (`logs/<fixture>--<dir>--<pm>.log`) and staged project (`runs/<fixture>--<dir>--<pm>/`) are available for forensics.

## Relation to aube-conformance

`tests/aube-conformance/` is the comprehensive Direction B suite (nub writes → real PM reads), spanning eight fixtures and a `nub pm use` round-trip leg. This harness is complementary:

- **New in this harness:** Direction A (real PM writes → nub reads frozen). This direction was not tested anywhere before 2026-06-11.
- **Shared coverage:** Direction B with the `simple` and `peers` fixtures is redundant with what `aube-conformance` covers — both suites pass, and that's healthy (two independent witnesses for the same property).
- **Where to add harder cases:** complicated fixtures (workspaces, overrides, platform-conditionals, patched deps) belong in `tests/aube-conformance/fixtures/`; run both harnesses to cover both directions for those shapes.
