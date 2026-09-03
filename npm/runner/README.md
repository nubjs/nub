# @nubjs/runner

Run TypeScript on Node.js, from the [Nub](https://nubjs.com) project. Install it, and `nubr` runs a TypeScript file, a `package.json` script, or an installed bin — powered by the same native oxc-based transform the Nub CLI uses, in a package with no package manager, no registry client and no network code in it.

```sh
npm install --save-dev @nubjs/runner
nubr app.ts
```

## Running scripts

```json
{
  "scripts": {
    "dev": "nubr src/index.ts",
    "build": "nubr build.ts --minify"
  }
}
```

```sh
nubr dev              # runs the "dev" script
nubr build -- --watch # extra arguments reach the script
```

Scripts run through the same shell npm uses, with `node_modules/.bin` on the path and `pre`/`post` hooks honored, and every Node process a script starts inherits the TypeScript support.

## Running installed bins

A name that is neither a file nor a script resolves against `node_modules/.bin`:

```sh
nubr vitest run
nubr tsc --noEmit
```

Nothing is fetched from the registry — the tool has to be installed already. Names resolve most-specific-first: a file, then a script, then a bin, so a script wins over the bin it is usually named after, matching npm.

## As a Node preload

When the `node` invocation is not yours to change — a test runner, a framework CLI — register the package the way tsx or ts-node is registered:

```sh
node --import @nubjs/runner app.ts            # one run
NODE_OPTIONS="--import @nubjs/runner" vitest  # a tool that spawns node itself
node --require @nubjs/runner app.ts           # CommonJS delivery
node --import @nubjs/runner/esm app.ts        # ESM hooks only
```

## What it does

- Transpiles `.ts` / `.tsx` / `.mts` / `.cts` / `.jsx` on the fly — full TypeScript, including enums, namespaces and legacy decorators, not just type stripping.
- Resolves TypeScript conventions: tsconfig `paths` and `baseUrl`, extensionless imports, the `.js` → `.ts` emit-convention swap, directory index files.
- Augments CommonJS `require()` with the same resolution and transpile, not only `import`.
- Loads data formats as modules: `.yaml`, `.toml`, `.json5`, `.jsonc`, `.txt`, and `with { type: "text" }` imports.
- Lowers `using` / `await using` and other syntax newer than the running Node.
- Inline source maps, on for every transpiled file.
- Applies inside worker threads automatically (Node inherits the preload).

Dependencies under `node_modules` are never transpiled, and files Node handles natively load byte-for-byte unchanged — the package adds behavior, it does not modify Node's.

## Node flags

Flags that Node reads at startup go before the file:

```sh
nubr --inspect app.ts
nubr --max-old-space-size=4096 app.ts
```

## Node support

Node 18.19 and newer. On Node 22.15+ hooks register synchronously in-thread (`module.registerHooks`); older versions run them in Node's loader worker (`module.register`). The `--require` delivery needs `require(esm)` (Node 20.19+ / 22.12+); below that use `--import`.

## Relationship to the Nub CLI

The [`@nubjs/nub`](https://www.npmjs.com/package/@nubjs/nub) CLI is a complete TypeScript-first toolchain — runner, package manager, Node version management — and does everything this package does without any flags. Reach for this one when you cannot install the binary, or when the `node` invocation itself is fixed.

Platform binaries ship as `optionalDependencies` (`@nubjs/runner-*`) for macOS, Linux (glibc and musl), and Windows, on x64 and arm64.
