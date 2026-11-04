---
name: catalog-probe
description: Run the build-jail catalog probe — measure the minimum OS capability grant a package's lifecycle scripts need, sweep a worklist of packages, and collate the results into the catalog. Invoke (via the Skill tool) whenever you are about to run, restart, extend, or debug a grant sweep under tests/build-jail-search/, whenever a probe reports HARNESS-ERROR / HARNESS-CRASH / BROKEN-EVEN-WITH-EVERYTHING, whenever you change the catalog SHAPE (the Rust parser, the collator, or the synthesized cell catalogs must all move together), or whenever you are about to draw a conclusion from sweep results. Carries the failure modes that have each already cost a full sweep: a binary rebuilt mid-run without the override feature, a pre-flight check that validated an artifact adjacent to the question, editing the harness while a batch runs, and a 54%-silent-failure sweep whose survivors were reported as the corpus.
---

# Running the build-jail catalog probe

The probe measures the **minimum capability grant** a package's lifecycle scripts need, by walking a
54-state capability space in ascending cost order and taking the first state that reproduces an
unjailed control. Its output is the build-jail catalog.

Everything here is a failure that has already happened. None of it is hypothetical.

## Before you run anything

**1. Build with the override feature, or nothing works.**

```sh
scripts/rust-build.sh build -p nub-cli --profile fast \
  --features nub-cli/build-jail-catalog-override
```

**Any cargo command on a profile rewrites that profile's binary with ITS features.** A
`cargo test --profile fast` in another shell silently strips the override and every subsequent
package records a control failure. `run-batch.sh` snapshots the binary to defend against this, but a
bare `nub` invocation outside the batch still uses the live one.

**2. Never edit the harness while a batch is running.** Each package is a fresh
`node search.mjs` invocation, so an edit mid-sweep changes the harness under the remaining packages.
This has produced a sweep where the first half and the second half were measured by different code —
and, in the worst case, 54 of 100 packages crashed because the file changed beneath them.

**3. Run one cheap package first as an instrument check.**

```sh
./run-batch.sh <nub> --force is-odd@3.0.1     # expect verdict=MINIMUM, state=(nothing), 2 cells
```

If that is not clean, nothing after it means anything.

## Running a sweep

```sh
./run-batch.sh <nub> --file worklist.txt          # a worklist, one pkg@version per line
./run-batch.sh <nub> --force <pkg>@<version>      # one package, --force re-measures
```

Long sweeps go in a **background shell** (`run_in_background: true`), never a foreground call and
never `nohup`/`setsid` — a detached run cannot be tracked and never wakes you.

## Reading the results — coverage first, always

```sh
node watch-sweep.mjs results/runs <since-ms> worklist.txt
```

**Pass the worklist.** Without it you get a summary of what succeeded and no idea what did not run.
The single most expensive mistake made with this tool was reporting the survivors of a sweep as its
result: 54 of 100 packages produced no record, the batch discarded their stderr, and the remaining
46 looked like a finished corpus. **The failures are not randomly distributed** — heavy native
builds fail most, and those are exactly the packages most likely to need a grant, so the surviving
sample is biased toward "needs nothing."

Rules for reading a sweep:

- **`attempted` / `recorded` / `FAILED` is the headline**, printed at the end of every batch. If
  `FAILED` is not zero, you do not have a corpus.
- **A run of identical failures indicts the HARNESS, not the packages.** Check the FIRST one and
  fix that; the other ninety-nine are the same fault repeated.
- **Read the first error in a log, never the last.** A node stack trace ends with the version
  banner, and an install log ends with a summary — the cause is usually ~40 lines earlier. Five
  successive wrong diagnoses of one package all came from reading the tail.

## The instrument has no test — the failure with no symptom

**A change to the fixture or to `baseline.json` is a change to the measuring instrument.** Its
failure mode is not an error; it is that every package measures as needing NOTHING. Every verdict
`MINIMUM`, coverage 100%, nothing fails. Three times in one session:

- A hand-written `package-lock.json` with an empty `packages` map — nub believed the project had no
  dependencies. Puppeteer's control fell from **9,629 installed files to 32**.
- A baseline entry using `$home/...` — the wrong grammar (see sentinels below), so the jail failed
  to **compile** and no lifecycle script spawned. Surfaced as `failed to spawn`, which reads as a
  nub defect.
- Worst: a measurement taken *during* the second window was written up as a finding — "this
  package's grant dissolved" — when the jail was simply not running.

All three were caught by **disbelieving the number**, never by a check. Eight packages needing
nothing, including ones that cannot work without downloading a binary, is not a measurement.

**The pre-flight now runs a FIXTURE CANARY**: `puppeteer@25.4.0` must install >5000 files and be
materialized, or the batch refuses. It asserts the control's SHAPE, not a verdict — a package that
legitimately needs nothing looks identical either way, so `is-odd` cannot catch this.
`NUB_PROBE_SKIP_CANARY=1` disables it when deliberately testing the fixture.

**Never report a measurement taken while the harness was known-broken. Re-run it first.**

## Three `$` vocabularies, and they are not interchangeable

| Namespace | Valid names | Used by |
|---|---|---|
| Compiler fs sentinels | `$cache`, `$tmp` (closed set), plus `~/` | baseline paths, catalog fs rules |
| Harness path tokens | `$proj/`, `$store/`, `$home/` | recorded paths, `writePaths` entries |
| Network host sets | `$<name>` on the net axis | net rules only |

`$home` is meaningful in the second and **invalid** in the first. The compiler rejects an unknown
sentinel by name and lists the valid ones — that message is what makes this a one-step diagnosis.

## Verdicts

| Verdict | Means | Do |
|---|---|---|
| `MINIMUM` | Measured. `state` is the minimal grant. | Nothing. |
| `HARNESS-CRASH` / `HARNESS-TIMEOUT` | The probe itself failed. | Read `harness-stderr.log` beside the record. Never a package fact. |
| `HARNESS-ERROR` | The catalog override did not engage in the control. | Wrong binary, **or** the harness emits a catalog shape the parser rejects. |
| `BROKEN-IN-ENVIRONMENT` | Fails under npm too, same signature. | Grant nothing. Check `needsInvestigation`. |
| `BROKEN-EVEN-WITH-EVERYTHING` | Fails jailed at the widest grant, but npm succeeds. | A nub defect — the most valuable output. Never a grant gap. |

## Changing the catalog shape — five places move together

Written in one place, read in four. Missing one fails as something else entirely: a shape change
that reached the parser but not the harness produced a hundred-package sweep in which every package
reported that the override had not engaged, which reads as a broken binary.

1. `crates/nub-sandbox/src/catalog_v2.rs` — types, parse, validation, resolution
2. `crates/nub-sandbox/src/catalog_override.rs` — grant count and lookup
3. `tests/build-jail-search/collate.mjs` — writes the catalog
4. `catalogFor` in `tests/build-jail-search/search.mjs` — **synthesizes a catalog per cell, every run**
5. `tests/build-jail-search/overrides/` — hand-written entries

Plus the `--selftest` assertions, which read the synthesized shape and will silently pass on the
wrong one if not updated.

**The pre-flight probe catalog must come from `catalogFor`, never a literal.** It is emitted by
`search.mjs --emit-sample-catalog` for exactly this reason. A hand-written probe drifts from what
the harness emits, and a catalog with an empty package map is the worst possible probe because it
parses under every shape there has ever been.

## The oracle, and why it is shaped this way

- **Judge the ARTIFACT, not the exit code.** A cell passes only if it reproduces the control on exit
  code *and* on the digest of the sorted path list. A hook installer that cannot see the project
  writes zero of seventeen hooks and exits 0.
- **The control runs TWICE, combined by UNION.** Never intersection — that compares on fewer paths,
  so a cell that failed to write an unstable path still passes and the recorded minimum is too
  narrow, which is the exact failure the jail exists to avoid.
- **Every other package is held at full grant**, so the package under test is the only variable.
- **When the oracle says something impossible, suspect the oracle.** "Failed all 55 cells,
  nondeterministic" was 3 varying paths out of 2,734 — all one timestamped log filename.

## Ground truth

- **The tarball manifest, not the packument.** They disagree: `fsevents@2.3.3`'s packument declares
  `install: node-gyp rebuild` and its published tarball does not. nub runs the tarball.
- **Prefer a global `baseline`/`env` entry over a per-package grant or a harness filter.** A filter
  hides one tool's write after the fact and must be re-derived per tool. Two entries already earn
  their place: `PYTHONDONTWRITEBYTECODE=1` and `npm_config_logs_max=0`, each of which stops a write
  happening at all rather than filtering it afterwards.
- **Over-granting is the safe direction.** The failure to avoid is packages breaking.

## Related

- `wiki/design/build-jail.md` — the canonical design: capability model, bands, placement
- `.frizz/build-jail-catalog-schema.md` — the catalog schema spec
- `rust-build` — cargo mechanics and the profile/feature trap
