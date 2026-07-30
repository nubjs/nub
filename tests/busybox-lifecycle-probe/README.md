# busybox-lifecycle-probe — the dependency lifecycle shell, on real Windows

A throwaway, branch-scoped CI probe (see `.claude/skills/ci-adhoc-test`). No PR is
needed: pushing to the `busybox-lifecycle-shell` branch runs
`.github/workflows/busybox-lifecycle-probe.yml` on a real `windows-latest` runner.

Sibling of `tests/busybox-run-probe/`, which covers the same shell swap for
`nub run`. This one covers the engine's **dependency lifecycle** path
(`preinstall`/`install`/`postinstall`), which used to hardcode `cmd.exe`.

## The differential

One `nub.exe`, each case installed twice, and the only variable is whether the
`busybox.exe` sidecar sits next to the binary:

| arm | sidecar | shell the engine resolves |
| --- | --- | --- |
| `cmd` | absent | `cmd.exe /d /s /c` — `apply_lifecycle_script_shell` warns and leaves the platform default, byte-identical to the pre-change behavior |
| `busybox` | staged as the win32 package lays it out | `busybox.exe sh -c` |

Every case body is POSIX and is chosen to fail under `cmd.exe`, and the probe
asserts the split in **both** directions: the `busybox` arm must produce the
POSIX result, and the `cmd` arm must **not**. A case that passes in both arms is
not a discriminator and is reported as a failure.

| case | body | asserts |
| --- | --- | --- |
| `param_expansion_and_test` | `echo "MARK=${LIFECYCLE_PROBE:-posix}" > out.txt && test -d . && …` | braced expansion with a default, plus the `test` utility |
| `invokes_shipped_sh_script` | `./scripts/build.sh` | the shape that makes this a fix: `detox-recorder` and `svf-lib` invoke a shipped shell script and fail under `cmd.exe` today |
| `glob_loop_and_redirect` | `for f in src/*.txt; do cat $f >> out.txt; done` | shell-side glob expansion, loop, append redirect |

## Three things the harness has to get right

Each was a real bug caught while building it, and each produced a control that
"confirmed" the wrong answer:

1. **One fixture and one install per case.** A lifecycle failure aborts the
   `JoinSet` driving the parallel pass, which kills sibling scripts mid-write —
   that truncated one case's marker and suppressed another's entirely.
2. **`side-effects-cache=false` in every fixture.** The cache is on by default and
   keys on `(name, version, engine, input hash)`, which is identical across the
   two arms because the shell is not part of the key. With it on, the second arm
   hardlinks the first arm's built tree back and never runs the script.
3. **A distinct package name per case.** With one shared name the
   content-addressed store served the first case's built package directory to all
   of them, so all three reported the first case's marker.

The probe also guards its own setup: it asserts `approve-builds --all` actually
approved the build in each arm, because an unapproved build produces no marker in
either arm and the split would then read as "cmd failed" for the wrong reason.

## Reproducing locally

The runner is cross-platform, but the control arm is meaningful only on Windows —
`apply_lifecycle_script_shell` is Windows-gated, so elsewhere both arms run
`/bin/sh` and the control checks report `SKIP`. A Unix run is still a useful
harness smoke test: it proves the fixtures, the approve step, and every POSIX
body are correct.

```sh
node tests/busybox-lifecycle-probe/run-probe.mjs target/fast/nub /tmp/bblp vendor/busybox-w32/busybox64.exe
```
