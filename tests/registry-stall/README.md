# Registry-stall harness

This directory checks what `nub install` does when a registry accepts a request and then stops making progress. Unit tests cannot see this defect class. A stalled stream is not an error, so nothing fails and nothing logs, and the install waits at 0% CPU until a bound expires. The report that started this harness ([#715](https://github.com/nubjs/nub/issues/715)) described "`nub install` hangs forever". The real cause was a very long wait, and only a wall clock can tell those two apart.

## The loop

1. Build a binary: `scripts/rust-build.sh build -p nub-cli --profile fast`.
2. Run the matrix: `tests/registry-stall/run-stall-matrix.sh "$(scripts/rust-build.sh --print-target)/fast/nub"`.

It needs no network and takes about 70 seconds. The default-bound case sets that time. Every case runs in parallel against its own local registry, and the script exits non-zero if any case fails. It is not fail-fast, because a regression shows where it is by the cases that move together.

Each case gets a fresh project, `HOME` and `XDG_*` directories, and an environment with every `npm_config_*`, `NPM_CONFIG_*`, `pnpm_config_*`, `PNPM_*`, `NUB_*` and proxy variable removed. An exported setting or a user `.npmrc` outranks the fixture, and would change the measurement without any warning.

**Pass the binary path explicitly.** The build wrapper writes to a content-hashed directory, so an old binary can sit at the obvious path and still look valid.

## What the assertions read

`stall-registry.mjs` logs each request and each socket close with a timestamp. A case asserts on two numbers from that log:

- **requests**: how many times the client asked for the stalled URL, which is the retry count;
- **span**: the time from the first stalled request to the last socket close, which is the bound.

Neither number includes process startup, so the windows stay tight on a loaded machine. The case also fails if the install exits 0, or if the cap kills it.

## The cases

The package manager bounds each request with `fetch-timeout`, which is the longest time a request may go without receiving data (default 60s). Waiting for the connection, the response head, and each body chunk all count. A failed request is tried again `fetch-retries` times (default 2). Before retry *n* it waits `min(fetch-retry-mintimeout × fetch-retry-factor^n, fetch-retry-maxtimeout)`, which is 10s, then 60s at the defaults. So a stalled metadata request ends after 250s at the defaults.

A Nub project reads these settings from `.npmrc` and `npm_config_*`. A pnpm project reads them from `pnpm-workspace.yaml`, like pnpm does, and ignores them in `.npmrc`.

| Case | Stall | Pins | Break control |
| --- | --- | --- | --- |
| `fetch-timeout` | no response head | `fetch-timeout` from `.npmrc` | `STALL_OMIT=fetch-timeout`: 60s |
| `fetch-timeout-body` | half a body | the same bound on the body-read path | `STALL_OMIT=fetch-timeout`: 60s |
| `fetch-timeout-env` | no response head | `npm_config_fetch_timeout` | `STALL_OMIT=fetch-timeout`: 60s |
| `default-fetch-timeout` | no response head | the 60s default | `STALL_SET=fetch-timeout=20000`: 20s |
| `pnpm-workspace-yaml` | no response head | `fetchTimeout` in a pnpm project's `pnpm-workspace.yaml` | `STALL_OMIT=fetch-timeout`: 60s |
| `fetch-retries` | no response head | the attempt count, and `fetch-retry-mintimeout` | `STALL_OMIT=fetch-retries`: 3 requests; `STALL_OMIT=fetch-retry-mintimeout`: 16s |
| `fetch-retry-factor` | no response head | `fetch-retry-factor` | `STALL_OMIT=fetch-retry-factor`: 20s |
| `fetch-retry-maxtimeout` | no response head | `fetch-retry-maxtimeout` | `STALL_OMIT=fetch-retry-maxtimeout`: 20s |
| `fetch-timeout-tarball` | half a tarball | the same bound on the tarball path | `STALL_OMIT=fetch-timeout`: 120s |

Use `STALL_ONLY=<case>` to run one case with its break control. A case that stays green when you remove the setting it names does not test that setting.

## Expected failures

Two cases are marked `xfail`. Each one describes a stall that is not bounded by the settings above. The matrix passes while the case fails for the reason it states. If the case starts to pass, the matrix reports `XPASS` and exits non-zero, so change that case to `pass` in the same change as the fix.

- **`fetch-retries-tarball`**: the client requests a stalled tarball twice, even with `fetch-retries=0`. The download that starts during resolution uses one full retry budget. When it fails, it clears its cache slot, and the install then fetches the tarball again with a second budget. At the defaults a stalled tarball takes 500s, which is twice the metadata bound. pnpm 12.4.1 does the same.
- **`trickle`**: a response that sends one byte a second is never cut off. `fetch-timeout` restarts on every byte, and no total deadline or minimum speed applies. `fetchMinSpeedKiBps` only prints a warning, and only after a download succeeds. pnpm uses the same design on purpose, so that a large download on a slow link can finish ([pnpm/pnpm#14604](https://github.com/pnpm/pnpm/issues/14604)). The case records that a registry which keeps dribbling data can still hold an install open forever.

To check that the `XPASS` path works, run `STALL_TRICKLE_MS=100000 STALL_ONLY=trickle`. That trickle is slower than `fetch-timeout`, so the bound ends it.

## Stall shapes

`stall-registry.mjs` serves one package, `stall-probe@1.0.0`, with a real packument and a gzipped tarball built in memory. It stalls one kind of request, and the shape sets how:

| Shape | What the registry sends |
| --- | --- |
| `ok` | everything; the fixture's positive control |
| `meta-silent` | nothing after it reads the request |
| `meta-partial-head` | a status line and one header |
| `meta-partial-body` | full headers and half the packument |
| `meta-trickle` | headers, then one packument byte per `--trickle-ms` |
| `tarball-partial-body` | the packument, then half the tarball |
| `tarball-trickle` | the packument, then one tarball byte per `--trickle-ms` |

Every stalled socket stays open. A closed socket makes the client fail at once, so it would prove nothing about the bounds. `meta-partial-head` behaves the same as `meta-silent` and is not in the matrix. It is there for when those two paths need to be told apart.

To watch a shape by hand:

```sh
node tests/registry-stall/stall-registry.mjs --port 4999 --shape meta-partial-body
# then, in a project with registry=http://127.0.0.1:4999/ in .npmrc and "stall-probe": "1.0.0" in dependencies:
nub install
```
