# Why Windows packages land at `write:"disk"`

The build jail's capability catalog records `write:"disk"` — no filesystem confinement at all — for around 96 of 1,466 measured Windows package-versions, against 0 on macOS and 8 on Linux. This doc captures what those packages actually write, measured with Process Monitor rather than inferred from a grant walk, and identifies the mechanisms behind the asymmetry.

The corpus counts here are quoted from the collator, not re-derived: different filters over the same records give 96, 101 or 102 for the Windows figure, and nothing below turns on which. What this doc adds is the per-package path evidence and the mechanisms, both measured directly.

**The headline: none of the measured packages needs the whole disk, and none of the failures is a write failure.** One of the six writes into the user profile, and it does so because its preinstall is a global npm install. Five distinct mechanisms make these packages fail under confinement, and four of the five are read-side or exec-side. `write:"disk"` clears all of them for one reason unrelated to writing — it is the rung at which nub declines the AppContainer token, so the confinement that caused the failure is no longer present.

## Method

Two instruments, run on a Windows Server 2022 box as an ordinary interactive user.

**Ground truth: Process Monitor on the unjailed lifecycle script.** The package is fetched with `npm pack`, unpacked, and its own dependencies materialized with `npm install --ignore-scripts` — all outside the capture window, so npm's file traffic never enters the trace. Procmon then captures while the lifecycle command runs under `cmd.exe`, and the CSV is filtered to that command's **process subtree**, reconstructed from Procmon's own `Process Create` records.

Only operations that changed the filesystem are counted:

| counted as a write | signal |
| --- | --- |
| file created | `CreateFile` with `OpenResult: Created`, `Overwritten` or `Superseded` |
| file written | successful `WriteFile`, `SetEndOfFileInformationFile`, `SetAllocationInformationFile` |
| renamed / deleted | `SetRenameInformationFile`, `SetDispositionInformationFile` with `Delete: True` |
| symlink created | `FileSystemControl` with `FSCTL_SET_REPARSE_POINT` |

A `CreateFile` returning `OpenResult: Opened` is **not** counted. A `mkdir` that collides with an existing directory creates nothing, and counting it inflates the apparent requirement — the same correction that cut an earlier Linux tail from four candidates to one.

**The instrument was validated against a known answer before any package ran.** A synthetic package writing to five pre-chosen paths — project, `$HOME`, `AppData\Local`, `TEMP`, and the `C:\` root — was traced. All five were captured with exact paths and correct scope, and subtree attribution followed `cmd.exe → node.exe → cmd.exe` two levels down. 255,458 CSV rows reduced to 153 subtree rows, so the filter discriminates rather than passing everything through. Without this control, a result of "no writes outside the project" is indistinguishable from a broken filter.

**Second instrument: jailed arms with a catalog override.** Each package is installed into a fresh fixture with `NUB_BUILD_JAIL_CATALOG` naming a one-package grant, against an unjailed control fixture. Every arm is checked for the `build-jail catalog OVERRIDDEN` banner and for `REJECTED`, because a malformed override falls back to the compiled catalog silently.

**Third instrument: a probe package.** A one-file postinstall that reports its environment and attempts a fixed list of operations — resolving its temp directory, spawning tools, enumerating directories. It isolates one mechanism per line, where a real package confounds several.

## Measured write paths

Unjailed, real environment. "Real" means something was created, written, renamed or deleted — not merely opened for write.

| package | real write paths | project / own package dir | under user profile | elsewhere |
| --- | --- | --- | --- | --- |
| `@arkweid/lefthook@0.7.7` | 2 | `<proj>\.git\hooks\prepare-commit-msg`, `<proj>\lefthook.yml` | none | none |
| `@evilmartians/lefthook@2.1.10` | 4 | all under `<proj>` | none | none |
| `gifsicle@4.0.1` | 1 | `<pkg>\vendor\gifsicle.exe` | none | none |
| `cz-customizable@2.6.0` | 1 symlink | `<pkg>\cz-config` → `<proj>\.cz-config.js` | none | none |
| `@nuxt/components@2.1.0` | 4 | 1 under `node_modules` | 3 — `%TEMP%\v8-compile-cache\…` | none |
| `docxtemplater@0.3.0` | 2,080 | none | 1,478 `AppData\Roaming\npm`, 301 `AppData\Local\npm-cache`, 301 other | none |

Five of the six write nothing outside the project or their own package directory. `@nuxt/components`' three out-of-project writes are `v8-compile-cache` entries created by the yarn process it spawns, not by the package. Child processes were attributed and real — `lefthook.exe` plus eight `git.exe` for the Evil Martians build, `vendor\gifsicle.exe --version` for gifsicle — so these are not no-ops that wrote nothing because they bailed early.

**`docxtemplater@0.3.0` is the one genuine user-profile writer, and it is still not a disk writer.** Its preinstall is literally `npm install gulp -g`, so it writes the global npm prefix under `AppData\Roaming\npm` and npm's cache under `AppData\Local\npm-cache`, and nothing else. The raw analyzer output reported 2,082, two of which are NTFS volume metadata (`C:` and `C:\$Directory`) that the noise filter did not match; they are excluded here and the filter has been widened. `write:{userHome}` plus `network` covers it. This agrees with the Linux trace of the same package, which found real creations under `/usr/local` for the same reason — the one spec in that tail that survived refutation.

Proposed grants on the path evidence alone, before the mechanisms below are taken into account:

| package | proposed grant |
| --- | --- |
| `@arkweid/lefthook@0.7.7` | `write:{project}` |
| `@evilmartians/lefthook@2.1.10` | `write:{project}` |
| `gifsicle@4.0.1` | `write:{deps}` + `network` |
| `cz-customizable@2.6.0` | `write:{deps}` + `write:{project}` |
| `@nuxt/components@2.1.0` | `write:{deps}` |
| `docxtemplater@0.3.0` | `write:{userHome}` + `network` |

## The five mechanisms

### 1. The container temp directory cannot be resolved

Under the jail, Windows gives the LowBox child a redirected `%LOCALAPPDATA%`, and its temp directory lands inside the per-launch AppContainer profile. The probe, jailed with `{"network": true}` and no filesystem grant:

```
env.LOCALAPPDATA        = <home>\AppData\Local\Packages\nub_sbx_5524_18c912b94b5c9fec_0\AC
os.tmpdir()             = <home>\AppData\Local\Packages\nub_sbx_5524_18c912b94b5c9fec_0\AC\Temp
lstat(os.tmpdir)        = OK
write(os.tmpdir/canary) = OK
realpath(os.tmpdir)     = EPERM, lstat '<home>\AppData\Local\Packages'
```

The child can write into its temp directory. It cannot resolve the path of it. `realpath` opens every component in turn, and `Packages` — the parent of the container profile — carries no ACE for the AppContainer SID.

That is fatal for a large family, because `temp-dir` calls `fs.realpathSync(os.tmpdir())` at module load and is a transitive dependency of `tempfile`, which `download`, `decompress`, `bin-build` and `bin-wrapper` all pull in. The downloader-backed binary-tool cohort dies before doing any work:

```
Error: EPERM: operation not permitted, lstat '…\home\AppData\Local\Packages'
    at Object.lstatSync (node:fs:1716:25)
    at Object.<anonymous> (…\node_modules\temp-dir\index.js:9:13)
```

**Root cause.** `ancestor_chain` in [`crates/nub-sandbox/src/backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) collects the directories needing a traverse ACE from `read_grants`, `write_grants`, `cwd` and `program`. Its own doc comment names the operation exactly — "the directories Node's `realpathSync` opens as targets on its way to a granted leaf". The container profile is not in any of those lists: it is created separately, later in the same file, which makes `<LOCALAPPDATA>\Packages\<profile>` plus its `AC` and `AC\Temp` children and gives each a **leaf** ACE. Nothing grants `Packages` itself anything, so the repair that exists for every other ancestor never runs for this one.

**The fix, measured.** Granting the AppContainer SID traverse on `Packages` — what the repair would do in-process — was tested against the same probe at the same grant, varying only the ACE:

| ACE granted on `Packages` | `realpath(os.tmpdir)` |
| --- | --- |
| none | `EPERM` on `…\AppData\Local\Packages` |
| `(OI)(CI)(RX)` — inheritable | `OK` |
| `(RX)` — not inheritable | `OK` |

No filesystem grant was involved in any arm. The non-inheritable form suffices, so the fix need not propagate into sibling containers' profiles. The right in-process form is the non-inherited `TRAVERSE_MASK` the ancestor repair already uses; `(RX)` as tested is broader — it adds read-data and read-EA — but the same in kind.

**The catalog workaround today.** Because the failure is a read, a read grant clears it. Measured on `gifsicle@4.0.1` against an unjailed control:

| grant | files | bytes | vs control |
| --- | --- | --- | --- |
| control, jail off | 9 | 695,877 | baseline |
| `write:{deps}` + `network` | 8 | 481,757 | binary missing |
| `write:{deps}` + `writePaths:["AppData/Local/Packages"]` + `network` | 8 | 481,757 | binary missing |
| `write:{deps, userHome}` + `network` | 9 | 695,877 | identical |
| `write:{deps}` + `read:{userHome}` + `network` | 9 | 695,877 | identical |

A read-only grant on `userHome` reproduces the unconfined artifact exactly. That both proves the mechanism is a traverse failure and offers a narrowing with no code change. `writePaths` is the wrong lever: it promotes a path out of the jail's private home into the real one, and does not grant traverse on an ancestor the child reaches by absolute path.

### 2. Globally installed CLIs disappear from `PATH` — this is the exit-127 family

The probe, jailed with `{"network": true}`, against the unjailed control:

| check | control | jailed |
| --- | --- | --- |
| `PATH` contains `…\AppData\Roaming\npm` | true | true |
| `readdir` that directory | `gulp, gulp.cmd, gulp.ps1, node_modules, yarn` | **EPERM** |
| `spawn("yarn --version")` | `0` — `1.22.22` | `'yarn' is not recognized as an internal or external command` |

Every CLI installed with `npm i -g` lives in that one directory, and Windows puts its literal absolute path on `PATH`. The jail leaves the `PATH` entry in place and denies read on the directory, so the shell cannot find the shim and reports command-not-found. **This is a missing read/execute grant, not a missing write grant**, and it accounts for the shape of the 51 cells recorded as `` `postinstall` exited with code 127 ``.

Two of the nine packages here are in exactly this position: `@nuxt/components@2.1.0`'s postinstall is `yarn link && yarn link @nuxt/components`, and `docxtemplater@0.3.0`'s preinstall is `npm install gulp -g` — and `gulp` is one of the entries listed above.

### 3. Git for Windows cannot open `/dev/null`

Same probe, same grant:

```
spawn(git --version)  status=128
  fatal: could not open '/dev/null' for reading and writing: Permission denied
```

The control returns `git version 2.47.1.windows.1`. Git for Windows' MSYS layer maps `/dev/null` onto `\Device\Null`, which the AppContainer token denies.

This is what actually breaks the hook installers, and it breaks them **silently**. `@arkweid/lefthook@0.7.7` jailed at `write:{project}` exits 0 and produces a package directory byte-identical to the control — while `lefthook.yml` and `.git/hooks/prepare-commit-msg` are both absent. The install log carries lefthook's own diagnosis:

```
This command must be executed within git repository.
Change working directory or initialize new repository with 'git init'.
```

Lefthook shelled out to `git rev-parse`, git failed on `/dev/null`, lefthook concluded there was no repository and bailed without writing anything.

The obvious competing explanation — that `write:{project}` does not cover `.git` — is ruled out by a control: the probe re-run at `write:{project}`, the exact grant the lefthooks used, still gets `status=128` and the same `/dev/null` message from `git --version`, a command that touches no repository at all. Mechanism 2 reproduces at that grant too.

The `NULL-device limit` is already a known open item; this is a precise witness for it, and it means the `.git/hooks` cohort cannot be narrowed by any grant until it is closed.

### 4. Symlink creation is refused

`cz-customizable@2.6.0` writes nothing in the ordinary sense. Its postinstall creates a symlink:

```
>>> config file doesn't exist. I will create one for you.
>>> cz-customizable is about to create this symlink "…\package/.cz-config" to point to your project root directory, 2 levels up.
```

The unjailed control creates it — `cz-config` is a real `SymbolicLink` targeting `<proj>\.cz-config.js`. A LowBox token holds essentially no privileges, `SeCreateSymbolicLinkPrivilege` among them, so under the jail the call fails whatever the filesystem grant says. A capability grant cannot restore a stripped privilege, so this class is structurally irreducible through the catalog.

Two bounds. The measurement account is an elevated administrator with that privilege enabled, so the control succeeds here where an ordinary user without Developer Mode would also fail — this package may be broken on Windows independent of the jail. And the symlink was invisible to the first version of the analyzer, because a symlink is `FSCTL_SET_REPARSE_POINT` rather than `CreateFile` plus `WriteFile`; the op set above was corrected after this package read as "0 write paths" while having created one.

### 5. A freshly written binary cannot be executed

After `gifsicle` downloads its binary under a grant that clears mechanism 1, `bin-wrapper` runs it once as a self-test, that spawn fails, and the failure sends it into a from-source fallback that dies for want of `autoreconf`:

```
? spawn UNKNOWN
? gifsicle pre-build test failed
i compiling from source
- Error: Command failed: cmd.exe /s /c "autoreconf -ivf"
'autoreconf' is not recognized as an internal or external command
```

`spawn UNKNOWN` is already recorded in `backend/windows.rs` as a known signature covering 26 of 56 cells for this shape.

## What this means for the measurement

Three of these mechanisms produce a **non-zero exit on a run that did everything right**, and one produces a **zero exit on a run that did nothing at all**. Both directions corrupt a grant walk that reads exit codes:

- `gifsicle` at `write:{deps} + read:{userHome} + network` produces an artifact set byte-identical to the unconfined control and still exits 1, because of mechanism 5. A walk that escalates on non-zero exit climbs past a working narrow grant all the way to `write:"disk"`.
- `@arkweid/lefthook` at `write:{project}` exits 0 having written neither of its two output files, because of mechanism 3. A walk that accepts a zero exit records a grant that does not work.

Whether the corpus oracle reads exit codes or artifacts is therefore the single highest-value thing to check next, because it bounds how much of the 96 is a real requirement rather than a measurement artifact. On the evidence here the answer is: very little of it.

A third variant is worth naming because it is invisible from the outside. `@aws-amplify/cli@12.9.0`'s postinstall is:

```
node ./lib/install.js || echo "failed to install amplify binary"
```

The `|| echo` swallows the failure, so **the script exits 0 whether or not it installed anything**, and the string the corpus records as this package's signature — `failed to install amplify binary` — is just that echo firing. It carries no information about the cause. For this package and any shaped like it, an exit code cannot distinguish success from failure at any rung, and only an artifact comparison can.

**This methodological trap caught this investigation too.** The first jailed lefthook arms were scored on the installed package directory, which is populated by nub's linker rather than by the lifecycle script, so both arms matched at 12 files / 40,232,920 bytes and read as a clean pass. The missing `lefthook.yml` only appeared on a direct listing of the project directory. Comparing artifacts instead of exit codes is necessary but not sufficient — the artifact compared has to be the one the script produces.

**The corpus harness does not have this gap**, checked rather than assumed: its `paths()` walk roots at the whole fixture, covering both `proj/` and `home/`, and it skips git's own bookkeeping while explicitly descending into `.git/hooks`. Both of lefthook's outputs are therefore inside its digest. The blind spot was in the ad-hoc harness written for this investigation, not in the corpus.

## Reading the corpus's blocked paths

The measurement harness redirects the child's `USERPROFILE`, `LOCALAPPDATA` and `APPDATA` into the fixture home. A corpus record's `home/AppData/Local` prefix therefore names `<fixture-home>\AppData\Local`, not the machine's real profile. The Procmon arm here runs with the real environment. The two agree in the scope vocabulary — `AppData\Local` is under `userHome` either way — but the absolute path strings are not comparable, and diffing them literally produces a false divergence.

## Bounds

- **Six packages traced, not nine.** `@aws-amplify/cli`, `jpegtran-bin` and `mozjpeg` were still running when this was written. The latter two are the same `bin-wrapper` cohort as `gifsicle` and are expected to share mechanism 1; that expectation is not a measurement.
- **`@nuxt/components` and `docxtemplater` are placed in mechanism 2 by their traced spawns**, which name `…\AppData\Roaming\npm\node_modules\yarn\bin\yarn.js` and npm's global install respectively — the directory the probe measured as unreadable under the jail. Neither has been re-run jailed to watch it fail there.
- **The jailed arms ran on nub at `553d8b62ce`**, a corpus-built binary, not the branch tip. Later commits change the `read:"disk"` rung and exclude the project's own `.env`; nothing in them changes what a package writes, so this is sound for capturing write paths and for testing `write:{deps}`, `read:{userHome}` and `write:{project}` grants. It is not sound for evaluating the `read:"disk"` rung.
- **Mechanism 1's fix is proven at the mechanism level, not end to end.** Pre-granting traverse makes `realpath` succeed; no package has been installed to completion under `write:{deps}` alone on a patched binary, because mechanisms 3 and 5 sit behind it.
- **No narrow grant is yet proven end to end for any package.** Mechanism 1 has a working catalog workaround for `gifsicle`, but mechanism 5 keeps its exit code non-zero; the lefthooks are blocked by mechanism 3, which the catalog cannot address at all. The correct order is to fix mechanisms 1, 2 and 3 in the backend and re-measure, not to widen grants.
- **The trace runs the lifecycle script directly**, with `node_modules/.bin` on `PATH` and `INIT_CWD` set, rather than through a real `nub install`. That is what makes attribution correct by construction, and it means anything the package manager does around the script is out of frame.

## Reproducing

**Building nub on that box needs two things that are not obvious.** Wipe `~/.cargo/registry/src` and `~/.cargo/registry/cache` first — a corrupted registry copy produces 41 errors confined to `dashmap`, mixing `E0412` with `E0514`, which reads like a compiler-version problem and is not. Then put `cmake` on `PATH`: `libz-ng-sys` builds through it, VS BuildTools ships it at `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin`, and nothing puts it there. Have the build write its own `EXIT=<n>` status file: a scheduled task's `LastTaskResult` is the shell's exit code, not cargo's, and reports 0 over a failed build.

Tracer, analyzer, jail harness and probe live on the Windows box under `C:\pm`; per-package results are written to `C:\pm\runs\<spec>\analysis.json`. Long runs must be launched as a detached scheduled task under an S4U principal and polled with short calls — three long-held SSH sessions were killed at around thirteen minutes. The task writes an identity file as its first act, because a task registered to run as `SYSTEM` holds privileges an ordinary user does not and silently changes what is measured.

## Changelog

- 2026-08-05 — Initial write-up. Five mechanisms behind the Windows `write:"disk"` tail, none of them a write requirement: AppContainer temp-path resolution, the unreadable global npm bin directory behind the exit-127 family, `/dev/null` denial breaking Git for Windows and with it the hook installers, refused symlink creation, and `spawn UNKNOWN` on a freshly written binary. Root cause for the first located in `ancestor_chain` and confirmed by a single-variable experiment.
