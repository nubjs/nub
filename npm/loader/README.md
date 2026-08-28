# nubjs

Standalone TypeScript loader for Node.js, from the [Nub](https://nubjs.com) project. Register it the way tsx or ts-node is registered, and TypeScript works in `import`, in `require()`, and in worker threads — powered by the same native oxc-based transform the Nub CLI uses.

```sh
npm install --save-dev nubjs
node --import nubjs app.ts
```

Any way Node accepts a preload works:

```sh
node --import nubjs app.ts          # one run
NODE_OPTIONS="--import nubjs" vitest # tools that spawn node themselves
node --require nubjs app.ts          # CommonJS delivery (see below)
```

## What it does

- Transpiles `.ts` / `.tsx` / `.mts` / `.cts` / `.jsx` on the fly — full TypeScript, including enums, namespaces, and legacy decorators, not just type stripping.
- Resolves TypeScript conventions: tsconfig `paths` and `baseUrl`, extensionless imports, the `.js` → `.ts` emit-convention swap, directory index files.
- Augments CommonJS `require()` with the same resolution and transpile, not only `import`.
- Loads data formats as modules: `.yaml`, `.toml`, `.json5`, `.jsonc`, `.txt`, and `with { type: "text" }` imports.
- Lowers `using` / `await using` and other syntax newer than the running Node.
- Inline source maps, on for every transpiled file.
- Applies inside worker threads automatically (Node inherits the preload).

Dependencies under `node_modules` are never transpiled, and files Node handles natively load byte-for-byte unchanged — the loader adds behavior, it does not modify Node's.

## Entry points

```sh
node --import nubjs app.ts        # ESM hooks + CommonJS require() augmentation
node --require nubjs app.ts       # same, delivered as a CommonJS preload (Node 20.19+)
node --import nubjs/esm app.ts    # ESM hooks only
```

Module formats follow Node's own rules: a `.cts` file is CommonJS and a `.mts` file is an ES module, and the loader transpiles types and syntax without converting one format into the other.

## Node support

Node 18.19 and newer. On Node 22.15+ hooks register synchronously in-thread (`module.registerHooks`); older versions run them in Node's loader worker (`module.register`). The `--require` delivery needs `require(esm)` (Node 20.19+ / 22.12+); below that use `--import`.

## Relationship to the Nub CLI

The [`@nubjs/nub`](https://www.npmjs.com/package/@nubjs/nub) CLI is a complete TypeScript-first toolchain — runner, package manager, Node version management — and does everything this loader does without any flags. This package is the loader alone, for cases where the `node` invocation itself is fixed: existing tooling, test runners, other CLIs that spawn `node`.

Platform binaries ship as `optionalDependencies` (`@nubjs/loader-*`) for macOS, Linux (glibc and musl), and Windows, on x64 and arm64.
