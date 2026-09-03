---
**Scope:** Whether Nub should pre-bake its `--import` preload via Node's `--build-snapshot` / `--snapshot-blob` to cut cold-start tax and sidestep `--permission` grants for the preload path.
**Status:** v1, 2026-05-18. Empirically tested on Node v24.14.0, macOS arm64. Findings supersede earlier speculation about a snapshot-based preload. That record's decision — don't auto-inject snapshot flags, document as not-warranted for default mode — stands; this research strengthens it with the addon-impossible and dynamic-import-broken findings, and by showing the cold-start math does not justify the architecture cost.
**Builds on:** [[research/snapshot-env-reads]] — `process.env` semantics in snapshotted JS; [[research/cold-start]] — Node cold-start phase breakdown.
---

# Snapshot-based preload — architecture evaluation

An empirical evaluation of pre-baking Nub's preload into a V8 startup snapshot, run against Node v24.14.0 on macOS arm64. The verdict is no, and the addon route is dead outright.

## 1. TL;DR

**Verdict: not viable today. Possibly viable later for a narrow slice (snapshot-only-the-hook-registration, leave all FS work lazy) with material caveats. Not viable at all as a way to sidestep `--allow-addons` under `--permission`.**

The five vectors:

1. **`--snapshot-blob` does load under `--permission` with no fs grant** — verified. The blob is opened with raw `fopen()` in `node.cc:1525` *before* `Environment` exists, so the permission gate isn't installed yet. This is not an "explicit-input auto-grant" carve-out like the entry script gets in `env.cc:967`; snapshot load simply happens in an earlier phase of process lifetime than permissions. **Consequence:** the load path itself is fine, but everything the snapshot can usefully do is gated by post-deserialize permission checks, which run normally.
2. **N-API addons cannot be snapshotted.** Trying to `require()` a real-world addon (sqlite3) during `--build-snapshot` produces a V8 fatal: `CheckGlobalAndEternalHandles failed`. Third-party addons don't register external references via `NODE_BINDING_EXTERNAL_REFERENCE` and the V8 serializer refuses to proceed. **Consequence:** snapshot pre-load cannot bypass `--allow-addons`. That entire line of investigation is dead.
3. **Module-customization hooks DO survive the snapshot.** Hooks registered via `module.registerHooks()` at build time fire for post-deserialize CJS resolves invoked through `Module.createRequire(anchor)`. The hook arrays in `lib/internal/modules/customization_hooks.js` are plain module-scope arrays and serialize correctly.
4. **Dynamic `import()` is broken across snapshot boundary.** Any `import('node:fs')` or user URL from a snapshot-built script fails with `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING`, even though `initializeESM()` is called from `prepareMainThreadExecution(false)` in the snapshot-deserialize main wrapper. Root cause: the script was compiled with a `host_defined_options` symbol at build time that doesn't bind to anything in the post-deserialize realm. **Consequence:** any preload that does `await import(...)` at runtime — which Nub's preload may need to do — is broken under snapshot.
5. **Cold-start delta is negligible-to-negative.** For a noop preload, snapshot is ~2ms *slower* than plain `node -e ""` (the 5.7MB blob read cost exceeds the bootstrap it skips). For a deliberately heavy preload (fs walk + JSON parsing of `node_modules/*/package.json`), snapshot saves ~9ms (38ms → 29ms) — but Nub's design is explicitly to avoid that kind of eager work in the preload (hooks are lazy by construction). For a realistic Nub preload (register hooks, set up resolver, no eager FS work), the savings collapse to ~1-3ms — below the noise floor on macOS dyld jitter and not worth the architecture cost.

The vectors pushing away — addon-impossible, dynamic-import-broken, marginal perf, cache-invalidation complexity, opaque failures — are stronger than they looked before testing, and the ones pushing toward are weaker.

**Recommendation: do not pursue snapshot. Reconsider only if (a) a future `nub compile` command needs it for its own bundling pipeline, or (b) Node ships `--build-snapshot` work that fixes the dynamic-import and `node:module` warnings.**

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

## 5. Cache architecture proposal — if we were to do this

Hypothetical, to make the cost of a design we recommend against concrete.

### Storage

Blobs live at `~/.cache/nub/snapshots/<key>.blob` on Linux/macOS and `%LOCALAPPDATA%\nub\snapshots\<key>.blob` on Windows — the standard XDG / Apple cache layout.

### Invalidation key

The key must include every input that, if changed, would silently produce wrong behavior:

```
key = sha256(
  nub_version           // our own version
  + node_executable_path // user may have multiple nodes
  + node_version_string  // process.versions
  + node_v8_version      // process.versions.v8 — V8 snapshot format compatibility
  + arch                 // arm64 vs x64 vs ...
  + platform             // darwin vs linux vs win32
  + preload_source_hash  // our preload JS hash
)
```

What's missing from this key (and why it's hard to add):

- **`node_modules` contents.** If the preload populates a resolver cache pointing at `node_modules/x/y.js`, and the user updates the package, the cache is wrong. Including `node_modules` in the key is impractical (multi-GB hash). Mitigation: don't pre-populate the resolver from the preload; only register the hook.
- **`tsconfig.json` contents along the walk-up path.** Cached `compilerOptions` would be stale. Mitigation: don't cache it in the preload.
- **`.env` contents.** Read in-body, not at top scope (per [[research/snapshot-env-reads]] pattern).
- **User source files (`.ts`, `.tsx`).** Any cached transpile output is stale on edit. Mitigation: the per-file content-hashed transpile cache already covers this; do NOT put cached transpile output into the snapshot.

The pattern that emerges: **anything snapshot-worth-baking is trivially small (hook registration), and anything substantive is invalidated by user edits we can't predict.** The snapshot therefore pays for itself only on the minimal "register hook, construct empty resolver table" payload — the ~1-3ms savings we measured.

### Generation strategy

Three options:

1. **Ship pre-built blobs in distribution.** One blob per `(node_version × arch × platform)` tuple. With 7 supported Node versions × 3 arches × 3 platforms = ~63 blobs. Each ~6MB. Adds ~380MB to distribution — unacceptable.
2. **Lazy generate at first run.** Spawn `node --build-snapshot --snapshot-blob=... preload-anchor.js` once on first Nub invocation. Adds ~500-1000ms to the first run, $0 after. Need a lock file to handle concurrent first-invocations. Workable but adds a "first run is slow" footgun.
3. **Generate at install time.** `npm install -g @nubjs/nub` postinstall hook runs the snapshot build. Adds ~1s to install but warm from first run. Risks: postinstall scripts are often disabled (`npm install --ignore-scripts`); Node version may change after install (nvm switch) and invalidate the snapshot; permission model on the install target may not allow writing the cache dir.


### Cache busting

Invalidate (delete-and-regenerate) when the key changes. Since the key includes Nub version and Node version, normal Nub upgrades and nvm switches handle it for free. Disk consumption: at most a few blobs per user's lifetime, ~6MB each. Acceptable.

### Permission-model interaction

The cache dir is in `$HOME` (or `%LOCALAPPDATA%`). Under `--permission`, reading from `~/.cache/nub/snapshots/<key>.blob` does NOT require a grant (per §3, snapshot blob load is pre-permission).

Writing the cache at generation time does require `--allow-fs-write=$HOME/.cache/nub`. The existing "disable transpile-cache writes under `--permission`" decision would apply here too: skip snapshot generation when `--permission` is detected, take the cold-start tax.

## 6. The addon question — definitive answer

**Q: Can we use snapshot pre-load to carry an N-API addon's state past the `--allow-addons` requirement under `--permission`?**

**A: No. The snapshotter refuses to serialize addon state.** Loading any third-party N-API addon during `--build-snapshot` triggers a V8 fatal (`CheckGlobalAndEternalHandles failed`) — see §2.2 for the stack trace. The cause is structural: V8's serializer requires every external reference (C++ function pointers, eternal handles) to be registered in the snapshot's external-reference table, which Node does for its built-in bindings via `NODE_BINDING_EXTERNAL_REFERENCE` (declared in `src/node_external_reference.h`) and third-party addons cannot do without forking Node.

This rules out the "build snapshot at install time when there's no permission gate, load the snapshot under `--permission --no-allow-addons` and skip the dlopen" trick.

The path forward for any addon that Nub ships (the resolver crate exposed as N-API for in-process use, websocket vendoring, etc.) is to **document that `--permission` requires `--allow-addons=<addon-path>`**.

Getting an addon snapshotted by patching V8 / Node is theoretically possible, but the patch surface is large: the snapshot would need to record external-reference patches per-addon, addon authors would need a new API to register their externals with the snapshot system, and the snapshot format itself would become addon-version-dependent. Multi-year upstream work, not in scope for Nub.

## 7. Performance estimate

Method: `hyperfine --warmup 5 --runs 50` on macOS 25.5.0 / arm64 / Node v24.14.0.

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

The realistic Nub preload sits between "noop" and "heavy" — closer to noop because we don't eagerly walk filesystems or pre-parse JSON in the preload. Savings expected: ~1-3ms.


Bun's startup advantage over Node (~22ms) comes from JSC vs V8 macOS dyld characteristics, static linking, and skipping `pre_execution.js`. None of those are capturable by a snapshot mechanism on top of the user's installed Node. See [[research/cold-start]] for the full breakdown.

## 8. Security considerations

The snapshot blob loads before the permission gate exists (§3), so an attacker who can write the cache dir gets pre-bootstrap code execution. Version, dependency and source drift each have a mitigation; the opacity of snapshot failures does not.

### Blob tampering

The snapshot blob is loaded with raw `fopen()` before the permission system exists (§3). An attacker who can write to `~/.cache/nub/snapshots/<key>.blob` can replace it with a malicious blob that runs arbitrary code at deserialize time.

The deserialized code runs with whatever permissions the user grants the process, but the *bootstrap path* is entirely unsanitized.

Mitigations:

- Verify a sha256 of the blob against an expected value at load time. This has to happen before `--snapshot-blob` is passed to Node — a Rust-side verification in the Nub CLI wrapper — and adds a disk read of the blob for hashing, defeating most of the perf win.
- Trust filesystem permissions: `~/.cache/nub/snapshots/` is 0700, blob files 0600. An attacker with write access already controls the user's account, so this is the standard cache trust model. The impact of a cache write is arbitrary code in the user's Node processes, the same end state as tampering with the transpile cache.
- Skip snapshot entirely under `--permission`, matching the transpile-cache write-disable decision.

The un-snapshotted preload, by contrast, ships with Nub and is read from Nub's install directory (under `/opt/homebrew/...` or `~/.local/share/...`), which is harder for an attacker to write to than a user cache dir.

### Version drift

The snapshot is V8-version-tied. Node version mismatch produces either a hard fail (`Cannot use snapshot, V8 version mismatch`) or in pathological cases subtle bytecode misbehavior.

Mitigation: include `process.versions.v8` in the cache key (§5). On nvm-switch, the old cache becomes invalid and is regenerated — costing ~500ms-1s on the next Nub invocation after the switch. Not catastrophic but a footgun.

### Dependency drift

The preload doesn't depend on the user's `node_modules` directly (it only registers hooks). But if we ever cached resolver state in the snapshot, dep changes would silently produce wrong results.

Mitigation: don't cache resolver state in the snapshot. Resolution runs at hook-fire time, against the current FS.

### Source drift

When a user edits a `.ts` file, the transpile cache, separately keyed by content hash, handles it correctly.

Putting transpile output into the snapshot would force a choice between (a) stale output served post-edit, or (b) invalidating the whole snapshot per `.ts` edit, which is unworkable — it would invalidate on every keystroke. Don't put transpile output in the snapshot.

### New attack surfaces

Surfaces the non-snapshot path does not have:

1. **Pre-permission-gate code execution** via blob tampering.
2. **Cache-poisoning across permission-mode invocations** — even if the user runs Nub under `--permission`, the snapshot blob was built with full FS access at generation time. An attacker who controlled the generation environment can ship a backdoored blob.
3. **Opacity of failures.** When something goes wrong with a snapshot, errors look like `ERR_INTERNAL_ASSERTION` from `node:internal/v8/startup_snapshot:112:5` — uselessly internal. Compare the un-snapshotted preload, where errors point at user-visible Nub JS files with clear stack traces.

## 9. Recommendation

**Do not pursue snapshot. Reconsider only if specific conditions are met.**

### Today: no snapshot

The posture stays as it is today: pass the flags through, generate nothing, ship no blobs.

- The "Does our preload still run on snapshot-load?" question is settled: hook registrations persist; dynamic `import()` is broken; `createRequire` usage must be re-constructed post-deserialize; addons are impossible.
- Continue the existing posture: pass `--build-snapshot` / `--snapshot-blob` through to Node unchanged; recommend `--node` / `NODE_COMPAT=1` for users who actually need snapshots.
- Don't auto-generate snapshots from the Nub CLI. Don't ship blob files. Don't add a `~/.cache/nub/snapshots/` directory.

### Conditions under which to reconsider

Three triggers: a `nub compile` pipeline that needs a snapshot internally, an upstream fix for the `node:module` warning, or cold-start pressure that no lazier preload design can relieve.

1. **`nub compile` needs a snapshot internally** for a SEA-wrapped output. SEA can embed a snapshot blob, and the compile-time environment is controlled. Internal snapshot use as part of `nub compile`'s output is a different question — evaluate when that command is designed.
2. **Node fixes the snapshot warning for `node:module`.** The warning text — *"It's not yet fully verified whether built-in module 'node:module' works in user snapshot builder scripts. It may still work in some cases, but in other cases certain run-time states may be out-of-sync after snapshot deserialization."* — indicates that the Node team is aware this is broken and hasn't fixed it yet. When the warning is removed and the dynamic-import and createRequire issues are resolved upstream, revisit.
3. **A specific cold-start budget pressure forces it.** If `nub <file.ts>` cold-start becomes the bottleneck (it won't — the bottleneck is V8 isolate construction, not the preload), and we've exhausted lazier preload designs, snapshot could shave 1-3ms. Not before.

## 10. Open questions

Five questions left open, none of them blocking the recommendation. Each becomes relevant only if the snapshot direction is reconsidered.

- **Worker threads + snapshot.** All testing here was main-thread. Workers re-bootstrap via the embedded snapshot with a per-thread Realm. Whether a user-snapshot-built script can register hooks that fire for worker-thread imports is untested, and becomes relevant only if we reconsider snapshot.
- **SEA + snapshot interaction for `nub compile`.** SEA can embed a snapshot blob via the `useSnapshot: true` config. Whether the combination simplifies or complicates the Nub preload story is a question for when `nub compile` is designed.
- **PR/issue history for dynamic-import-callback-missing under snapshot.** Worth a sweep of nodejs/node issues for `ERR_VM_DYNAMIC_IMPORT_CALLBACK_MISSING` + `snapshot` keywords to confirm this is a known-unfixed limitation rather than a misuse on our part. Not blocking the recommendation, but useful context if we re-investigate.
- **Snapshot + `--inspect-brk`.** Untested. If snapshot mangles the source map / script-id mapping, the debugger could attach to a process that has no recognizable user scripts. Relevant to any future debugger integration.
- **Whether Node 25/26 fixes any of this.** The `node:module` warning suggests upstream work is in progress. A quick check of the Node 25 nightlies (if/when available) for snapshot-related PRs would be worth doing before re-evaluating in 6+ months.

## Changelog

Revision history for this document.

- 2026-07-30 — Initial publication.
- 2026-08-28 — Trimmed to the measured findings and current behavior.
