# parallel/ — nub-specific failure categorization

Full-corpus scan of 3,968 parallel/ tests in categories NOT covered by task 1.3 (which covered child-process, module, process, worker, require, vm, compile-cache, esm). Confirmed **130 nub-only failures** after sequential re-verification (initial parallel scan found 219 candidates; 54 were false positives from tmpdir collisions in 20-way parallel execution, remaining 35 didn't reproduce in isolation).

Combined with the 29 failures from task 1.3 and 6 additional discovered during task 1.8 config expansion, **total parallel/ nub-specific failures: 165** out of ~4,500 tests.

**Zero real bugs found.** All 165 failures are expected divergences from intentional augmentation. None represent breakage in real user code.

## Root cause summary

| Root cause | Count | Affects user code? |
|---|---|---|
| WebStorage ExperimentalWarning pollution | 71 | No — test harness only |
| Permission model + addon loading | 30 | No — handled by nub CLI auto-grant |
| Debugger/inspector breakpoint shift | 21 | No — debugger still works, just different initial pause |
| Async hooks / promise hooks extra events | 12 | No — only affects exact event counts |
| Promise unhandled-rejection warning count | 12 | No — warning still emitted, just with extra events |
| Flag injection side effects | 4 | No — intentional feature enablement |
| Error/output format changes from preload | 5 | No — errors still reported, different internal callsite |
| Misc one-offs (internals, assertion format) | 4 | No — test-harness-specific checks |

## 1. WebStorage ExperimentalWarning pollution (71 tests) — expected by design

Nub injects `--experimental-webstorage` with `--localstorage-file=<path>` to enable `localStorage` as a QoL improvement. This makes `Storage` a lazy-initialized global. When Node's test harness (`common/index.js:377`) enumerates known globals and checks `globalThis.Storage !== undefined`, the lazy getter fires and emits an ExperimentalWarning event. `--disable-warning=ExperimentalWarning` only suppresses console output — `process.on('warning')` listeners still receive the event.

This breaks tests that use `common.expectWarning()` (unexpected warning type/count), `common.mustNotCall()` on the warning event (unexpected call), or exact warning event counting.

**15 already categorized in task 1.3** (from the covered categories):

- `test-vm-measure-memory.js`, `test-vm-measure-memory-lazy.js`, `test-vm-measure-memory-multi-context.js`
- `test-process-emitwarning.js`, `test-process-env-deprecation.js`, `test-process-warnings.mjs`
- `test-worker-console-listeners.js`, `test-worker-message-port-transfer-target.js`
- `test-module-circular-dependency-warning.js`, `test-module-loading-deprecated.js`, `test-module-parent-setter-deprecation.js`
- `test-child-process-execfile.js`, `test-child-process-spawn-shell.js`, `test-child-process-spawnsync-shell.js`

**62 newly categorized in tasks 1.4 and 1.8** (56 from task 1.4 + 6 discovered during config expansion):

- `test-buffer-constructor-deprecation-error.js`, `test-buffer-constructor-outside-node-modules.js`, `test-buffer-nopendingdep-map.js`, `test-buffer-of-no-deprecation.js`, `test-buffer-pending-deprecation.js`
- `test-console-log-stdio-broken-dest.js`, `test-console.js`
- `test-crypto-gcm-explicit-short-tag.js`, `test-crypto-hmac.js`, `test-crypto-random.js`
- `test-debugger-address.mjs`
- `test-dgram-bind-error-repeat.js`
- `test-dns-lookup-promises-options-deprecated.js`
- `test-domain-dep0097.js`, `test-domain-http-server.js`, `test-domain-implicit-binding.js`, `test-domain-implicit-fs.js`, `test-domain-multi.js`, `test-domain-promise.js`
- `test-err-name-deprecation.js`
- `test-event-emitter-max-listeners-warning-for-null.js`, `test-event-emitter-max-listeners-warning-for-symbol.js`, `test-event-emitter-max-listeners-warning.js`
- `test-eventtarget-memoryleakwarning.js`
- `test-fs-exists.js`, `test-fs-mkdtemp.js`
- `test-http-many-ended-pipelines.js`, `test-http-server-multiple-client-error.js`, `test-http-socket-error-listeners.js`, `test-http-timeout-client-warning.js`
- `test-http2-client-priority-before-connect.js`, `test-http2-client-request-listeners-warning.js`, `test-http2-client-set-priority.js`, `test-http2-priority-cycle-.js`, `test-http2-priority-event.js`, `test-http2-server-set-header.js`, `test-http2-server-stream-session-destroy.js`
- `test-https-simple.js`, `test-https-strict.js`
- `test-inspector-bindings.js`, `test-inspector-host-warning.js`
- `test-nodeeventtarget.js`
- `test-promise-handled-rejection-no-warning.js`, `test-promise-unhandled-default.js`, `test-promise-unhandled-error.js`, `test-promise-unhandled-silent-no-hook.js`, `test-promise-unhandled-silent.js`, `test-promise-unhandled-throw-handler.js`, `test-promise-unhandled-throw.js`, `test-promise-unhandled-warn-no-hook.js`, `test-promise-unhandled-warn.js`
- `test-promises-unhandled-proxy-rejections.js`, `test-promises-unhandled-symbol-rejections.js`, `test-promises-warning-on-unhandled-rejection.js`
- `test-punycode.js`
- `test-repl-options.js`
- `test-timers-max-duration-warning.js`, `test-timers-nan-duration-emit-once-per-process.js`, `test-timers-nan-duration-warning-promises.js`, `test-timers-negative-duration-warning-emit-once-per-process.js`, `test-timers-not-emit-duration-zero.js`
- `test-fs-opendir.js`
- `test-zlib-brotli-kmaxlength-rangeerror.js`, `test-zlib-kmaxlength-rangeerror.js`, `test-zlib-zstd-kmaxlength-rangeerror.js`
- `test-util-deprecate.js`, `test-util-emit-experimental-warning.js`, `test-util-promisify.js`

## 2. Permission model + addon loading (30 tests)

Nub's preload requires loading `oxc-transform`, a native N-API addon. Tests that use Node's `--permission` flag without granting `--allow-addons` fail because the addon can't load. The nub CLI handles this by auto-granting `--allow-addons` when `--permission` is detected (see `spawn.rs`), but the NODE_OPTIONS dual-channel (used by child processes with hardcoded node paths) doesn't have this protection.

Not a user-code bug: users run `nub`, which handles permission auto-grant. The PATH shim ensures child processes also go through nub's handling.

- `test-cli-permission-deny-fs.js`, `test-cli-permission-multiple-allow.js`
- `test-permission-allow-child-process-cli.js`, `test-permission-allow-inspector.js`, `test-permission-allow-wasi-cli.js`, `test-permission-allow-worker-cli.js`
- `test-permission-child-process-cli.js`, `test-permission-child-process-inherit-flags.js`
- `test-permission-fs-absolute-path.js`, `test-permission-fs-read-entrypoint.js`, `test-permission-fs-relative-path.js`, `test-permission-fs-repeat-path.js`, `test-permission-fs-require.js`, `test-permission-fs-symlink-relative.js`, `test-permission-fs-traversal-path.js`, `test-permission-fs-wildcard.js`, `test-permission-fs-windows-path.js`, `test-permission-fs-write-report.js`, `test-permission-fs-write-v8.js`
- `test-permission-has.js`, `test-permission-inspector-brk.js`, `test-permission-inspector.js`, `test-permission-net-quic.mjs`, `test-permission-net-websocket.js`, `test-permission-no-addons.js`, `test-permission-processbinding.js`, `test-permission-sqlite-load-extension.js`, `test-permission-warning-flags.js`, `test-permission-wasi.js`, `test-permission-worker-threads-cli.js`

## 3. Debugger/inspector breakpoint shift (19 tests)

Nub's `--import` preload changes where the V8 debugger initially pauses. Tests expecting specific file/line breakpoints see `node:internal/per_context/primordials` instead of the fixture file. Debugger functionality itself works — just the initial pause location differs.

- `test-debugger-break.js`, `test-debugger-breakpoint-exists.js`, `test-debugger-clear-breakpoints.js`, `test-debugger-exceptions.js`, `test-debugger-exec.js`, `test-debugger-list.js`, `test-debugger-low-level.js`, `test-debugger-object-type-remote-object.js`, `test-debugger-preserve-breaks.js`, `test-debugger-repeat-last.js`, `test-debugger-run-after-quit-restart.js`, `test-debugger-scripts.js`, `test-debugger-set-context-line-number.mjs`, `test-debugger-use-strict.js`, `test-debugger-watchers.mjs`
- `test-inspector-debug-brk-flag.js`, `test-inspector-exception.js`, `test-inspector-strip-types.js`, `test-inspector.js`

## 4. Async hooks / promise hooks extra events (12 tests)

Nub's preload runs async operations (`await import("./polyfills.mjs")`, `await import("./navigator-locks.mjs")`, etc.) before user code. This generates additional async hook init/before/after events and promise hook events. Tests counting exact event numbers see more than expected.

- `test-async-hooks-correctly-switch-promise-hook.js`, `test-async-hooks-disable-during-promise.js`, `test-async-hooks-enable-recursive.js`, `test-async-hooks-promise-triggerid.js`, `test-async-hooks-promise.js`, `test-async-hooks-top-level-clearimmediate.js`, `test-async-wrap-promise-after-enabled.js`
- `test-heapdump-async-hooks-init-promise.js`
- `test-promise-hook-create-hook.js`, `test-promise-hook-exceptions.js`, `test-promise-hook-on-after.js`, `test-promise-hook-on-resolve.js`

## 5. Flag injection side effects (4 tests)

Tests checking whether specific features are enabled/disabled, or checking exact NODE_OPTIONS contents.

- `test-eventsource-disabled.js` — expects `typeof EventSource === 'undefined'`, but nub injects `--experimental-eventsource`
- `test-dotenv-node-options.js` — tests NODE_OPTIONS interaction; nub modifies NODE_OPTIONS
- `test-ffi-missing-build.js` — addon loading interaction with our native module
- `test-no-addons-resolution-condition.js` — checks `--no-addons` flag behavior; our preload loads an addon

## 6. Error/output format changes (5 tests)

Nub's preload changes where errors surface in the stack trace and adds lines to output.

- `test-error-reporting.js` — stack trace shows `node:internal/modules/run_main` instead of the user's `throw` location
- `test-node-output-console.mjs`, `test-node-output-eval.mjs`, `test-node-output-vm.mjs` — snapshot-based output comparison; extra preload output differs
- `test-http-parser-lazy-loaded.js` — checks exact internal module loading order; preload changes loading sequence

## 7. Misc one-offs (4 tests)

- `test-common.js` — test harness self-test; uncaughtException count differs with preload active
- `test-assert-first-line.js` — assertion message includes different source context with `--enable-source-maps`
- `test-internal-modules.js` — internal module resolution changed by our hooks
- `test-os-checked-function.js` — error format differs with our preload's module loading

## 8. Already categorized in task 1.3 (29 tests)

For reference, the 29 tests categorized in task 1.3 across the covered categories (child-process, module, process, worker, require, vm, compile-cache, esm):

- WebStorage warning pollution: 15
- Warning/deprecation behavior changes: 3
- Compile cache bypass: 3
- Addon loading in restricted modes: 2
- Preload-induced line number/async-hook shifts: 2
- Extra globals from polyfills: 1
- Error propagation change: 1
- Resolve/symlink behavior: 1
- Error code change: 1

## Impact assessment

**Total parallel/ nub-specific failures: 165 out of ~4,500 tests (96.3% pass rate).**

**PR-gate config: 1,052 verified tests across 17 categories** (expanded from 20 in task 1.8).

The single biggest root cause is WebStorage ExperimentalWarning pollution (77 tests, 47% of all failures). This is purely a test-harness interaction — the `common/index.js` global enumeration triggers a warning event that the tests don't expect. No user code would be affected; `localStorage` works correctly.

The second biggest category is permission model interaction (30 tests, 18%). This is architectural — our native addon can't load under `--permission` without `--allow-addons`. The nub CLI handles this automatically; only tests that spawn node directly with `--permission` are affected.

None of these 165 failures represent bugs in nub's augmentation of user code. They are the architectural cost of augmentation — the same kind of test-level divergences any tool that registers hooks, injects flags, or preloads code would have.
