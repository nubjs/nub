# Benchmark harnesses

Each benchmark family lives in its own subdirectory with its own README, fixtures, scripts, and saved results.

| Benchmark | Directory | Primary script |
|-----------|-----------|----------------|
| Package installs | [`install/`](install/) | `bash tests/bench/install/run-warm-gvs.sh` |
| Script-runner dispatch | [`script-runner/`](script-runner/) | `bash tests/bench/script-runner/run-vs-node.sh` |
| Bin-runner dispatch | [`bin-runner/`](bin-runner/) | `bash tests/bench/bin-runner/run-pure.sh` |
| Runtime augmentations | [`runtime/`](runtime/) | `nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/threadpool.sh --detach` |
| Windows dispatch (`nub.cmd` vs `nub.exe`) | [`windows-dispatch/`](windows-dispatch/) | `gh workflow run bench-windows-dispatch.yml` (Windows only; runs under Git Bash on `windows-latest`) |

Run benchmarks on a quiet machine. Install and dispatch timings are sensitive to filesystem load, CPU contention, Spotlight indexing, and concurrent builds.

Saved canonical JSON lives under each benchmark's local `results/` directory. By default, scripts write to a temp directory; pass `--save` only when updating checked-in results.
