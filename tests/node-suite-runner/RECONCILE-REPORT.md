# Node v26.5.1 suite reconciliation

| | count |
|---|---:|
| entries before | 2554 |
| removed (deleted upstream) | 1 |
| entries after | 5000 |
| added (active, nub passes) | 2367 |
| added (ignore, nub diverges) | 80 |
| skipped (upstream Node 26 fails here) | 41 |
| existing active now failing (review) | 8 |
| existing ignored now passing (un-ignore candidates) | 87 |

## Removed: no longer present upstream

- `parallel/test-http-rawheaders-limit.js`

## Added-as-ignore, by reason category

- divergence: 59
- source-maps: 20
- webstorage: 1

## Existing active entries that now FAIL under nub

These were curated as passing. Each is a regression or an environment change; not auto-flipped.

- `parallel/test-process-finalization.mjs` (fail) — [process 994]: --- stderr ---
- `parallel/test-url-revokeobjecturl.js` (fail) — node:internal/assert/utils:146
- `parallel/test-util-inspect.js` (fail) — node:internal/assert/utils:135
- `es-module/test-esm-dynamic-import.js` (fail) — node:internal/process/promises:324
- `es-module/test-esm-loader-default-resolver.mjs` (fail) — ▶ default resolver
- `es-module/test-esm-loader-http-imports.mjs` (fail) — ▶ ESM: http import via loader
- `es-module/test-esm-module-not-found-commonjs-hint.mjs` (fail) — ▶ ESM: module not found hint
- `module-hooks/test-async-loader-hooks-process-exit-async.mjs` (fail) — [process N]: --- stderr ---

## Existing ignored entries that now PASS under nub

- `parallel/test-buffer-constructor-deprecation-error.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-buffer-constructor-outside-node-modules.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-buffer-of-no-deprecation.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-buffer-nopendingdep-map.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-buffer-pending-deprecation.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-child-process-execfile.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-child-process-spawn-shell.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-child-process-spawnsync-shell.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-console-log-stdio-broken-dest.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-console.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-crypto-gcm-explicit-short-tag.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-crypto-hmac.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-crypto-random.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-debugger-address.mjs` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-dgram-bind-error-repeat.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-dns-lookup-promises-options-deprecated.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-domain-dep0097.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-domain-http-server.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-domain-implicit-binding.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-domain-implicit-fs.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-domain-multi.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-domain-promise.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-err-name-deprecation.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-event-emitter-max-listeners-warning-for-null.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-event-emitter-max-listeners-warning-for-symbol.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-event-emitter-max-listeners-warning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-eventtarget-memoryleakwarning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-fs-exists.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-fs-mkdtemp.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-fs-opendir.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http-many-ended-pipelines.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http-server-multiple-client-error.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http-socket-error-listeners.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http-timeout-client-warning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-client-priority-before-connect.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-client-request-listeners-warning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-client-set-priority.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-priority-event.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-priority-cycle-.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-server-set-header.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-http2-server-stream-session-destroy.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-https-simple.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-https-strict.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-inspector-bindings.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-inspector-host-warning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-module-circular-dependency-warning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-module-loading-deprecated.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-module-parent-setter-deprecation.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-module-strip-types.js` — was: feature-enabled: nub's hooks supersede Node's native stripTypeScriptTypes
- `parallel/test-module-symlinked-peer-modules.js` — was: layout-artifact: the symlinked-peer-modules layout + nub's preload can't resolve its vendored resolve-pkg-maps when pnpm symlinks it; the shipped flat (real-dir) runtime passes. Verified locally.
- `parallel/test-nodeeventtarget.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-process-emitwarning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-process-env-deprecation.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-handled-rejection-no-warning.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-unhandled-default.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-unhandled-error.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-unhandled-silent.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-unhandled-silent-no-hook.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-unhandled-throw-handler.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- `parallel/test-promise-unhandled-throw.js` — was: webstorage: --experimental-webstorage triggers ExperimentalWarning in test harness
- …and 27 more

## Skipped: upstream Node v26.5.1 does not pass these under this harness

Not added, because a test real Node fails here measures the environment, not nub.

- `parallel/test-benchmark-cli.js` (fail)
- `parallel/test-cli-node-cli-manpage-env-vars.mjs` (fail)
- `parallel/test-cli-node-cli-manpage-options.mjs` (fail)
- `parallel/test-cli-node-options-docs.js` (fail)
- `parallel/test-cli-options-as-flags.js` (fail)
- `parallel/test-config-file.js` (fail)
- `parallel/test-dotenv-node-options.js` (fail)
- `parallel/test-error-value-type-detection.mjs` (fail)
- `parallel/test-icu-minimum-version.js` (fail)
- `parallel/test-npm-version.js` (fail)
- `parallel/test-npm-install.js` (fail)
- `parallel/test-permission-audit-child-process-inherit-flags.js` (fail)
- `parallel/test-permission-child-process-inherit-flags-substring.js` (fail)
- `parallel/test-permission-child-process-inherit-flags.js` (fail)
- `parallel/test-permission-drop-ffi.js` (fail)
- `parallel/test-process-env-allowed-flags-are-documented.js` (fail)
- `parallel/test-process-load-env-file.js` (fail)
- `parallel/test-process-versions.js` (fail)
- `parallel/test-quic-h3-stream-idle-timeout.mjs` (fail)
- `parallel/test-quic-internal-setcallbacks.mjs` (fail)
- `parallel/test-quic-stream-idle-timeout.mjs` (fail)
- `parallel/test-readline-promises-csi.mjs` (fail)
- `parallel/test-release-changelog.js` (fail)
- `parallel/test-release-npm.js` (fail)
- `parallel/test-runner-v8-deserializer.mjs` (fail)
- `parallel/test-shadow-realm-gc-module.js` (fail)
- `parallel/test-shadow-realm-gc.js` (fail)
- `parallel/test-snapshot-incompatible.js` (fail)
- `parallel/test-tls-get-ca-certificates-worker-no-use-system-ca.js` (fail)
- `parallel/test-tls-get-ca-certificates-worker-use-system-ca.js` (fail)
- `parallel/test-vm-module-referrer-realm.mjs` (fail)
- `parallel/test-watch-file-shared-dependency.mjs` (fail)
- `parallel/test-watch-mode-files_watcher.mjs` (fail)
- `es-module/test-defer-import-eval.mjs` (fail)
- `es-module/test-defer-import-with-module-tree.mjs` (fail)
- `es-module/test-esm-import-text.mjs` (fail)
- `es-module/test-esm-named-exports.mjs` (fail)
- `es-module/test-esm-preserve-symlinks-not-found-plain.mjs` (fail)
- `es-module/test-esm-preserve-symlinks-not-found.mjs` (fail)
- `es-module/test-esm-resolve-type.mjs` (fail)
- `sequential/test-without-async-context-frame.mjs` (fail)
