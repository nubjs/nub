# Compiled code-cache checks

The probe compiles an application, hides its source tree, and runs the executable from another directory. It compares application output with plain Node and checks that Node accepts the packaged bytecode on the first launch.

```sh
node probe.mjs --nub /path/to/built/nub --out /tmp/compile-cache-check \
  --target "$(node -p process.versions.node)"
```

Run from this directory with Node 24 or later and a compiler built with the `compile` feature. The output directory must be new. CI supplies its already-built launcher through the internal template override; without one, the compiler obtains the matching launcher normally.

The fixture covers:

- Package-scoped ESM `.js`, an embedded asset, cyclic imports, top-level await, dynamic imports, and a worker.
- No application evaluation during compilation.
- Cold and warm execution, disabled and portable caches, different V8 flags, an unavailable cache directory, and a user preload.
- Published-app reuse without repairing unrelated files, missing/invalid-marker repair, concurrent extraction, and read-only published directories.
- Concurrent code-cache publication, an unchanged `--smol` artifact, and the small-program path.

For a control predating packaged bytecode, pass `--expect-cache false`. For a launcher that still scans the full extracted tree on warm runs, also pass `--expect-warm-reuse false`. The application output must match in every case.

The lower-level tests exercise cache relocation, source validation, damaged packs, reproducible generation, and read-only caches with writable directories on Node 26.8 or later:

```sh
node --test ../../crates/nub-cli/src/compile/code_cache.test.cjs
```

## Warm startup measurements

The Unix measurement script interleaves executable launches in a fixed random order. Each arm gets its own Node cache and two untimed warmups. Nonzero exits and inconsistent output fail the run; JSON retains every sample, CPU time, artifact hash, and host load.

```sh
python3 measure.py --arm before=/path/to/before --arm after=/path/to/after \
  --out startup.json --rounds 21
```

For an OpenCode reproduction, build [the port at `ae208df84f`](https://github.com/nubjs/nopencode/tree/ae208df84f25d3530c8c00ae98c25f313de885e7) twice with `packages/cli/script/build-nub.mjs`. Set `NUB_BIN` to each compiler, `OUT` to distinct artifact paths, and `SKIP_WEB_UI=1` for both. The script pins Node 26.6.0. Use the same installed dependency tree and launcher template for both builds, then run the executables from outside the source tree.

These are warm short-command measurements, not cold extraction or interactive readiness. Bytecode adds build work and storage; measure those separately. Shared-host latency can vary substantially, so retain the sample spread rather than reporting a ratio alone.

The [recorded OpenCode run](startup-results.json) used an Apple M1 Max, macOS 26.6.2, and Node 26.6.0. Across 21 interleaved launches, `--version` changed from 188.5 ms median (184.4–194.9 ms) to 157.8 ms (155.2–167.0 ms). The executable grew from 48.4 MB to 65.6 MB. Both builds used the same source, dependency tree, and release launcher; the raw results bind their compiler commits and artifact hashes.
