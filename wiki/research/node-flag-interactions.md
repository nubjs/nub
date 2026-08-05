---
**Scope:** Audit every Node 22.15+ / 24 / 25 / 26 CLI flag (and the
relevant `NODE_*` env vars) against Nub's augmentation surfaces —
`module.registerHooks()` load/resolve hooks, our `--import`
preload, eager `.env*` loading, auto-flag-injection, hijack-by-
default PATH shim, and the `--node` / `NODE_COMPAT=1` compat
escape hatch. Produce per-flag verdicts and a short hijack ×
auto-flag × `NODE_OPTIONS` test matrix.

**Status:** v1, 2026-05-18. A point-in-time survey; where a finding is later
superseded it is corrected in the changelog rather than rewritten in place.

**Relevant to:**
`../runtime/auto-flag-injection.md`,
`../runtime/ts-transpilation.md`,
`../runtime/env-loading.md`,
`../runtime/source-maps.md`,
`../runtime/hijack-by-default.md`,
`../commands/watch.md`,
`../commands/inspect.md`,
`../architecture/compat-mode.md`.
A handful of suggested new plan docs are called out at the end.

**Related docs:**
- `../architecture/augmenter-not-fork.md`
  — load-bearing mechanism rule.
- `../runtime/additivity.md` — what's
  in scope behaviorally.
- [`experimental-flags-unflagging.md`](experimental-flags-unflagging.md)
  — sibling research; covers the "which `--experimental-*` should
  we inject?" question. This doc is the inverse — for every
  pre-existing flag, do we break it?
- [`env-file-loading.md`](env-file-loading.md),
  [`env-expansion-and-test-skip.md`](env-expansion-and-test-skip.md)
  — env-loading research.
- `watch-mode-scope-thesis.md`,
  `watch-mode.md` — watch-mode plumbing.
---

# Node CLI flag interactions with Nub's augmentation surfaces

## 1. TL;DR — the high-risk interactions, ranked

The premise: Nub installs a sync `module.registerHooks()` load+resolve hook via `--import`, prepends V8/Node flags (`--no-warnings`, `--enable-source-maps`, version-conditional `--experimental-*`), eager-loads `.env*` before spawning, prepends a PATH shim so child `node` invocations re-enter Nub, and offers `--node` / `NODE_COMPAT=1` as a global off switch.

Ranked by likelihood × severity:

1. **`--permission` (Node 24+, formerly `--experimental-permission`). Catastrophic interaction. Default-deny.** Our `--import` preload reads from `<nub-binary>/lib/` (or wherever we ship the preload module); our transpile cache writes under `~/.cache/nub/`; our PATH shim creates a temp dir under `/tmp`. None of these have permission grants. If a user passes `--permission` to Nub today, the preload's first `fs.readFileSync` (or addon `dlopen`) explodes with `ERR_ACCESS_DENIED` before user code runs. **Action:** Nub must either (a) compute a permission-grant supplementation set (`--allow-fs-read=<preload-dir>`, `--allow-fs-read=<cache-dir>`, `--allow-fs-write=<cache-dir>`, `--allow-addons` if our addon loads) or (b) reject `--permission` at the argv layer with a "use `--node` if you need `--permission`" error. We must not silently break it. See [§4.1](#41-permission).

2. **`--watch` × our load hook.** The good news: Node's watcher in modern versions (22.x+) is loader-instrumented, not glob-based, and uses the `WATCH_REPORT_DEPENDENCIES` IPC mechanism we already plan to piggyback on. The not-yet-verified news: we need to confirm that when our load hook returns `{ source, format, shortCircuit: true }` for a `.ts` URL, Node's watcher registers the **`.ts` source path** (not the in-memory transpiled blob, not the cached `.js` file in our content-addressed cache). The `WATCH_REPORT_DEPENDENCIES` channel is the canonical mechanism for hooks to push dep info; our preload needs to emit `process.send({'watch:import': [tsUrl]})` explicitly. Without that, `node --watch script.ts` watches only files Node sees natively — which might be **zero** if the entry itself is a `.ts` file our hook fully owns. See [§4.2](#42-watch).

3. **`--env-file` × eager `.env*` loading.** Two systems loading env files concurrently with different precedence. Node's `--env-file=.env` loads at process boot (after preloads). Nub's loader runs **before spawning Node** and prepends to the child env. If both fire, we get a precedence ordering question that depends on the relative load order of "shell env (Nub's precedence base) vs file (Node's `--env-file`)." Worse: Node's `--env-file` does not do `${VAR}` expansion the same way we do. If a user has `--env-file=.env` in their `NODE_OPTIONS` and Nub also loads `.env`, the values may differ. **Action:** detect user-passed `--env-file` and `--env-file-if-exists` in argv / `NODE_OPTIONS`; if present, **skip Nub's eager loader** for that file path to avoid double-load and let Node's behavior win for the explicitly-named file. Documented behavior, not silent suppression. See [§4.3](#43-env-file).

4. **`--frozen-intrinsics`**. Locks down `Object.prototype`, `Array.prototype`, etc. as frozen. **Our oxc-transpiled output may emit helper code that mutates `Array.prototype` or `Object.prototype`** (rare, but the oxc helper-injection pipeline for some transforms — class fields, async generators — has historically used prototype methods that get patched). More importantly, the JS-land machinery our preload depends on (notably any third-party deps we vendor into the preload) almost certainly does not survive frozen intrinsics. **Working assumption:** Nub is incompatible with `--frozen-intrinsics` in default mode. Either reject at argv layer or document loudly. Vite, Vitest, and most of the JS toolchain ecosystem are also incompatible. See [§4.4](#44-frozen-intrinsics).

5. **`--preserve-symlinks` / `--preserve-symlinks-main` × tsconfig-paths and extensionless probing.** Our resolve hook does `fs.statSync` to check candidate extensions (`.ts`/`.tsx`/`.mts`/`.cts`); we don't currently call `fs.realpath`. If the user passes `--preserve-symlinks`, our hook needs to honor it (don't follow symlinks when resolving the final URL) **and** propagate it correctly to `nextResolve`. `NODE_PRESERVE_SYMLINKS=1` env var is the same — must reach our hook. The risk is subtle: Nub could silently dereference a symlink the user explicitly told Node not to. See [§4.5](#45-preserve-symlinks).

6. **`--conditions` × our resolve hook.** When the user passes `--conditions=dev`, Node propagates that to ESM exports resolution. Our resolve hook receives `context.conditions` and must call `nextResolve(specifier, context)` passing the same conditions through. If we accidentally drop them (e.g., by reconstructing the context object without copying), the user's `--conditions` is silently ignored for any specifier our hook touches. **Pure mechanical**, easy to get right, easy to get subtly wrong. Test coverage essential. See [§4.6](#46-conditions).

7. **`--require` (CJS preload) × our `--import` (ESM preload).** The user passes `--require ./instrument.cjs`; Nub also injects `--import ./nub-preload.mjs`. Order matters because `register()` / `registerHooks()` in our preload only intercept loads that happen **after** registration. If the user's `--require` does anything dynamic-import-y at boot (`@opentelemetry/instrumentation`, `dd-trace`, `newrelic`), those imports run **before** our hook is installed and therefore don't get TS-transpiled or path-aliased. For most APM agents this is fine (they load `.js` from `node_modules`, we skip those anyway). For an APM that triggers eager-loading of user code, it's a latent ordering bug. Document, don't fix. See [§4.7](#47-require-vs-import).

8. **`--inspect-brk` × source maps.** The inspector consumes source maps and presents original sources. Our transpile output embeds inline base64 source maps with `sourcesContent`; Chrome DevTools and `nub inspect` should both surface `.ts` source correctly. Mostly "should work," but **breakpoints set via the inspector before our hook has loaded a file** address the transpiled JS line numbers, not the `.ts` line numbers. The workaround is `--inspect-wait` so the user can set breakpoints after sources are known. See [§4.8](#48-inspect).

Items 9+ (lower priority but worth documenting): `--no-warnings` opt-out edge cases, `--abort-on-uncaught-exception` interaction with hook errors, `--cpu-prof`/`--heap-prof` output dir collision with our temp dir, and `--build-snapshot` (entirely incompatible — we can't be in a startup snapshot because we're an `--import`-loaded preload).

The single highest-priority pre-v0.1 action: **`--permission` auto-grant or explicit rejection.** Everything else is a tractable documentation or test-matrix story.

## 2. Methodology

### Categories

For every flag I assigned one of six interaction categories:

- **A. No interaction.** Flag is orthogonal — affects memory, TLS ciphers, network resolution order, profiling output, etc. Our hooks and resolver don't touch it. Default verdict: passthrough, no action.
- **B. Plays nicely.** Flag works correctly through our hooks / our injection composes well. Example: `--enable-source-maps` is something **we inject by default**, and our transpiler emits source maps — they compose by construction.
- **C. Conflict / overlap.** Flag does something Nub also does. Concrete cases: `--env-file`, `--watch`, `--require` vs our `--import`, `--no-warnings` (we inject; user may want them). Resolution model is per-flag.
- **D. Plausibly broken with our hooks.** Flag relies on Node's vanilla loader behavior that our hook short-circuits. Worry: `--watch`-watched-set, `--trace-require-module`, deprecation warnings the user wants to see for `.ts` files our hook owns.
- **E. Subtly broken.** Flag interacts with our resolver in non-obvious ways. The biggest examples: `--preserve-symlinks`, `--conditions`, `--permission` denying our preload's reads.
- **F. Mooted by our defaults.** Flag is one we already inject or supersede. `--experimental-detect-module` is already default-on in supported Node range; `--experimental-strip-types` is mooted by our hook (per [`../research/node-strip-types-interaction.md`](node-strip-types-interaction.md) pending). Note and move on.

### Sources

- `https://nodejs.org/api/cli.html` accessed 2026-05-18 via WebFetch. Full inventory pulled in one shot.
- `node --help` for cross-reference (Node 22.15, 24.0, 25 current per local installs).
- Node release notes for behavior changes in the supported range (Node 22.15.0 floor per `../runtime/target-version.md`).
- Spot-check of `lib/internal/main/watch_mode.js`, `lib/internal/process/pre_execution.js`, `src/permission/*` via the Node GitHub when behavior wasn't documented.
- Sibling research: [`experimental-flags-unflagging.md`](experimental-flags-unflagging.md) for any flag we already plan to inject.

### Scope boundaries

I covered: every documented `--flag` and `NODE_*` env var in Node's CLI doc.

I did **not** cover in depth:

- V8 raw flags (`--harmony-*`, `--turboshaft`, `--max-old-space-size`, GC pacing). Their interaction with Nub's hooks is uniformly "none" — they affect V8 internals, not the JS module loader. Logged in the table for completeness.
- Windows-specific path semantics. Should be re-audited on Windows CI; flagged where relevant.
- Worker-thread-specific flag inheritance. Workers can override some flags via `Worker` constructor options (`execArgv`); a deeper audit of "do our hooks register inside Worker threads?" deserves its own research write-up.

## 3. Flag-by-flag table

One row per flag or coherent group. Category letters match [§2](#2-methodology). Verdict is one line; details for the non-A entries appear in [§4 Deep-dives](#4-deep-dives) where relevant.

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
| `--enable-source-maps` | B/F | **We inject this by default.** Our hook emits inline base64 source maps. They compose. If the user passes `--no-enable-source-maps` (negation), per `auto-flag-injection.md`'s opt-out rule we honor it and skip injection. |
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
| `--import=` | C | **We inject our own `--import` for the preload.** User-passed `--import` chains; both fire. Order is per Node's `--import`-processing (multiple `--import`s run in argv order). Our preload comes first (it's prepended); user's preload runs after, so our hooks are already installed when user code runs. **This is the desired order.** |
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
| `--no-experimental-require-module` | D | Disables `require(esm)`. **Affects users with CJS code that requires our transpiled-as-ESM `.ts` output.** If our hook returns `format: 'module'` for a `.ts` file required from CJS, that `require()` fails under `--no-experimental-require-module`. The recommended fix is for our format-determination to honor source syntax (which it does per `commonjs-handling.md`) — but in mixed-format projects this is still a footgun. Document. |
| `--no-experimental-sqlite` | C | Disables `node:sqlite`. Interacts with our planned `node:sqlite` unflag — if user opts out, we honor (per the opt-out rule). |
| `--no-experimental-websocket` | C | Disables WebSocket global. Same opt-out rule. |
| `--no-experimental-webstorage` | A | localStorage disable. |
| `--no-extra-info-on-fatal-exception` | A | Diagnostic verbosity. |
| `--no-force-async-hooks-checks` | A | async_hooks tuning. |
| `--no-global-search-paths` | E | Disables `$NODE_PATH` / `$HOME/.node_modules` search. **Our resolver doesn't use these paths; passthrough is correct.** But verify that our resolve hook calling `nextResolve` correctly forwards this constraint. |
| `--no-network-family-autoselection` | A | DNS. |
| `--no-require-module` | D | Same as `--no-experimental-require-module`. |
| `--no-strip-types` | F/E | Disables Node's built-in strip-types. **Per our research, our hook owns `.ts` files, so Node's strip-types is mooted regardless.** If user passes `--no-strip-types` expecting that `.ts` files won't be transpiled, they're wrong — Nub's hook fires anyway. Document loudly. (If user wants Nub off too: `--node`.) |
| `--no-warnings` | B/F | **We inject.** Per `auto-flag-injection.md`, user can opt out via `--no-no-warnings` — but Node doesn't recognize `--no-no-warnings`. The real opt-out is `NODE_OPTIONS=--warnings` (Node also doesn't recognize) or removing our injection via `--node`. **This is a known gap.** Likely action: support a Nub-side `--show-warnings` flag that suppresses our `--no-warnings` injection. |
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
| `--watch-path` | C | Extra watch paths. **Our `nub watch` subcommand uses union semantics with the import graph**; Node's `--watch-path` replaces. If user passes `--watch-path` to `nub node --watch`, they get Node semantics. If to `nub watch`, see `commands/watch.md`. |
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

1. **Preload module read.** Node's `--import <preload>` opens `<preload>` for reading. The path is whatever Nub's CLI passes. Without `--allow-fs-read=<preload-path>`, this fails before our hooks ever register. Per Node docs, the permission check runs *during* the FS call, not as a static analysis step at boot, but the effect is the same: `ERR_ACCESS_DENIED` at the first read.
2. **Source-file reads.** Our load hook reads `.ts`/`.tsx` source files. Each read needs `--allow-fs-read=<source-path>` or `--allow-fs-read=<project-root>` or `--allow-fs-read=*`.
3. **Transpile cache writes.** Our content-addressed cache writes to `~/.cache/nub/transpile/` (or equivalent). Needs `--allow-fs-write=<cache-dir>`.
4. **N-API addon load.** If we ship a `.node` addon for transpile or resolver fast-path, addon loading needs `--allow-addons`.
5. **PATH shim temp dir.** Our hijack creates `/tmp/nub-node-<pid>-<hash>/`. This is created by the Rust CLI *before* Node spawns, so the permission model doesn't gate it. But child-process spawns (PATH-shim re-entry) need `--allow-child-process`.

**Verification status:** I checked the Node permission-model docs (`cli.html#--permission`) and `src/permission/permission.cc` in the Node source for the path-matching semantics. Permissions are checked via `Permission::is_granted(scope, resource)` calls inside the fs binding; the preload-loading path goes through the same. **Needs prototyping to verify** the exact error shape and which read fails first.

**Resolution options:**

1. **Auto-grant supplementation.** When Nub detects `--permission` in user argv or `NODE_OPTIONS`, the Rust CLI computes the minimum grant set Nub needs and prepends: `--allow-fs-read=<preload-dir>`, `--allow-fs-read=<cache-dir>`, `--allow-fs-write=<cache-dir>`, `--allow-addons` (if we use one), `--allow-fs-read=<project-root>` (so our source-file reads work — but this is a *big* grant that effectively defeats the user's permission scoping). Trade-off: convenient, but Nub broadens the permission scope without explicit user consent.
2. **Refuse and direct to compat mode.** Nub detects `--permission`, prints "Nub's runtime augmentation requires broad filesystem access; use `nub --node` or `NODE_COMPAT=1` if you need `--permission` semantics," exits non-zero.
3. **Document a precise grant recipe.** Nub emits a one-shot `nub --explain-permission` command that prints the exact grant flags the user needs to add. Doesn't auto-grant; doesn't refuse; teaches the user. Most honest option.

**Recommendation:** Ship Option 2 in v0.1 (refuse with a helpful message). Promote Option 3 to v0.2. Auto-grant (Option 1) is probably never the right call — it silently defeats the user's security posture.

**Action items:**
- [ ] Verify the exact failure mode in a prototype: Node 24, `--permission`, Nub preload, what error does the user see?
- [ ] Decide between Option 2 and Option 3 for v0.1; document in `auto-flag-injection.md` or a new `runtime/permission-interaction.md` plan doc.
- [ ] Add a CI test that runs `nub script.ts --permission` and asserts the documented behavior.

### 4.2. `--watch`

**Status in Node:** Stable in Node 22+. Loader-instrumented as of 22.0 (the watcher reads dependencies from the resolver, not from filesystem globs).

**The mechanism:**

- `lib/internal/main/watch_mode.js` spawns the user's command as a child with `WATCH_REPORT_DEPENDENCIES=1` set in env, and an IPC pipe on fd 3.
- The child process's ESM/CJS loader, via `lib/internal/modules/cjs/loader.js` and `lib/internal/modules/esm/loader.js`, calls `process._rawDebug` / `process.send` with `{ 'watch:require': [path] }` or `{ 'watch:import': [url] }` for each file actually loaded.
- The parent `FilesWatcher` registers `fs.watch` on each reported path; on change, kills + respawns the child.

**The Nub question:** when our `registerHooks` load hook fires for `./script.ts` and returns `{ source: '<transpiled JS>', format: 'module', shortCircuit: true }`, does Node still emit the `watch:import` event for `./script.ts`?

**Working hypothesis (needs prototyping):** Node emits `watch:import` from the loader **before** invoking the load chain. The URL Node knows about is the resolved URL passed to the loader; that's `file:///abs/path/to/script.ts`. So the watcher should see `script.ts` and watch it, regardless of what our hook returns.

**Where it gets murky:**

- If our **resolve** hook rewrites a specifier (e.g., a tsconfig path alias `@/utils` → `file:///abs/src/utils.ts`), the watcher sees the rewritten URL — which is what we want. Good.
- If our **load** hook synthesizes a URL (rare, but conceivable for an in-memory virtual module), the watcher sees the URL but can't `fs.watch` it because there's no real file. Currently not a concern (we don't synthesize URLs in v0).
- **Transitive imports inside transpiled output** — our transpiled JS contains `import './foo.ts'`. Node's loader resolves and loads `./foo.ts`; our hook fires for it; the watcher sees `./foo.ts` and watches it. **Verify this in CI** because it's the load-bearing case.
- **Files our hook reads that aren't in the import graph** — e.g., the `tsconfig.json` we read to compute path aliases. The watcher won't see this; if the user edits `tsconfig.json`, no restart. **Bug.** Fix: our preload emits an explicit `process.send({'watch:require': ['/abs/tsconfig.json']})` when it loads the tsconfig, piggybacking on the same IPC channel.

**Documented in `commands/watch.md`** as the v0.1 design — our `nub watch` subcommand depends on this mechanism working. The plan there says "the child process emits `process.send({'watch:require': [path]})` over fd 3 and Node's existing FilesWatcher registers those paths." This deep-dive confirms the plan is mechanically sound but flags two needs-verification corners:

- [ ] **CI test:** `nub watch script.ts`, edit `script.ts`, confirm restart fires.
- [ ] **CI test:** `nub watch script.ts`, edit `tsconfig.json`, confirm restart fires (after we wire the explicit `watch:require` emission for tsconfig).
- [ ] **CI test:** `nub watch script.ts`, edit `./utils.ts` (transitively imported), confirm restart fires.

### 4.3. `--env-file`

**Status in Node:** `--env-file=<path>` since Node 20.6; `--env-file-if-exists=<path>` since 21.7 / 22.x. Both load env vars from a `.env`-format file at process boot, *after* preloads have registered but *before* user code runs.

**What Nub does:** `runtime/env-loading.md`. Nub's CLI reads `.env*` files **before** spawning Node and prepends the parsed values to the child env. Disabled under `--node` / `NODE_COMPAT=1`. Does workspace-aware discovery and `${VAR}` expansion.

**The interaction:**

If user has neither `--env-file` flag nor `nub --node`, Nub loads `.env*` and user's code sees those values. Node's `--env-file` is not involved. ✓

If user passes `--env-file=.env.custom`, the question becomes: "does Nub *also* load `.env*` from the workspace?"

- Option A: Nub's loader fires regardless; Node's `--env-file` runs *after* the child spawns; whichever value sticks depends on whether `--env-file` overrides existing env. Per Node docs: `--env-file` does **not** override existing env vars (shell env wins, per the docs). So if Nub pre-loaded `FOO=bar` and Node's `--env-file` says `FOO=baz`, the final value is `bar` (Nub's value, because it's in the env when Node starts, so it counts as "existing").
- Option B: Nub detects user-passed `--env-file` and disables its own eager loader. Predictable, surprising for users who also want Nub's workspace `.env*` files.
- Option C: Nub detects user-passed `--env-file=<path>` and skips *that specific path* in its eager loader; still loads other `.env*` files in the workspace. Behavior closest to "compose them," minimum surprise.

**Expansion mismatch:** Nub does `${VAR}` and `$VAR` expansion with shell-env-first ordering, per [`env-expansion-and-test-skip.md`](env-expansion-and-test-skip.md). Node's `--env-file` does **not** do `${VAR}` expansion at all (per Node docs, last verified 2026-05-18; this may evolve). So a `.env` file with `PASSWORD=${SECRET}` loaded via:
- Nub → expanded to value of `SECRET` from shell env, or empty.
- Node's `--env-file` → literal string `${SECRET}`.

If both load the same file, the value the user sees depends on which loader wrote last, and on whether Node's `--env-file` overrides existing (per docs: it doesn't, so Nub's expanded value wins).

**Recommendation:** Option C — Nub's eager loader skips paths the user explicitly passed via `--env-file` / `--env-file-if-exists`. Other workspace `.env*` files still load via Nub.

**Action items:**
- [ ] Implement the skip-detection in the Rust CLI's `--env-file` argv parser.
- [ ] Document the interaction in `env-loading.md`.
- [ ] CI test: `nub script.ts --env-file=.env.custom`, with a workspace `.env` and a custom `.env.custom`, both loaded.

### 4.4. `--frozen-intrinsics`

**Status in Node:** Experimental since 10.x. Freezes `Object.prototype`, `Array.prototype`, etc. so user code can't mutate them. Audience: lockdown environments (SES-style, secure mashups).

**Why it's incompatible with our preload (working assumption):**

Our preload module imports from `node:module`, sets up `module.registerHooks()`, and likely uses `JSON.parse` / `JSON.stringify` / `Map` / `Set` / array spreads. All of these work under `--frozen-intrinsics` as long as we don't mutate prototypes. The transpile-cache machinery and tsconfig-path machinery may use prototype methods more aggressively.

**The bigger problem is third-party deps we vendor.** If our preload imports anything (oxc bindings via NAPI shim, vendored JSON-schema validators, etc.), each of those is a potential prototype-mutation site. SES surveys have shown that ~30% of popular npm packages don't survive frozen intrinsics.

**Transpiled output:** oxc's emit is generally well-behaved (no prototype mutation in stripped TS), but specific transforms (e.g., async generators in older targets, decorators) may emit helpers that touch prototypes. Needs verification per transform.

**Recommendation:** Document Nub as incompatible with `--frozen-intrinsics` in v0. Users who need frozen intrinsics should use `--node` (which disables our preload, leaving them on vanilla Node + their own SES setup).

**Action items:**
- [ ] CI test: `nub script.ts --frozen-intrinsics` — assert the failure mode is clear, not a cryptic stack trace.
- [ ] Document in `additivity.md` or a new `runtime/frozen-intrinsics-decision.md`.

### 4.5. `--preserve-symlinks`

**Status in Node:** Stable. `--preserve-symlinks` and `--preserve-symlinks-main` instruct the module loader to **not** call `realpath` on resolved module URLs, so the same physical file linked from two locations appears as two different modules (important for `npm link` development workflows where you want the linked package to be evaluated in its linked location's context, not its real-disk context).

**`NODE_PRESERVE_SYMLINKS=1`** is the env-var form.

**What our resolve hook does:**

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

- `fs.statSync` follows symlinks by default. Under `--preserve-symlinks`, we should be calling `fs.lstatSync` (or similar) to check existence without dereferencing.
- The URL we pass to `nextResolve` should be the **link path**, not the realpath. As long as we don't call `realpath` ourselves, this is automatic.
- `nextResolve` honors `--preserve-symlinks` internally — we just need to not pre-empt it by passing a realpath'd URL.

**Verification:** I read `lib/internal/modules/esm/resolve.js#finalizeResolution` — realpath happens inside `finalizeResolution`, called by `moduleResolve`. If our resolve hook passes a path to `nextResolve`, the eventual `finalizeResolution` call honors `--preserve-symlinks`. **As long as our hook doesn't call realpath, we're fine.**

**Action items:**
- [ ] Audit our resolve hook implementation; ensure no `fs.realpath` calls.
- [ ] CI test: symlink fixture, `--preserve-symlinks`, confirm resolution path matches expected.
- [ ] Document the contract in `tsconfig-paths.md` and `extensionless-probing.md`.

### 4.6. `--conditions`

**Status in Node:** `-C, --conditions=<name>` (Node 14.9+) adds custom export conditions for package.json `exports` resolution. Multi-use: `-C dev -C testing` adds both. Node honors them in addition to the built-in conditions (`node`, `import`, `require`, `default`).

**What our resolve hook does:** when our hook calls `nextResolve(specifier, context)`, Node's default resolver consults `context.conditions` to pick the right export. Per Node's hook API contract, `context.conditions` is populated by Node before calling the hook chain.

**The risk:**

- If our hook reconstructs the context object (rather than passing it through), and forgets to copy `conditions`, the default resolver sees `undefined` and falls back to the built-in conditions only. User's `--conditions=dev` is silently dropped.
- If our hook **adds** conditions (e.g., we want to set a `'nub-runtime'` condition for our own use), we must merge with user-set conditions, not replace.

**Recommendation:** never reconstruct the context object; always pass it through. If we need to add conditions, mutate a copy:

```js
resolve(specifier, context, nextResolve) {
  const newContext = {
    ...context,
    conditions: [...(context.conditions ?? []), 'nub-runtime'],
  };
  return nextResolve(specifier, newContext);
}
```

**Open question (for `additivity.md` or a new doc):** do we add a `'nub-runtime'` condition? If yes, packages can ship Nub-specific exports (e.g., a Nub-optimized build of a polyfill). If no, simpler / less surface. **Lean: no for v0**, revisit if a package author asks.

**Action items:**
- [ ] CI test: package with `exports: { 'dev': './dev.js', 'default': './prod.js' }`, run with `nub --conditions=dev script.ts`, verify `dev.js` is loaded.
- [ ] Audit the hook code path to confirm `conditions` is passed through.

### 4.7. `--require` vs `--import`

**Status in Node:**
- `-r, --require <module>` preloads a CJS module before user code. Module is loaded synchronously via Node's CJS loader. Predates `register()` / `--import`. Multi-use OK.
- `--import <module>` preloads an ESM module before user code. Multi-use OK. Runs sync at boot.

**What Nub does:** prepends `--import <our-preload>` so our `registerHooks` registration runs first.

**The interactions:**

1. **User passes `--require ./instrument.cjs`.** Order in argv (after our prepend): `--import <nub> --require ./instrument.cjs <user-script>`. Per Node's boot order: CJS `--require` modules load *before* ESM `--import` modules. So `./instrument.cjs` runs **before** our hook registration. If `./instrument.cjs` does APM monkey-patching of `Module.prototype.require`, that patching is in place when our preload loads. We then call `registerHooks()`; APM doesn't see this as a require (it's an ESM import inside our preload). For most APMs this is fine because they care about user-code patterns; our preload code is opaque to them.

**Subtle issue:** if APM does eager-load of user code (some `dd-trace` configurations preload entry-point modules), the APM-loaded user code goes through CJS-loader without our hooks. Files loaded that way miss TS transpilation. Documented edge case; affects ~zero users in practice (APMs typically only patch require-paths, don't preload user code).

2. **User passes `--require ./tsx-something.cjs`.** If the user is on tsx and also Nub (transitional state), both hooks register. Behavior is per-Node's hook-chaining model: hooks run in registration order. Both would fire on `.ts` files; whichever returns first with `shortCircuit: true` wins. Likely tsx's hook fires first (because `--require` runs before `--import`) and we get tsx's transpile. **Not a Nub bug**; document as "don't run two TS loaders simultaneously."

3. **User passes `--import ./their-preload.mjs`.** Order in argv: `--import <nub> --import ./their-preload.mjs`. Both run; ours first, theirs second. Their preload sees a runtime where our hooks are already registered. They can call `registerHooks` themselves; chains correctly. ✓

**Action items:**
- [ ] CI test: `--require ./instrument.cjs` + `nub script.ts`, confirm script runs without surprises.
- [ ] CI test: two `--import` preloads, confirm hook ordering.
- [ ] Document order semantics in a new `runtime/preload-ordering.md` or as a section in `ts-transpilation.md`.

### 4.8. `--inspect` / `--inspect-brk` / `--inspect-wait`

**Status in Node:** Stable. `--inspect` activates the V8 inspector on a port; `--inspect-brk` does the same and breaks at the start of user code; `--inspect-wait` waits for a debugger to attach before running.

**What Nub does:** emits source maps in transpile output (per `source-maps.md`). Injects `--enable-source-maps` by default. Has its own `nub inspect` subcommand (per `commands/inspect.md`) that wraps `--inspect-brk` with TS-aware UX.

**The risk:**

- **Breakpoints set before our hook fires.** If the user uses `--inspect-brk` and sets a breakpoint in `script.ts` at line 10 *before* our hook has loaded the file, the breakpoint resolves against the URL `file:///abs/script.ts` line 10. When our hook fires and returns transpiled JS, line numbers are preserved by source-map round-trip — Chrome DevTools and `nub inspect` should respect the source map and apply the breakpoint to the original `.ts` line 10. **Should work**, but edge cases exist:
  - Source-map URL resolution differs across inspector clients (Chrome DevTools, VSCode, IntelliJ). Inline base64 maps are most portable; we emit those.
  - `sourcesContent` must be embedded so DevTools can display the original source even after the file is transpiled. We embed by default.
- **Inspector protocol's `Debugger.scriptParsed` event.** Fires with the transpiled source's URL. Chrome maps to original via source map; some custom inspector clients don't. Not our problem; document.

**Recommendation:** `nub inspect` should default to `--inspect-wait` (not `--inspect-brk`), so the debugger attaches *before* user code starts but *after* our preload has registered the hooks. This way, when the user sets breakpoints in DevTools, they target the URL the inspector knows about — which is the `.ts` source URL via source maps — and the breakpoint takes effect on the first hit of that line.

**Action items:**
- [ ] Verify the `--inspect-brk` vs `--inspect-wait` default for `nub inspect`; document in `commands/inspect.md`.
- [ ] CI test: `nub inspect script.ts`, attach Chrome DevTools, set breakpoint in `.ts`, hit it, verify source displays.

### 4.9. `--no-warnings` opt-out gap

**The gap:** Nub injects `--no-warnings`. There's no `--no-no-warnings` flag in Node. The user who wants warnings back on under Nub has these options:

1. Use `--node` / `NODE_COMPAT=1` (turns off **all** of Nub's augmentation, including TS transpilation).
2. Use Nub's CLI to pass a hypothetical `--show-warnings` flag that suppresses our injection (doesn't exist yet).

Per `auto-flag-injection.md`'s "User opt-out: `--no-experimental-*` and friends" section, the opt-out mechanism is a Rust-side subtract: if user passes `--no-X` in argv or `NODE_OPTIONS`, Nub doesn't inject `--X`. But `--no-warnings` is **already** the negation; there's no positive form to set.

**Recommendation:** add a Nub-side `--show-warnings` flag (or similar) that suppresses our `--no-warnings` injection. Simple, no Node-side support needed.

**Action items:**
- [ ] Add `--show-warnings` to Nub's CLI surface.
- [ ] Document in `auto-flag-injection.md`.

## 5. The compose-with-hijack test matrix

The combinatorics that need test coverage:

- Auto-inject set varies by detected Node version.
- `NODE_OPTIONS` env var is parsed *before* argv per Node docs.
- User argv flags (passed to `nub`).
- Compat mode env var (`NODE_COMPAT=1`).
- Per-Node-version differences in flag recognition.
- Process-tree depth via PATH hijack.

The full Cartesian space is too big to test exhaustively (5+ dimensions, each with 2–5 values). Below are 10 scenarios that exercise the integration points; each represents a class of real-world configurations.

### Scenario 1: vanilla path

**Setup:** Node 22.15 installed. User runs `nub script.ts`. No `NODE_OPTIONS`, no flags.

**Expected:** Nub detects Node 22.15, injects `--no-warnings --enable-source-maps`, prepends `--import <preload>`, sets up PATH shim, spawns Node. Script runs, TS transpiled.

**What it tests:** baseline integration.

### Scenario 2: NODE_OPTIONS opt-out of injected flag

**Setup:** `NODE_OPTIONS=--no-experimental-vm-modules`. User runs `nub script.ts`. Nub would inject `--experimental-vm-modules` (per the planned unflag).

**Expected:** Nub's argv parser sees the user's `--no-` opt-out in `NODE_OPTIONS`, subtracts `--experimental-vm-modules` from the inject set, spawns Node with only `--no-warnings --enable-source-maps --import <preload>`. Node's own `NODE_OPTIONS` parsing applies `--no-experimental-vm-modules`. Net result: vm-modules disabled, user gets what they asked for.

**What it tests:** the [§4 deep-dive](#4-deep-dives) `NODE_OPTIONS` precedence handling per the `auto-flag-injection.md` contract.

### Scenario 3: user `--import` chains with Nub's

**Setup:** `nub --import ./my-preload.mjs script.ts`.

**Expected:** Nub's CLI passes user's `--import` through. Final Node argv: `--import <nub-preload> --import ./my-preload.mjs script.ts`. Nub's hooks register first; user's preload runs after with hooks already active. Both `--import`s fire; user's preload sees Nub's hook chain.

**What it tests:** preload chaining, no clobbering.

### Scenario 4: hijack-through-spawn

**Setup:** `nub run dev` where the `dev` script in package.json is `"node ./server.ts"`. Real Node exists at `/usr/local/bin/node`.

**Expected:** Nub's PATH shim prepends `/tmp/nub-node-<pid>-<hash>/` which contains `node` → Nub. When the script does `node ./server.ts`, the PATH resolves to the shim → Nub runs as `node` → Nub-node dispatch path → spawns the real Node with Nub's augmentation. `./server.ts` transpiles and runs.

**What it tests:** PATH hijack, argv0 dispatch, depth-2 process tree, augmentation propagation.

### Scenario 5: hijack-through-spawn with compat mode

**Setup:** `nub --node script.ts`. The script does `child_process.spawn('node', ['./worker.ts'])`.

**Expected:** Nub is in compat mode for the root; PATH shim is **not** set up. `child_process.spawn('node', ...)` resolves to the user's real Node (not Nub). Real Node tries to run `./worker.ts` — fails (no TS support on plain Node). User sees a normal Node error.

This is the **correct** behavior for compat mode: the user opted out of Nub, the child also runs un-augmented.

**What it tests:** compat mode disables hijack; `NODE_COMPAT=1` propagates correctly.

### Scenario 6: hijack with NODE_COMPAT in child shell

**Setup:** `nub run dev` where the `dev` script is `"NODE_COMPAT=1 node ./server.ts"`. (User explicitly sets compat for a child.)

**Expected:** Nub's PATH shim is set up by the parent (root `nub run`). `node` resolves to Nub → Nub starts → sees `NODE_COMPAT=1` in its env → runs in compat mode for the child → spawns real Node without augmentation. `./server.ts` runs on real Node → fails (no TS).

**What it tests:** `NODE_COMPAT` is consulted at each Nub entry, not just root.

### Scenario 7: --permission interaction

**Setup:** `nub --permission script.ts`.

**Expected (per [§4.1 recommendation](#41-permission)):** Nub prints "use `--node` if you need `--permission`", exits non-zero.

**Alternative if we implement auto-grant:** Nub computes minimum grants, prepends `--allow-fs-read=<preload-dir> --allow-fs-read=<cache-dir> --allow-fs-write=<cache-dir> --allow-fs-read=<cwd>`, spawns; script runs with permissions restricted (mostly) per user's intent.

**What it tests:** the permission-model interaction, whichever path we choose.

### Scenario 8: --watch with TS entry

**Setup:** `nub script.ts --watch` (or `nub watch script.ts`). `script.ts` imports `./utils.ts`. Both files exist; `tsconfig.json` in workspace root.

**Expected:** Initial run transpiles both `.ts` files. Watcher registers `script.ts`, `utils.ts`, **and** `tsconfig.json` (the last via explicit `process.send({'watch:require': ['/abs/tsconfig.json']})` in our preload). Edit any of the three → restart fires.

**What it tests:** [§4.2 watch interaction](#42-watch), specifically the `tsconfig.json` watch piggyback.

### Scenario 9: NODE_OPTIONS adds env-file, Nub also discovers .env

**Setup:** `NODE_OPTIONS=--env-file=.env.custom`. Workspace has `.env` (Nub's discovery) and `.env.custom` (user-specified). Per [§4.3 recommendation](#43-env-file), Nub should skip `.env.custom` from its own discovery to avoid double-load.

**Expected:** Nub loads `.env` only. Node's `NODE_OPTIONS=--env-file=.env.custom` loads `.env.custom`. Final env: shell > .env > .env.custom (per Node's "don't override existing").

**What it tests:** double-load detection, env precedence correctness.

### Scenario 10: Worker thread with execArgv

**Setup:** `nub script.ts`. `script.ts` does:
```ts
import { Worker } from 'node:worker_threads';
const w = new Worker('./worker.ts');
```

**Expected (the open question):** does the worker thread also get our preload? Per Node docs, workers inherit `execArgv` from the parent process by default. Our preload's `--import` was injected via Nub's spawn; it's in the parent's `process.execArgv`. So the worker should inherit it. The worker's `./worker.ts` is a `.ts` file — without our preload it would fail. With our preload, it transpiles and runs.

**Edge case:** if the user passes `execArgv: []` explicitly in the Worker constructor, the worker spawns *without* our preload. `./worker.ts` fails. This is "user explicitly opted out of inheritance" and is correct behavior, though surprising.

**What it tests:** worker_threads `execArgv` inheritance, deepest practical process-tree depth for our augmentation.

### Coverage summary

These 10 scenarios cover:

- Baseline (S1)
- `NODE_OPTIONS` opt-out merging (S2)
- `--import` chaining (S3)
- Hijack depth 2 (S4)
- Compat mode disables hijack (S5)
- `NODE_COMPAT` mid-tree (S6)
- Permission model (S7)
- Watch + TS + tsconfig (S8)
- Env-file double-load avoidance (S9)
- Worker `execArgv` inheritance (S10)

Each should be a CI test. The matrix is small enough to maintain and large enough to catch the integration bugs the reviewer was worried about.

## 6. Action items per flag (consolidated)

### Pre-v0.1 (blocking)

- [ ] **`--permission`:** Implement rejection-with-helpful-message (or auto-grant, per [§4.1](#41-permission) decision). Add CI test S7. Document in a new `runtime/permission-interaction.md` plan doc.
- [ ] **`--watch`:** Wire explicit `process.send({'watch:require': [path]})` in our preload for files we read that aren't in Node's import graph (notably `tsconfig.json`). Add CI tests S8 and the per-file tests in [§4.2](#42-watch). Document the mechanism in `commands/watch.md` and `runtime/tsconfig-paths.md`.
- [ ] **`--env-file` / `--env-file-if-exists`:** Implement user-passed `--env-file` detection in argv parser; skip those paths in Nub's eager loader. Add CI test S9. Update `runtime/env-loading.md`.
- [ ] **`--no-warnings`:** Add `--show-warnings` CLI flag that suppresses our injection. Document in `auto-flag-injection.md`.
- [ ] **`--conditions`:** Audit resolve hook implementation; ensure context-pass-through preserves `conditions`. Add CI test for the conditions scenario.
- [ ] **`--preserve-symlinks`:** Audit resolve hook for any `realpath` calls; remove or guard. Add CI test with symlink fixture.
- [ ] **`--frozen-intrinsics`:** Add a CI test that asserts the failure mode is clear (or, if we make it work, that it works). Document compat-mode-recommendation in `additivity.md` or a new decision doc.

### v0.1 / pre-stable (high priority)

- [ ] **`--inspect-wait` default for `nub inspect`:** Verify and document in `commands/inspect.md`.
- [ ] **`--no-addons` interaction:** If we ship an addon, document the fallback. If we don't (JS-only preload), no-op.
- [ ] **`--disallow-code-generation-from-strings`:** Audit our preload and oxc output for `eval` / `new Function`. If clean, no action; if not, document.
- [ ] **`--disable-proto`:** Audit for `__proto__` usage. Should be clean; verify.
- [ ] **`--build-snapshot` / `--snapshot-blob`:** Document incompatibility; recommend compat mode.
- [ ] **`--experimental-loader=`:** Document chaining behavior with our `registerHooks`; mention deprecation status.
- [ ] **`--require` ordering:** Document [§4.7](#47-require-vs-import) in a new `runtime/preload-ordering.md` or section.
- [ ] **`--test-isolation=worker`:** Verify worker `execArgv` inheritance keeps our preload registered in worker threads. CI test S10. Likely a separate research doc on worker-thread augmentation if it gets thorny.

### Post-v0.1 (document only)

- [ ] **`--trace-sync-io`:** Document that our sync `registerHooks` load hook fires this constantly; recommend not using it with Nub.
- [ ] **`--trace-env`:** Document that Nub's preload reads `process.env` (cache-dir, hijack detection).
- [ ] **`--redirect-warnings`:** Document that our `--no-warnings` suppresses everything, so the redirect file ends up empty.
- [ ] **`--experimental-test-module-mocks`:** Prototype the interaction with our `registerHooks`. If it works, document; if not, document the limitation.
- [ ] **`--run` (Node's npm-script runner):** Document precedence vs `nub run`.

## 7. Open questions

1. **Permission model auto-grant vs reject (§4.1).** Recommendation is "reject with helpful message" for v0.1, but auto-grant could be implemented later. Left as an open product decision.
2. **`--watch` reports for non-import files.** Working hypothesis is that our preload emits `watch:require` for tsconfig and other indirect-dep files. **Needs prototyping** to confirm Node's `WATCH_REPORT_DEPENDENCIES` IPC accepts these reports from an arbitrary preload (vs only from the loader internals).
3. **Worker-thread preload inheritance (§5 S10).** Default `execArgv` inheritance should propagate our `--import`, but needs verification across Node 22.15 / 24 / 25. Especially important if `--test-isolation=worker` becomes a common pattern.
4. **`--frozen-intrinsics` compatibility floor.** Could we audit + adapt our preload to work under frozen intrinsics? Probably yes for our own code; not for arbitrary user deps. Decision: don't try; document incompat.
5. **`--experimental-test-module-mocks` × our hooks.** Both register loader hooks. Hook chaining order TBD; needs prototyping with a real `node:test` fixture.
6. **`'nub-runtime'` package.json export condition.** Should `--conditions` injection include a `'nub-runtime'` so package authors can ship Nub-specific code? Lean no for v0; revisit if asked.
7. **`--inspect-brk` vs `--inspect-wait` default for `nub inspect`.** Working recommendation is `--inspect-wait` so DevTools attach *after* our hooks register but *before* user code starts. Verify in practice.
8. **`--allow-fs-read=<cwd>` scope under auto-grant.** If we go that route for `--permission`, the cwd grant is broad and defeats some of the user's security posture. Could we narrow to `<entry-file-dir>` or `<resolved-import-graph>`? Probably not without prohibitive complexity. Trade-off discussion needed.
9. **`--build-snapshot` + compat-mode fallback UX.** If user passes `--build-snapshot`, should Nub auto-fall-back to compat mode silently, or refuse loudly? Refuse-loudly is safer (snapshots are unusual; user should know why their TS isn't bundled).
10. **Per-Node-version behavior drift.** Node 22.15, 24, 25, 26 all have subtly different flag-recognition behavior. We don't currently test against multiple Node versions in CI. Should we? Probably yes; cost is non-trivial.

## 8. Suggested follow-on plan docs

Based on this audit, the following per-feature/per-decision docs should be created:

1. **`runtime/permission-interaction.md`** — Decision record + mechanism for `--permission`. High priority.
2. **`runtime/preload-ordering.md`** — How `--require`, `--import`, and our injection compose. Useful reference; not blocking.
3. **`runtime/frozen-intrinsics-decision.md`** — Decision to document Nub as incompatible with `--frozen-intrinsics`; compat-mode is the escape. Small doc.
4. **`runtime/worker-thread-augmentation.md`** — Worker thread `execArgv` inheritance, how our preload propagates, what to do when user passes `execArgv: []`. Possibly a research doc first.
5. **`runtime/snapshot-incompatibility.md`** — Why `--build-snapshot` doesn't work with our preload model; compat-mode recommendation; possible future "Nub snapshot" feature (separate from Node's).

Plus updates to existing docs as called out in [§6 action items](#6-action-items-per-flag-consolidated).

## 9. Sources

- [`nodejs.org/api/cli.html`](https://nodejs.org/api/cli.html) — canonical flag list, accessed 2026-05-18 via WebFetch.
- [`nodejs.org/api/permissions.html`](https://nodejs.org/api/permissions.html) — permission model documentation.
- [`nodejs.org/api/module.html#moduleregisterhooksoptions`](https://nodejs.org/api/module.html) — `registerHooks()` API contract, including `context.conditions` propagation.
- [`nodejs.org/api/cli.html#--watch`](https://nodejs.org/api/cli.html) — watch mode docs.
- Node source spot-checks: `lib/internal/main/watch_mode.js`, `lib/internal/process/pre_execution.js`, `src/permission/*`, `lib/internal/modules/esm/resolve.js#finalizeResolution`.
- Sibling research: [`experimental-flags-unflagging.md`](experimental-flags-unflagging.md), [`env-expansion-and-test-skip.md`](env-expansion-and-test-skip.md), [`env-file-loading.md`](env-file-loading.md), `watch-mode-scope-thesis.md`, `watch-mode.md`.
- Nub plan docs: as cited in section 1.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
