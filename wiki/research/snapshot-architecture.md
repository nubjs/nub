---
**Scope:** Whether Nub should pre-bake its `--import` preload via Node's `--build-snapshot` / `--snapshot-blob` to cut cold-start tax and sidestep `--permission` grants for the preload path.
**Status:** v2, 2026-09-05. Tested on Node v24.14.0/macOS arm64, Node v26.7.0/Linux x64, and Node main at `6f41e415639b5ec3dd816e44945cc73b4d7651e3`. The preload decision still stands; the expanded evidence explains why Node's built-in snapshot helps while application snapshots often do not.
**Builds on:** [[research/snapshot-env-reads]] — `process.env` semantics in snapshotted JS; [[research/cold-start]] — Node cold-start phase breakdown.
---

# Node.js snapshot startup economics

An empirical evaluation of Node.js startup snapshots for preloads and single executables. Snapshots are effective when they replace expensive computation with little serialized state, but they cannot remove Node's native startup floor.

## 1. TL;DR

**Verdict: not useful as Nub's default startup path. Node's built-in snapshot captures the generic win; an application snapshot pays the same native startup floor plus workload-specific deserialization.**

It becomes useful only when it replaces substantially more computation than state. It cannot sidestep `--allow-addons` under `--permission`.

Six findings:

1. **`--snapshot-blob` does load under `--permission` with no fs grant** — verified. The blob is opened with raw `fopen()` in `node.cc:1525` *before* `Environment` exists, so the permission gate isn't installed yet. This is not an "explicit-input auto-grant" carve-out like the entry script gets in `env.cc:967`; snapshot load simply happens in an earlier phase of process lifetime than permissions. **Consequence:** the load path itself is fine, but everything the snapshot can usefully do is gated by post-deserialize permission checks, which run normally.
2. **N-API addons cannot be snapshotted.** Trying to `require()` a real-world addon (sqlite3) during `--build-snapshot` produces a V8 fatal: `CheckGlobalAndEternalHandles failed`. Third-party addons don't register external references via `NODE_BINDING_EXTERNAL_REFERENCE` and the V8 serializer refuses to proceed. **Consequence:** snapshot pre-load cannot bypass `--allow-addons`.
3. **Module-customization hooks DO survive the snapshot.** Hooks registered via `module.registerHooks()` at build time fire for post-deserialize CJS resolves invoked through `Module.createRequire(anchor)`. The hook arrays in `lib/internal/modules/customization_hooks.js` are plain module-scope arrays and serialize correctly.
4. **Dynamic `import()` is broken across snapshot boundary.** Any `import('node:fs')` or user URL from a snapshot-built script fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, even though `initializeESM()` is called from `prepareMainThreadExecution(false)` in the snapshot-deserialize main wrapper. Root cause: the script was compiled with a `host_defined_options` symbol at build time that doesn't bind to anything in the post-deserialize realm. **Consequence:** any preload that does `await import(...)` at runtime — which Nub's preload may need to do — is broken under snapshot.
5. **The payoff follows a compute-to-state ratio, not executable packaging.** On Node v26.7.0/Linux, a built-in snapshot reduced empty-process startup from 77.60ms to 29.17ms. The same runtime's SEA user snapshot made an empty program 3.75ms slower and a 100,000-object program 7.19ms slower, but saved 23.56ms when 20 million loop iterations collapsed to one serialized integer.
6. **SEA snapshot loading contains a separate avoidable copy.** Node copies the V8 startup-data region out of the executable's process-lifetime resource before deserializing it. A source prototype that borrowed those mapped bytes reduced median startup by 1.64–1.72ms for 6.75MB snapshots and 4.74ms for a 12.9MB object snapshot.

Together, the addon and module limitations constrain what can be captured, while the measurements constrain when capturing the remaining state can pay for itself.

**Decision: do not use an application snapshot for Nub's preload or enable SEA `useSnapshot` by default. A snapshot is an opt-in workload optimization only when measurement shows that build-time computation dominates the added deserialization and artifact cost.**

## 2. Verified behaviors

All tests run on `node v24.14.0`, macOS 25.5.0, arm64.

### 2.1 Snapshot loads under `--permission` with no grants

```
$ node --permission --snapshot-blob=/tmp/nub-snap-test/perm.blob
[main] permission enabled = true
[main] has fs.read /etc/hosts = false
[main] has fs.read /tmp/nub-snap-test/app.blob = false
[main] has fs.read * = false
[main] has child = false
[main] has addon = false
[main] read /etc/hosts blocked: ERR_ACCESS_DENIED
[main] read userland.ts blocked: ERR_ACCESS_DENIED
```

The blob is read without any `--allow-fs-read=app.blob`. Post-deserialize, `process.permission.has('fs.read', ...)` returns false for everything, and subsequent `fs.readFileSync` calls raise `ERR_ACCESS_DENIED` as expected.

### 2.2 N-API addon in snapshot: V8 fatal

Requiring a real-world N-API addon during `--build-snapshot` kills the process. Reproduced with sqlite3.

```
$ node --build-snapshot --snapshot-blob=cr.blob test-cr.js
# (snapshot script: req('./node_modules/sqlite3/lib/sqlite3.js'))
# Fatal error in , line 0
# CheckGlobalAndEternalHandles failed
 1: node::NodePlatform::GetStackTracePrinter()::$_0::__invoke()
 2: V8_Fatal(char const*, ...)
 3: v8::internal::CreateSnapshotDataBlobInternal(...)
 4: node::SnapshotBuilder::CreateSnapshot(...)
 5: node::BuildSnapshotWithoutCodeCache(...)
 6: node::SnapshotBuilder::Generate(...)
 7: node::GenerateAndWriteSnapshotData(...)
 8: node::Start(int, char**)
```

The V8 serializer refuses to proceed because the addon registered external pointers / eternal handles that aren't in the snapshot's external-reference table. This isn't a soft warning — it's a process-killing fatal during the V8 `CreateSnapshotDataBlobInternal` call. The check at `src/node_snapshotable.cc:981-1025` (`ValidateBindings`) only allows bindings whose `nm_modname` is in `EXTERNAL_REFERENCE_BINDING_LIST` (built-in bindings) or the hardcoded allowlist `bindings_without_external_references`. Third-party N-API addons fail this check.

### 2.3 Hook registration survives snapshot, fires for post-deserialize requires

```js
// build script:
Module.registerHooks({ load(url, ctx, next) { hookHits++; ... } });

v8.startupSnapshot.setDeserializeMainFunction(() => {
  const req = Module.createRequire('/tmp/nub-snap-test/');
  const mod = req('./userland-cjs.js');  // hook fires here
  console.log(mod.greet('snap'), hookHits);  // "hello, snap" 1
});
```

```
$ node --snapshot-blob=hook5.blob
[main] hits at boot = 0
[hook] RESOLVE spec= ./userland-cjs.js
[hook] LOAD url= file:///private/tmp/nub-snap-test/userland-cjs.js
[main] mod.greet = hello, snap
[main] hits after require = 1
```

The hook's closures are captured in the snapshot. `Module.createRequire` constructs a fresh user-mode Module instance and the resolver consults the snapshotted hook array (`customization_hooks.js`'s `resolveHooks`/`loadHooks` module-scope arrays).

### 2.4 Naked `require()` of user file from `setDeserializeMainFunction`: fails

A bare `require()` of an absolute user path inside the deserialize-main callback raises `MODULE_NOT_FOUND`, even though the path itself resolves.

```js
v8.startupSnapshot.setDeserializeMainFunction(() => {
  const mod = require('/tmp/nub-snap-test/userland-cjs.js');  // fails
});
```

```
[main] require failed: Cannot find module '/tmp/nub-snap-test/userland-cjs.js'.  MODULE_NOT_FOUND
```

But `Module._findPath('/tmp/nub-snap-test/userland-cjs.js', [''], false)` on its own returns the correct path. The failure is that `require` inside the deserialize-main callback has no proper parent module context — `module` is undefined / the bootstrap module. `createRequire` is the workaround — awkward but workable for a Nub preload structured entirely through an explicit `createRequire(anchor)`.

### 2.5 Dynamic `import()` from snapshot-built script: broken

Any `await import(...)` inside the deserialize-main callback fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, including a bare `node:fs` with no user URL involved.

```js
v8.startupSnapshot.setDeserializeMainFunction(async () => {
  const m = await import('node:fs');  // fails
});
```

```
[main] dynamic import node:fs FAILED: A dynamic import callback was not specified.
```

The failure is `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, raised because the V8-side `HostImportModuleDynamically` callback chain has no entry for the host-defined options symbol embedded in the snapshot-compiled script. `initializeESM()` IS called from `prepareMainThreadExecution(false)` inside the snapshot deserialize main wrapper at `lib/internal/v8/startup_snapshot.js:106-118` — it runs `setImportModuleDynamicallyCallback(importModuleDynamicallyCallback)` at `lib/internal/modules/esm/utils.js:309`. But the script being executed was compiled during the snapshot-build process with a host_defined_options bound to the *build-time* realm, not the deserialize-time realm, so the script's host options never reach the installed callback. This is the failure mode the warning `Warning: It's not yet fully verified whether built-in module "node:module" works in user snapshot builder scripts` is hinting at.

This is the deal-breaker for any snapshot-based preload that needs to `await import('node:fs')` (which Nub's preload would, for lazy loading of the transpiler).

### 2.6 Captured `createRequire` closure: also broken on second use

Snapshotted `createRequire` closures are single-use: the one built during the snapshot build cannot be called again after deserialization.

```
[main] val = {"hello":"world"} err = null   # captured at build time
# But invoking the captured req() at runtime:
ERR_INTERNAL_ASSERTION
    at deserializeMain (node:internal/v8/startup_snapshot:112:5)
```

A `createRequire` closure built and used at snapshot build works within the build script. The captured value persists. But calling the captured closure again from `setDeserializeMainFunction` triggers `ERR_INTERNAL_ASSERTION` — the internal Module reference inside the closure is stale post-deserialize. Workaround: re-construct `createRequire(anchor)` inside the deserialize main, don't capture it across the boundary.

### 2.7 `process.env` and `globalThis` state: preserved correctly

```
[main] STATE.builtAt = 1779151912557   # set at build time
[main] STATE.hookActive = true
[main] tsconfig target = ES2022
[main] build-time env keys = 62
[main] live process.env keys = 62
```

Plain JS state on `globalThis` round-trips losslessly. `process.env` is a Proxy backed by `RealEnvStore` (see [[research/snapshot-env-reads]]) so it reflects the boot-time env, not the build-time env — as expected.

## 3. The permission interaction — definitive answer

**Q: Does `--snapshot-blob=PATH` get an explicit-input auto-grant under `--permission`, like the entry script does?**

**A: No, and the question is built on the wrong model.** The snapshot blob isn't gated by the permission system at all because it loads in an earlier phase of process startup than the permission gate is installed.

The relevant sequence:

1. `node::Start()` → `StartInternal()` (src/node.cc:1559)
2. `InitializeOncePerProcessInternal()` — parses argv, no Environment yet
3. **`LoadSnapshotData(&snapshot_data)`** (src/node.cc:1618) — opens the blob file with raw `fopen(filename.c_str(), "rb")` at line 1525, reads contents, deserializes. **The permission system does not exist at this point.**
4. `NodeMainInstance main_instance(snapshot_data, ...)` — creates V8 isolate using snapshot
5. `main_instance.Run()` — eventually creates `Environment`
6. `Environment::InitializeDiagnostics()` invokes `EnablePermissions()` (src/env.cc:921) and applies the deny lists for scopes the user didn't allow. This is when fs / addon / child-process / etc. capability checks become live.

The auto-grant for the entry script in `env.cc:953-968` is a separate mechanism. It populates `options_->allow_fs_read.push_back(first_argv)` specifically for `argv[1]` (and `--require` modules in `preload_cjs_modules`). Snapshot blob loading is not in that path.

The snapshot blob's privilege at load time is whatever the operating-system FS permits the node process to read — the same authority Node has to read its own embedded snapshot. The permission system gates *user-mode JS behavior after bootstrap*, not pre-bootstrap C++ file reads. The same principle applies to `--env-file=`, which bypasses `--permission` for the env file itself but enforces the permission gate on everything afterward.

This means:

- **Snapshot is not a privilege-escalation surface for `--permission`** — whatever the snapshot blob does at deserialize time is subject to the post-bootstrap permission gate. It cannot read `/etc/hosts` without `--allow-fs-read`, cannot dlopen an addon without `--allow-addons`, etc.
- **The "blob path doesn't need an fs-read grant" trick** is real but small. It saves the user from typing one path in their `--allow-fs-read` list. That's not load-bearing for the architecture.
- **Pre-baking the preload state into the snapshot does NOT let the preload do things post-deserialize that it couldn't do otherwise.** Specifically: the snapshot cannot pre-load an addon to skip `--allow-addons`, because the snapshotter refuses to serialize addon state (§2.2).

## 4. What persists through snapshot

Twelve categories of build-time state and whether each survives deserialization. The two hard breaks — captured `createRequire` closures and dynamic `import()` — are what constrain the preload's shape.

| Thing | Persists? | Caveat |
|---|---|---|
| Plain JS module-scope state, `globalThis.*` | yes | trivial round-trip |
| Hook registrations via `module.registerHooks()` | yes | hook arrays in `customization_hooks.js` round-trip; hook fires for post-deserialize `createRequire(...)` requires |
| `Module.createRequire(anchor)` closures | no (broken) | calling the captured `req()` post-deserialize raises `ERR_INTERNAL_ASSERTION`; re-construct inside the deserialize main |
| Dynamic `import(...)` callbacks for snapshotted scripts | no (broken) | `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`; the V8 host-defined options don't survive |
| V8 code cache for hot paths | partially | snapshot writes a `BuildCodeCacheFromSnapshot` blob (`node_snapshotable.cc:1160`) for code that ran during build; user code loaded post-deserialize doesn't benefit |
| `process.env.X` values | not as captured | `process.env` is a live Proxy → `uv_os_getenv`; top-level reads at build time freeze build-time values into module scope; in-body reads see boot-time values. See [[research/snapshot-env-reads]] |
| Parsed JSON, regex compiles, prepared data structures | yes | useful for "expensive constant computations done once" pattern |
| Cached transpile output (URL → string mapping) | yes, but stale | the cache survives but reflects build-time file contents; stale on user edit |
| Resolver tables (path → realpath, package.json → ExportsTree) | yes, but stale | same staleness problem |
| FFI / N-API addon `dlopen()` handle | not at all | snapshotter fatal — see §2.2 |
| JS-side state populated by an addon at build time | n/a | can't even build the snapshot if an addon was loaded |
| `pre_execution.js` side effects (V8 isolate callbacks, etc.) | runs again post-deserialize | `prepareMainThreadExecution(false)` is called from the snapshot-deserialize main wrapper |

The pattern: **plain JS data round-trips. Anything that depends on V8 isolate state or external (dlopen) state doesn't.** This is consistent with how V8 snapshots work everywhere (Lambda SnapStart, Chrome process snapshots) — they're a JS-heap-and-bytecode mechanism, not a "freeze the entire OS process" mechanism.

## 5. Cache economics

Application snapshots bind serialized state to the exact Node.js/V8 build and to every application input captured before serialization. The invalidation boundary is wider than the startup work worth caching for Nub's lazy preload.

A valid artifact key must cover the Node.js and V8 versions, executable, platform, architecture, Nub version, and preload source. Any snapshotted resolver table, parsed `tsconfig.json`, dependency metadata, or transpile output also makes the corresponding project files part of the key.

That creates the central mismatch for Nub's preload:

- Hook registration and empty resolver construction are stable but cheap, so snapshot loading has little work to replace.
- Filesystem walks, dependency metadata, and transpile output can be expensive but become stale when the project changes.
- `process.env` and command-line-dependent state must be refreshed after deserialization rather than treated as build-time constants. Node.js documents this model in [the environment and CLI handling discussion](https://github.com/nodejs/node/issues/55603).
- Snapshot and code-cache artifacts are platform-specific. Node.js requires both features to be disabled for cross-platform SEA generation in [the SEA documentation](https://github.com/nodejs/node/blob/main/doc/api/single-executable-applications.md#single-executable-application-configuration).

The cache therefore cannot turn Nub's deliberately lazy preload into a large startup win without also capturing volatile project state.

## 6. The addon question — definitive answer

**Q: Can snapshot pre-load carry an N-API addon's state past the `--allow-addons` requirement under `--permission`?**

**A: No. The snapshotter refuses to serialize addon state.** Loading any third-party N-API addon during `--build-snapshot` triggers a V8 fatal (`CheckGlobalAndEternalHandles failed`) — see §2.2 for the stack trace. The cause is structural: V8's serializer requires every external reference (C++ function pointers, eternal handles) to be registered in the snapshot's external-reference table, which Node does for its built-in bindings via `NODE_BINDING_EXTERNAL_REFERENCE` (declared in `src/node_external_reference.h`) and third-party addons cannot do without forking Node.

This rules out the "build snapshot at install time when there's no permission gate, load the snapshot under `--permission --no-allow-addons` and skip the dlopen" trick.

Supporting addon state would require V8 external-reference patching, an addon API for registering those references, and snapshot compatibility keyed to addon versions. That is a different mechanism from the current user snapshot and does not solve post-deserialize permission checks.

## 7. Startup economics

Snapshots replace repeatable JavaScript work with blob deserialization. They do not remove executable loading, native-library initialization, V8 isolate creation, post-deserialize refresh work, or process teardown.

The original preload measurements used `hyperfine --warmup 5 --runs 50` on macOS 25.5.0/arm64 with Node v24.14.0.

### Baseline

Plain Node startup with no preload and no snapshot, as the reference for every measurement below.

```
node -e ""                              27.3 ± 2.0 ms
```

### Snapshot of empty/noop preload (5.7MB blob)

```
node --snapshot-blob=noop.blob          38.5 ± 34.6 ms  (high variance)
```

The 5.7MB blob read + V8 deserialize costs more than the bootstrap it skips, making snapshot *slower* than baseline for an empty preload.

### Realistic-light preload (hook registration only)

```
node -e ""                              26.5 ± 0.9 ms
node --import preload-noop.js -e ""     29.3 ± 4.1 ms  (~3ms preload tax)
node --snapshot-blob=noop.blob          28.3 ± 3.8 ms  (~2ms snapshot tax)
```

Net delta: snapshot saves ~1ms. Below noise floor.

### Heavy preload (fs walk + JSON parse, ~8ms of eager work)

```
node -e ""                              27.7 ± 7.1 ms
node --import heavy-preload.js -e ""    38.8 ± 1.1 ms  (~11ms preload tax)
node --snapshot-blob=heavy.blob         29.5 ± 3.2 ms  (~2ms snapshot tax)
```

Net delta: snapshot saves ~9ms. But this is a degenerate case — the entire point of Nub's preload design is to **not** do eager fs work; all real work is lazy inside the hook on first relevant import.

### Implication

The realistic Nub preload sits between "noop" and "heavy" — closer to noop because it does not eagerly walk filesystems or pre-parse JSON. Savings expected: ~1-3ms.

### Built-in snapshot control

Node's own embedded snapshot is materially effective because it replaces the runtime's large, stable bootstrap rather than a small application prelude.

On an idle Linux x64 VM with the official Node v26.7.0 binary, 160 randomized interleaved cycles after 20 warmups produced:

| Command | Median |
|---|---:|
| `node empty.cjs` | 29.172ms |
| `node --no-node-snapshot empty.cjs` | 77.600ms |

The built-in snapshot saved 48.428ms, or 62.4%. Node's bootstrap source describes that snapshot as the V8 heap initialized by `lib/internal/bootstrap/`, which is deserialized instead of running the bootstrap scripts on the main thread ([source](https://github.com/nodejs/node/blob/6f41e415639b5ec3dd816e44945cc73b4d7651e3/lib/internal/bootstrap/node.js#L25-L35)). This is the generic snapshot win; an application snapshot starts after the same native process startup and replaces only application-specific work.

Node has continued moving stable initialization into its built-in snapshot. Including the ESM loader improved the measured ESM process case by 6.06% ([landed change](https://github.com/nodejs/node/pull/61769)), and starting Workers from the built-in main context reduced the empty Worker bootstrap from 21.1ms to 10.6ms ([landed change](https://github.com/nodejs/node/pull/65336)). Those wins do not imply that a second, application-level snapshot is free.

### SEA state-size sweep

Application snapshots carry a fixed V8 startup-data payload before application state is added. For Node v26.7.0, an empty SEA snapshot added 6,754,304 bytes to the executable.

The same Linux host ran 160 randomized interleaved cycles for functionally identical SEA programs that constructed arrays of small objects:

| Objects | Plain SEA | Snapshot SEA | Snapshot delta | Snapshot blob |
|---:|---:|---:|---:|---:|
| 0 | 27.255ms | 31.010ms | +3.755ms | 6,755,762 bytes |
| 1,000 | 27.499ms | 31.278ms | +3.778ms | 6,807,098 bytes |
| 10,000 | 34.684ms | 34.405ms | -0.279ms | 7,361,026 bytes |
| 100,000 | 56.637ms | 63.825ms | +7.188ms | 12,948,026 bytes |

Serialization does not turn this object graph into ready-to-map heap pages. V8 consumes a serialization stream and reconstructs the isolate state, so the larger graph trades object construction for larger binary reads and more deserialization. The 10,000-object case reached parity; the 100,000-object snapshot lost despite moving all construction to build time.

`useCodeCache: true` was within 0.24ms of the plain SEA at every object count. The small script leaves parsing and compilation below the native startup floor, and a cache does not remove that floor.

### Compute-to-state sweep

The inverse control performed increasing build-time computation but retained only one integer in the snapshot. The snapshot stayed near 6.75MB at every count.

| Loop iterations | Plain SEA | Snapshot SEA | Snapshot delta |
|---:|---:|---:|---:|
| 100,000 | 29.155ms | 32.061ms | +2.906ms |
| 1,000,000 | 30.779ms | 31.713ms | +0.934ms |
| 5,000,000 | 36.368ms | 31.666ms | -4.703ms |
| 20,000,000 | 55.041ms | 31.477ms | -23.565ms |

This is the crossover Node snapshots are designed for: expensive deterministic initialization whose output is small. Packaging the program as an executable is not the relevant property. The relevant quantity is runtime work avoided per byte of serialized state.

### SEA snapshot copy

The SEA path adds one avoidable cost that external `--snapshot-blob` loading does not make avoidable in the same way.

At Node main commit `6f41e415639b5ec3dd816e44945cc73b4d7651e3`, the SEA loader receives a `std::string_view` into a process-lifetime executable resource ([`src/node.cc`](https://github.com/nodejs/node/blob/6f41e415639b5ec3dd816e44945cc73b4d7651e3/src/node.cc#L1512-L1532), [`src/node_sea.cc`](https://github.com/nodejs/node/blob/6f41e415639b5ec3dd816e44945cc73b4d7651e3/src/node_sea.cc#L249-L260)). `SnapshotDeserializer::Read<v8::StartupData>()` then allocates another buffer and copies the entire V8 startup-data region into it ([`src/node_snapshotable.cc`](https://github.com/nodejs/node/blob/6f41e415639b5ec3dd816e44945cc73b4d7651e3/src/node_snapshotable.cc#L179-L195)).

A source prototype kept copying file-backed snapshot blobs but let SEA borrow the process-lifetime resource bytes. A clean Node build and 240 randomized interleaved cycles after 30 warmups on an idle Linux x64 VM produced:

| Workload | Baseline | Borrowed bytes | Delta |
|---|---:|---:|---:|
| Empty snapshot, 6.75MB | 28.335ms | 26.610ms | -1.725ms |
| 20-million-iteration result, 6.75MB | 28.131ms | 26.491ms | -1.640ms |
| 100,000 objects, 12.9MB | 56.285ms | 51.541ms | -4.745ms |

The executable bytes already remain mapped for the process lifetime in `FindSingleExecutableBlob()` ([source](https://github.com/nodejs/node/blob/6f41e415639b5ec3dd816e44945cc73b4d7651e3/src/node_sea_bin.cc#L45-L66)), so borrowing them preserves the lifetime V8 needs. This removes duplicate allocation and copying, but it does not remove V8 deserialization or the native startup floor. It is a bounded improvement to SEA snapshots, not a general snapshot breakthrough.


The remaining process and isolate floor is outside the application snapshot. See [[research/cold-start]] for its measured phase breakdown.

## 8. Security considerations

A user snapshot is executable state loaded before Node's permission gate exists. Integrity, compatibility, and input freshness must therefore be properties of the artifact pipeline rather than assumptions made after deserialization.

### Blob integrity

An attacker who can replace a snapshot blob can run captured code at deserialization time.

Restrictive filesystem permissions provide the ordinary cache trust boundary. An independent digest provides stronger integrity but requires reading the blob again and erodes the startup benefit.

An embedded SEA snapshot inherits the executable's distribution and signing boundary instead of adding a separate writable file. That improves artifact integrity but does not change the authority of code after deserialization.

### Version and input drift

Snapshots are tied to the producing Node.js/V8 version and platform. A version mismatch normally fails at load time, while stale application state can remain structurally valid and produce wrong answers.

Any source file, dependency manifest, environment-derived constant, or resolver result captured before serialization becomes an artifact input. Content-addressing or post-deserialize refresh is required for each one.

### Failure opacity

Snapshot failures often surface inside `node:internal/v8/startup_snapshot` rather than at the application source that produced the stale or unserializable state. This makes a snapshot path harder to diagnose than the equivalent ordinary preload.

## 9. Current decision

The results separate Nub's product decision from improvements that belong in Node.js core. Application snapshots are workload-dependent; the SEA loader's duplicate copy is not.

### Nub application snapshots

Nub should not snapshot its preload or enable SEA `useSnapshot` by default.

- The preload deliberately defers filesystem and transpiler work, leaving little expensive deterministic initialization for a snapshot to replace.
- The snapshot cannot carry N-API addon state and breaks important module behaviors across the serialization boundary.
- An empty application snapshot adds about 6.75MB and a few milliseconds before it captures any useful state.
- Single-executable packaging does not change the economics. The state-size and compute-to-state sweeps show that snapshot eligibility must be established by a workload measurement, not by output format.

Nub continues to pass Node.js snapshot flags through unchanged. Node's SEA interface exposes `useSnapshot`, but current `nub compile` output does not enable it, and executable output alone provides no basis for enabling it.

### Node.js core

The SEA startup-data copy is the strongest snapshot-specific Node.js contribution found here. It is localized and reduced median startup by 1.64–4.74ms in a clean source-build comparison.

The prototype preserves existing file-backed snapshot ownership and borrows only the SEA resource whose lifetime is already process-wide.

That change cannot eliminate the fixed native floor. Work that removes process-wide initialization helps every Node.js invocation and composes with snapshots: current examples include symbol-visibility work on macOS ([#65526](https://github.com/nodejs/node/pull/65526)), deferred `atexit` and crypto initialization ([#65549](https://github.com/nodejs/node/pull/65549)), and seeding V8 directly from the OS CSPRNG ([#65796](https://github.com/nodejs/node/pull/65796)). A V8 hash-seed optimization was closed in Node.js because it belongs upstream in V8 first ([#65795](https://github.com/nodejs/node/pull/65795)).

Moving more stable Node.js bootstrap into the built-in snapshot also has direct precedent in the ESM-loader and Worker results above. Expanding user snapshots to more built-ins, ESM module graphs, dynamic import, or addons would increase the workloads that can be represented, but it would not change the fixed-floor or deserialization math. Node.js tracks the packaging limitations explicitly in [the startup snapshot integration discussion](https://github.com/nodejs/node/issues/42566) and [the current snapshot documentation](https://github.com/nodejs/node/blob/main/doc/api/cli.md#--build-snapshot).

A process checkpoint, zygote, or resident daemon would attack a different layer by avoiding OS loading and isolate creation. It is not a refinement of Node's V8 startup snapshot and would introduce lifecycle, security, native-resource, and cross-platform constraints far beyond the SEA mechanism.

## Changelog

Revision history for this document.

- 2026-07-30 — Initial publication.
- 2026-08-28 — Trimmed to the measured findings and current behavior.
- 2026-09-05 — Added controlled SEA state/compute sweeps, the built-in-snapshot control, and a source-built measurement of the SEA startup-data copy.
