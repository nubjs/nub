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

### The mirror gate: a shared delta cannot manufacture a PASS either

Closing the direction above opened the other one. Once six hook installers are GRANTED, a granted
sibling writes real hooks under the jail too, so an ungranted shard-mate reads that same delta as its
own HIT and scores a pass off work it did not do. `lefthook@2.1.10` breaks in `pilot`, where it is
the only installer, and passes in `default7`, where granted `shared-git-hooks` writes 17 symlinks —
same package, same binary.

The test is a differential over the window's LINES, not a prose classifier. Asking whether a line
looks like a failure is the guess `errsig.mjs` refuses to make, and here it is also wrong in both
directions: `pre-commit` and `pre-push` print `No .git found in …` for every parent directory while
working perfectly, while `@arkweid/lefthook`'s real denial (`This command must be executed within git
repository.`) contains no failure word at all. What separates them is that the broken rows printed
lines their own unconfined arm never printed. A confined arm that printed strictly LESS introduced no
failure and keeps its pass — the ordinary case, not a corner: `shared-git-hooks` loses one `.bak`
line and `pre-commit` loses three, both from shard ordering.

Scoped three ways: only where the project evidence is SHARED (a shard's sole project-scoped row owns
its HIT), only where the row would otherwise PASS (the mirror of the gate above, which fires only
where the row would otherwise BREAK — that scoping is what keeps `msw` a survivor), and only where
both arms carried an uncapped line list, so an oversized window makes the gate decline rather than
rule on a truncated diff. The verdict is `BREAK:MANUFACTURED-PASS`, and the note quotes the lines.

`errsig.mjs` pairs with it. Its unclassified-line fallback used to drop a window's failure text
whenever the window returned 0 and the row scored `DID-WORK-AND-SUCCEEDED` — which for a
project-scoped row suppresses the one piece of per-package evidence contradicting the verdict under
suspicion. That suppression no longer applies to project-scoped rows. The cost, stated: a healthy
project-scoped row printing a problem-shaped word now picks one up too (`simple-git-hooks` declares
`[ERROR] Config was not found!` in both arms). It is reporting only and feeds no verdict.

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
  delta, so a 700× slowdown reads as a clean pass. **The general blind spot is real and still
  stands; this particular instance did not reproduce on the 2026-07-31 re-run**, where `duckdb`
  compiled from source in the *unconfined* arm as well, making its cost symmetric and not a jail
  effect. Read the blind spot as a property of the verdict machinery, not as a settled fact about
  `duckdb`.

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
  artifact. **Confirmed still live on the 2026-07-31 re-run with a both-arms differential, and worse
  than recorded here: it now hits a different package set and every row it touches scores
  *inadmissible* rather than break, so it is invisible in the headline.** See that section.

## macOS on the granted catalog, 2026-07-31 (supersedes the 158 / 91 / 67 table above)

Fresh sweep, binary `f63d3c62c3`, sha256 `e2c1adec…`, **no catalog override** — the compiled catalog,
which now carries 48 `packageNetwork.full` entries and 10 `packageGrants`. 26 arms, every one
`arm_effect=confirmed`, `PER_PKG=1`, 0 inconclusive, 0 denominator-suspect.

| | baseline (`27d383ef2e`, override, re-scored) | this run, as it scored | + manufactured-pass gate | **+ the four isolated re-runs** |
| --- | --- | --- | --- | --- |
| rows | 389 | 372 | 372 | 376 |
| admissible | 158 | 163 | 163 | **161** |
| survives the jail | 91 (57.6%) | 157 (96.3%) | 155 | **153 (95.0%)** |
| breaks | 67 | 6 | 8 | **8** |
| inconclusive | 0 | 0 | 0 | 0 |

**Quote the right-hand column.** The 163 / 157 / 6 the run itself printed carries the manufactured-pass
defect described below; the gate turns two of those passes into breaks, and the four isolated re-runs
remove `yorkie` and `simple-git-hooks` as no-ops.

The last two columns are *derived from a re-score*, and a re-score of this archive loses two more rows
than the run did, so read the arithmetic rather than the totals alone. Re-scoring with the UNMODIFIED
scorers moves six rows against the run's own table — every one a `content_nonce` row going
`UNSIGNALLED`, because a finished run's store is gone and the operator says UNEVALUABLE rather than
guessing — of which only `@prisma/client@6.19.3`, in two shards, was admissible. That control
therefore lands at **161 / 155 / 6**, and nothing else moves. Against it the gate moves exactly two
rows (**161 / 153 / 8**) and folding the isolated re-runs in gives **159 / 151 / 8**. Adding back the
two rows the archive cannot evaluate — neither is project-scoped, so the gate cannot reach them —
gives the run-path figures tabulated above.

**Enforcement was proved in the same binary, both directions.** A local `file:` package with no
catalog entry: under `PROD` `write_own=OK` — the positive control that the script ran — with
`write_outerhome`, `read_secret`, `write_project` all denied and `net_connect=DENIED(EPERM)`; under
`A0` all five `OK`, and the two files it was asked to plant outside its cell were on disk afterwards.

**The A0 arm did not weaken, which is what makes the movement attributable.** Every closed row still
scores `DID-WORK-AND-SUCCEEDED` unconfined, and none has an A0 delta under half the baseline's. The
closures are the confined arm improving, not the denominator eroding.

Sixty-one break rows closed, over 57 distinct packages, and they split cleanly by mechanism:

| closed by | packages |
| --- | --- |
| a new `packageNetwork.full` entry | 40 |
| a new `packageGrants` entry (the hook installers) | 6 |
| **no new grant** — the store-entry write and the project-root cwd node | **11** |

The 11 are the fix classes rather than the catalog: `@vscode/sqlite3`, `mongodb-crypt-library-version`,
`node-mac-contacts`, `node-zopfli-es`, `os-dns-native`, `ref-napi`, `sse4_crc32`, `tree-sitter-kotlin`,
`nice-napi`, `weak-napi` (store-entry write root) and `@arkweid/lefthook` (project-root cwd node).

Artifact bytes rather than presence, on the rows the earlier run scored off linker bin symlinks:
`azure-functions-core-tools` 601,659,764 B in both arms; `cldr-data` 276,427,271 B over 12,908 files;
`tree-sitter-cli` 20,208,016 B against the earlier control's 0 B; `taiko` 351 MB; `dugite` 147 MB.

### A granted hook installer now manufactures a PASS for its ungranted shard-mates

The shared-project-delta gate corrects one direction only. It refuses a manufactured BREAK — a row
whose two arms print the same thing — and that was the only direction that could be wrong while no
hook installer succeeded under the jail. Granting six of them created the other direction: one
granted installer writes `.git/hooks`, and the project delta it leaves satisfies the predicate for
every project-scoped row in the shard, confined arm included.

The differential is inside this one run. `lefthook@2.1.10` **breaks** in `pilot`, where it is the only
hook installer, and **passes** in `default7`, where the granted `shared-git-hooks` writes 17 symlinks —
same package, same version, same binary. Its own `default7` window prints `fatal: not a git repository`
and `exit status 128` while the row reads `DID-WORK-AND-SUCCEEDED`.

Four packages re-run alone settle it, and the control runs in both directions:

| package | in its batch shard | alone |
| --- | --- | --- |
| `ghooks@2.0.4` (granted) | SURVIVE | **SURVIVE** |
| `@arkweid/lefthook@0.7.7` | SURVIVE | **BREAK** |
| `yorkie@2.0.0` | SURVIVE | **NO-OP-BY-DESIGN, inadmissible** |
| `simple-git-hooks@2.13.1` | SURVIVE | **NO-OP-BY-DESIGN, inadmissible** |

So the run's raw table carried four spurious passes, and the corrected headline is **161 admissible,
153 survive, 8 break — 95.0%.**

**That correction is now produced by the scorers rather than by hand** — see the mirror gate above.
Against the re-score control the gate moves **two rows and no others**: `lefthook@2.1.10` in
`default7` and `@arkweid/lefthook@0.7.7` in `default6`, both to `BREAK:MANUFACTURED-PASS`. The four
isolated re-runs then supersede their batch rows, and the two the gate ruled on are the check on it —
isolation reaches the same BREAK on `@arkweid/lefthook` that the gate did, and the same pass on
`ghooks`, which the gate never touched. The arithmetic is in the headline table above.

`msw`, `ghooks`, `shared-git-hooks`, `pre-commit`, `pre-push`, `git-validate` and
`git-commit-msg-linter` are untouched by the gate. Every hook-installer survivor still carries
`project_scope_shared`.

### The eight remaining breaks

- **`handbrake-js@8.0.2` (twice) — the grant worked and the break moved.** It now downloads all
  38,059,875 bytes under the jail, byte-identical to the unconfined arm, and dies on
  `hdiutil attach mac.dmg` → `attach failed - Device not configured`. Mounting a disk image is not a
  filesystem or network capability, so no catalog entry expresses it.
- **`ssh2@1.17.0` — node-gyp cannot find Python, and only here.** Reproduced in isolation. `ssh2`
  builds an optional second binding; under `PROD` that node-gyp run reports `"python3" is not in PATH
  or produced an error` for both candidates, while unconfined it resolves `/usr/local/bin/python3`.
  Measured directly with a `file:` probe: bare `python3` and `/usr/local/bin/python3` both return
  `EPERM` under the jail while `/usr/bin/python3` runs — `/usr/local/bin/python3` is a symlink into
  `/Library/Frameworks`, and an ungranted symlink read is what aborts libuv's PATH walk. Unexplained:
  node-gyp resolves Python in 104 of the 105 other `PROD` windows in this sweep, including nub's own
  earlier node-gyp invocation inside `ssh2`'s own window. Whether that first invocation is confined at
  all is not established.
- **`esbuild@0.28.1` on `default1` — the row is two versions.** `windows.json` records the window's
  jobs as `esbuild@0.11.23` + `esbuild@0.28.1`, and the failure is 0.11.23's postinstall shelling
  `npm` at `registry.npmjs.org`, which is deliberately not granted. `esbuild@0.28.1` alone survives,
  in `pilot` and in an isolated re-run. The bare-name-with-two-pending-versions window is the
  documented limit of `PER_PKG=1`; here it mislabels a real break with the wrong version.
- **`@evilmartians/lefthook@2.1.10`, `lefthook@2.1.10` (twice) and `@arkweid/lefthook@0.7.7`** — none is
  in `packageGrants`, so none gets the two `.git` file reads the granted installers have. The first
  two lefthook rows fail outright with `fatal: not a git repository`; the `default7` lefthook row and
  the `@arkweid` row are the two the manufactured-pass gate recovers, and an isolated re-run confirms
  `@arkweid/lefthook` independently.

### This is still not a platform comparison

**Superseded 2026-07-31 by the Linux re-run below, which removes the mismatch this section
describes.** The paragraphs that follow remain accurate about *these two tables*; the comparison
itself is now made against a matched Linux run rather than this one.

The Linux table above ran `92ed4cc78f`, whose catalog holds **5** `packageNetwork.full` entries and
**3** `packageGrants` against this run's 48 and 10, and which predates the project-root cwd node. The
follow-up on the Linux section asked for a macOS re-run on "`92ed4cc78f`-or-later"; later is where the
grants landed, so the variables have swapped sides rather than been removed. 96.3% beside 63.9% is not
a platform result either.

Of 97 package-level divergences, 42 are Linux breaks on packages granted after `92ed4cc78f`. Of the
remaining 55, about 35 are `linux=SURVIVE / macOS=INADMISSIBLE` native builds whose fixture is invalid
on macOS arm64 unconfined — a toolchain difference, not a jail one, and the reason the macOS
denominator is 141 admissible packages against Linux's 171. The three that are genuinely jail-side all
have macOS-specific mechanisms: `handbrake-js` (`hdiutil`), `ssh2` (Seatbelt on an ungranted symlink),
`esbuild` (the two-version window). `re2` and `keytar`, Linux breaks, pass here — the Linux
Node-provisioning-inside-the-jail failure did not reproduce on macOS.

Coverage: 161 of the census's 387 packages by row (41.6%), 141 of 387 by distinct admissible package
(36.4%). The `$HOME`-cache downloader class stays outside the denominator on both platforms —
`cypress@15.19.0` scores `NO-OP-BY-DESIGN` here as it does on Linux, because the artifact never enters
a store cell. `duckdb@1.4.4` passes in both arms on macOS, with the same blind spot the Linux section
records: the verdict machinery judges tree delta, so a denied prebuilt falling back to a source build
reads as clean.

## Linux on the granted catalog, 2026-07-31 — and the first real platform comparison

Binary `77e3b74afe`, sha256 `65bc2a48…`, **compiled catalog, no override** — built without the
`build-jail-catalog-override` feature, and the absence verified behaviourally rather than by
inspection: with `NUB_BUILD_JAIL_CATALOG` set to a readable file the binary exits 1 with
"was not built with the `build-jail-catalog-override` feature, so it cannot honour it". An override is
not expressible, and **zero `OVERRIDDEN` banners appear across all 46 runs.** Ubuntu 24.04 on GCE
`n2-standard-16`, kernel `6.17.0-1021-gcp`, **Landlock ABI 7**, `landlock` present in
`/sys/kernel/security/lsm`. 26 sweep arms (13 shards x 2) plus 20 isolated re-run arms — 46 in total,
every one `arm_effect=confirmed` — with `PER_PKG=1`, 0 inconclusive and 0 denominator-suspect. (The
table reports 22 "isolated" runs rather than 20: `shard-d5e` holds a single package, so it is flagged
the same way an isolated re-run is.)

| | prior Linux (`92ed4cc78f`, re-scored) | **this run (`77e3b74afe`)** |
| --- | --- | --- |
| admissible | 169 | **158** |
| survives the jail | 108 (63.9%) | **146 (92.4%)** |
| breaks | 61 | **12**, over 11 distinct packages |

Breaks by class: binary-downloader 5 · hook-installer 3 · native-build-prebuilt 3 · self-configuring 1.
Coverage: 158 of 387 by row (40.8%), 140 of 387 by distinct admissible package (36.2%).

**Enforcement was proved on this exact binary, both directions, before the sweep** — see
`enforcement-probe/`. Under `PROD`: `write_own=OK` (the positive control, so the script ran) with
`write_outerhome`, `write_realhome`, `read_secret` and `write_project` all `EACCES` and
`net_connect=EPERM`; under `A0` all six `OK`, with all three planted files present on disk afterwards.

### The variables that make this a comparison, and the one that remains

The previous attempt failed because catalog and code moved between the two tables. Here everything
except the operating system is pinned to the macOS v3 run:

| variable | state |
| --- | --- |
| catalog | **byte-identical** — `build-jail-catalog.json` is unchanged between `f63d3c62c3` and `77e3b74afe` (48 `packageNetwork.full`, 10 `packageGrants`, 4 `networkHosts`) |
| scoring | **byte-identical** — `lib/*`, `run-shard.sh` and `rescore.mjs` unchanged; the only harness diff is where `aggregate.mjs` writes its table |
| shard set | identical 13 shards, 372 rows |
| Node | v26.5.0 on both, pinned rather than taken from the image |
| node-gyp | same per-package resolution — `11.5.0,12.1.0,12.4.0` on the shared shard |
| product code | **not identical**: macOS ran `f63d3c62c3`, this ran `77e3b74afe`. The delta is the side-effects cache key plus a `would_confine`/`confines` split; `should_confine` — the decision itself — is unchanged, and the cache is disabled in the fixture |

Pinning the toolchain was not in the original brief and mattered: two runs on different node-gyp
majors would have produced a "platform difference" that was a tool difference.

### The comparison, with denominator differences separated out

Of the 127 packages **admissible on both platforms** — the only ones a jail comparison can speak to —
the two platforms agree on **123, or 96.9%**.

| | packages |
| --- | --- |
| agree, both survive | 119 |
| agree, both break | 4 — `@arkweid/lefthook`, `@evilmartians/lefthook`, `lefthook`, `esbuild` |
| **Linux breaks only** | 3 — `keytar`, `kerberos`, `opencode-ai` |
| **macOS breaks only** | 1 — `ssh2` |

Each divergence has a named mechanism. The three Linux-only breaks are all uncatalogued egress:
`keytar` and `kerberos` are refused their GitHub prebuild (`EAI_AGAIN github.com`) and fall back to a
source build that fails; `opencode-ai`'s postinstall shells `npm` at `registry.npmjs.org`. The single
macOS-only break is `ssh2`, the Seatbelt ungranted-symlink PATH-walk abort — a mechanism that does not
exist on Linux, where an ungranted path denies with `EACCES`, inside libuv's continue set.

**A further 25 packages differ for denominator reasons and are not jail findings.** 13 are admissible
only on Linux (macOS cannot run them unconfined — `INVALID-FIXTURE` native builds such as
`node-expat`, `lz4`, `mmmagic`, `node-rdkafka`); 12 only on macOS (`NO-OP-BY-DESIGN` on Linux —
`keccak`, `blake-hash`, `@instana/autoprofile` — plus `handbrake-js`, whose `hdiutil` path does not
exist here). **This class does not shrink now that the toolchains match, and it cannot:** it is macOS
arm64 against Linux x64, an architecture difference rather than a node-gyp-version one.

### The manufactured pass reproduces on Linux, with the same signature

A granted hook installer satisfies the project-scoped predicate for its ungranted shard-mates on this
platform too. `lefthook@2.1.10`'s `default7` `PROD` window prints `fatal: not a git repository` and
`exit status 128` while the row reads `DID-WORK-AND-SUCCEEDED`, beside granted `shared-git-hooks`
writing 17 symlinks. Ten packages were re-run alone to settle it, and the controls hold both ways:

| package | granted | in its batch shard | alone |
| --- | --- | --- | --- |
| `ghooks`, `pre-commit`, `pre-push`, `git-validate`, `git-commit-msg-linter`, `shared-git-hooks` | yes | SURVIVE | **SURVIVE** |
| `@arkweid/lefthook`, `lefthook` | no | SURVIVE | **BREAK** |
| `yorkie`, `simple-git-hooks` | no | SURVIVE | **inadmissible no-ops** |

All six granted installers are honest passes; the four ungranted ones were spurious. The table above
already carries these isolated verdicts, which is the one respect in which it is not directly
comparable to the raw macOS v3 table — that run's identical correction was applied by hand in prose
and never written back, so it must be re-applied before the two are compared.

**The `BREAK:MANUFACTURED-PASS` gate cannot correct a run recorded before it existed, and declines
silently.** Re-scoring this sweep with the post-`fec9087e29` scorer returns **162 / 151 / 11, byte-for-
byte what the older scorer returned** — the gate fires on nothing. The reason is structural rather
than a defect in it: its precondition is `a0.window?.lines && prod.window?.lines`, and that per-window
line list is recorded by the RUNNER, so an archive made with the older `run-shard.sh` does not carry
it. `lefthook`'s row here has `project_scoped: true` and `project_scope_shared: 2` but no `window`
field at all. So the gate's own selftest passes, the re-score exits 0, and nothing indicates that the
correction did not happen. Any archived run — this one and the macOS v3 one alike — still has to be
corrected by isolation; only sweeps recorded with the new runner can be corrected by re-scoring.

### The Node-provisioning defect persists, and is now invisible

The prior Linux sweep recorded that nub's own Node provisioning runs inside the jail and fails closed.
It still does — five `PROD` windows hit `ERR_NUB_NODE_PROVISION_FAILED` — but **every package it now
hits scores inadmissible rather than break, so none of them appear in the 12.** The differential on
`@pdftron/pdfnet-node@12.0.0`, one run, one binary:

```
A0    Using Node.js 24.18.1 (resolved from package.json#engines.node)
      Installing from nodejs.org... (31 MB)   Installed in 2.2s
PROD  ERR_NUB_NODE_PROVISION_FAILED: failed to provision Node >=10 <=24
      ... dns error: failed to lookup address information: Temporary failure in name resolution
```

The jail plainly breaks it. The row is scored `NO-OP-BY-DESIGN` because the unconfined arm also left
no attributable delta, so it leaves the denominator instead of counting against the jail.
`parse-server` and `@matteodisabatino/gc_info` are the same shape. **92.4% is therefore a slight
overstatement**, and the fix is a real one: provisioning should not be inside the confinement.

`re2@1.26.1`, a break on the prior sweep, passes here for an unrelated reason — its GitHub prebuild is
still refused, but the local source build now succeeds. `duckdb@1.4.4` compiled from source in **both**
arms, so its ~35-minute cost is a linux-x64 fixture property here rather than the denied-prebuilt
asymmetry the prior section recorded.

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

Thirty-two assertions against synthetic snapshot pairs, driving the real scorers end to end. **Every
one is a differential** — each asserted failure is paired with the nearest input that must still
pass, because a gate that rejects everything is not a gate, and four of the five bugs these cases
encode were originally hidden by exactly that kind of one-sided check. Each fix was verified to make
its own assertion go red when removed, and only its own: the controls are what caught the no-op gate
over-firing on `msw`.

**Ablate the CONTROLS too, not just the fixes.** A control only earns its place if some over-broad
version of the fix turns it red — and one here did not. `fullsized`'s window read `done (0 errors)`,
`\berror\b` does not match `errors`, and so the assertion that a healthy row picks up no failure
signature passed because nothing was ever a candidate. Deleting the suppression it guards left it
green. The line now reads `download complete, 0 failed`, and the four ablations behave: removing the
manufactured-pass gate (or its `lines` input) reddens its three assertions and no control; removing
the `errsig` scoping reddens only the triage assertion; widening the gate to "the windows differ"
reddens only the printed-less control; removing the `errsig` suppression outright reddens only the
two healthy-row controls.

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
