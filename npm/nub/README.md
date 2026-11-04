# Nub

TypeScript-first developer supertool for Node.js. Run `.ts`/`.tsx` files directly on your installed Node, a faster `npm run`, a pnpm-compatible package manager, and a built-in Node version manager — no config, no lock-in, no nub-specific public APIs.

**Documentation:** https://nubjs.com/docs

## Install

```sh
# macOS / Linux
curl -fsSL https://nubjs.com/install.sh | bash

# Windows (PowerShell)
irm https://nubjs.com/install.ps1 | iex

# Or via npm
npm install -g @nubjs/nub
```

## Quickstart

```sh
# Run TypeScript directly — no tsconfig, no build step
nub server.ts

# Run package.json scripts (faster than npm/pnpm run)
nub run dev
nub run build

# Execute a local or remote binary
nubx vitest --run

# One name for a file, a script, or an installed bin
nubr app.ts

# Watch mode — restart on change
nub watch server.ts
```

## What Nub does

- **TypeScript just works.** Files ending in `.ts`, `.tsx`, `.mts`, `.cts`, `.jsx` execute directly. Enums, decorators, parameter properties, and namespaces are handled. Source maps work in error traces.
- **The `.env` loading is built in.** Workspace-aware, with `${VAR}` expansion and `.env.local` / `.env.production` layering.
- **The script runner is faster.** Running `nub run` resolves scripts, adds `node_modules/.bin` to PATH, and runs lifecycle hooks — without the per-invocation Node bootstrap that `npm run` / `pnpm run` pay.
- **Auto-flag injection.** Experimental Node features are unflagged based on your Node version; opt out with `--no-experimental-*`.
- **The tsconfig paths resolve.** A path like `@lib/utils` resolves via `compilerOptions.paths` with no build step. Extensionless and data-format imports (`.jsonc`, `.json5`, `.toml`, `.yaml`) work too.
- **Polyfills.** `Temporal`, `URLPattern`, `RegExp.escape`, `Error.isError`, `Promise.try`, `navigator` — feature-detected, native wins.
- **Package management.** Commands such as `nub install` / `add` / `remove` are pnpm-compatible on the CLI and lockfile-compatible with whatever the project already uses (npm / pnpm / bun round-trip, yarn read-only) — Nub does not impose its own lockfile format.
- **Node version management.** Nub provisions and pins the project's Node version, so `nvm` / `corepack` are not needed.

## How it works

Nub is not a Node fork. It is a Rust CLI that orchestrates your installed Node via Node's own extension surfaces — `module.registerHooks()`, `--import` preloads, V8 flag injection, an N-API addon for fast transpilation, and a per-invocation PATH shim. Code targeting Node runs on Nub byte-for-byte. For orchestration without augmentation, use `nub run --node`.

There are no nub-specific public APIs to import, no `nub:*` module namespace, and no config field you author named after the tool. Adopting Nub is reversible — the project keeps running under plain Node and your existing package manager.

## Requirements

Nub runs on your installed Node, version **18.19.0 or newer**. The fast tier (sync `module.registerHooks()`) engages on Node **22.15+**; versions 18.19–22.14 run in a compatibility tier via async `module.register()`. Supported platforms: macOS (arm64, x64), Linux (x64, arm64), Windows (x64).

## License

MIT — see [LICENSE](./LICENSE). Full docs at https://nubjs.com/docs.
