# nub-adapted run of aube's bats e2e suite

aube ships a black-box bats suite (`vendor/aube/test/*.bats`) that spawns the `aube` binary against a committed offline Verdaccio registry. Since nub embeds aube's command layer as its PM engine, that suite is the strongest available e2e validation of nub's PM surface — this harness runs a curated subset of it with **nub standing in for `aube`**.

## How it works

`run.sh <path-to-nub> [suite.bats ...]` builds a scratch mirror of the layout aube's `common_setup.bash` derives from the test directory (`PROJECT_ROOT/test`, `PROJECT_ROOT/fixtures`, `PROJECT_ROOT/target/debug`) out of symlinks into `vendor/aube`, and writes a shim at `PROJECT_ROOT/target/debug/aube` that exec's nub — so the harness's own `PATH` prepend resolves `aube` straight to nub with zero outside `PATH` surgery. The shim also translates the harness's `AUBE_*` knobs (`AUBE_ADVISORY_CHECK`, `AUBE_LOW_DOWNLOAD_THRESHOLD`) to their `npm_config_*` spellings, because nub deliberately deadens the engine's `AUBE_*` env family while the same settings stay reachable through the `npm_config_*` sources registered in `aube-settings/settings.toml`. The registry needs no translation (`common_setup` writes it into the per-test `.npmrc`), and aube's update notifier needs none (nub's dispatch never calls it).

The default curated subset is the install family: `install.bats ci.bats add.bats remove.bats update.bats lockfile_settings.bats lockfile_dir.bats`.

## The two lists

Tests that cannot pass under nub are annotated at staging time (a `skip` line injected into a patched copy — `vendor/aube` is never modified) from two separate files, and the distinction is load-bearing:

- [`skips.txt`](skips.txt) — **permanent, intended divergences** (`skip "nub-divergence: …"`): aube-branded behavior nub deliberately toggles off — the `node_modules/.aube` virtual-store stem (nub: `node_modules/.nub`), `aube-lock.yaml` as the fresh-project lockfile (nub: `pnpm-lock.yaml` via the `defaultLockfileFormat` embedder default), and verb/flag collisions where nub's own surface wins (`run` is nub's script runner; `-v` is `--version`).
- [`known-gaps.txt`](known-gaps.txt) — **real gaps in-flight work must close** (`skip "KNOWN-GAP: …"`): unwired verbs/flags/aliases (`recursive`, `clean-install`, `--fix-lockfile`, `--lockfile-dir`, `--reporter`, `--network-concurrency`), engine info/warn lines that don't surface through nub's presentation yet, and exit-code mapping. **This list must shrink** — when the wiring lands, delete the entries so the tests assert for real. `run.sh` hard-fails if an entry matches no test, so stale entries can't linger silently.

## Running locally

```sh
cargo build -p nub-cli
tests/aube-bats/run.sh target/debug/nub                 # full curated subset
tests/aube-bats/run.sh target/debug/nub install.bats    # one suite
```

Requires Node on `PATH`; the registry harness (`vendor/aube/test/registry/start.bash`) installs Verdaccio via `npm install --global verdaccio@6` if it isn't already present, and serves the committed `storage/` fixtures offline on port 4873. bats itself is vendored at `vendor/aube/test/bats/bin/bats`.

CI: the `aube-bats-nub` job in [`.github/workflows/aube-parity.yml`](../../.github/workflows/aube-parity.yml), one ubuntu shard, gated on the same paths as the parity job plus this harness.
