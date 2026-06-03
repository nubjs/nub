# module-hooks/ — 21 nub-specific failures

All 21 failures share one root cause: **Nub's preload registers hooks via `module.registerHooks()` before the test code runs.** These tests assume they are the only hooks registered and assert on hook chain behavior that our pre-existing hooks alter.

This is an **expected divergence** — inherent to Nub's architecture. Not fixable without removing our hooks, which would remove all augmentation.

## Category breakdown

### HOOK-CONFLICT (14 tests)

Tests that register their own hooks to override built-in module loading. Our hooks run first in the chain and alter the behavior the tests assert on. Failure pattern: `false == true` assertion on a hook-override check.

- `test-module-hooks-load-builtin-override-commonjs.js`
- `test-module-hooks-load-builtin-override-json.js`
- `test-module-hooks-load-builtin-override-module.js`
- `test-module-hooks-load-builtin-require.js`
- `test-module-hooks-resolve-builtin-builtin-require.js`
- `test-module-hooks-resolve-builtin-on-disk-require-with-prefix.js`
- `test-module-hooks-resolve-builtin-on-disk-require.js`
- `test-module-hooks-resolve-load-builtin-override-both-prefix.js`
- `test-module-hooks-resolve-load-builtin-override-both.js`
- `test-module-hooks-resolve-load-builtin-redirect-prefix.js`
- `test-module-hooks-resolve-load-builtin-redirect.js`
- `test-module-hooks-load-builtin-import.mjs`
- `test-module-hooks-resolve-builtin-builtin-import.mjs`
- `test-module-hooks-resolve-builtin-on-disk-import.mjs`

### HOOK-ORDER (1 test)

Test asserts the first hook in the chain is its own loader. Sees `runtime/preload.mjs` instead.

- `test-async-loader-hooks-called-with-expected-args.mjs`

### ASYNC-LOADER interference (6 tests)

Tests for the async `module.register()` API. Our sync `registerHooks()` preload interacts with the async loader pipeline in ways these tests don't expect.

- `test-async-loader-hooks-called-with-register.mjs`
- `test-async-loader-hooks-register-with-cjs.mjs`
- `test-async-loader-hooks-register-with-require.mjs`
- `test-async-loader-hooks-register-with-url-parenturl.mjs`
- `test-async-loader-hooks-require-resolve-default.mjs`
- `test-async-loader-hooks-require-resolve-opt-in.mjs`

## Impact assessment

These failures do NOT indicate bugs in Nub's augmentation of user code. They indicate that Nub's hook registration is visible to tests that introspect the hook chain. Real-world user code does not typically assert on hook ordering or count — it just uses hooks.

The 75 passing module-hooks tests confirm that our hooks compose correctly with user-registered hooks for the normal use cases (resolve, load, format detection).
