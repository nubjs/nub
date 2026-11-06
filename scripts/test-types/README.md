# Script test types

Run the typed unit tests directly with Node's built-in TypeScript stripping:

```sh
node --test 'scripts/*.test.mts'
```

Check their types separately, without adding dependencies to the runtime package:

```sh
npm ci --prefix scripts/test-types --ignore-scripts --no-audit --no-fund
npm run --prefix scripts/test-types check
```

The [tsrs compiler and lint backend](https://github.com/oxc-project/tsrs/blob/main/docs/USAGE.md),
Oxlint frontend, and Node declarations are pinned in this directory. `check` runs
Oxlint with `--type-aware --type-check` through `tsrs-lint` and enables `TSRS_FAST=1`.
Warnings fail the check. Only the direct `scripts/*.test.mts` files are linted;
this never requests full-repository linting. The scoped TypeScript program also
checks imported TypeScript helpers. For compiler diagnostics alone, use
`npm run --prefix scripts/test-types typecheck`; `lint` is an alias for `check`.

`../tsconfig.json` makes the test program discoverable to the lint backend and
extends the scoped test configuration. The explicit type root points to this
isolated tooling directory. The shared
`../tsconfig.tests.json` uses strict checking and permits only erasable TypeScript
syntax. It emits no JavaScript. `Preserve` and `Bundler` resolve the existing mix
of `.ts`, `.mts`, and JavaScript script imports without treating `.ts` helpers as
CommonJS merely because the root package has no `type` field. Imported JavaScript
provides inferred types but is not checked as part of this migration.

The test runner and type checker are separate: running a `.mts` test strips its
types but does not check them. Test functions serialized into child processes are
stripped before `Function.prototype.toString()` reads their source.
