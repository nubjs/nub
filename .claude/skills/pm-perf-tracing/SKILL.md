---
name: pm-perf-tracing
description: Performance-trace Nub package-manager installs using the embedded engine's phase tracing and the sampling-profiler workflow. Use when an install or package-manager operation is unexpectedly slow and the bottleneck needs to be localized before changing code.
metadata:
  internal: true
---

# pm-perf-tracing

How to performance-trace the nub package manager (resolve, virtual store, **link**, apply) and find where the time actually goes. Reach for this any time a `nub install` / PM operation is "mysteriously slow" — do NOT reverse-engineer from source; the instrumentation already exists, turn it on.

## The tracing that already exists

The engine is pnpm 12's Rust engine, consumed as git dependencies on `nubjs/pnpm` (AGENTS.md, "The pnpm engine fork and pin"). It emits `tracing` events, and nub renders them through its own subscriber (`crates/nub-cli/src/pm_engine/log.rs`) as `LEVEL message field=value …` on stderr. `RUST_LOG` sets the filter; nothing prints without it. The engine's own `TRACE` variable does nothing under nub, because nub's subscriber is installed first and keeps precedence.

1. **Phase timings.** `RUST_LOG=pacquet::install::phase=info nub install` prints one line per engine phase:

   ```text
   INFO phase complete phase=resolve_workspace elapsed_ms=187 importers=1 nodes=1
   INFO phase complete phase=create_virtual_store_partition warm=0 cold=1 skipped=0 total=1 node_linker=Hoisted
   INFO phase complete phase=link.symlink_direct_deps elapsed_ms=1
   INFO phase complete phase=apply_materialization_result elapsed_ms=11
   ```

   A fresh-lockfile install prints, roughly in order: `load_wanted_lockfile`, `load_current_lockfile`, `resolve_level` / `resolve_workspace`, `build_fresh_lockfile`, `virtual_store_layout_new`, `create_virtual_store_partition`, `link_slots`, `create_virtual_store`, `link.*`, `apply.*`, `apply_materialization_result`. That split alone tells you whether the time is resolution or materialization. `create_virtual_store_partition` also counts warm / cold / skipped slots and names the linker that ran.

2. **Package import method.** Add `pacquet::package_import_method=info` to the filter and the engine prints `INFO selected package import method method=<clone|hardlink|copy>` — once for each method the install used, not once per file. It tells you which tier of the `packageImportMethod` ladder (default `auto`) did the linking.

```sh
scripts/rust-build.sh build -p nub-cli --profile fast
NUB="$(scripts/rust-build.sh --print-target)/fast/nub"
RUST_LOG='pacquet::install::phase=info,pacquet::package_import_method=info' "$NUB" install --offline 2> /tmp/phases.log; echo "EXIT=$?"
grep -E 'phase complete|import method' /tmp/phases.log
```

`RUST_LOG=debug` works too, but it floods stderr with every other engine event; filter by target.

## The measurement discipline (load-independent — this host is permanently contended)

The dev box runs load ~30–50 and never goes quiet, so **absolute wall-clock is untrustworthy**. Measure things contention can't ruin:

- **Verified-clean warm loop:** `rm -rf node_modules` and *assert it's gone*, warm store already populated, `--offline` (the final `Progress:` line shows `downloaded 0`), and check **rc=0** on every run (a timing from an errored install — e.g. npm's `rm: Directory not empty` purge failures → rc=254 — is garbage).
- **Counts, not seconds:** the warm / cold / skipped slot counts and the selected import method are facts regardless of load. That's what proves a design gap.
- **Back-to-back A/B on the same box, report the RATIO:** e.g. `--node-linker hoisted` vs the isolated default on the same fixture, same load window → the relative delta is robust even when both absolutes are inflated.
- For a real clean wall-clock number, hand it to a quiet machine / CI runner — never block on this box settling.

## Layout matters — always check which linker path runs

The engine's default linker is isolated (`NodeLinker::Isolated` in the engine's `pnpm-config` crate); a project selects hoisted with `nodeLinker`, or one install with `--node-linker hoisted`. In a Nub project outside CI, nub also turns on the global virtual store by default (`fill_install_defaults` in `crates/nub-cli/src/pm_engine/host_settings.rs`), so a warm isolated install links into the machine-global store and can skip every slot. The `node_linker=` field on `create_virtual_store_partition` says which layout ran. When a perf question is about linking, A/B the linker AND the global virtual store (`npm_config_enable_global_virtual_store=false`) and diff.

## When spans aren't enough — sampling profiler

Span instrumentation can distort a syscall-bound, parallel pass (observer effect). For "where inside the link phase do the syscalls go" use a sampling profiler on a **release** build: `samply record -- <NUB> install --offline` (macOS/Linux), or `cargo flamegraph`. Phase lines tell you *which phase*; the sampler tells you *which syscalls/functions* dominate.

## Fixtures

- A heavy tree: CoffeeScript 2.0.1's dependency set (519 packages, ~76k files in a hoisted layout), kept at `/tmp/coffee2-demo` when present. It carries an npm `package-lock.json`, which no install reads, so run `nub pm migrate` there first.
- A minimal repro: a `package.json` with `webpack@3.6.0` + `underscore` (no `node_modules`).

## The one-liner to remember

`RUST_LOG=pacquet::install::phase=info nub install` for the phase split; add `pacquet::package_import_method=info` for the link tier; A/B `--node-linker hoisted` and the global virtual store against the defaults; judge by counts + ratio, never the contended absolute.
