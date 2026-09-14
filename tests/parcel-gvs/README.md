# Parcel under the global virtual store — regression harness

This directory guards `parcel build` under the global virtual store (GVS). Parcel's main thread and its worker threads must load the same `@parcel/core` instance: the package keeps a module-scoped serializer registry, and with two copies installed the worker farm fails at startup with `DataCloneError`.

## The failure it guards

A lockfile stays portable because resolution keeps every common platform's optional native dependencies, while linking keeps only the host's. Parcel's tree is full of them (`@parcel/watcher-<platform>`, `@swc/core-<platform>`, `lmdb`, `msgpackr-extract`). If one install step names a package's shared-store directory from the portable graph and another from the host graph, the same package lands in two directories, and `parcel` resolves one `@parcel/core` while `@parcel/workers` resolves the other. Nub's previous package-manager engine split this way until both steps hashed the host-filtered graph. The harness checks the outcome, so it holds for any engine.

## Running the harness

```sh
tests/parcel-gvs/run.sh target/fast/nub                      # default version matrix
tests/parcel-gvs/run.sh target/fast/nub 2.9.3 2.16.4         # specific versions
```

For each Parcel version, `run.sh` builds a minimal worker-farm fixture (`make-fixture.sh`), installs it into a fresh, isolated store with the GVS forced on, and asserts one copy of `@parcel/core` plus a `parcel build` that exits 0.

- **Copies are counted wherever a package can live:** the shared store (`$XDG_CACHE_HOME/nub/store/v*/links/@parcel/core/<version>/<hash>`) and the project's own `node_modules/.store`, where nub places a package it keeps out of the shared store. Parcel 2.9.3's `@parcel/core` lands in the shared store and 2.16.4's in the project store.
- **Store isolation is load-bearing.** Each version gets its own `XDG_CACHE_HOME` and `XDG_DATA_HOME`: a machine-global store accumulates directories across installs and masks a split.
- **`CI` is unset for the install**, because nub turns the GVS off in CI.
- **The fixture decides Parcel's install scripts `false` in `allowScripts`.** Those packages ship prebuilt platform binaries and their scripts are fallbacks, so the install passes the approve-builds gate without compiling anything.

Verified on Node 24 against 2.9.3 and 2.16.4.

## Why an end-to-end harness

The failure only reproduces against a real Parcel tree materialized into a real shared store: the split is in on-disk directory naming and in Parcel's own module-singleton assumption, and no unit test stands in for either.
