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

The population grew from 87 packages to 180. The 87 were hand-curated, and measured against the
npm top-downloaded set they carried only 59.9% of the weekly downloads that reach an install script.
The remaining 93 have since been swept, so all three platforms now cover the full 180 - every rate
below is over the whole population.

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
