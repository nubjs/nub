# install-script sweep results

Two generations of results live here and they are not the same measurement. Read this before
comparing them, because the schemas differ and only one of them is reproducible.

## `install-script-sweep-macos.tsv` — the hand-adjudicated generation. Keep it.

Eight columns, the last being `npm-differential`: a prose judgement on each failing row, of the form
`npm: IDENTICAL failure - ... Not nub-caused`. **No script in this repository produces that column.**
It was written by hand, so nothing can regenerate it and nothing can check it — which is exactly why
it is worth keeping: it is the known-answer control the adjudicated generation had to reproduce
before its verdicts were worth anything.

Its `JAIL-SUSPECT` verdict is a proxy, not a control. It fires when a failing run's log merely looks
permission- or network-shaped, which is also what ecosystem rot looks like from the outside.

## `install-script-sweep-<os>-adjudicated.tsv` — produced by the sweep itself

Eleven columns, written by `../install-script-sweep.sh`. A failing row is classified by running the
same package two more ways rather than by reading its log:

| verdict | what happened | what it means |
| --- | --- | --- |
| `OK` | jailed arm passed | nothing to adjudicate |
| `JAIL-CAUSED` | failed jailed, **passed** with `install.buildJail: false` | the jail did it. The only verdict that indicts the jail, and the only one that fails the gate. |
| `NUB-CAUSED` | failed both nub arms, **npm passed** | a package-manager defect, not the jail |
| `UPSTREAM` | all three failed | the package is broken for everyone |
| `NO-SCRIPT-RAN` | no confined spawn | nothing was measured; not a pass |

Columns: `package`, `verdict`, `rc=`, `confined-spawns=`, `jail-lines=`, `net-err=`, `deny=`,
`nojail-rc=`, `npm-rc=`, first error, control error. The three detector columns survive as
diagnostics and no longer decide the verdict.

`confined-spawns=0` on a passing row would mean the jail never ran, so a sweep is only as good as
that column: check it before reading any tally.

⛔ **The three landed TSVs predate that column and spell it `scripts-ran=`, counting POLICY-DUMP LINES
rather than spawns.** `NUB_JAIL_DUMP_POLICY` emits a multi-line dump per confined spawn whose length
tracks the grant’s rule count, so one spawn measured 36 lines for `core-js@3.46.0` and 54 for
`bufferutil@4.0.9`; exactly one line per spawn carries `pkg=`, which is what the column counts now.
No verdict depended on the difference — both forms answer “did the jail engage at all” identically —
but the old number read as a script tally and was not one. Divide an old value by nothing: it is not
a fixed multiple.

## Reproducing

```sh
NUB=/path/to/nub ./tests/jail-acceptance/install-script-sweep.sh \
  --population tests/jail-acceptance/results/install-script-population.tsv
```

Cold every time, `allowBuilds` on so the script actually runs, and the grant left exactly as shipped.
Passing `--population` pins the package set; omitting it rediscovers one from the registry.

## Coverage

The population is **450 packages**, discovered from the registry rather than hand-listed, each pinned to the version people actually install. It was 179 until 2026-09-06, built by asking whether a package's `latest` release runs an install script — a question whose answer had drifted a long way from the one that matters.

**A carrier is a package where some version people install runs an install script, so what counts as "people install" decides the entire figure.** That turned out to be worth measuring rather than choosing. Over the same 25,421 packages above the download gate, one instrument, four rules:

| rule | carriers | change |
| --- | ---: | --- |
| top 3 releases by download | 244 | the census's window |
| top 5 releases by download | 362 | +48% |
| any release holding ≥1% of downloads | 403 | +11% |
| **≥1% summed across releases that carry a script** | **420** | +4% |

**The convergence is the finding, not the last row.** A count that moves 48% between two defensible window widths is a property of the instrument rather than of npm; one that moves 4% at the end of the sequence is close to a property of npm. The same sequence over the band *below* the gate moves by the same 16% from the top-5 window to the shipped rule (864 to 1,001) — one rule, two independent bands, the same correction.

So the shipped rule has no window and no rank in it. A release holding a real share of a package's downloads is installed by real users whatever its rank, and a jail that breaks them is broken. The share is summed across carrying releases rather than taken from the best one, because over a long release history it is spread thin: `@pulumi/azure-native` has 719 carrying releases of which the biggest holds 0.77% of downloads, and together they hold 1.82%. `pickInstalledVersion` in [`discover-install-scripts.mjs`](../discover-install-scripts.mjs) carries the rule and the reasoning.

⛔ **Prior versions of this section reported 75.3% and then 98.53%. Both are withdrawn** — the first measured a population built by reading `latest`, the second measured a population against a carrier set counted under a narrower rule than the one that built it. The figure below is the first where the population and the set it is measured against come from the same instrument.

| | packages | weekly | share of all install-script weight |
| --- | ---: | ---: | ---: |
| swept | 450 | 1,697.3M | **98.39%** |
| missed above the gate | **0** | 0 | **0.00%** |
| missed below the gate | 983 | 27.8M | 1.61% |

The blind spot is **1.61%** of install-script download weight, against a bar whose margin is half a point — 3.2x that margin, and all of it below the gate. Bringing it inside the margin means sweeping the heaviest **362** below-gate carriers, which hold 19.2M weekly between them; they are listed by weight in [`uncovered-carriers.tsv`](uncovered-carriers.tsv), so acting on it is a concatenation rather than another investigation.

⛔ **That figure is scoped to the band the population is drawn from, so it cannot detect what lies below it.** The band is the packages above 100,000 weekly downloads — the gate `scripts/npm-install-script-census.ts` applies with its default `--threshold`. The metric compares the population against the set it was drawn from and would read the same wherever the gate sat. Read it as completeness within the band, and read the third row as the size of what the gate excludes.

**The population is not purely threshold-selected, and that matters for reading the gap.** Placing all 450 against the ranking puts 417 above the gate and 30 below it, reaching down to `iohook` at about 1,100 weekly — `appmetrics`, `bluetooth-hci-socket`, `epoll`, `rocksdb`, `bignum`, `robotjs`, `blake3` and others. Those come from the hand-curated half of the list, which selects on "does this build native code" rather than on popularity. So the sweep already reaches deliberately into the class the threshold misses; it just does not do so exhaustively.

**One package the adjudicated sweep files carry is no longer in the population: `union`.** It carries an install script on `latest` and on almost nothing anyone installs — **484 of its 7.6M weekly downloads are on the release that runs one, against 99.2% on a release that does not**, and it has exactly one carrying release, so summing changes nothing. `netlify` and `@whiskeysockets/baileys` were out under earlier rules and are back in under this one, which is the summed share doing its job — `netlify` has 115 carrying releases holding 7.05% between them, a package no rule that looks at one release at a time was going to catch. The `union` row records a real measurement, so it stays; the package is not swept again.

### The band below the gate

Over ranks ~25,500 to 70,000: 44,056 packages, 346 unresolved, **1,001 carriers holding 27.8M weekly**, of which 286 run a script only on a non-latest release. 18 of those carriers are already in the population, leaving the 983 above. The count is a floor — unresolved lookups are counted separately rather than folded into "no script", so a package's absence from the list is not evidence it never runs one.

Three properties of that band decide what to do about it, and they pull in different directions:

- **It does not run dry.** Carrier density is flat across the rank slices, so scanning further back keeps finding carriers at the same rate.
- **It decays by weight.** Weekly downloads per slice fall about sevenfold from the top of the band to the bottom, so what limits going deeper is weight, not carrier supply.
- **The missed weight is diffuse.** There is no small head to add cheaply — reaching most of it takes hundreds of packages, which is why the 362 above is a large number rather than a shortlist.

What those install scripts DO was sampled by matching their script text — over an earlier 720-package cut of this band, so read the shares rather than the counts. At least 22% of the weight compiles native code and at least 29% downloads a binary; funding and deprecation notices, the case where a breakage would not matter, account for just 6%. The first two are floors rather than shares, because the largest bucket at 40% is scripts that delegate to a file the match never read (`node build.js`, `./scripts/preinstall.sh`), and spot-checking it finds native builds and binary downloads sitting in there — `fibers` compiles, `fast-folder-size` fetches Sysinternals. So the uncovered band is mostly consequential work, and classifying it properly would mean reading the delegated files rather than the manifest.

**Going back further than rank 70,000 is measurably not worth it, and that was not obvious in advance.** A scan over ranks 70,000-150,000 finds *more* carriers, not fewer — 1,821 against 720 at the same window, a higher density — but they hold only 4.6M weekly against the nearer band's 19.9M, so weight per carrier falls from about 27,600 to about 2,500. The whole of that deeper band is worth roughly half a point of blind spot for 1,821 packages. Scanning deeper stays cheap and is worth doing periodically; sweeping deeper does not pay.

⛔⛔ **A rising carrier density with rank appears in these numbers and remains unmeasured rather than established.** The deep band's figure comes from a `latest`-only scan while both nearer bands have been re-measured under the shipped rule, so the comparison is between two instruments rather than two bands. The `latest`-only bias runs in exactly the direction that would manufacture the trend: it bites hardest where the migration to prebuilt binaries has gone furthest, which is on popular packages. Re-running the deep band with the shipped rule is what would settle it.

⛔ **Extending the population is a standing cost, not a one-off, and it differs by platform.** The sweep runs packages serially (`for pkg in $PKGS`), and the per-package jailed-arm times these files record (`jail-s=`) put the current 450 at about 1.4h on Linux, 1.7h on macOS and 2.6h on Windows. Those are the jailed arm alone; the two control arms run only for a row the jailed arm did not clear, which took one measured Windows sweep from 1.05h of jailed-arm time to 1h42m of wall clock. Budget nearer 2.3h, 2.7h and 4.2h per run, per platform. Discovery is the cheap half — about 45 minutes for a whole band, of which 11 is the ranking sweep, so a re-run against a cached ranking costs about 35 minutes, on one machine rather than one per platform. Scan wide, sweep by weight.

⛔ **One Linux row was not produced by a single sweep run: `duckdb`.** Its three arms each allow
twenty minutes, and no prebuilt exists for its Node ABI, so every arm falls back to a source build
that outlives any one builder - three arms together never fit. It was measured instead as three
separate one-arm jobs and the row assembled from them, which is why its three detector columns read
`n/a` rather than a fabricated zero. The verdict stands on the arms themselves: the jail-on arm
engaged the jail (one confined spawn) and the jail-off arm did not (zero), and both then failed with
a byte-identical 404 for `duckdb-v1.4.4-node-v147-linux-x64.tar.gz`, as did npm. Read the row as
"all three arms failed inside twenty-five minutes" rather than "all three arms exited".

⛔ The `rocksdb` and `duckdb` rows were measured after the column rename below, so they read
`confined-spawns=` where the other 85 read `scripts-ran=`. Both mean the same thing for every
verdict in the file.

Windows is the only platform with JAIL-CAUSED rows: fourteen of 180, against zero in 360
measurements across macOS and Linux on the same instrument. The file records that state, measured on
a Server 2022 box before the fixes described below.

Those fourteen are not one defect, and grouping them by how the error reads merges unrelated causes.
Two have since been fixed, and a re-run of all fourteen against the fixed binary flips eight of them
to OK with node-expat held as a negative control that still fails.

The first cause is the interpreter path. nub pre-resolves Python and names it in npm_config_python so
the read grant can be bounded; node-gyp passes that on as --python, and the node-pre-gyp family
re-emits its options onto a shell command line unquoted, so the default install path
C:\Program Files\Python312\python.exe arrived split in two. Naming the interpreter by its 8.3 short
path fixes it.

The second is stdio. libuv maps an "ignore" stdio slot on fd 0 to 2 to CreateFileW("NUL"), and a
LowBox token is refused that device, so uv_spawn fails EPERM before the child starts - measured
directly, and identically for a System32 image and for the interpreter nub stages itself, so the slot
is the variable rather than the image. Separately, a builtin ESM named export is a snapshot taken
when its module facade is created, so a package written as ESM reached the unpatched child_process
and bypassed the stdio repair entirely; that is what left puppeteer spinning inside uv_spawn rather
than failing. Both are repaired in the Windows stdio shim.

What remains is a mix: two packages fail in gyp with a dependency path resolved one level too high,
two fail to run a downloaded binary and fall back to autotools that are not present, one fails a DLL
initialisation routine, and cpu-features cannot reach the Visual Studio setup COM server, which
returns access denied to a LowBox token and which no filesystem grant reaches.
