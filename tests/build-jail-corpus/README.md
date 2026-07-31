# Build-jail corpus probe

Measures whether real npm packages' install-time lifecycle scripts still do their job when the
build jail confines them, on a platform that only a real runner can provide.

The probe answers three questions: which filesystem grants lifecycle scripts require, which
packages genuinely need network access and to which hosts, and which need access to the project
directory.

## Why the exit code is not the signal

A lifecycle script whose inputs are missing aborts early, exits 0, and reads as a pass — it never
touched the filesystem or the network, so it never got the chance to fail. Judging on the exit code
records "works under the jail", which is wrong in the reassuring direction.

Two other signals were measured and rejected:

- **Artifact presence** false-positives whenever a shared content-addressed store persists an
  artifact across runs — the file is there because a previous run made it.
- **Timestamps** are unusable in both directions. Hardlink and clonefile materialisation both bump
  a file's `ctime` past the fence without anything writing to it, and the extractor stamps
  `mtime=now`, so a freshly extracted file and a freshly compiled one look identical.

The signal is therefore a **path-set difference plus a content digest** across two snapshots that
bracket the script window, with timestamps recorded only as corroborators.

**And a path-set difference is still not enough on its own.** A path is satisfied by a directory, by
a symlink the package manager made, and by a file the package manager wrote into the confined arm's
private HOME. Each of those was measured scoring a package as having done work it had not done, so
the delta additionally carries **type and size per entry**, and a predicate may require regular
FILES and BYTES rather than paths. See "What a verdict has to prove" below.

## The three-state verdict

```
DID-WORK-AND-SUCCEEDED   the class effect is present in the window's delta
DID-WORK-AND-FAILED      acted, but the class effect is absent (silent degradation)
NEVER-RAN-ITS-REAL-PATH  installed, no owned delta, no class effect
```

`NEVER-RAN-ITS-REAL-PATH` is a legitimate and common outcome, not a harness failure: much of the
ecosystem now ships platform binaries through `optionalDependencies`, so an install script's
default path is a no-op.

A validity gate sits on top. A jail-arm verdict is admissible only if the same package reached its
class effect with the jail **off** — otherwise it is reported as `NO-OP-BY-DESIGN` or
`INVALID-FIXTURE` rather than folded into a compatibility number.

## The two arms

| arm | how |
| --- | --- |
| `A0` | jail off, via `dependenciesMeta.<name>.sandbox: false` in the project manifest |
| `PROD` | jail on, shipped defaults |

There is no global off-switch, so `A0` opts each package out individually. Every run asserts its
own **arm effect** — the opt-out warning must be present in `A0` and absent in `PROD` — and refuses
to report numbers when that assertion fails. An earlier harness selected arms with environment
variables that nothing read, so every arm ran the identical configuration; the assertion exists so
that cannot recur silently.

`PROD` means different things per platform, which is deliberate and useful: on macOS the network
axis is a curated per-host allowlist enforced through a proxy, while on Linux it is a binary deny.

## Attribution

Output is attributed by **store path**, not by log: lifecycle output is not framed per package, so
a shard-level exit code cannot say which script failed. The isolated store materialises each
package in its own cell keyed by `name@version`, and build output lands there, so attribution is a
parse. A write landing outside the writer's own cell is reported as unattributed — that is the
interaction case worth surfacing, not noise.

## A batch run is a screen, not a result

**A shard cannot attribute an ABSENCE.** aube stops *scheduling* queued lifecycle jobs once any
sibling has failed; jobs already running drain, jobs still behind the semaphore return having done
nothing. A package skipped that way is indistinguishable from one the jail blocked — both leave no
delta.

The bias is asymmetric and it points the wrong way. A tighter jail makes the first failure fire
earlier, which skips more siblings, which manufactures more breaks. Measured 2026-07-31 on macOS:
every one of the 14 arms was truncated, and in six of the seven shards a *different* package failed
first in each arm, so the A0 denominator and the PROD numerator were computed over two
differently-truncated executions.

The gate is applied in `aggregate.mjs` and needs BOTH arms, because the manufactured break has
exactly one shape: **A0 proves the package had work to do, PROD left no trace, and the PROD window
was truncated by some other package failing first.** Those rows are `INCONCLUSIVE-SCHEDULING` —
counted in neither the numerator nor the denominator — and land in the worklist. Nothing else is
touched: a package silent in *both* arms is the ordinary no-op the A0 gate already excludes, and a
verdict backed by positive evidence (it acted, or it produced its class effect) proves the job was
scheduled. Ruling on one arm alone instead condemns 315 of 345 rows and throws the corpus away;
that was tried.

The mirror case does not manufacture a break, it shrinks the corpus — an A0 arm that skipped a
package reports it as a no-op and the row is dropped. That is reported separately as
`denominator possibly under-counted` rather than absorbed silently.

Two stages, mechanically linked, so the default path is trustworthy without isolating everything:

```
run-shard.sh × N  →  aggregate.mjs  →  inconclusive.txt  →  run-isolated.sh --from-worklist
```

Isolating the whole corpus is correct and costs an order of magnitude more; isolating the rows the
screen could not decide costs 30 of 345. An isolated cell **supersedes** the batch cell for the same
package in the aggregate — they are not two samples to average, one was taken through a window that
may have been truncated and the other was not.

## What a verdict has to prove

A verdict says the class effect was present. It cannot say the effect was the right SIZE, and until
2026-07-31 nothing did — the `binary-downloader` class note deferred that to a post-hoc check nobody
ran. Of the 44 downloaders the first macOS corpus scored as surviving the jail, **5 were false
passes**, each satisfying `min_created: 1` with something that is not a download:

| package | what satisfied the predicate under the jail | what was missing |
| --- | --- | --- |
| `azure-functions-core-tools@4.12.1` | 3 bin symlinks the linker made | 601 MB of DLLs |
| `chromium@3.0.3` | 3 bin symlinks the linker made | the entire `Chromium.app` |
| `cldr-data@36.0.5` | 3 bin symlinks the linker made | 13,296 locale files |
| `@go-task/cli@3.52.0` | one empty staging directory | 49 MB; its log reads `getaddrinfo ENOTFOUND github.com` |
| `@eversdk/lib-node@1.48.0` | a zero-length `eversdk.node` | 22 MB |

Four layers now stand between a verdict and that shape, in the order a false pass meets them:

1. **`.bin` inside a cell is the linker's work, not the script's.** It is attributed as
   `cell-bin-link` and can never be a package's own evidence. The `.`-prefix rule that let it
   through was there for Prisma's `.prisma/client`.
2. **nub's own jail-home scratch never enters the delta.** The jail gives each script a private
   HOME and writes `<home>/.cache/nub/jail-home/<pkg>-*/.cache/nub/node-discovery.json` (117 B) into
   it — present only in the confined arm, so it inflated PROD and not A0. Excluded at the delta
   rather than in a predicate, because a predicate-level filter protects only the predicate that
   remembers to apply it.
3. **`min_created_files` / `min_created_bytes`** replace path counts for every artifact-shaped
   class. A directory and a symlink are not artifacts; 64 KiB is a floor below any real downloaded
   binary. This is a floor, not a measurement.
4. **The cross-arm size ratio**, in `aggregate.mjs`, is the measurement: a would-be pass whose
   confined artifact is under 10% of its unconfined one by bytes or by file count is
   `BREAK:UNDERSIZED-ARTIFACT`. Control: across the nine isolated runs independently verified as
   genuine passes the ratio lands between 0.913 and 1.000, so the threshold has three orders of
   magnitude of headroom and fires on none of them.

**The denominator was inflated the same way.** Applying the same rules to the A0 arm removes 106 of
the 139 cells the first run called admissible: their unconfined arm had produced nothing but linker
bin symlinks either, so they had never been shown to do any work at all. `@cloudflare/wrangler`
scored on `.bin/rimraf`; `lzma-native` and `cbor-extract` on `.bin/node-gyp-build*`;
`electron-chromedriver` on `.bin/extract-zip`. A compatibility rate over a denominator like that
measures nothing in either direction.

## Two packages the class system cannot score, and what replaced it

`msw` and `@danmarshall/deckgl-typings` are classed `codegen`, whose predicate is `content_nonce` —
the strongest operator here and, for these two, unsatisfiable **by construction**: both emit FIXED
content, so no per-run nonce can ever appear in it. Both also write outside their own store cell, so
per-package attribution saw nothing.

`classes.json` now carries `_package_predicates`, keyed by `name@version` or bare `name`, replacing
the class predicate outright. `msw` is scored on `proj:public/mockServiceWorker.js`;
`@danmarshall/deckgl-typings` on the 22 `@types/deck.gl*` trees it copies beside itself. Both are
created in A0 and PROD alike — **both work under the jail**, matching the by-hand verification.

The bar for adding an entry: the class predicate must be unsatisfiable by construction for that
package, and the replacement must name a specific artifact only the real code path produces. An
entry that merely widens a predicate until the package passes is the false-pass machinery this
harness exists to prevent, wearing a different hat.

Fixing this exposed a third bug the first two had been hiding. The verdict rule consulted the
package's own-cell `kind` *before* the class effect, so a package whose effect lands elsewhere —
every hook installer, plus these two — was scored `NEVER-RAN` however well it had worked. It never
surfaced because linker bin symlinks had been giving those packages a spurious own-cell delta. Two
bugs cancelling is not two bugs fixed.

## Corrected macOS numbers, 2026-07-31

Re-scored from the archived snapshots; no new runs.

| | first reported | corrected |
| --- | --- | --- |
| batch screen, admissible cells | 139 | 11 |
| batch screen, breaks | 18 | 4 |
| batch rows needing isolation | not distinguished | 30 |
| after folding in 23 isolated re-runs | — | 30 admissible, 16 survive, **14 break** |

Of the original 18 batch breaks, isolation resolves **11 as real and 7 as clean** — `ffi`,
`interruptor`, `cld`, `diskusage`, `heapdump`, `@airbnb/node-memwatch`, `deasync` all install fine
under the same jail when run alone. (An earlier note in this file said 10 installed cleanly; the
archived isolated runs say 7. `@sitespeed.io/chromedriver` passes at verdict level in isolation and
is falsified by the artifact check, which is why it counts as a break.)

Quote the batch number as a screen and the isolated number as the finding; never hand over one
labelled as the other.

## Running it

```sh
export NUB_BIN=/path/to/nub NUB_EXPECT_GIT_SHA=<sha>
export STUDY_PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
./run-shard.sh pilot shard-pilot.tsv A0
./run-shard.sh pilot shard-pilot.tsv PROD
node aggregate.mjs out
./run-isolated.sh --from-worklist inconclusive.txt   # resolve what the screen could not
node aggregate.mjs out                                # isolated cells supersede batch ones
```

Each run gets a fresh `HOME`, cache, and project directory, and writes its log outside every
observed root — a log written inside the project makes every package look like it acted.

### Re-scoring a finished run

```sh
node rescore.mjs <out-dir> <run-dir>...
node aggregate.mjs <out-dir>
```

An arm costs hours; a scoring bug costs a verdict. The snapshots bracketing the window are the whole
evidence base and they stay on disk, so a predicate change can be replayed against runs that already
happened — which is what makes "did this number move because of the fix or because of scheduling
noise?" answerable. It re-invokes the real scorers rather than reimplementing them. One limit:
`content_nonce` reads the generated file off disk and a finished run's store is gone, so those rows
come back `UNSIGNALLED` with an explicit UNEVALUABLE reason rather than a false break.

### Self-tests

```sh
node selftest.mjs        # the scoring gates, offline, ~1s
```

Eleven assertions against synthetic snapshot pairs, driving the real scorers end to end. **Every one
is a differential** — each asserted failure is paired with the nearest input that must still pass,
because a gate that rejects everything is not a gate, and four of the five bugs these cases encode
were originally hidden by exactly that kind of one-sided check. Each fix was verified to make its own
assertion go red when removed.

The live counterpart withholds one project-file capability, which must move only the packages that
depend on it:

```sh
SUPPRESS=prisma-schema ./run-shard.sh pilot shard-pilot.tsv A0
```

With the schema withheld the codegen archetype writes its unconditional stub files and exits 0 —
every naive check passes — and the harness must still return `DID-WORK-AND-FAILED`. If it does not,
the verdict logic is wrong and no result from it should be trusted.

## Manifest notes

- `@prisma/client@6.19.3` is pinned deliberately: the codegen postinstall was removed in 7.0.0, so
  a fixture on `latest` installs cleanly, exits 0, and exercises nothing.
- It must be paired with `prisma@6.19.3`. With a 7.x CLI co-resolved the 6.x client's postinstall
  gets usage text instead of generation, and the archetype fails for a fixture reason.
- `bufferutil@4.0.8` is a light package that exercises the store-coordinate path.
