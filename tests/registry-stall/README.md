# Registry-stall harness — how the fetch timeout bounds are iterated on

This directory is the working system for nub's behavior when a registry accepts a connection and then goes quiet. It exists because the defect class here is invisible to unit tests: a stalled stream is not an error, so nothing fails, nothing logs, and the resolver simply blocks at 0% CPU until some bound expires. The bug that motivated it ([#715](https://github.com/nubjs/nub/issues/715)) presented as "`nub install` hangs forever" and was really a 970-second wait — a distinction only wall-clock can make.

`wiki/` carries no decision record for this; the mechanism is documented in `crates/aube-registry/src/client/retry_policy.rs` and the `fetchStallTimeout` entry in `settings.toml`. This README documents the *loop*.

## The loop

1. Build a binary: `scripts/rust-build.sh build -p nub-cli --profile fast`.
2. Run the matrix: `tests/registry-stall/run-stall-matrix.sh "$(scripts/rust-build.sh --print-target)/fast/nub"`.

It takes about three minutes, needs no network, and exits non-zero on the first case outside its window.

**Pass the binary path explicitly.** The build wrapper writes to a content-hashed bucket that moves whenever a depended-on crate (`vendor/aube`, `nub-core`) changes, so a stale artifact can sit at the obvious path looking valid. That is not hypothetical — it happened while this harness was being written, and a stale binary reports a confusing out-of-window failure rather than an error. The script warns when the binary has no `fetchStallTimeout` symbol.

## Why elapsed time is the assertion

Every case pins a *different* bound, so the number is the test. A bound that silently stops being read does not throw — it falls back to another bound and the wait changes. That is exactly how two defects shipped in the original fix: the setting declared a CLI flag that did not exist, and its only env alias was one the nub profile disables, so the knob had no env route at all. Both were invisible to `cargo test` and to code review, and both show up here as a case landing in the wrong window.

| Case | Pins | Fails if |
| --- | --- | --- |
| `default-60s` | `fetchStallTimeout` default (60s) fires on a silent connection | the idle bound is gone; falls back to the 300s `fetchTimeout` |
| `npmrc-5s` | the `.npmrc` key reaches the client | settings plumbing breaks between `settings.toml` and `FetchPolicy` |
| `env-20s` | `npm_config_fetch_stall_timeout` reaches the client | the env route is dropped, or only an `AUBE_*` alias is declared (inert under nub, whose `env_prefix` is `None`) |
| `disabled-falls-back` | `0` disables the idle bound and `fetchTimeout` alone applies | `0` is treated as a real value, or the disable path stops falling back |
| `warn-disabled` | `fetchWarnTimeoutMs=0` silences the in-flight line without moving the bound | the "still waiting" ticker ignores the documented disable |

The last one guards a real regression: the first implementation of the ticker turned `fetchWarnTimeoutMs=0` into *one warning every 10 seconds*, the exact inverse of what the setting documents.

## The two stall shapes

`stall-registry.mjs` serves both, because they exercise different bounds and only one of them is reachable with a mock HTTP server.

- `--mode blackhole` accepts the connection and never writes a byte. No response head ever arrives, so only a bound on the head wait ends it. This is what the matrix uses.
- `--mode proxy --stall-at N` forwards everything to the real registry except the Nth response, which gets its real headers and ~4 KB of real body and then stops forever. This is the shape [#715](https://github.com/nubjs/nub/issues/715)'s reporter hit, and it is how you reproduce the original symptom against a real dependency graph:

  ```sh
  node tests/registry-stall/stall-registry.mjs --port 4998 --mode proxy --stall-at 800
  # then, in a workspace with registry=http://127.0.0.1:4998/ in .npmrc:
  nub install --lockfile-only
  ```

  Watch the resolver go to 0% CPU and the progress bar freeze, which is what the reporter saw. It surfaces elsewhere as `error decoding response body`, and confirming those are the same underlying condition is what this mode is for.

## What this cannot test

`reqwest`'s read timeout resets on every delivered frame, so a large-but-progressing response is never cut off. Neither mode here proves that: the blackhole delivers nothing, and the proxy stalls permanently rather than dribbling. A genuine test of the per-frame reset needs a server that spaces body frames on a timer, which neither `wiremock` (used by the Rust unit tests) nor this proxy does. The positive control for it is a real cold resolve of a workspace pulling a multi-megabyte packument — `@remotion/google-fonts` is ~11.7 MB and is the one to watch.
