# Why Windows packages land at `write:"disk"`

The build jail's capability catalog records `write:"disk"` — no filesystem confinement at all — for 96 of 1,466 measured Windows package-versions, against 0 on macOS and 8 on Linux. This doc captures what those packages actually write, measured with Process Monitor rather than inferred from a grant walk, and identifies the mechanisms behind the asymmetry.

**The headline: most of the Windows tail is not a write requirement.** The dominant mechanism is a read-side failure — a jailed script cannot resolve the path of its own temp directory — and it defeats every grant rung except the one that switches confinement off. Two smaller mechanisms account for the rest.

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

**Second instrument: jailed arms with a catalog override.** Each package is installed into a fresh fixture with `NUB_BUILD_JAIL_CATALOG` naming a one-package grant, against an unjailed control fixture. Every arm is checked for the `build-jail catalog OVERRIDDEN` banner and for `REJECTED`, because a malformed override falls back to the compiled catalog silently. Arms are compared by **file inventory**, never by exit code.

## Mechanism 1 — the container temp directory cannot be resolved

This is the largest of the three and the one that explains the flat grant ladders.

Under the jail, Windows gives the LowBox child a redirected `%LOCALAPPDATA%`, and the child's temp directory lands inside the per-launch AppContainer profile. A probe package run jailed with `{"network": true}` and no filesystem grant reports:

```
env.LOCALAPPDATA        = <home>\AppData\Local\Packages\nub_sbx_5524_18c912b94b5c9fec_0\AC
os.tmpdir()             = <home>\AppData\Local\Packages\nub_sbx_5524_18c912b94b5c9fec_0\AC\Temp
lstat(os.tmpdir)        = OK
write(os.tmpdir/canary) = OK
realpath(os.tmpdir)     = EPERM, lstat '<home>\AppData\Local\Packages'
```

The child can write into its temp directory. It cannot resolve the path of it. `realpath` opens every component of the path in turn, and `Packages` — the parent of the container profile — carries no ACE for the AppContainer SID.

That is fatal for a large family of packages because `temp-dir` calls `fs.realpathSync(os.tmpdir())` at module load, and it is a transitive dependency of `tempfile`, which `download`, `decompress`, `bin-build` and `bin-wrapper` all pull in. The whole downloader-backed binary-tool cohort therefore dies before doing any work:

```
Error: EPERM: operation not permitted, lstat '…\home\AppData\Local\Packages'
    at Object.lstatSync (node:fs:1716:25)
    at Object.<anonymous> (…\node_modules\temp-dir\index.js:9:13)
```

`write:"disk"` clears it, but not by granting writes. It is the rung at which nub declines the AppContainer token altogether, so there is no container temp redirect left to resolve. Every narrower rung keeps the token and fails identically — which is exactly the flat ladder the grant walk records, and why the walk cannot localize the cause.

**This is why the tail is Windows-only.** macOS and Linux have no AppContainer and no known-folder temp redirect. `os.tmpdir()` is `/tmp`, there is no container profile in the path, and nothing walks into one.

### Root cause

`ancestor_chain` in [`crates/nub-sandbox/src/backend/windows.rs`](../../crates/nub-sandbox/src/backend/windows.rs) collects the directories that need a traverse ACE from `read_grants`, `write_grants`, `cwd` and `program`. Its own doc comment names the operation precisely — "the directories Node's `realpathSync` opens as targets on its way to a granted leaf".

The container profile is not in any of those lists. It is created separately, later in the same file, which makes `<LOCALAPPDATA>\Packages\<profile>` plus its `AC` and `AC\Temp` children and grants each a **leaf** ACE. Nothing grants `Packages` itself anything, so the repair that exists for every other ancestor never runs for this one.

### The fix, measured

Pre-creating `<LOCALAPPDATA>\Packages` and granting the AppContainer SID traverse on it — what the repair would do in-process — was tested against the same probe at the same `{"network": true}` grant, changing one variable:

| arm | ACE granted on `Packages` | `realpath(os.tmpdir)` |
| --- | --- | --- |
| `{"network": true}` | none | `EPERM` on `…\AppData\Local\Packages` |
| `{"network": true}` | `(OI)(CI)(RX)` — inheritable | `OK` |
| `{"network": true}` | `(RX)` — not inheritable | `OK` |

No filesystem grant was involved in any arm, and only the ACE varied. The non-inheritable form is enough, which matters: the fix does not need to propagate into sibling containers' profiles. The correct in-process form is the non-inherited `TRAVERSE_MASK` the ancestor repair already uses, applied to the container profile's ancestors. `(RX)` as tested is broader than `TRAVERSE_MASK` — it adds read-data and read-EA — but the same in kind, and the mask that already ships is the one to reuse.

### What the catalog can do today

Until that lands, a read grant is enough, because the failure is a read. Measured on `gifsicle@4.0.1`, comparing file inventories against an unjailed control:

| grant | files | bytes | vs control |
| --- | --- | --- | --- |
| control, jail off | 9 | 695,877 | baseline |
| `write:{deps}` + `network` | 8 | 481,757 | binary missing |
| `write:{deps}` + `writePaths:["AppData/Local/Packages"]` + `network` | 8 | 481,757 | binary missing |
| `write:{deps, userHome}` + `network` | 9 | 695,877 | inventory identical |
| `write:{deps}` + `read:{userHome}` + `network` | 9 | 695,877 | inventory identical |

A read-only grant on `userHome` reproduces the unconfined artifact exactly. That is both the proof that the mechanism is a traverse failure and a narrowing available with no code change: `write:"disk"` becomes `write:{deps} + read:{userHome} + network`, which grants no write anywhere outside the package's own directory.

`writePaths` is the wrong lever. It promotes a path out of the jail's private home into the real one; it does not grant traverse on an ancestor the child reaches by absolute path.

## Mechanism 2 — a freshly written binary cannot be executed

Both passing arms above still exit non-zero, with an artifact set byte-identical to the control. After the binary downloads, `bin-wrapper` runs it once as a self-test, that spawn fails, and the failure sends it into a from-source fallback that dies for want of `autoreconf`:

```
? spawn UNKNOWN
? gifsicle pre-build test failed
i compiling from source
- Error: Command failed: cmd.exe /s /c "autoreconf -ivf"
'autoreconf' is not recognized as an internal or external command
```

`spawn UNKNOWN` is already recorded in `backend/windows.rs` as a known signature affecting 26 of 56 cells for this shape, so it is not new. What is new is its consequence for measurement: **a grant that produces the correct artifacts is recorded as failing if the oracle reads the exit code.** A walk that escalates on a non-zero exit will climb past a working narrow grant to `write:"disk"` on a package that never needed it. Whether the corpus oracle is exit-code-based is worth checking directly, because it bounds how much of the 96 is real.

## Mechanism 3 — a refused symlink, which no grant can fix

`cz-customizable@2.6.0` writes nothing at all in the ordinary sense. Its postinstall creates a symlink:

```
>>> config file doesn't exist. I will create one for you.
>>> cz-customizable is about to create this symlink "…\package/.cz-config" to point to your project root directory, 2 levels up.
```

The unjailed control does create it — `cz-config` is a real `SymbolicLink` targeting `<proj>\.cz-config.js`. A LowBox token holds essentially no privileges, `SeCreateSymbolicLinkPrivilege` among them, so under the jail the call fails no matter what the filesystem grant says. This class is structurally irreducible through the catalog: a capability grant cannot restore a stripped privilege.

Two bounds on that reading. The measurement account is an elevated administrator with `SeCreateSymbolicLinkPrivilege` enabled, so the control succeeds here where an ordinary user without Developer Mode would also fail — this package may be broken on Windows independent of the jail. And the symlink was invisible to the first version of the analyzer, because a symlink is `FSCTL_SET_REPARSE_POINT` rather than `CreateFile` plus `WriteFile`; the op set above was corrected after this package read as "0 write paths" while having created one.

## Measured write paths

Unjailed, real environment. "Real" means something was created, written, renamed or deleted — not merely opened for write.

| package | real write paths | inside project / deps | under user profile | elsewhere |
| --- | --- | --- | --- | --- |
| `@arkweid/lefthook@0.7.7` | 2 | `<proj>\.git\hooks\prepare-commit-msg`, `<proj>\lefthook.yml` | none | none |
| `@evilmartians/lefthook@2.1.10` | 4 | all under `<proj>` | none | none |
| `gifsicle@4.0.1` | 1 | `<pkg>\vendor\gifsicle.exe` | none | none |
| `cz-customizable@2.6.0` | 1 symlink | `<pkg>\cz-config` → `<proj>\.cz-config.js` | none | none |

Proposed grants, on the paths alone:

| package | proposed grant | basis |
| --- | --- | --- |
| `@arkweid/lefthook@0.7.7` | `write:{project}` | two project files, no network, no home |
| `@evilmartians/lefthook@2.1.10` | `write:{project}` | four project files |
| `gifsicle@4.0.1` | `write:{deps}` + `network` | downloads one binary into its own directory |
| `cz-customizable@2.6.0` | `write:{deps}` + `write:{project}` | symlink in its own dir, target in the project |

Every one of these carries `write:"disk"` on Windows in the corpus and a narrow grant on macOS and Linux. None of the four writes a single byte outside the project or its own package directory. Their child processes were attributed and real — `lefthook.exe` plus eight `git.exe` for the Evil Martians build, `vendor\gifsicle.exe --version` for gifsicle — so these are not silent no-ops that wrote nothing because they bailed early.

## Reading the corpus's blocked paths

The measurement harness redirects the child's `USERPROFILE`, `LOCALAPPDATA` and `APPDATA` into the fixture home. A corpus record's `home/AppData/Local` prefix therefore names `<fixture-home>\AppData\Local`, not the machine's real profile. The Procmon arm here runs with the real environment. The two agree in the scope vocabulary — `AppData\Local` is under `userHome` either way — but the absolute path strings are not comparable, and diffing them literally will produce a false divergence.

## Bounds

- **Four packages, not nine.** `@nuxt/components`, `docxtemplater`, `@aws-amplify/cli`, `jpegtran-bin` and `mozjpeg` were still running when this was written. `jpegtran-bin` and `mozjpeg` are the same `bin-wrapper` cohort as `gifsicle` and are expected to share mechanism 1; that expectation is not a measurement.
- **The jailed arms ran on nub at `553d8b62ce`**, a corpus-built binary, not the branch tip. Later commits on the branch change the `read:"disk"` rung and exclude the project's own `.env`; nothing in them changes what a package writes, so this is sound for capturing write paths and for testing `write:{deps}` and `read:{userHome}` grants. It is not sound for evaluating the `read:"disk"` rung.
- **Mechanism 1's fix is proven at the mechanism level, not end to end.** Pre-granting traverse makes `realpath` succeed; a package install completing under `write:{deps}` alone on a patched binary has not been run, because mechanism 2 sits behind it for the downloader cohort.
- **The trace runs the lifecycle script directly**, with `node_modules/.bin` on `PATH` and `INIT_CWD` set, rather than through a real `nub install`. That is what makes attribution correct by construction, and it means anything the package manager itself would have done around the script is out of frame.

## Reproducing

Tracer, analyzer and jail harness live on the Windows box under `C:\pm`; per-package results are written to `C:\pm\runs\<spec>\analysis.json`. Long runs must be launched as a detached scheduled task under an S4U principal and polled with short calls — three long-held SSH sessions were killed at around thirteen minutes. The task writes an identity file as its first act, because a task registered to run as `SYSTEM` holds privileges an ordinary user does not and silently changes what is measured.

## Changelog

- 2026-08-05 — Initial write-up. Identifies the AppContainer temp-path resolution failure as the dominant cause of the Windows `write:"disk"` tail, with the root cause located in `ancestor_chain` and the fix confirmed by a single-variable experiment.
