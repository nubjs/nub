---
**Scope:** Audit every Node 22.15+ / 24 / 25 / 26 CLI flag (and the relevant `NODE_*` env vars) against Nub's augmentation surfaces — `module.registerHooks()` load/resolve hooks, the `--import` preload, eager `.env*` loading, auto-flag-injection, the hijack-by-default PATH shim, and the `--node` / `NODE_COMPAT=1` compat escape hatch. Produce per-flag verdicts and a short hijack × auto-flag × `NODE_OPTIONS` test matrix.

**Status:** v1, 2026-05-18. A point-in-time survey; where a finding is later superseded it is corrected in the changelog rather than rewritten in place.

**Related docs:**
- [`experimental-flags-unflagging.md`](experimental-flags-unflagging.md) — sibling research covering which `--experimental-*` flags to inject. This doc is the inverse: for every pre-existing flag, does Nub break it?
- [`env-file-loading.md`](env-file-loading.md), [`env-expansion-and-test-skip.md`](env-expansion-and-test-skip.md) — env-loading research.
---

# Node CLI flag interactions with Nub's augmentation surfaces

## 1. TL;DR — the high-risk interactions, ranked

The premise: Nub installs a sync `module.registerHooks()` load+resolve hook via `--import`, prepends V8/Node flags (`--no-warnings`, `--enable-source-maps`, version-conditional `--experimental-*`), eager-loads `.env*` before spawning, prepends a PATH shim so child `node` invocations re-enter Nub, and offers `--node` / `NODE_COMPAT=1` as a global off switch.

Ranked by likelihood × severity:

1. **`--permission` (Node 24+, formerly `--experimental-permission`). Catastrophic interaction. Default-deny.** The `--import` preload reads from `<nub-binary>/lib/`, the transpile cache writes under `~/.cache/nub/`, and the PATH shim creates a temp dir under `/tmp` — none of which have permission grants. If a user passes `--permission` to Nub today, the preload's first `fs.readFileSync` (or addon `dlopen`) fails with `ERR_ACCESS_DENIED` before user code runs. **Action:** either (a) compute a permission-grant supplementation set (`--allow-fs-read=<preload-dir>`, `--allow-fs-read=<cache-dir>`, `--allow-fs-write=<cache-dir>`, `--allow-addons` if the addon loads) or (b) reject `--permission` at the argv layer with a "use `--node` if you need `--permission`" error. Silent breakage is not an option. See [§4.1](#41-permission).

2. **`--watch` × the load hook.** Node's watcher in 22.x+ is loader-instrumented rather than glob-based, and uses the `WATCH_REPORT_DEPENDENCIES` IPC mechanism Nub already plans to piggyback on. Unverified: that when the load hook returns `{ source, format, shortCircuit: true }` for a `.ts` URL, Node's watcher registers the **`.ts` source path** rather than the in-memory transpiled blob or the cached `.js` file. `WATCH_REPORT_DEPENDENCIES` is the canonical channel for hooks to push dep info, so the preload must emit `process.send({'watch:import': [tsUrl]})` explicitly. Without that, `node --watch script.ts` watches only files Node sees natively — possibly **zero** if the entry itself is a `.ts` file the hook fully owns. See [§4.2](#42-watch).

3. **`--env-file` × eager `.env*` loading.** Two systems load env files with different precedence. Node's `--env-file=.env` loads at process boot, after preloads; Nub's loader runs **before spawning Node** and prepends to the child env. If both fire, precedence depends on the relative order of shell env (Nub's precedence base) and file (Node's `--env-file`) — and Node's `--env-file` does not do `${VAR}` expansion the way Nub's does, so a user with `--env-file=.env` in `NODE_OPTIONS` plus Nub's own `.env` load can get different values. **Action:** detect user-passed `--env-file` and `--env-file-if-exists` in argv / `NODE_OPTIONS`; if present, **skip Nub's eager loader** for that file path, so Node's behavior wins for the explicitly-named file. Documented behavior, not silent suppression. See [§4.3](#43-env-file).

4. **`--frozen-intrinsics`**. Freezes `Object.prototype`, `Array.prototype`, and friends. **Nub's oxc-transpiled output may emit helper code that mutates `Array.prototype` or `Object.prototype`** — rare, but the oxc helper-injection pipeline for class fields and async generators has historically used prototype methods that get patched. More importantly, the JS-land machinery the preload depends on, including any vendored third-party deps, almost certainly does not survive frozen intrinsics. **Working assumption:** Nub is incompatible with `--frozen-intrinsics` in default mode; either reject at the argv layer or document loudly. Vite, Vitest, and most of the JS toolchain are also incompatible. See [§4.4](#44-frozen-intrinsics).

5. **`--preserve-symlinks` / `--preserve-symlinks-main` × tsconfig-paths and extensionless probing.** The resolve hook does `fs.statSync` to check candidate extensions (`.ts`/`.tsx`/`.mts`/`.cts`) and does not call `fs.realpath`. Under `--preserve-symlinks` the hook must honor it — not following symlinks when resolving the final URL — **and** propagate it to `nextResolve`. The `NODE_PRESERVE_SYMLINKS=1` env var is the same case and must reach the hook. The risk is subtle: Nub could silently dereference a symlink the user explicitly told Node not to. See [§4.5](#45-preserve-symlinks).

6. **`--conditions` × the resolve hook.** When the user passes `--conditions=dev`, Node propagates that to ESM exports resolution. The resolve hook receives `context.conditions` and must call `nextResolve(specifier, context)` passing the same conditions through. Dropping them — for instance by reconstructing the context object without copying — silently ignores the user's `--conditions` for any specifier the hook touches. **Pure mechanical**, easy to get right and easy to get subtly wrong; test coverage is essential. See [§4.6](#46-conditions).

7. **`--require` (CJS preload) × Nub's `--import` (ESM preload).** The user passes `--require ./instrument.cjs`; Nub also injects `--import ./nub-preload.mjs`. Order matters because `register()` / `registerHooks()` only intercept loads that happen **after** registration. If the user's `--require` does anything dynamic-import-y at boot (`@opentelemetry/instrumentation`, `dd-trace`, `newrelic`), those imports run **before** the hook is installed and get no TS transpile or path aliasing. For most APM agents this is fine — they load `.js` from `node_modules`, which Nub skips anyway. For an APM that triggers eager-loading of user code it is a latent ordering bug. Document, don't fix. See [§4.7](#47-require-vs-import).

8. **`--inspect-brk` × source maps.** The inspector consumes source maps and presents original sources. Nub's transpile output embeds inline base64 source maps with `sourcesContent`, so Chrome DevTools and `nub inspect` should both surface `.ts` source correctly. The exception: **breakpoints set via the inspector before the hook has loaded a file** address the transpiled JS line numbers, not the `.ts` line numbers. The workaround is `--inspect-wait`, so the user sets breakpoints after sources are known. See [§4.8](#48-inspect).

Lower priority but worth documenting: `--no-warnings` opt-out edge cases, `--abort-on-uncaught-exception` interaction with hook errors, `--cpu-prof`/`--heap-prof` output-dir collision with Nub's temp dir, and `--build-snapshot` (incompatible — an `--import`-loaded preload cannot be in a startup snapshot).

The single highest-priority pre-v0.1 action: **`--permission` auto-grant or explicit rejection.** Everything else is a documentation or test-matrix story.

## 2. Methodology

### Categories

Every flag was assigned one of six interaction categories:

- **A. No interaction.** Flag is orthogonal — memory, TLS ciphers, network resolution order, profiling output. Nub's hooks and resolver don't touch it. Default verdict: passthrough, no action.
- **B. Plays nicely.** Flag works correctly through the hooks, or Nub's injection composes with it. Example: `--enable-source-maps` is **injected by default** and Nub's transpiler emits source maps, so they compose by construction.
- **C. Conflict / overlap.** Flag does something Nub also does: `--env-file`, `--watch`, `--require` vs Nub's `--import`, `--no-warnings` (injected, but the user may want warnings). Resolution model is per-flag.
- **D. Plausibly broken with the hooks.** Flag relies on vanilla loader behavior the hook short-circuits: the `--watch` watched-set, `--trace-require-module`, deprecation warnings the user wants to see for `.ts` files the hook owns.
- **E. Subtly broken.** Flag interacts with the resolver in non-obvious ways. The biggest examples: `--preserve-symlinks`, `--conditions`, and `--permission` denying the preload's reads.
- **F. Mooted by our defaults.** Flag is one we already inject or supersede. Node's `--experimental-detect-module` is default-on in the supported range; `--experimental-strip-types` is mooted by our hook (see [`node-strip-types-interaction.md`](node-strip-types-interaction.md)). Note and move on.

### Sources

- `https://nodejs.org/api/cli.html`, accessed 2026-05-18 via WebFetch. Full inventory pulled in one shot.
- `node --help` for cross-reference (Node 22.15, 24.0, 25 current per local installs).
- Node release notes for behavior changes in the supported range (Node 22.15.0 floor).
- Spot-check of `lib/internal/main/watch_mode.js`, `lib/internal/process/pre_execution.js`, `src/permission/*` via the Node GitHub where behavior wasn't documented.
- Sibling research: [`experimental-flags-unflagging.md`](experimental-flags-unflagging.md) for any flag Nub already plans to inject.

### Scope boundaries

Covered: every documented `--flag` and `NODE_*` env var in Node's CLI doc. Not covered in depth:

- V8 raw flags (`--harmony-*`, `--turboshaft`, `--max-old-space-size`, GC pacing). Their interaction with Nub's hooks is uniformly "none" — they affect V8 internals, not the JS module loader. Logged in the table for completeness.
- Windows-specific path semantics. Should be re-audited on Windows CI; flagged where relevant.
- Worker-thread-specific flag inheritance. Workers can override some flags via `Worker` constructor `execArgv`; whether the hooks register inside Worker threads deserves its own write-up.

## 3. Flag-by-flag table

One row per flag or coherent group; category letters match [§2](#2-methodology). Non-A entries with a deep-dive link them to [§4](#4-deep-dives).

### CLI flags

| Flag | Cat | Verdict |
|------|-----|---------|
| `-` (stdin script) | A | Read stdin, treat as eval. Our `--import` preload still fires; hooks active. Stdin script can't be a `.ts` file (no URL extension); use `--input-type=module-typescript` for that. |
| `--` (end of opts) | A | Argument boundary. No interaction. |
| `--abort-on-uncaught-exception` | E | If our hook throws (NAPI transpile error, oxc panic), this dumps core instead of letting the user see a friendly error. Document; the trade is per-user. |
| `--allow-addons` | C | Only meaningful under `--permission`. See `--permission`. |
| `--allow-child-process` | C | Same. We spawn no child processes from inside the preload itself; we do from the Rust CLI. Under `--permission`, the user's spawn calls work or not per their grant. |
| `--allow-fs-read` | C | Same. **Our preload needs at least `--allow-fs-read=<preload-dir>`** to read its own module body. |
| `--allow-fs-write` | C | Same. **Our transpile cache needs `--allow-fs-write=<cache-dir>`** when cache-to-disk is enabled. |
| `--allow-ffi` | A | We don't use FFI in v0. |
| `--allow-inspector` | C | Same as the others. |
| `--allow-net` | A | We don't open sockets from the preload. |
| `--allow-wasi` | A | We don't use WASI. |
| `--allow-worker` | C | If user spawns workers, the workers must also load our preload (worker `execArgv` inheritance question — see open Qs). Under `--permission`, worker creation requires this grant. |
| `--build-sea=config` | D | SEA generation. Incompatible with our preload model — SEA bundles a single JS entry into a binary; our preload is an external file. Reject at argv layer or fall back to compat mode. |
| `--build-snapshot` | D | Snapshot generation. Snapshots run *before* preloads (preloads aren't snapshottable in current Node). If the user is building a snapshot, our hooks aren't in it, and the resulting binary doesn't have TS support baked in. Document as "use compat mode for snapshot builds." |
| `--build-snapshot-config` | D | Same as `--build-snapshot`. |
| `-c, --check` | B | Syntax check only; doesn't execute. Our hook still transpiles before Node parses; check happens on the transpiled JS, which means TS syntax errors in the user's source surface as JS errors against transpiled output. Source maps mostly redirect line numbers; verify in [§4.8](#48-inspect)-adjacent test. |
| `--completion-bash` | A | Prints completion script. No runtime. |
| `-C, --conditions=` | E | Must be propagated through our resolve hook to `nextResolve`. See [§4.6](#46-conditions). |
| `--cpu-prof` | A | Profiler. Output dir is `./isolate-*.cpuprofile` by default; doesn't collide with our temp dir. |
| `--cpu-prof-dir` | A | Same. |
| `--cpu-prof-interval` | A | Same. |
| `--cpu-prof-name` | A | Same. |
| `--diagnostic-dir` | A | Output dir for diagnostics. No interaction. |
| `--disable-proto=delete\|throw` | E | Removes `__proto__` access. If our transpiled output or our preload uses `__proto__`, this breaks us. Quick audit: oxc output rarely uses `__proto__`; our preload should be `__proto__`-free by policy. Test in CI. |
| `--disable-sigusr1` | A | Disables debugger-on-SIGUSR1. We don't use SIGUSR1. |
| `--disable-warning=` | C | Same family as `--no-warnings`. Already-suppressed warnings are double-suppressed harmlessly. |
| `--disable-wasm-trap-handler` | A | V8 internal. No interaction. |
| `--disallow-code-generation-from-strings` | D | Blocks `eval`/`new Function`. **oxc output may emit `new Function`** in rare edge cases (regex compilation? — verify). Our preload shouldn't use eval. If breakage found, document; possibly reject at argv layer with "Nub's preload requires code generation; use `--node` for `--disallow-code-generation-from-strings`." |
| `--dns-result-order` | A | DNS resolution. No interaction. |
| `--enable-fips` | A | TLS crypto mode. No interaction. |
| `--enable-source-maps` | B/F | **Injected by default.** Nub's hook emits inline base64 source maps, so they compose. A user-passed `--no-enable-source-maps` is honored under the auto-flag-injection opt-out rule, and Nub skips the injection. |
| `--entry-url` | C | Treats entry as URL. Our hook works on URLs; this should be a no-op for behavior. Verify the entry URL is the `.ts` source URL, not a transpiled-blob URL. |
| `--env-file=` | C | See [§4.3](#43-env-file). |
| `--env-file-if-exists=` | C | Same. |
| `-e, --eval` | C | Eval'd string. Doesn't pass through our load hook (it has no URL). If the eval contains TS, it fails — use `--input-type=module-typescript` (Node 22.7+) or just use a file. |
| `--experimental-addon-modules` | F | Out of scope for v0 default-inject per [experimental-flags-unflagging](experimental-flags-unflagging.md). User can pass; we don't fight it. |
| `--experimental-config-file` / `--experimental-default-config-file` | E | Loads `node.config.json` which can include `nodeOptions`. **Subtle:** if user has a stale `node.config.json` that conflicts with our injected flags, the merge result is per-Node-version. Document. |
| `--experimental-eventsource` | F | Plan to inject on versions where flagged. See sibling research. |
| `--experimental-ffi` | A | Node 26+, niche, not injected. No interaction. |
| `--experimental-import-meta-resolve` | F | Plan to inject. See sibling research. |
| `--experimental-inspector-network-resource` | A | Inspector internal. |
| `--experimental-loader=` | D | **Deprecated** in favor of `register()`/`registerHooks()`. If user passes their own loader, Node chains it with ours. Both run for each load; non-determinism risk. We don't inject this. Document chaining behavior. |
| `--experimental-network-inspection` | A | DevTools network feature. |
| `--experimental-print-required-tla` | C | Diagnostic warning flag. Conflicts with our `--no-warnings`. If user passes both, our `--no-warnings` swallows the print. Document; user can pass `--no-no-warnings` (Node doesn't recognize that — user really needs `--node` for full warning visibility). |
| `--experimental-quic` | A | Node 25+, niche. |
| `--experimental-sea-config` | D | Same as `--build-sea`. |
| `--experimental-shadow-realm` | A | Adds `ShadowRealm` global. Doesn't touch our paths. |
| `--experimental-storage-inspection` | A | DevTools internal. |
| `--experimental-stream-iter` | A | New module. |
| `--experimental-test-coverage` | A | node:test runner feature. No interaction with our hooks (test runner uses Node's loader path; our hook intercepts). |
| `--experimental-test-module-mocks` | E | node:test runner's module mocking. **Interacts with our hooks** — both want to intercept loads. Module mocking is registered via `node:test` API and likely uses `register()`. Order: whichever hook is registered later wins for a given URL. Untested; needs prototyping. |
| `--experimental-vm-modules` | F | Plan to inject. |
| `--experimental-wasi-unstable-preview1` | A | WASI. |
| `--experimental-worker-inspection` | A | DevTools. |
| `--expose-gc` | A | Adds `globalThis.gc`. Doesn't touch us. |
| `--force-context-aware` | A | Native-addon constraint. We ship N-API addons; they're context-aware by default. |
| `--force-fips` | A | TLS. |
| `--force-node-api-uncaught-exceptions-policy` | A | Addon-callback exception policy. Our N-API addon respects this. |
| `--frozen-intrinsics` | E | See [§4.4](#44-frozen-intrinsics). Plausibly broken. |
| `--heap-prof` / `--heap-prof-dir` / `--heap-prof-interval` / `--heap-prof-name` | A | Heap profiler. |
| `--heapsnapshot-near-heap-limit` | A | Heap snapshot trigger. |
| `--heapsnapshot-signal` | A | Same. |
| `-h, --help` | A | Help. |
| `--icu-data-dir` | A | ICU data location. |
| `--import=` | C | **Nub injects its own `--import` for the preload.** A user-passed `--import` chains and both fire, in argv order. Nub's preload is prepended and so comes first, leaving its hooks installed before the user's preload and user code run. **This is the desired order.** |
| `--input-type=` | C | Affects how `--eval`/stdin is interpreted. `module-typescript` and `commonjs-typescript` invoke Node's built-in strip-types on the eval'd string. Our hook only intercepts URL loads, not eval. **The user's `--input-type=module-typescript` flag is honored by Node-native strip-types, not by us.** OK behavior; document. |
| `--insecure-http-parser` | A | HTTP parser leniency. |
| `--inspect` / `--inspect-brk` / `--inspect-port` / `--inspect-publish-uid` / `--inspect-wait` | E | See [§4.8](#48-inspect). |
| `-i, --interactive` | A | REPL flag. REPL doesn't load `.ts` files through URLs; hooks fire only on imports. |
| `--jitless` | A | V8 internal. |
| `--localstorage-file` | A | Per-process localStorage file (only meaningful with `--experimental-webstorage`). |
| `--max-http-header-size` | A | HTTP. |
| `--max-old-space-size-percentage` | A | V8 heap. |
| `--napi-modules` | A | No-op compatibility flag. |
| `--network-family-autoselection-attempt-timeout` | A | DNS. |
| `--no-addons` | E | Disables loading of native addons. **Breaks our N-API addon if we ship one for transpile or resolver acceleration.** If user passes `--no-addons`, our preload's `require('./nub-addon.node')` fails. Fallback: JS-only path in preload (slower transpile). Or reject at argv layer with "use `--node`." |
| `--no-async-context-frame` | A | AsyncLocalStorage implementation switch. |
| `--no-deprecation` | C | Same family as `--no-warnings`. Mostly composes. |
| `--no-experimental-detect-module` | E | Disables ESM/CJS syntax detection. **Affects our format-determination logic** if user has a `.js` file with no `"type"` and our code path was relying on detect-module. Subtle — verify. |
| `--no-experimental-global-navigator` | A | Removes `navigator` global. |
| `--no-experimental-repl-await` | A | REPL TLA. |
| `--no-experimental-require-module` | D | Disables `require(esm)`. **Affects users with CJS code that requires our transpiled-as-ESM `.ts` output.** If our hook returns `format: 'module'` for a `.ts` file required from CJS, that `require()` fails under `--no-experimental-require-module`. The recommended fix is for format-determination to honor source syntax (which it does — see [`commonjs-handling.md`](commonjs-handling.md)), but mixed-format projects remain a footgun. Document. |
| `--no-experimental-sqlite` | C | Disables `node:sqlite`. Interacts with our planned `node:sqlite` unflag — if user opts out, we honor (per the opt-out rule). |
| `--no-experimental-websocket` | C | Disables WebSocket global. Same opt-out rule. |
| `--no-experimental-webstorage` | A | localStorage disable. |
| `--no-extra-info-on-fatal-exception` | A | Diagnostic verbosity. |
| `--no-force-async-hooks-checks` | A | async_hooks tuning. |
| `--no-global-search-paths` | E | Disables `$NODE_PATH` / `$HOME/.node_modules` search. **Our resolver doesn't use these paths; passthrough is correct.** But verify that our resolve hook calling `nextResolve` correctly forwards this constraint. |
| `--no-network-family-autoselection` | A | DNS. |
| `--no-require-module` | D | Same as `--no-experimental-require-module`. |
| `--no-strip-types` | F/E | Disables Node's built-in strip-types. **Nub's hook owns `.ts` files, so Node's strip-types is mooted regardless.** A user passing `--no-strip-types` to stop `.ts` transpilation still gets Nub's hook. Document loudly; `--node` turns Nub off too. |
| `--no-warnings` | B/F | **Injected by Nub.** The auto-flag-injection opt-out would be `--no-no-warnings`, which Node doesn't recognize; nor does it recognize `NODE_OPTIONS=--warnings`. The only real opt-out today is removing the injection via `--node`. **A known gap.** Likely action: a Nub-side `--show-warnings` flag that suppresses the `--no-warnings` injection. |
| `--node-memory-debug` | A | Memory debug. |
| `--openssl-config` | A | OpenSSL. |
| `--openssl-legacy-provider` | A | OpenSSL. |
| `--openssl-shared-config` | A | OpenSSL. |
| `--pending-deprecation` | C | Emits pending deprecations. Composes with `--no-warnings` (suppressed). |
| `--permission` | E | See [§4.1](#41-permission). |
| `--permission-audit` | A | Audit mode for permission model. Same family. |
| `--preserve-symlinks` | E | See [§4.5](#45-preserve-symlinks). |
| `--preserve-symlinks-main` | E | Same. |
| `-p, --print` | C | Same as `-e`, prints result. |
| `--prof` / `--prof-process` | A | V8 profiler. |
| `--redirect-warnings` | C | Writes warnings to file. **Our `--no-warnings` suppresses everything**, so this file ends up empty. Composes; surprising. Document. |
| `--report-*` family (compact, dir, exclude-env, exclude-network, filename, on-fatalerror, on-signal, signal, uncaught-exception) | A | Diagnostic reports. No hook interaction. |
| `-r, --require` | C | See [§4.7](#47-require-vs-import). |
| `--run` | E | Experimental `npm run`-style runner that bypasses npm. **Conflicts with `nub run` entirely.** If user does `nub --run foo`, the `--run` gets passed to Node and runs the script per Node's interpretation. Nub's own `nub run` subcommand takes precedence (it's parsed at the CLI layer, not as a Node flag). Document the distinction. |
| `--secure-heap` / `--secure-heap-min` | A | Secure heap. |
| `--snapshot-blob` | D | Loads a snapshot. **Mooted by `--build-snapshot` incompatibility** above — if snapshots can't include our hooks, loading a snapshot that doesn't have them means TS execution fails. Document. |
| `--test` | E | node:test runner. Our hook fires on `.test.ts` files; should work. **Watch for** test-isolation modes (`--test-isolation=process` spawns children — our PATH shim takes over, fine; `--test-isolation=worker` uses worker_threads — worker `execArgv` inheritance question). |
| `--test-concurrency` / `--test-coverage-*` / `--test-force-exit` / `--test-global-setup` / `--test-isolation` / `--test-name-pattern` / `--test-only` / `--test-random-seed` / `--test-randomize` / `--test-reporter` / `--test-reporter-destination` / `--test-rerun-failures` / `--test-shard` / `--test-skip-pattern` / `--test-timeout` / `--test-update-snapshots` | C | node:test subflags. Pass through. Verify `--test-isolation=worker` doesn't lose our preload. |
| `--throw-deprecation` | C | Throws on deprecation. Composes with `--no-warnings` (warnings off, throws still fire). |
| `--title` | A | Process title. |
| `--tls-cipher-list` / `--tls-keylog` / `--tls-max-v1.*` / `--tls-min-v1.*` | A | TLS. |
| `--trace-deprecation` | C | Deprecation traces. Suppressed by `--no-warnings`. |
| `--trace-env` / `--trace-env-js-stack` / `--trace-env-native-stack` | E | Traces env-var access. **Our preload reads `process.env` for cache-dir, hijack-detection, etc.** With `--trace-env`, users see Nub's env reads alongside their own. Mostly noise; not broken. Document. |
| `--trace-event-categories` / `--trace-event-file-pattern` / `--trace-events-enabled` | A | Trace events. |
| `--trace-exit` | A | Stack trace on exit. |
| `--trace-require-module` | E | Traces require-of-ESM calls. **Our hook returning `format: 'module'` for a CJS-required `.ts` file would show up here.** Useful diagnostic. No action. |
| `--trace-sigint` | A | SIGINT trace. |
| `--trace-sync-io` | C | Warns on sync I/O. **Our sync `registerHooks` load hook does sync I/O** (read source, write cache, NAPI transpile). This will fire warnings *constantly* under our preload. Documented expected behavior; user shouldn't use `--trace-sync-io` with Nub. |
| `--trace-tls` | A | TLS trace. |
| `--trace-uncaught` | C | Uncaught traces. Composes. |
| `--trace-warnings` | C | Warning stack traces. Suppressed by `--no-warnings`. |
| `--track-heap-objects` | A | Heap tracking. |
| `--unhandled-rejections` | A | Rejection handling mode. |
| `--use-bundled-ca` / `--use-openssl-ca` / `--use-system-ca` | A | TLS CA source. |
| `--use-env-proxy` | A | Proxy from env. |
| `--use-largepages` | A | V8 page allocation. |
| `-v, --version` | A | Version. We intercept `--version` at the CLI layer to print Nub's version; user can `nub node --version` for Node's. |
| `--v8-options` | A | Prints V8 options. |
| `--v8-pool-size` | A | V8 thread pool. |
| `--watch` | D | See [§4.2](#42-watch). |
| `--watch-kill-signal` | C | Kill signal on file change. Forwards to our watch impl. |
| `--watch-path` | C | Extra watch paths. **The `nub watch` subcommand uses union semantics with the import graph**; Node's `--watch-path` replaces. Passing `--watch-path` to `nub node --watch` gives Node semantics. |
| `--watch-preserve-output` | C | Preserve output across restart. Our `nub watch` defaults to preserve. |
| `--zero-fill-buffers` | A | Buffer hygiene. |

### Environment variables

| Var | Cat | Verdict |
|-----|-----|---------|
| `FORCE_COLOR` | A | Color forcing. Composes with our colors-styletext addition. |
| `NODE_COMPILE_CACHE` | C | V8 code cache. **Composes with our transpile cache** — they're orthogonal layers. NCC caches V8 bytecode; ours caches transpiled JS. Both can be on simultaneously. |
| `NODE_COMPILE_CACHE_PORTABLE` | C | Same. |
| `NODE_DEBUG` | A | Debug logging. |
| `NODE_DEBUG_NATIVE` | A | Native debug. |
| `NODE_DISABLE_COLORS` | A | Color disable. |
| `NODE_DISABLE_COMPILE_CACHE` | C | Disables NCC. Composes; our transpile cache stays on. |
| `NODE_EXTRA_CA_CERTS` | A | TLS CA. |
| `NODE_ICU_DATA` | A | ICU data path. |
| `NODE_NO_WARNINGS` | C | Same as `--no-warnings`. We set this implicitly via flag injection; user can also set it. |
| `NODE_OPTIONS` | E | **Critical interaction surface.** See [§5](#5-the-compose-with-hijack-test-matrix). Parsed before argv per Node docs. Affects flag-injection precedence. |
| `NODE_PATH` | E | Module search path. **Our resolve hook should pass through to `nextResolve`** which honors `NODE_PATH` for legacy CJS lookups. ESM doesn't consult NODE_PATH. Mostly correct by construction. |
| `NODE_PENDING_DEPRECATION` | A | Pending deprecations. |
| `NODE_PENDING_PIPE_INSTANCES` | A | Windows pipe. |
| `NODE_PRESERVE_SYMLINKS` | E | Env-var form of `--preserve-symlinks`. Same concern; same handling. See [§4.5](#45-preserve-symlinks). |
| `NODE_REDIRECT_WARNINGS` | C | Same as `--redirect-warnings`. |
| `NODE_REPL_EXTERNAL_MODULE` | A | REPL external. |
| `NODE_REPL_HISTORY` | A | REPL history file. |
| `NODE_SKIP_PLATFORM_CHECK` | A | Platform check bypass. |
| `NODE_TEST_CONTEXT` | A | Test context. |
| `NODE_TLS_REJECT_UNAUTHORIZED` | A | TLS reject. |
| `NODE_USE_ENV_PROXY` | A | Proxy. |
| `NODE_USE_SYSTEM_CA` | A | TLS CA. |
| `NODE_V8_COVERAGE` | A | Coverage output. |
| `NO_COLOR` | A | No-color. |
| `OPENSSL_CONF` | A | OpenSSL. |
| `SSL_CERT_DIR` | A | SSL. |
| `SSL_CERT_FILE` | A | SSL. |
| `TZ` | A | Timezone. |
| `UV_THREADPOOL_SIZE` | A | libuv thread pool. |

### V8 raw flags (cursory)

| Flag | Cat | Verdict |
|------|-----|---------|
| `--harmony-*` | A | Stage <4 proposals. No hook interaction. |
| `--max-heap-size` / `--max-old-space-size` / `--max-semi-space-size` | A | V8 heap sizing. |
| `--stack-trace-limit` | A | Stack frames. |
| `--turbofan-*` / `--turboshaft-*` | A | V8 compiler. |
| `--perf-basic-prof` / `--perf-prof` / `--perf-prof-unwinding-info` | A | Linux perf. |
| `--security-revert` | A | V8 security revert. |
| `--interpreted-frames-native-stack` | A | Stack trace style. |
| `--heap-snapshot-on-oom` | A | OOM heap snapshot. |
| `--enable-etw-stack-walking` | A | Windows ETW. |

## 4. Deep-dives

### 4.1. `--permission`

**Status in Node:** Renamed from `--experimental-permission` to `--permission` in Node 24; backing model promoted from Stability 1.1 → 1.2 per [PR #56201](https://github.com/nodejs/node/pull/56201). Default-deny security model: when `--permission` is on, every file-system read/write, network call, child-process spawn, addon load, worker creation, WASI instance, and inspector connection requires an explicit grant flag (`--allow-fs-read=<path>`, `--allow-addons`, etc.).

**What Nub does at boot that the permission model would deny:**

1. **Preload module read.** Node's `--import <preload>` opens `<preload>` for reading. Without `--allow-fs-read=<preload-path>` this fails before the hooks register. The permission check runs *during* the FS call rather than as a static boot-time analysis, but the effect is the same: `ERR_ACCESS_DENIED` at the first read.
2. **Source-file reads.** The load hook reads `.ts`/`.tsx` source files. Each read needs `--allow-fs-read=<source-path>`, `--allow-fs-read=<project-root>`, or `--allow-fs-read=*`.
3. **Transpile cache writes.** The content-addressed cache writes to `~/.cache/nub/transpile/`, needing `--allow-fs-write=<cache-dir>`.
4. **N-API addon load.** A `.node` addon for transpile or resolver fast-path needs `--allow-addons`.
5. **PATH shim temp dir.** The hijack creates `/tmp/nub-node-<pid>-<hash>/` from the Rust CLI *before* Node spawns, so the permission model doesn't gate it — but child-process spawns (PATH-shim re-entry) need `--allow-child-process`.

**Verification status:** checked against the Node permission-model docs (`cli.html#--permission`) and `src/permission/permission.cc`. Permissions go through `Permission::is_granted(scope, resource)` inside the fs binding, including the preload-loading path. **Needs prototyping to verify** the exact error shape and which read fails first.

**Resolution options:**

1. **Auto-grant supplementation.** On detecting `--permission` in user argv or `NODE_OPTIONS`, the Rust CLI computes the minimum grant set and prepends it: `--allow-fs-read=<preload-dir>`, `--allow-fs-read=<cache-dir>`, `--allow-fs-write=<cache-dir>`, `--allow-addons` where an addon is used, and `--allow-fs-read=<project-root>` for source-file reads — the last of which is a large grant that broadens the permission scope without explicit user consent, defeating the user's scoping.
2. **Refuse and direct to compat mode.** Nub detects `--permission`, prints "Nub's runtime augmentation requires broad filesystem access; use `nub --node` or `NODE_COMPAT=1` if you need `--permission` semantics," and exits non-zero.
3. **Document a precise grant recipe.** A one-shot `nub --explain-permission` command prints the exact grant flags to add. Neither auto-grants nor refuses; teaches the user. The most honest option.

**Recommendation:** ship Option 2 in v0.1, promote Option 3 to v0.2. Auto-grant is never the right call — it silently defeats the user's security posture.

**Action items:**
- [ ] Verify the exact failure mode in a prototype: Node 24, `--permission`, Nub preload — what error does the user see?
- [ ] Decide between Option 2 and Option 3 for v0.1 and document it.
- [ ] Add a CI test that runs `nub script.ts --permission` and asserts the documented behavior.

### 4.2. `--watch`

**Status in Node:** Stable in Node 22+. Loader-instrumented as of 22.0 (the watcher reads dependencies from the resolver, not from filesystem globs).

**The mechanism:**

- `lib/internal/main/watch_mode.js` spawns the user's command as a child with `WATCH_REPORT_DEPENDENCIES=1` set in env, and an IPC pipe on fd 3.
- The child process's ESM/CJS loader, via `lib/internal/modules/cjs/loader.js` and `lib/internal/modules/esm/loader.js`, calls `process._rawDebug` / `process.send` with `{ 'watch:require': [path] }` or `{ 'watch:import': [url] }` for each file actually loaded.
- The parent `FilesWatcher` registers `fs.watch` on each reported path; on change, kills + respawns the child.

**The Nub question:** when the `registerHooks` load hook fires for `./script.ts` and returns `{ source: '<transpiled JS>', format: 'module', shortCircuit: true }`, does Node still emit the `watch:import` event for `./script.ts`?

**Working hypothesis (needs prototyping):** Node emits `watch:import` from the loader **before** invoking the load chain. The URL Node knows about is the resolved URL passed to the loader — `file:///abs/path/to/script.ts` — so the watcher should see and watch `script.ts` regardless of what the hook returns.

**Where it gets murky:**

- If the **resolve** hook rewrites a specifier (a tsconfig path alias `@/utils` → `file:///abs/src/utils.ts`), the watcher sees the rewritten URL, which is the desired behavior.
- If the **load** hook synthesizes a URL (conceivable for an in-memory virtual module), the watcher sees the URL but cannot `fs.watch` it because no real file exists. Not a concern in v0, which synthesizes no URLs.
- **Transitive imports inside transpiled output** — the transpiled JS contains `import './foo.ts'`. Node's loader resolves and loads `./foo.ts`, the hook fires for it, and the watcher sees and watches `./foo.ts`. **Verify this in CI**; it is the load-bearing case.
- **Files the hook reads that aren't in the import graph** — the `tsconfig.json` read to compute path aliases. The watcher never sees it, so editing `tsconfig.json` triggers no restart. **Bug.** Fix: the preload emits an explicit `process.send({'watch:require': ['/abs/tsconfig.json']})` when it loads the tsconfig, over the same IPC channel.

The `nub watch` subcommand's v0.1 design rides this mechanism and is mechanically sound, with these corners needing verification:

- [ ] **CI test:** `nub watch script.ts`, edit `script.ts`, confirm restart fires.
- [ ] **CI test:** `nub watch script.ts`, edit `tsconfig.json`, confirm restart fires (after we wire the explicit `watch:require` emission for tsconfig).
- [ ] **CI test:** `nub watch script.ts`, edit `./utils.ts` (transitively imported), confirm restart fires.

### 4.3. `--env-file`

**Status in Node:** `--env-file=<path>` since Node 20.6; `--env-file-if-exists=<path>` since 21.7 / 22.x. Both load env vars from a `.env`-format file at process boot, *after* preloads have registered but *before* user code runs.

**What Nub does:** the CLI reads `.env*` files **before** spawning Node and prepends the parsed values to the child env, with workspace-aware discovery and `${VAR}` expansion. Disabled under `--node` / `NODE_COMPAT=1`.

**The interaction:**

With neither an `--env-file` flag nor `nub --node`, Nub loads `.env*`, user code sees those values, and Node's `--env-file` is not involved. ✓

With `--env-file=.env.custom` passed, the question is whether Nub *also* loads `.env*` from the workspace:

- Option A: Nub's loader fires regardless; Node's `--env-file` runs *after* the child spawns, so which value sticks depends on whether `--env-file` overrides existing env. Per Node docs it does **not** override existing env vars. So if Nub pre-loaded `FOO=bar` and Node's `--env-file` says `FOO=baz`, the final value is `bar` — Nub's value counts as "existing" because it is in the env when Node starts.
- Option B: Nub detects user-passed `--env-file` and disables its own eager loader. Predictable, but surprising for users who also want Nub's workspace `.env*` files.
- Option C: Nub detects user-passed `--env-file=<path>` and skips *that specific path* in its eager loader, still loading other workspace `.env*` files. Closest to composing them; minimum surprise.

**Expansion mismatch:** Nub does `${VAR}` and `$VAR` expansion with shell-env-first ordering, per [`env-expansion-and-test-skip.md`](env-expansion-and-test-skip.md). Node's `--env-file` does **not** do `${VAR}` expansion at all (per Node docs, last verified 2026-05-18; this may evolve). So a `.env` file with `PASSWORD=${SECRET}` loaded via:
- Nub → expanded to value of `SECRET` from shell env, or empty.
- Node's `--env-file` → literal string `${SECRET}`.

If both load the same file, Nub's expanded value wins under the same non-override rule.

**Recommendation:** Option C — Nub's eager loader skips paths the user explicitly passed via `--env-file` / `--env-file-if-exists`. Other workspace `.env*` files still load via Nub.

**Action items:**
- [ ] Implement the skip-detection in the Rust CLI's `--env-file` argv parser.
- [ ] Document the interaction on the env-loading page.
- [ ] CI test: `nub script.ts --env-file=.env.custom`, with a workspace `.env` and a custom `.env.custom`, both loaded.

### 4.4. `--frozen-intrinsics`

**Status in Node:** Experimental since 10.x. Freezes `Object.prototype`, `Array.prototype`, etc. so user code can't mutate them. Audience: lockdown environments (SES-style, secure mashups).

**Why it's incompatible with the preload (working assumption):**

The preload imports from `node:module`, sets up `module.registerHooks()`, and uses `JSON.parse`/`JSON.stringify`/`Map`/`Set`/array spreads — all fine under `--frozen-intrinsics` so long as no prototype is mutated. The transpile-cache and tsconfig-path machinery may use prototype methods more aggressively.

**The bigger problem is vendored third-party deps.** Every preload import — oxc bindings via the NAPI shim, vendored JSON-schema validators — is a potential prototype-mutation site. SES surveys have shown that ~30% of popular npm packages don't survive frozen intrinsics.

**Transpiled output:** oxc's emit does not mutate prototypes in stripped TS, but specific transforms (async generators on older targets, decorators) may emit helpers that touch them. Needs verification per transform.

**Recommendation:** document Nub as incompatible with `--frozen-intrinsics` in v0. Users who need frozen intrinsics use `--node`, which disables the preload and leaves them on vanilla Node plus their own SES setup.

**Action items:**
- [ ] CI test: `nub script.ts --frozen-intrinsics` — assert the failure mode is clear, not a cryptic stack trace.
- [ ] Document the incompatibility.

### 4.5. `--preserve-symlinks`

**Status in Node:** Stable. `--preserve-symlinks` and `--preserve-symlinks-main` tell the module loader **not** to call `realpath` on resolved module URLs, so the same physical file linked from two locations appears as two different modules — what `npm link` workflows want, so the linked package evaluates in its linked location's context.

**`NODE_PRESERVE_SYMLINKS=1`** is the env-var form.

**What the resolve hook does:**

```js
// Conceptually
resolve(specifier, context, nextResolve) {
  // 1. Check tsconfig path aliases
  const aliased = rewriteViaTsconfigPaths(specifier, context);
  if (aliased) {
    specifier = aliased;
  }
  // 2. Extensionless probing — try .ts, .tsx, .mts, .cts
  if (isExtensionlessAndCouldBeTs(specifier, context)) {
    for (const ext of ['.ts', '.tsx', '.mts', '.cts']) {
      const candidate = specifier + ext;
      if (fs.statSync(resolveUrl(candidate, context.parentURL))) {
        return nextResolve(candidate, context);
      }
    }
  }
  return nextResolve(specifier, context);
}
```

**The risk:**

- `fs.statSync` follows symlinks by default. Under `--preserve-symlinks` the hook should call `fs.lstatSync` to check existence without dereferencing.
- The URL passed to `nextResolve` must be the **link path**, not the realpath — automatic so long as the hook never calls `realpath` itself.
- Node's `nextResolve` honors `--preserve-symlinks` internally, so the only requirement is not pre-empting it with a realpath'd URL.

**Verification:** realpath happens inside `finalizeResolution` (`lib/internal/modules/esm/resolve.js`), called by `moduleResolve`, so a path the hook passes to `nextResolve` still honors `--preserve-symlinks`. **A hook that never calls realpath is safe.**

**Action items:**
- [ ] Audit the resolve hook implementation for `fs.realpath` calls.
- [ ] CI test: symlink fixture, `--preserve-symlinks`, confirm resolution path matches expected.
- [ ] Document the contract alongside the tsconfig-paths and extensionless-probing behavior.

### 4.6. `--conditions`

**Status in Node:** `-C, --conditions=<name>` (Node 14.9+) adds custom export conditions for package.json `exports` resolution. Multi-use: `-C dev -C testing` adds both. Node honors them in addition to the built-in conditions (`node`, `import`, `require`, `default`).

**What the resolve hook does:** when the hook calls `nextResolve(specifier, context)`, Node's default resolver consults `context.conditions` to pick the right export. Per Node's hook API contract, `context.conditions` is populated by Node before the hook chain runs.

**The risk:**

- A hook that reconstructs the context object and forgets to copy `conditions` leaves the default resolver seeing `undefined`, falling back to built-in conditions only, and the user's `--conditions=dev` is silently dropped.
- A hook that **adds** conditions — say a `'nub-runtime'` condition — must merge with user-set conditions, not replace them.

**Recommendation:** never reconstruct the context object; pass it through. To add conditions, mutate a copy:

```js
resolve(specifier, context, nextResolve) {
  const newContext = {
    ...context,
    conditions: [...(context.conditions ?? []), 'nub-runtime'],
  };
  return nextResolve(specifier, newContext);
}
```

**Open question:** should Nub add a `'nub-runtime'` condition? With it, packages can ship Nub-specific exports such as a Nub-optimized polyfill build; without it, less surface. **Lean: no for v0**, revisit if a package author asks.

**Action items:**
- [ ] CI test: package with `exports: { 'dev': './dev.js', 'default': './prod.js' }`, run with `nub --conditions=dev script.ts`, verify `dev.js` is loaded.
- [ ] Audit the hook code path to confirm `conditions` is passed through.

### 4.7. `--require` vs `--import`

**Status in Node:**
- `-r, --require <module>` preloads a CJS module before user code. Module is loaded synchronously via Node's CJS loader. Predates `register()` / `--import`. Multi-use OK.
- `--import <module>` preloads an ESM module before user code. Multi-use OK. Runs sync at boot.

**What Nub does:** prepends `--import <nub-preload>` so its `registerHooks` registration runs first.

**The interactions:**

1. **User passes `--require ./instrument.cjs`.** Argv after the prepend is `--import <nub> --require ./instrument.cjs <user-script>`, and Node's boot order loads CJS `--require` modules *before* ESM `--import` modules. So `./instrument.cjs` runs **before** hook registration. If it does APM monkey-patching of `Module.prototype.require`, that patching is in place when the preload loads; the subsequent `registerHooks()` call is an ESM import inside the preload, which the APM does not see as a require. For most APMs this is fine — they care about user-code patterns, and the preload is opaque to them.

**Subtle issue:** if an APM eager-loads user code (some `dd-trace` configurations preload entry-point modules), that code goes through the CJS loader without Nub's hooks and misses TS transpilation. A documented edge case affecting close to zero users, since APMs typically patch require-paths rather than preloading user code.

2. **User passes `--require ./tsx-something.cjs`.** A user transitionally on both tsx and Nub registers both hooks. Node's hook-chaining model runs them in registration order; both fire on `.ts` files and whichever returns first with `shortCircuit: true` wins. tsx's hook likely fires first, since `--require` runs before `--import`, giving tsx's transpile. **Not a Nub bug**; document as "don't run two TS loaders simultaneously."

3. **User passes `--import ./their-preload.mjs`.** Argv is `--import <nub> --import ./their-preload.mjs`; both run, Nub's first. Their preload sees a runtime where Nub's hooks are already registered and can call `registerHooks` itself, chaining correctly. ✓

**Action items:**
- [ ] CI test: `--require ./instrument.cjs` + `nub script.ts`, confirm script runs without surprises.
- [ ] CI test: two `--import` preloads, confirm hook ordering.
- [ ] Document the order semantics.

### 4.8. `--inspect` / `--inspect-brk` / `--inspect-wait`

**Status in Node:** Stable. `--inspect` activates the V8 inspector on a port; `--inspect-brk` does the same and breaks at the start of user code; `--inspect-wait` waits for a debugger to attach before running.

**What Nub does:** emits source maps in transpile output, injects `--enable-source-maps` by default, and ships a `nub inspect` subcommand wrapping `--inspect-brk` with TS-aware UX.

**The risk:**

- **Breakpoints set before the hook fires.** A user on `--inspect-brk` who sets a breakpoint in `script.ts` at line 10 *before* the hook has loaded the file resolves it against the URL `file:///abs/script.ts` line 10. When the hook returns transpiled JS, line numbers survive the source-map round-trip, so Chrome DevTools and `nub inspect` should apply the breakpoint to the original `.ts` line 10. **Should work**, with two edge cases:
  - Source-map URL resolution differs across inspector clients (Chrome DevTools, VSCode, IntelliJ). Inline base64 maps are the most portable, and Nub emits those.
  - `sourcesContent` must be embedded so DevTools can display the original source after transpilation. Nub embeds it by default.
- **Inspector protocol's `Debugger.scriptParsed` event.** Fires with the transpiled source's URL. Chrome maps back via the source map; some custom inspector clients don't. Document it.

**Recommendation:** default `nub inspect` to `--inspect-wait` rather than `--inspect-brk`, so the debugger attaches *before* user code starts but *after* the preload has registered the hooks. Breakpoints then target the `.ts` source URL the inspector knows about via source maps, and take effect on the first hit of that line.

**Action items:**
- [ ] Verify and document the `--inspect-brk` vs `--inspect-wait` default for `nub inspect`.
- [ ] CI test: `nub inspect script.ts`, attach Chrome DevTools, set breakpoint in `.ts`, hit it, verify source displays.

### 4.9. `--no-warnings` opt-out gap

**The gap:** Nub injects `--no-warnings`, and Node has no `--no-no-warnings` flag. A user who wants warnings back on under Nub has two options:

1. Use `--node` / `NODE_COMPAT=1`, which turns off **all** of Nub's augmentation, including TS transpilation.
2. Pass a hypothetical Nub-side `--show-warnings` flag that suppresses the injection — which does not exist yet.

The auto-flag-injection opt-out mechanism is a Rust-side subtract: a user `--no-X` in argv or `NODE_OPTIONS` means Nub doesn't inject `--X`. But `--no-warnings` is **already** the negation, so there is no positive form to set.

**Recommendation:** add a Nub-side `--show-warnings` flag that suppresses the `--no-warnings` injection. Simple, and needs no Node-side support.

**Action items:**
- [ ] Add `--show-warnings` to Nub's CLI surface.
- [ ] Document the opt-out.

## 5. The compose-with-hijack test matrix

The combinatorics that need test coverage:

- Auto-inject set varies by detected Node version.
- `NODE_OPTIONS` env var is parsed *before* argv per Node docs.
- User argv flags (passed to `nub`).
- Compat mode env var (`NODE_COMPAT=1`).
- Per-Node-version differences in flag recognition.
- Process-tree depth via PATH hijack.

The full Cartesian space is too big to test exhaustively — 5+ dimensions, each with 2–5 values. These 10 scenarios exercise the integration points, each a class of real-world configurations.

### Scenario 1: vanilla path

**Setup:** Node 22.15 installed. User runs `nub script.ts`. No `NODE_OPTIONS`, no flags.

**Expected:** Nub detects Node 22.15, injects `--no-warnings --enable-source-maps`, prepends `--import <preload>`, sets up PATH shim, spawns Node. Script runs, TS transpiled.

**What it tests:** baseline integration.

### Scenario 2: NODE_OPTIONS opt-out of injected flag

**Setup:** `NODE_OPTIONS=--no-experimental-vm-modules`. User runs `nub script.ts`. Nub would inject `--experimental-vm-modules` (per the planned unflag).

**Expected:** Nub's argv parser sees the user's `--no-` opt-out in `NODE_OPTIONS`, subtracts `--experimental-vm-modules` from the inject set, spawns Node with only `--no-warnings --enable-source-maps --import <preload>`. Node's own `NODE_OPTIONS` parsing applies `--no-experimental-vm-modules`. Net result: vm-modules disabled.

**What it tests:** the [§4 deep-dive](#4-deep-dives) `NODE_OPTIONS` precedence handling against the auto-flag-injection contract.

### Scenario 3: user `--import` chains with Nub's

**Setup:** `nub --import ./my-preload.mjs script.ts`.

**Expected:** Nub's CLI passes the user's `--import` through. Final Node argv: `--import <nub-preload> --import ./my-preload.mjs script.ts`. Nub's hooks register first, then the user's preload runs with hooks already active.

**What it tests:** preload chaining, no clobbering.

### Scenario 4: hijack-through-spawn

**Setup:** `nub run dev` where the `dev` script in package.json is `"node ./server.ts"`. Real Node exists at `/usr/local/bin/node`.

**Expected:** Nub's PATH shim prepends `/tmp/nub-node-<pid>-<hash>/`, which contains `node` → Nub. The script's `node ./server.ts` resolves to the shim → Nub runs as `node` → Nub-node dispatch path → spawns the real Node with Nub's augmentation, and `./server.ts` transpiles and runs.

**What it tests:** PATH hijack, argv0 dispatch, depth-2 process tree, augmentation propagation.

### Scenario 5: hijack-through-spawn with compat mode

**Setup:** `nub --node script.ts`. The script does `child_process.spawn('node', ['./worker.ts'])`.

**Expected:** Nub is in compat mode for the root, so the PATH shim is **not** set up and `child_process.spawn('node', ...)` resolves to the user's real Node, which fails on `./worker.ts` with a normal Node error (no TS support). That is **correct** compat-mode behavior: the user opted out of Nub, so the child runs un-augmented too.

**What it tests:** compat mode disables hijack; `NODE_COMPAT=1` propagates correctly.

### Scenario 6: hijack with NODE_COMPAT in child shell

**Setup:** `nub run dev` where the `dev` script is `"NODE_COMPAT=1 node ./server.ts"`. (User explicitly sets compat for a child.)

**Expected:** The parent (root `nub run`) sets up the PATH shim, so `node` resolves to Nub → Nub starts → sees `NODE_COMPAT=1` in its env → runs in compat mode for the child → spawns real Node without augmentation, and `./server.ts` fails on real Node (no TS).

**What it tests:** `NODE_COMPAT` is consulted at each Nub entry, not just root.

### Scenario 7: --permission interaction

**Setup:** `nub --permission script.ts`.

**Expected (per [§4.1 recommendation](#41-permission)):** Nub prints "use `--node` if you need `--permission`", exits non-zero.

**Alternative if we implement auto-grant:** Nub prepends the minimum grant set from [§4.1](#41-permission) plus `--allow-fs-read=<cwd>` and spawns; the script runs with permissions restricted (mostly) per the user's intent.

**What it tests:** the permission-model interaction, whichever path we choose.

### Scenario 8: --watch with TS entry

**Setup:** `nub script.ts --watch` (or `nub watch script.ts`). `script.ts` imports `./utils.ts`. Both files exist; `tsconfig.json` in workspace root.

**Expected:** Initial run transpiles both `.ts` files. Watcher registers `script.ts`, `utils.ts`, **and** `tsconfig.json` (the last via explicit `process.send({'watch:require': ['/abs/tsconfig.json']})` in our preload). Edit any of the three → restart fires.

**What it tests:** [§4.2 watch interaction](#42-watch), specifically the `tsconfig.json` watch piggyback.

### Scenario 9: NODE_OPTIONS adds env-file, Nub also discovers .env

**Setup:** `NODE_OPTIONS=--env-file=.env.custom`. Workspace has `.env` (Nub's discovery) and `.env.custom` (user-specified).

**Expected (per [§4.3](#43-env-file)):** Nub skips `.env.custom` in its own discovery and loads `.env` only. Node's `NODE_OPTIONS=--env-file=.env.custom` loads `.env.custom`. Final env: shell > .env > .env.custom (per Node's "don't override existing").

**What it tests:** double-load detection, env precedence correctness.

### Scenario 10: Worker thread with execArgv

**Setup:** `nub script.ts`. `script.ts` does:
```ts
import { Worker } from 'node:worker_threads';
const w = new Worker('./worker.ts');
```

**Expected (the open question):** does the worker thread also get the preload? Workers inherit `execArgv` by default, and Nub's spawn put the preload's `--import` in the parent's `process.execArgv`, so the worker should inherit it. Without the preload the worker's `./worker.ts` fails; with it, the file transpiles and runs.

**Edge case:** an explicit `execArgv: []` in the Worker constructor spawns the worker *without* the preload, and `./worker.ts` fails. The user explicitly opted out of inheritance, so this is correct behavior, if surprising.

**What it tests:** worker_threads `execArgv` inheritance, the deepest practical process-tree depth for Nub's augmentation.

### Coverage summary

Each of the 10 scenarios should be a CI test. The matrix is small enough to maintain and large enough to catch the integration bugs it targets.

## 6. Action items per flag (consolidated)

### Pre-v0.1 (blocking)

- [ ] **`--permission`:** Implement rejection-with-helpful-message (or auto-grant, per the [§4.1](#41-permission) decision). Add CI test S7 and a decision record.
- [ ] **`--watch`:** Wire explicit `process.send({'watch:require': [path]})` in the preload for files Nub reads that aren't in Node's import graph, notably `tsconfig.json`. Add CI test S8 and the per-file tests in [§4.2](#42-watch), and document the mechanism.
- [ ] **`--env-file` / `--env-file-if-exists`:** Implement user-passed `--env-file` detection in the argv parser and skip those paths in Nub's eager loader. Add CI test S9.
- [ ] **`--no-warnings`:** Add a `--show-warnings` CLI flag that suppresses the injection, and document it.
- [ ] **`--conditions`:** Audit the resolve hook so context pass-through preserves `conditions`. Add a CI test for the conditions scenario.
- [ ] **`--preserve-symlinks`:** Audit the resolve hook for `realpath` calls; remove or guard. Add a CI test with a symlink fixture.
- [ ] **`--frozen-intrinsics`:** Add a CI test asserting the failure mode is clear (or that it works, if made to). Document the compat-mode recommendation.

### v0.1 / pre-stable (high priority)

- [ ] **`--inspect-wait` default for `nub inspect`:** Verify and document.
- [ ] **`--no-addons` interaction:** If Nub ships an addon, document the fallback; with a JS-only preload, no-op.
- [ ] **`--disallow-code-generation-from-strings`:** Audit the preload and oxc output for `eval` / `new Function`. If clean, no action; if not, document.
- [ ] **`--disable-proto`:** Audit for `__proto__` usage. Should be clean; verify.
- [ ] **`--build-snapshot` / `--snapshot-blob`:** Document incompatibility; recommend compat mode.
- [ ] **`--experimental-loader=`:** Document chaining behavior with `registerHooks` and its deprecation status.
- [ ] **`--require` ordering:** Document [§4.7](#47-require-vs-import).
- [ ] **`--test-isolation=worker`:** Verify worker `execArgv` inheritance keeps the preload registered in worker threads. CI test S10.

### Post-v0.1 (document only)

- [ ] **`--trace-sync-io`:** Document that the sync `registerHooks` load hook fires this constantly; recommend not using it with Nub.
- [ ] **`--trace-env`:** Document that Nub's preload reads `process.env` (cache-dir, hijack detection).
- [ ] **`--redirect-warnings`:** Document that Nub's `--no-warnings` suppresses everything, so the redirect file ends up empty.
- [ ] **`--experimental-test-module-mocks`:** Prototype the interaction with `registerHooks` and document either the behavior or the limitation.
- [ ] **`--run` (Node's npm-script runner):** Document precedence vs `nub run`.

## 7. Open questions

1. **Permission model auto-grant vs reject (§4.1).** The recommendation is "reject with helpful message" for v0.1; auto-grant could be implemented later. An open product decision.
2. **`--watch` reports for non-import files.** The working hypothesis is that Nub's preload emits `watch:require` for tsconfig and other indirect-dep files. **Needs prototyping** to confirm Node's `WATCH_REPORT_DEPENDENCIES` IPC accepts these reports from an arbitrary preload rather than only from the loader internals.
3. **Worker-thread preload inheritance (§5 S10).** Default `execArgv` inheritance should propagate the `--import`, but needs verification across Node 22.15 / 24 / 25 — especially if `--test-isolation=worker` becomes a common pattern.
4. **`--frozen-intrinsics` compatibility floor.** Nub's own preload code could probably be adapted to survive frozen intrinsics; arbitrary user deps cannot. Decision: don't try; document the incompatibility.
5. **`--experimental-test-module-mocks` × Nub's hooks.** Both register loader hooks. Chaining order is TBD and needs prototyping with a real `node:test` fixture.
6. **`'nub-runtime'` package.json export condition.** Should `--conditions` injection include a `'nub-runtime'` so package authors can ship Nub-specific code? Lean no for v0; revisit if asked.
7. **`--inspect-brk` vs `--inspect-wait` default for `nub inspect`.** The working recommendation is `--inspect-wait`, so DevTools attach *after* the hooks register but *before* user code starts. Verify in practice.
8. **`--allow-fs-read=<cwd>` scope under auto-grant.** Under the auto-grant route for `--permission`, the cwd grant is broad enough to defeat part of the user's security posture. Narrowing it to `<entry-file-dir>` or `<resolved-import-graph>` looks prohibitively complex. Trade-off discussion needed.
9. **`--build-snapshot` + compat-mode fallback UX.** Should Nub fall back to compat mode silently, or refuse loudly? Refuse-loudly is safer: snapshots are unusual, and the user should know why their TS isn't bundled.
10. **Per-Node-version behavior drift.** Node 22.15, 24, 25, and 26 each recognize flags slightly differently, and CI does not currently test against multiple Node versions. Probably worth adding; the cost is non-trivial.

## 8. Suggested follow-on decision records

This audit implies five per-decision write-ups:

1. **Permission interaction** — decision record plus mechanism for `--permission`. High priority.
2. **Preload ordering** — how `--require`, `--import`, and Nub's injection compose. Useful reference; not blocking.
3. **Frozen intrinsics** — the decision to document Nub as incompatible with `--frozen-intrinsics`, with compat mode as the escape.
4. **Worker-thread augmentation** — worker `execArgv` inheritance, how the preload propagates, and what happens on `execArgv: []`.
5. **Snapshot incompatibility** — why `--build-snapshot` doesn't work with the preload model, the compat-mode recommendation, and a possible future Nub-side snapshot feature distinct from Node's.

Plus the updates to existing docs called out in [§6 action items](#6-action-items-per-flag-consolidated).

## 9. Sources

- [`nodejs.org/api/cli.html`](https://nodejs.org/api/cli.html) — canonical flag list, accessed 2026-05-18 via WebFetch.
- [`nodejs.org/api/permissions.html`](https://nodejs.org/api/permissions.html) — permission model documentation.
- [`nodejs.org/api/module.html#moduleregisterhooksoptions`](https://nodejs.org/api/module.html) — `registerHooks()` API contract, including `context.conditions` propagation.
- [`nodejs.org/api/cli.html#--watch`](https://nodejs.org/api/cli.html) — watch mode docs.
- Node source spot-checks: `lib/internal/main/watch_mode.js`, `lib/internal/process/pre_execution.js`, `src/permission/*`, `lib/internal/modules/esm/resolve.js#finalizeResolution`.
- Sibling research: [`experimental-flags-unflagging.md`](experimental-flags-unflagging.md), [`env-expansion-and-test-skip.md`](env-expansion-and-test-skip.md), [`env-file-loading.md`](env-file-loading.md).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
