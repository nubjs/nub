# Local build-jail falsification arms

Runs build-jail grant arms on a developer macOS host instead of the corpus CI runner. `arm.sh` ports the arm from `harness/v2/measure-macos.sh` in `nubjs/build-jail-corpus` and calls that harness's `dep-scaffold.mjs`, `artifact-gate.mjs` and `arm-falsifiability.mjs` directly, so it cannot drift from CI's definition of a passing arm.

```sh
NUB=/path/to/nub \
CORPUS_HARNESS=/path/to/build-jail-corpus/harness/v2 \
tests/local-falsification/arm.sh electron-chromedriver 43.2.0 \
  jailoff           '{}' \
  wide              '{"write":{"userHome":true},"network":true}' \
  no-network        '{"write":{"userHome":true}}' \
  no-write-userHome '{"network":true}' \
  zero              '{}'
```

`NUB` must be built with `--features nub-cli/build-jail-catalog-override`; `arm.sh` aborts if it is not, because without the feature every arm silently measures the shipped catalog. Build it with `scripts/rust-build.sh build -p nub-cli --profile fast --features nub-cli/build-jail-catalog-override`.

Run the `jailoff` label first. It installs with `buildJail:false`, which separates "nub cannot install this package at all" from "the jail blocked it" — without it an all-red ladder cannot be attributed.

## What it measures, and what it does not

It tests grants stated on the command line. It does not synthesize a grant from observed syscalls, and it does not name which capability a red arm needed; both are dtrace phases that need uid 0. On macOS the kernel denies silently, so a red arm here reports only that the grant was insufficient.

Each arm runs under its own throwaway `HOME`, which gives it a fresh CAS store, side-effects memo, jail private home and tool-cache leaves. That removes the replay paths the CI driver evicts against, at the cost of a cold download per arm. It also moves the `userHome` scope, because `sandbox_homes` reads `$HOME` — so a package that resolves its home outside `$HOME`, or reads a tool installed under the real home, is refused at every rung. That skews toward keeping a grant, never toward dropping one.

Two results are not narrowing evidence and the output labels both: an `ARMS-UNFALSIFIABLE` note from `arm-falsifiability.mjs`, and any arm reporting `OVERRIDDEN=0` or a non-zero `REJECTED` count, which means nub rejected the override and fell back to the shipped catalog.
