# win-shell-probe — which shell can run real npm script bodies on Windows?

A throwaway, branch-scoped CI probe (see `.claude/skills/ci-adhoc-test`). No PR
is needed: pushing to the `win-shell-probe` branch runs
`.github/workflows/win-shell-probe.yml` on a real `windows-latest` runner.

## The question

nub is evaluating a cross-platform script shell so `package.json` script bodies
behave the same on Windows as on macOS/Linux. Every candidate has to clear one
gate before any grammar question matters:

**Does it resolve and execute a `node_modules/.bin/*.cmd` shim on real Windows?**

That is the entire reason a cross-platform shell is wanted here. A shell with a
beautiful POSIX grammar that cannot launch `tsc` or `vite` from `.bin` is
useless for `nub run`.

## The compat bar is POSIX sh, not bash

npm runs script bodies with `sh -c`. On Debian and Ubuntu `/bin/sh` is dash, so
the guarantee script authors have ever had is POSIX, not bash. Every case in
`cases.mjs` is tagged:

| tag     | meaning                                                                 |
| ------- | ----------------------------------------------------------------------- |
| `POSIX` | mandated by POSIX.1-2017 "Shell Command Language" — a candidate must pass |
| `bash`  | bash/ksh extension — a bonus, never guaranteed by npm                    |
| `npm`   | `node_modules/.bin` shim resolution — the headline gate                  |
| `win`   | Windows path handling — no spec, but a hard practical requirement        |

A candidate that covers every `POSIX` row is sufficient.

## Candidates

| shell            | how it gets onto the runner                          |
| ---------------- | ---------------------------------------------------- |
| cmd.exe          | built in — today's npm/nub Windows default, the baseline |
| Git Bash (MSYS2) | preinstalled at `C:\Program Files\Git\bin\bash.exe`   |
| busybox-w32      | `choco install busybox`, else a single-exe download   |
| brush            | `cargo install brush-shell --locked --version 0.4.0`  |

brush publishes no Windows release binary and has no Windows CI leg upstream, so
it is built on the runner; the build time is itself reported as a finding.

Git Bash is run twice — once plain, once with `MSYS_NO_PATHCONV=1` and
`MSYS2_ARG_CONV_EXCL=*` — to quantify MSYS2's POSIX↔Windows argument
translation separately from its grammar coverage.

## The fixture

`make-fixture.mjs` writes npm 10's actual generated shims into a real
`node_modules/.bin`, with three deliberately asymmetric tools:

- `mytool` — the full npm triplet (`.cmd`, extensionless `sh`, `.ps1`), i.e. what
  a real `npm install` leaves behind.
- `onlycmd` — **only** `onlycmd.cmd`. Resolving it requires PATHEXT-style
  extension probing, which a naive POSIX `execvp` port does not do.
- `onlysh` — **only** the extensionless `#!/bin/sh` script. Resolving it requires
  shebang interpretation, which cmd.exe cannot do.

`node_modules/.bin` goes on the child's PATH exactly as `nub run` / `npm run`
assemble it.

## How the driver invokes each shell

`run-matrix.mjs` mirrors how nub spawns a script shell today
(`crates/nub-cli/src/cli.rs` ~3819-3890):

- POSIX shell → `<shell> -c <body>` with normal argv escaping.
- cmd.exe → `cmd /d /s /c <body>` with `windowsVerbatimArguments`, so the body
  reaches cmd exactly as written (npm's behavior, which nub copies).

Child stdio goes to **files, never pipes**. Two cases have known hang modes in at
least one candidate (`2>&1 |` deadlock, background `&` wait); a wedged grandchild
holding a pipe write-end would otherwise hang the driver past its own timeout.
Files make the per-case timeout authoritative, and results are written with
`fs.writeSync` so the log is complete right up to any hang.

## Reading the output

One line per case, greppable straight out of the run log:

```
BODY  |<shell>|<case>|<spec>|<the exact body, JSON-quoted>
RESULT|<shell>|<case>|<group>|<spec>|<exit>|<flag>|<elapsed>|<stdout>|<stderr>
TIMING|<shell>|<iters>|<total>|<mean>
SUM   |<case>|<spec>|<exit>|<flag>|<first stdout line>
TALLY |<shell>|posix_nonzero_or_flagged=n/N|npm_shim_nonzero_or_flagged=n/N
```

`flag` is `ok`, `TIMEOUT`, `SIGNAL:<sig>`, or `SPAWN_ERR:<code>`.

**`TALLY` is a coarse screen, not a verdict.** A zero exit does not mean the case
behaved correctly — `deno_task_shell`'s entire `${…}` class fails *silently* with
exit 0 and a literal passthrough. Read the per-case stdout.

`TIMING` includes a constant `spawnSync` overhead identical across shells, so
compare the shells against each other rather than treating the mean as an
absolute process-spawn cost.

## Reproducing locally

The harness is cross-platform; running it against a known-good `/bin/sh` is the
control that validates the case bodies themselves:

```sh
node tests/win-shell-probe/make-fixture.mjs /tmp/wsp/fixture
node tests/win-shell-probe/run-matrix.mjs \
  --shell sh --bin /bin/sh --fixture /tmp/wsp/fixture --work /tmp/wsp/work
```

macOS `/bin/sh` (bash 3.2 in POSIX mode) passes 55/55 `POSIX` rows and, as
expected, fails the two Windows-only shim rows (`onlycmd`, `mytool.cmd`).
