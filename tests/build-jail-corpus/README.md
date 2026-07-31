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

**Nothing a shard reports is attributable to the jail until the package has been re-run
alone.** aube stops *scheduling* queued lifecycle jobs once any sibling has failed; jobs
already running drain, jobs still behind the semaphore return having done nothing. A package
skipped that way is indistinguishable from one the jail blocked — both score
`NEVER-RAN-ITS-REAL-PATH`.

The bias is asymmetric and it points the wrong way. A tighter jail makes the first failure
fire earlier, which skips more siblings, which manufactures more breaks. Measured 2026-07-31
on macOS: every shard failed a lifecycle script in *both* arms and a different package failed
first in each, so the A0 denominator and the PROD numerator were computed over two
differently-truncated runs. Of the 18 packages that pass reported broken, **10 install
cleanly under the same jail when run alone.**

`./run-isolated.sh <package> [arm]` copies the package's manifest row verbatim into a
one-line shard and re-runs it. Quote the batch number as a screen and the isolated number as
the finding; never hand over one labelled as the other.

## Judge a downloader by artifact SIZE, not by the verdict

The `binary-downloader` predicate is `min_created: 1` over any created path, and the class
note says the corroborating signal — that the created entry is a large executable — is
"checkable post hoc". Run that check; it is not optional, and it had never been run.

Two things satisfy the predicate without a download happening: an empty directory the package
creates before failing, and **nub's own jail-home scratch** (`~/.cache/nub/jail-home/<pkg>-*/
.cache/nub/node-discovery.json`, 117 B), which exists only in the confined arm — so it inflates
PROD passes and not A0 ones. Applying the check to the 44 downloaders the macOS run scored as
surviving the jail falsified 5, including `@go-task/cli`, recorded as a pass while its own log
carries `getaddrinfo ENOTFOUND github.com`. A sixth, `@sitespeed.io/chromedriver`, passes only
in the isolated re-run:

| package | A0 largest | PROD largest |
| --- | --- | --- |
| `azure-functions-core-tools@4.12.1` | 601 MB | 310 B |
| `@go-task/cli@3.52.0` | 49 MB | 0 |
| `@eversdk/lib-node@1.48.0` | 22 MB | 0 |
| `cldr-data@36.0.5` | 13,296 files | 3 files |
| `chromium@3.0.3` | 176 files | 6 files |
| `@sitespeed.io/chromedriver@149.0.7827` (isolated) | 16.7 MB | 0 |

Diff the largest created file under the package's own store cell across the two arms. A pass
whose confined artifact is orders of magnitude smaller than its unconfined one is a false pass.

## Running it

```sh
export NUB_BIN=/path/to/nub NUB_EXPECT_GIT_SHA=<sha>
export STUDY_PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
./run-shard.sh pilot shard-pilot.tsv A0
./run-shard.sh pilot shard-pilot.tsv PROD
node aggregate.mjs out
```

Each run gets a fresh `HOME`, cache, and project directory, and writes its log outside every
observed root — a log written inside the project makes every package look like it acted.

### Self-test

Withholding one project-file capability must move only the packages that depend on it:

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
