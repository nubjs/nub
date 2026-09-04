# node-cf-probe

Re-tests [nodejs/node#44715](https://github.com/nodejs/node/issues/44715) on a modern macOS, and prices the fix.

Node's macOS binary links CoreFoundation and Security unconditionally. Between them they account for about 404 of the ~657 dyld initializers a `node -e 0` runs before `main` — measured on macOS 26.6.2 arm64, a trivial C binary runs 2, the same binary with `-framework CoreFoundation` runs 404. Nothing on the startup path uses either framework: the imports are 14 Security symbols reached only from `--use-system-ca` (`src/crypto/crypto_context.cc`) and, through CoreFoundation, two from cctz's `local_time_zone()`.

`patches/macos-cf.patch` marks both frameworks delay-init (`-Wl,-delay_framework,…` in `node.gypi` and `tools/v8_gypfiles/abseil.gyp`) and removes the four `CFSTR()` dictionary keys that would otherwise pin CoreFoundation to the launch path through a `__DATA_CONST,__cfstring` bind.

The 2022 issue was closed on one observation: Instruments' Heap Allocations recorder produced no stacks for a binary built without the framework, on macOS 12.6. Two jobs settle whether that still holds:

| job | what it does | cost |
| --- | --- | --- |
| `instruments` | builds three control binaries (CoreFoundation eager / delay-init / not linked) and records each under `xctrace --template Allocations`, reporting rows and backtrace frames per trace | a few minutes |
| `node-build` | builds Node at the pinned SHA, measures it, applies the patch, rebuilds incrementally, measures again, and records both binaries under the same Allocations template | ~90 min |

`instruments-probe.sh` needs no Node build — the question is about the loader state at launch, which a 30-line C++ program reproduces exactly. `node-instruments.sh` runs the same recording against a real `node`.

## Running it

Push to `probe/node-startup`; artifacts are `instruments-<os>` (the control comparison), `cf-results` (measurements, trace exports) and `cf-bins` (both binaries).
