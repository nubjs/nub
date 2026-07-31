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

**The package's own exit code vetoes its own success.** A predicate asks whether the class effect is
present; it cannot ask whether the script FINISHED, and for anything that emits incrementally those
are different questions. `gl@8.1.6` compiled 5,994 object files and then died on `"C++20 or later
required."` — rc=1, artifact predicate satisfied three ways over, scored `DID-WORK-AND-SUCCEEDED`,
and stood as the A0 denominator for a package that does not build at all on this host. So a package
whose own window exits non-zero (or that nub names in a `lifecycle script … failed for` line) is
`DID-WORK-AND-FAILED` however complete its artifact looks.

The cost, stated rather than hidden: a script that does its real work and then exits non-zero on
something incidental is demoted too, and its row leaves the corpus. That shrinks the denominator
instead of inventing a break, which is the right way to be wrong — and a package that fails its own
install unconfined was never a compatibility measurement in either arm. On the 2026-07-31 macOS
sweep it removed 27 rows that had been counted as surviving the jail while failing in BOTH arms, and
promoted 3 that were failing only under it.

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

**Filesystem effects** are attributed by **store path**. The isolated store materialises each
package in its own cell keyed by `name@version`, and build output lands there, so attribution is a
parse. A write landing outside the writer's own cell is reported as unattributed — that is the
interaction case worth surfacing, not noise.

**Log output** is attributed by the **window marker** the runner emits under `PER_PKG=1`
(`--- window: <name> (rc=N)`), which is a boundary the runner parsed rather than one the scorer
infers. Path-shaped rules are the fallback, for a `--all` run and for install-phase lines outside
every window. They were once the only mechanism, and they systematically lose the commonest
lifecycle failure there is — a bare message with no path in it. `getaddrinfo ENOTFOUND github.com`
names nothing, so it fell through to a `(shard-level)` bucket that joins to no row: 61 of 291
classified lines on `default3-PROD`, leaving 7 of that shard's 10 break rows with no signature at
all. With window attribution the same shard's residue is 1 of 293.

Where no errno rule matches the failure at all — `could not be installed: fetch failed`, `Could not
connect to CDN`, `getwd: invalid argument`, `not a git repository` — the window's own most
problem-shaped line is carried under an explicit `unclassified` kind. Widening the classifier
instead would mean guessing whether those are network denials or application bugs, which silently
moves cost between the two answers the study exists to produce.

### The project delta cannot be attributed at all

A project-scoped class (a hook installer, msw) has its effect land OUTSIDE its own cell, and
`delta.project_paths` is the WHOLE project's. So one shard member writing `.git/hooks` satisfies the
predicate for EVERY project-scoped row in that shard, and the confined arm — which writes none —
flips all of them to MISS together, manufacturing a break for packages that never did anything.
Both `yorkie` ("trying to install from sub 'node_module' directory, skipping") and
`simple-git-hooks` ("Config was not found!") print the identical line with the jail on and off and
were scored breaks off `ghooks`'s 17 hook files.

The evidence that IS per-package is the package's own window, so a row whose two arms print and
return exactly the same thing cannot be called a break: it is `NO-OP-BOTH-ARMS`, inadmissible. Two
scopings make this a gate rather than a blunt instrument — it applies only where the evidence is
project-scoped (a downloader can be blocked *silently*, with the break visible only in its own
attributed cell delta), and only where the row would otherwise BREAK (`msw` writes its service
worker in both arms, so nothing was manufactured and stripping its pass would just delete a
survivor). Rows that survive the gate still carry `project_scope_shared`, because per-package
attribution remains unavailable and any count built from them is an upper bound.

Real per-package attribution here needs a per-WINDOW snapshot of the project surface. An archived
run does not have one, and this gate is what an archived run can honestly say instead.

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

### `PER_PKG=1` removes the truncation instead of ruling around it

The gate above decides what a truncated window is allowed to say. `PER_PKG=1` stops the window
being truncated at all: instead of one `approve-builds --all`, the runner reads the pending set out
of the install's own `WARN_NUB_IGNORED_BUILD_SCRIPTS` line and drives one `approve-builds <name>`
per package. The `failed` flag that stops scheduling lives in a single install process, so with one
job per process it cannot reach the next package — truncation is impossible by construction rather
than detected after the fact. The expensive half, resolve and fetch and link, is still paid once for
the whole shard; what it costs is the serialisation of builds `--all` would have run five wide.

One shape survives: `approve-builds` rejects a `name@version` argument, and a bare name with two
pending VERSIONS approves both together. `windows.json` records every window's job list and exit
code, so that window is ruled on by the same test as a batch one and the rest of the shard is not
tarred with it. `scheduling` therefore becomes a per-row fact rather than a per-shard one.

Control, run before the sweep: on `shard-pilot` — which exits 0 in both arms and so was never
truncated — `PER_PKG=1` reproduces the `--all` verdict summary exactly. A mode that changed
verdicts on an untruncated shard would be changing the measurement, not the scheduling.

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

## Scoring-defect re-score, 2026-07-31 (supersedes the 190 / 121 / 69 headline)

Three scoring defects were fixed and the archived `runs-v2` sweep re-scored through them. No new
runs; same 60 arms, same snapshots.

The control first: re-scoring `runs-v2` through the **unmodified** scorers reproduces 190 / 121 / 69
with **zero verdicts moved**, so every number below is attributable to the fixes rather than to the
re-score path.

| | published | project-delta gate | exit-code veto | **both** |
| --- | --- | --- | --- | --- |
| admissible cells | 190 | 188 | 160 | **158** |
| survives the jail | 121 | 121 | 91 | **91** |
| **breaks** | **69** | **67** | 69 | **67** |
| breaks carrying a failure signature | 26 / 69 | 67 / 67 | 68 / 69 | **67 / 67** |

Movement, per fix:

- **Project-delta gate** — removes exactly the two manufactured hook-installer breaks, `yorkie` and
  `simple-git-hooks`. Both were no-ops in both arms scored off `ghooks`'s 17 hook files.
- **Exit-code veto** — 27 rows counted as *surviving the jail* were failing their own install in
  **both** arms (all 27 confirmed with a non-zero A0 window); 3 more were failing only under the
  jail and are now breaks (`esbuild` on `npm error code ENOTFOUND`, `nice-napi`, `weak-napi`); and 3
  A0 arms that had propped up a break row are no longer valid denominators, `gl@8.1.6` among them.
- **Window attribution** — every break is now triageable, matching the hand triage's 67 of 67.

The hook-installer count remains an upper bound for the reason above: the surviving 11 rows still
share one project delta.

## First Linux numbers, 2026-07-31

Full corpus at `92ed4cc78f` — the first Linux run of this harness, and the first on any platform to
include BUG-J's store-entry grant. Ubuntu 24.04 on GCE `n2-standard-16`, kernel `6.17.0-1021-gcp`,
**Landlock ABI 7**, `landlock` present in `/sys/kernel/security/lsm`. 30 arms, every one
`arm_effect=confirmed`, `PER_PKG=1`, `shard-default5` and `shard-d5d` split further so the sweep
could finish.

| | as run (harness at `92ed4cc78f`) | **re-scored (harness at `e45b8f8321`)** |
| --- | --- | --- |
| rows | 372 | 372 |
| admissible | 192 | **169** |
| survives the jail | 130 (67.7%) | **108 (63.9%)** |
| breaks | 62 | **61**, across 57 distinct packages |
| inconclusive (scheduling) | 0 | 0 |
| denominator possibly under-counted | 0 | 0 |

Re-scored breaks by class: binary-downloader 34 · hook-installer 11 · native-build-prebuilt 9 ·
self-configuring 7. `PER_PKG=1` decided every cell from the batch screen; no isolated re-run was
needed on either scoring.

**Enforcement was proved inside the same run rather than assumed.** A local `file:` package with no
catalog entry, PROD and A0 back to back: `write_own=OK` in both — the positive control, so the
script did run — and under PROD `write_outerhome=DENIED(EACCES)`, `read_secret=DENIED(EACCES)`,
`write_project=DENIED(EACCES)`, `net_connect=DENIED(EPERM)`, all four `OK` under A0.

### Do not read this against the macOS table as a platform comparison

The macOS sweep ran a **pre-`92ed4cc78f` binary** (`expect_git_sha` `2005bc8229` / `96491a134d`, so
without BUG-J) **and** a catalog override, `mac-catalog.json`, that carries **no `packageNetwork` key
at all** — 0 per-package egress grants against this run's 5. Three variables move between the two
tables, only one of which is the operating system, so `63.9%` beside `57.6%` is not a measurement of
either platform. Every per-package divergence is a lead. The clearest instance: `@vscode/sqlite3`,
`drivelist` and `cmark-gfm` break on macOS and pass on Linux, and those are exactly the three
packages BUG-J's own control fixed — a binary difference wearing a platform's clothes.

### Two Linux-specific results

- **The per-package egress grant works, and the artifact lands in the wrong home.**
  `cypress@15.19.0` completed its download under the jail at **771,299,496 bytes — byte-identical to
  the unconfined arm** — into
  `<home>/.cache/nub/jail-home/cypress-<hash>/.cache/Cypress/`, while `~/.cache/Cypress` stayed
  absent. `private_home_dir`'s own doc comment already states that tradeoff ("It does NOT make those
  artifacts resolvable at RUN time"); what is new is that the harness scores the row
  `NO-OP-BY-DESIGN` on **both** platforms, because the artifact never enters a store cell. The whole
  `$HOME`-cache downloader class is therefore outside the denominator rather than counted as
  surviving.
- **A denied prebuilt becomes a source build, and the verdict cannot see the cost.** `duckdb@1.4.4`
  was refused `npm.duckdb.org`, fell back to node-gyp, and produced a 66,587,160 B `duckdb.node`
  against the unconfined arm's 65,367,392 B — `DID-WORK-AND-SUCCEEDED` in both arms, at ~35 minutes
  of single-threaded compile instead of a seconds-long download. The measurement judges the tree
  delta, so a 700× slowdown reads as a clean pass.

### Harness notes from the Linux port

- **Nothing needed porting.** `run-shard.sh` already branches on `uname -s`; `selftest.mjs` passes
  unchanged on Linux.
- **The header's Linux network claim was stale** and is corrected in place. It read "BINARY DENY …
  `socket()` is refused outright"; `apply_landlock` reads the catalog verdict off the compiled IR,
  so egress is a per-package boolean — a catalogued package gets `AF_INET`/`AF_INET6` lifted out of
  the socket ceiling and reaches any host.
- **`npm_config_cache` masked the real failure signature** in the run above; it was independently
  removed at `8bf9df691a`, so no fix is needed, only a caveat on these numbers. Seven of the 61
  re-scored break rows lead with `EACCES: permission denied, mkdir …/cache/npm` rather than the
  denial that mattered. One was chased down: on `keytar@7.9.0`, one variable, with the pin `EACCES`
  and without it `getaddrinfo EAI_AGAIN github.com` — same verdict, different reason. The other six
  are unverified under the fixed runner.
- **nub's own Node provisioning runs inside the jail and fails closed.** `re2@1.26.1` and
  `redis-memory-server@0.17.0` hit `ERR_NUB_NODE_PROVISION_FAILED` fetching
  `https://nodejs.org/dist/index.json` — a package whose `engines.node` needs a Node the machine has
  not provisioned cannot get one from inside the jail. The corpus gives each run a fresh `HOME`, so
  this is worse here than on a machine with a warm `~/.cache/nub`; it is a real path, not an
  artifact.

## Running it

```sh
export NUB_BIN=/path/to/nub NUB_EXPECT_GIT_SHA=<sha>
export STUDY_PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
export PER_PKG=1                                   # one window per package; see above
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

Twenty-six assertions against synthetic snapshot pairs, driving the real scorers end to end. **Every
one is a differential** — each asserted failure is paired with the nearest input that must still
pass, because a gate that rejects everything is not a gate, and four of the five bugs these cases
encode were originally hidden by exactly that kind of one-sided check. Each fix was verified to make
its own assertion go red when removed, and only its own: the controls are what caught the no-op gate
over-firing on `msw`.

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
