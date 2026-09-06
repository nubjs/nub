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

The population is 179 packages, grown from a hand-curated 87. Measured against the npm top-downloaded set, those 87 carried only 59.9% of the weekly downloads that reach an install script. The rest have since been swept, so all three platforms cover the whole population and every rate below is over it.

The three adjudicated sweep files carry 180 rows rather than 179. The extra row is `@whiskeysockets/baileys`, measured while it was in the population and dropped from it afterwards: its stable release carries no install script, and it was only ever discovered because the lookup judged it by a prerelease dist-tag. The row records a real measurement, so it stays; the package is not swept again.

⛔ **The coverage figure is scoped to the band the population is drawn from, so it cannot detect what lies below it.** "Covers the script-carrying userbase" is measured against the npm top-downloaded set, and that set is the packages above 100,000 weekly downloads - the gate `scripts/npm-install-script-census.ts` applies with its default `--threshold`. The metric therefore compares the population against the set it was drawn from, and would read the same wherever the gate sat. Read it as completeness within the band, never as a claim about the ecosystem.

⛔⛔ **The population is also incomplete INSIDE the band it claims to cover, which matters more than the gate's position.** Ranking each package's versions by actual weekly downloads over ranks 1-25,500 finds 244 install-script carriers, of which the sweep covers 107. The other **137 hold 286.7M weekly downloads between them - 30.8% of the population's own weight** - and they are listed in [`uncovered-above-gate.tsv`](uncovered-above-gate.tsv). `sharp` at 83M and `protobufjs` at 75M are the two largest. Five were hand-verified against their manifests, including `tree-sitter-python` running `node-gyp-build`.

⭐ **Those 137 have now been swept on all three platforms, and the jail survives them: zero `JAIL-CAUSED` anywhere.** 121 of the 137 resolved to a usable version; per-platform results are in [`uncovered-above-gate-sweep-linux.tsv`](uncovered-above-gate-sweep-linux.tsv), [`-macos`](uncovered-above-gate-sweep-macos.tsv) and [`-win`](uncovered-above-gate-sweep-win.tsv).

| platform | OK | JAIL-CAUSED | UPSTREAM | NO-SCRIPT-RAN | BLOCKED |
| --- | ---: | ---: | ---: | ---: | ---: |
| linux | 109 | **0** | 7 | 4 | 1 |
| macos | 109 | **0** | 7 | 4 | 1 |
| win | 115 | **0** | 1 | 4 | 1 |

⛔ **The run means something only because versions were PINNED to the release that carries the script.** 40 of the 121 run an install script on an older release than `latest`, so a normal sweep would have scored them `NO-SCRIPT-RAN` - the jail never engaging, every row reading as a pass, and a clean result that tested nothing. Pinned, **116 of 121 ran a confined script** on every platform. Windows carried its own guards for the same reason: the sweep refuses to run as `SYSTEM`, whose extra privileges have previously produced a clean and wrong reproduction, and refuses a backslash `NUB` path, which makes the busybox sidecar unfindable and silently drops the engine onto `cmd.exe`.

⇒ **So this is a MEASUREMENT hole, not a live breakage.** The 286.7M weekly downloads are not being broken by the jail today; they are simply invisible to the harness. That lowers the urgency of adding them to the population and raises the value of fixing discovery, since what is wrong is that we cannot see them.

Two separate causes produce that, and only one is subtle. 56 of the 137 run their install script on a version that is *not* `latest` - `sharp`'s most-installed release, 0.34.5 at 30.7M weekly, runs one while `latest` does not - and `discover-install-scripts.mjs` judges candidates by `latest`, so it cannot see them however good its candidate list is. `scripts/npm-install-script-census.ts` was built to handle exactly this and says so in its header, but the population is generated by the discovery script rather than from the census, so that version-awareness never reaches it. The other 81 carry a script on `latest` and are simply absent from the hardcoded `CANDIDATES` array, so the claim that it unions in every carrier in the top-downloaded set does not hold today whatever was true when it was written.

Below the gate, over ranks ~25,500-70,000, the same version-aware scan finds 678 carriers holding 18.4M weekly, 103 of them visible only because it reads more than `latest`. So the two gaps are wildly different sizes, and the effort they justify is not close:

| | packages | weekly | share of all install-script weight |
| --- | ---: | ---: | ---: |
| swept today | 179 | 931.4M | 75.3% |
| **missed inside the claimed band** | **137** | **286.7M** | **23.2%** |
| missed below the gate | 678 | 18.4M | 1.5% |

The blind spot is 24.7% of install-script download weight, against a bar whose margin is half a point. Nearly all of it is the first row. Closing it costs about 0.4h on Linux or 0.8h on Windows of jailed-arm time for 137 packages; the below-gate band costs five times that for a fifteenth of the weight, which is **77x less weight per package swept**. Fix the band the population already claims before widening it.

Measured below that gate on 2026-09-05, over ranks ~25,500 to ~70,000: 44,055 packages, 696 of them unresolved, and **678 of the rest carry an install script on a version people install, holding 18.4M weekly downloads** - about 2.0% of the weight this population covers (931M weekly, taken from the same source and the same monthly-to-weekly conversion, so the two sides of that ratio are comparable).

⛔ An earlier pass over the same band read each package's `latest` manifest alone and reported 720. That number is withdrawn, and the size of the correction is the argument for the method: the two answers share only **575** packages. 103 carriers run a script only on a non-latest release and were invisible to the old read; 145 that it counted carry one on a release almost nobody installs. So a fifth of the old figure was wrong in each direction, not merely low.

⛔ 678 is still a floor. 696 lookups did not resolve and are counted separately rather than folded into "no script", so a package's absence from the list is not evidence it never runs one.

Three properties of that band decide what to do about it, and they pull in different directions:

- **It does not run dry.** Carrier density is flat at 1.33%-1.83% across all eight rank slices, so scanning further back keeps finding carriers at the same rate.
- **It decays by weight.** The weekly downloads per slice fall about sevenfold from the top of the band to the bottom, so what limits going deeper is weight, not carrier supply.
- **The missed weight is diffuse, not concentrated.** The top 25 carriers hold 10.9% of it, the top 100 hold 35.4%, and reaching 80% of it takes about 400 packages. There is no small head to add cheaply. (That 400 is a share of the missed weight; the separate figure of 380 below is what brings the blind spot inside the bar's margin. They are close by coincidence, not by construction.)

What those install scripts DO was sampled by matching their script text - over the earlier 720-package set, so read the shares rather than the counts - which puts a floor under how much of the missed weight is risky rather than trivial. At least 22% of it compiles native code and at least 29% downloads a binary; funding and deprecation notices - the case where a breakage would not matter - account for just 6%. Those first two are floors rather than shares, because the largest bucket at 40% is scripts that delegate to a file the match never read (`node build.js`, `./scripts/preinstall.sh`, `node scripts/postinstall.mjs`), and spot-checking it finds native builds and binary downloads sitting in there - `fibers` compiles, `fast-folder-size` fetches Sysinternals. So the honest reading is that the uncovered band is mostly consequential work, and classifying it properly would mean reading the delegated files rather than the manifest.

The population is not purely threshold-selected, and that matters for reading the gap. Placing all 179 against the ranking puts 151 above the gate and 28 below it, reaching down to `iohook` at about 1,100 weekly - `appmetrics`, `bluetooth-hci-socket`, `epoll`, `rocksdb`, `bignum`, `robotjs`, `blake3`, `hrtime`, `llnode`, `node-report` and others. Those come from the hand-curated half of the list, which selects on "does this build native code" rather than on popularity. So the sweep already reaches deliberately into the class the threshold misses; it just does not do so exhaustively.

The uncovered head is mostly native builders and prebuilt fetchers - `@vscode/windows-process-tree`, `@vscode/spdlog`, `iconv`, `lzma-native`, `couchbase`, `cld`, `webgpu`, `@ffmpeg-installer/linux-arm` - so the band is not a low-risk tail.

**What the gap costs, measured against the bar the jail is actually held to.** Unmeasured weight is uncertainty in any measurement against the 99.5% userbase-weighted target, whose margin is half a point. Today the blind spot is 2.09% of all install-script weight - **4.2x that margin**. Adding the top 200 of these carriers takes it to 0.93%, and it drops inside the margin at **about 380 packages**, which hold 15.2M weekly between them. At the measured per-package times that is roughly 1.2h on Linux, 1.4h on macOS and 2.2h on Windows of jailed-arm time. The 657 rows worth considering are listed in [`uncovered-carriers.tsv`](uncovered-carriers.tsv), ordered by weight, so acting on this is a concatenation rather than another investigation.

**Going back further than rank 70,000 is measurably not worth it, and that was not obvious in advance.** The same scan run over ranks 70,000-150,000 finds *more* carriers, not fewer - 1,821 against 720, at a higher density of 2.31% - but they hold only 4.6M weekly between them against the nearer band's 19.9M. Both sides of that comparison are the latest-only read, which is the like-for-like pairing; the deep band has not been re-scanned version-aware, which is why the density gradient below stays withdrawn. Weight per carrier falls from about 27,600 to about 2,500. So the whole of that deeper band is worth 0.48 points of blind spot, roughly the bar's own margin, for 1,821 packages and something like 5.7h on Linux or 10.7h on Windows of jailed-arm time. The nearer band's top 380 buys three times as much for a fifth of the effort. Scanning deeper stays cheap and remains worth doing periodically; sweeping deeper does not pay.

⛔⛔ **A rising carrier density with rank appears in these numbers, and it remains unmeasured rather than established.** The nearer band now reads 1.6% under a version-aware scan and the deeper one reads 2.31% under a latest-only scan, so the comparison is between two instruments rather than two bands - which is worse than when the gradient was first noted, not better. The latest-only bias runs in exactly the direction that would manufacture this trend: it bites hardest where the migration to prebuilt binaries has gone furthest, which is on popular packages, thinning the apparent density of the nearer bands specifically. Re-running the deep band with the same version-aware rule is what would settle it, and until that happens the gradient is not evidence that install scripts are moving to the long tail.

⛔ Extending the population is a standing cost, not a one-off, and it is not the same cost on every platform. The sweep runs packages serially (`for pkg in $PKGS`), and the per-package jailed-arm times these results files already record (`jail-s=`) put the 678 further packages at about 2.2h on Linux, 2.5h on macOS and 4.0h on Windows. Those figures are the jailed arm alone; the two control arms run only for a row the jailed arm did not clear, which is what took the measured Windows sweep from 1.05h of jailed-arm time to 1h42m of wall clock. Budget nearer 3h, 3.5h and 6.5h respectively, per run, per platform. Discovery is the cheap half: about 45 minutes for the whole band end to end, of which 11 is the ranking sweep and 35 the registry lookups, so a re-run against a cached ranking costs about 35. That is minutes against hours, on one machine rather than one per platform, which is why the two halves are worth keeping separate: scan wide, sweep by weight.

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
