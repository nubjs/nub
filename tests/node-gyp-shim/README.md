# node-gyp lazy-shim probe

Exercises the lazy `node-gyp` shims on a real OS — above all **Windows**, where nothing else covers them. `native-deps.yml` is the only workflow that touches node-gyp and it is `runs-on: ubuntu-latest`, so the `.cmd` shim otherwise ships unverified.

```sh
node tests/node-gyp-shim/run.mjs <path-to-nub>
```

Exits non-zero if any scenario fails. Scenarios 3 and 8 reach the real registry to bootstrap node-gyp; the rest are offline.

## What it covers

| # | Scenario | Guards |
| --- | --- | --- |
| 1 | `nub --version` | the binary spawns at all on this OS |
| 2 | install with no build scripts | fixture, store, linker |
| 3 | approved build calls `node-gyp`, none on PATH | **positive control** — the shim resolves, bootstraps, and leaves a bucket |
| 4 | approved build never calls node-gyp, registry unreachable | the regression: install succeeds and bootstraps nothing |
| 5 | shim run with `AUBE_NODE_GYP_PROJECT_DIR` unset | the cwd fallback in the `sh` and `cmd` shims |
| 6 | shim runs a failing node-gyp | `setlocal` / `exec` still propagate a non-zero exit |
| 7 | shim run with `AUBE_NODE_GYP_EXE` unset | fails fast — the shim is the `node-gyp` on PATH, so a bare-name fallback would re-exec forever |
| 8 | six concurrent builds, cold bucket | the tool-dir project lock serializes the race |
| 9 | a real `binding.gyp` addon compiles | the toolchain path a `--version` call never touches (MSVC on Windows), via the synthesized `node-gyp rebuild` |
| 10 | node-gyp already on PATH | the user's copy wins; on Windows also the three-name `.cmd`/`.exe`/bare lookup |
| 11 | `npm_config_node_gyp` | the `.js` shim, a channel separate from PATH |
| 12 | `jailBuilds=true`, build calls node-gyp | jailed jobs get node-gyp resolved *outside* the jail |
| 13 | `jailBuilds=true`, nothing needs node-gyp, no registry | the up-front resolve is best-effort and cannot sink the install |

## What it found

Windows failed **6 of 13** on its first run while macOS and Linux passed 13/13 — two independent bugs:

- The `.cmd` shim's `for /f "usebackq"` capture had **never worked**: `for /f` goes through `cmd /c`, which strips the outer quote pair when a string both starts and ends with a quote, mangling the exe path into `…\nub.exe" boot "…\proj`. Latent until the bootstrap went lazy and made the shim primary. `cmdquote.mjs` isolated the fix in one build-free run.
- Node aborts at startup under the build jail on Windows (`ncrypto::CSPRNG` assertion). Unrelated to node-gyp, pre-existing, and **not** fixed here — scenarios 12 and 13 SKIP with that reason printed rather than failing over a defect they do not test.

## Two things that make it trustworthy

**Scenario 3 is a positive control for scenario 4.** Asserting "no bucket was created" against a path nothing ever writes passes for free. Scenario 3 must first prove a bucket appears at that exact path when one is earned. This matters more than it looks: the cache root resolves through `XDG_CACHE_HOME` before `%LOCALAPPDATA%` on Windows, and getting that backwards would have made every negative assertion vacuous.

**Scenario 5 is falsifiable, and was falsified.** Against the pre-fix `sh` shim — which read `$AUBE_NODE_GYP_PROJECT_DIR` unguarded under `set -eu` — it fails with status 1. Against the fixed shim it passes. A scenario that cannot fail is not evidence.

## Platform notes

- Node cannot spawn a `.cmd` directly since CVE-2024-27980 (bare name gives `ENOENT`, the `.cmd` spelling gives `EINVAL`), so Windows invokes the shim through `cmd.exe /c`.
- Build scripts run a checked-in `marker.cjs` rather than `node -e "..."`. An inline script is re-parsed by `cmd.exe` on Windows and by `sh` elsewhere, which breaks for reasons unrelated to anything under test.
- Every shim invocation carries a timeout. A bad fallback would make the shim re-exec itself forever, and a hang has to register as a failure rather than stall the run.
- `PATH` is filtered to drop any directory providing a `node-gyp`, so the shim is the only way one can resolve. The header line reports whether the scrub held; if it prints `YES (probe is INVALID)` the run proves nothing.
