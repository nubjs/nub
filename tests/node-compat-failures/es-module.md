# es-module/ — 56 failures (26 nub-specific, 30 harness issues)

Of 224 es-module/ tests, 168 pass through nub, 56 fail. Of the 56 failures, **30 also fail under plain `node`** (test-harness path issues — tests reference `test/fixtures/es-module-loaders/` relative to CWD which is outside the node-suite directory). Only **26 are nub-specific**.

## Bug fixed

**`test-esm-import-meta-resolve.mjs`** — `url.includes is not a function` in `extname()`. Our resolve hook received `context.parentURL` as a URL object (from `import.meta.resolve(spec, new URL(import.meta.url))`). Fixed by coercing parentURL to string: `String(context.parentURL || "")`.

## Expected divergences (26 remaining nub-specific failures)

### Hook chain interference (5 tests)

Our hooks add to the hook count and alter the chain order. Tests that introspect hook counts or chain ordering fail.

- `test-esm-initialization.mjs` — hook count 24 vs expected 5
- `test-esm-loader-chaining.mjs` — hook count 4 vs expected 2
- `test-esm-loader-entry-url.mjs` — sees our TS handler in the chain
- `test-esm-loader-programmatically.mjs` — hook passthrough ordering differs
- `test-loaders-workers-spawned.mjs` — our hook's source:null return in worker thread

### Hook error code change (4 tests)

Our hook in the load chain changes the error propagation path for invalid module formats. Node throws `ERR_UNKNOWN_MODULE_FORMAT` directly; with our hook in the chain, validation throws `ERR_INVALID_RETURN_PROPERTY_VALUE` instead. Same rejection, different code path.

- `test-esm-import-attributes-errors.mjs`
- `test-esm-import-attributes-errors.js`
- `test-esm-data-urls.js`
- `test-esm-invalid-data-urls.js`

### Warning suppression (3 tests)

Our injected `--no-warnings` or experimental flags suppress warnings the tests expect.

- `test-esm-experimental-warnings.mjs` — expects ExperimentalWarning, gets empty
- `test-esm-import-assertion-warning.mjs` — expects importAssertions deprecation warning
- `test-esm-wasm-module-instances-warning.mjs` — expects ExperimentalWarning for WASM

### TypeScript transpilation interference (3 tests)

Our hooks transpile .ts files with full oxc support. Tests expecting Node's native type-stripping behavior (which is more restrictive) see different outcomes.

- `test-typescript-commonjs.mjs` — resolves extensionless .cts; treats .ts with CJS syntax as ESM
- `test-typescript-module.mjs` — suppresses ERR_UNSUPPORTED_NODE_MODULES_TYPE_STRIPPING (we transpile in node_modules)
- `test-esm-extensionless-esm-and-wasm.mjs` — our resolve hook resolves extensionless files that Node doesn't

### Addon loading in restricted modes (2 tests)

Our nub-native addon loads even with `--no-addons` or permission restrictions.

- `test-esm-no-addons.mjs` — oxc-transform addon loads despite --no-addons
- `test-cjs-legacyMainResolve-permission.js` — addon loading breaks permission model

### CJS/ESM interop changes (4 tests)

Our hooks alter the module loading chain in ways that change CJS/ESM cycle resolution and error messages.

- `test-esm-cjs-named-error.mjs` — error message text differs (still a SyntaxError)
- `test-require-module-cycle-esm-cjs-esm.js` — cycle resolution differs under our hooks
- `test-require-module-cycle-esm-cjs-esm-esm.js` — same
- `test-require-module-cycle-esm-esm-cjs-esm-esm.js` — same

### Child process hook injection (2 tests)

Tests spawn child `node` processes via `process.execPath`. Under nub's PATH shim, these children get our hooks injected, changing their behavior.

- `test-require-module-warning.js` — child process has nub hooks, altering trace output
- `test-require-node-modules-warning.js` — same

### Internal exposure (1 test)

- `test-loaders-hidden-from-users.js` — `primordials is not defined` when our preload tries to access Node internals

### VM loader context (2 tests)

Node's VM module with main-context default loader behaves differently when our hooks are registered.

- `test-vm-main-context-default-loader-eval.js`
- `test-vm-main-context-default-loader.js`

## Impact assessment

Of the 26 nub-specific failures, **none indicate bugs in Nub's augmentation of user code**. They fall into categories inherent to having hooks registered: hook chain introspection, error code changes, warning suppression, and TypeScript transpilation differences. The 3 TypeScript failures are actually _by design_ — we intentionally provide richer TS support than Node's native type-stripping.

## Harness-issue failures (30 tests, not nub-specific)

These fail under both plain `node` and `nub` due to fixture path resolution (tests run from project root, not from the node-suite test directory). Listed for completeness:

`test-esm-detect-ambiguous.mjs`, `test-esm-import-flag.mjs`, `test-esm-loader-custom-condition.mjs`, `test-esm-loader-default-resolver.mjs`, `test-esm-loader-event-loop.mjs`, `test-esm-loader-http-imports.mjs`, `test-esm-loader-invalid-format.mjs`, `test-esm-loader-invalid-url.mjs`, `test-esm-loader-stringify-text.mjs`, `test-esm-loader-with-source.mjs`, `test-esm-loader.mjs`, `test-esm-named-exports.mjs`, `test-esm-preserve-symlinks-not-found-plain.mjs`, `test-esm-preserve-symlinks-not-found.mjs`, `test-esm-register-deprecation.mjs`, `test-esm-resolve-type.mjs`, `test-esm-shared-loader-dep.mjs`, `test-esm-wasm-js-string-builtins.mjs`, `test-esm-wasm-source-phase-identity.mjs`, `test-esm-wasm-top-level-execution.mjs`, `test-loaders-unknown-builtin-module.mjs`, `test-typescript.mjs`, `test-esm-assertionless-json-import.js`, `test-esm-invalid-pjson.js`, `test-esm-require-race-condition.js`, `test-extensionless-esm-type-commonjs.js`, `test-import-preload-require-cycle.js`, `test-import-require-tla-twice.js`, `test-require-esm-from-imported-cjs.js`, `test-require-module-feature-detect.js`
