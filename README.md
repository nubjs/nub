# Nub

TypeScript-first developer supertool. A Rust CLI that augments your Node.js — TS execution, `.env` loading, auto-flag injection, and more. Zero config.

```sh
npm install -g @nubjs/nub
```

## Quickstart

```sh
# Run TypeScript directly — no tsconfig, no build step
nub server.ts

# Run package.json scripts (faster than pnpm run)
nub run dev
nub run build

# Execute a local binary
nubx vitest --run

# Watch mode
nub watch server.ts
```

## What Nub does

- **TypeScript just works.** `.ts`, `.tsx`, `.mts`, `.cts`, `.jsx` — all execute directly. Enums, decorators, parameter properties, namespaces are handled. Source maps work in error traces.
- **`.env` loading built in.** Workspace-aware, `${VAR}` expansion, `.env.local` / `.env.production` / etc.
- **Faster script runner.** `nub run` resolves scripts, adds `node_modules/.bin` to PATH, runs lifecycle hooks — with no Node bootstrap for the wrapper, removing the per-invocation overhead `npm run` / `pnpm run` pay (see [`benchmarks/results.md`](benchmarks/results.md)).
- **Auto-flag injection.** Experimental Node features unflagged based on your Node version. `--experimental-vm-modules`, `--experimental-sqlite`, etc. — on by default, opt out with `--no-experimental-*`.
- **tsconfig paths.** `@lib/utils` resolves via `compilerOptions.paths` without a build step.
- **Extensionless imports.** `import './foo'` resolves to `./foo.ts` automatically from TS files.
- **Data format imports.** `import config from "./config.jsonc"` works for `.jsonc`, `.json5`, `.toml`, `.yaml`, `.txt`.
- **Polyfills.** `Temporal`, `URLPattern`, `RegExp.escape`, `Error.isError`, `Promise.try`, `navigator` — feature-detected, native wins.

## How it works

Nub is **not a Node fork**. It's a Rust CLI that orchestrates your installed Node via extension surfaces:

- `module.registerHooks()` for TS transpilation and resolution
- `--import` preloads for polyfill injection
- V8 flag injection for unflagging experimental features
- N-API addon (oxc-transform) for fast transpilation
- Per-invocation PATH shim for subprocess augmentation

Code targeting Node runs on Nub byte-for-byte. For plain Node: type `node` in your shell. For orchestration without augmentation: `nub run --node`.

## Requirements

- **Node 22.15+** (for sync `module.registerHooks()`)
- macOS (arm64, x64), Linux (x64, arm64), Windows (x64)

## Performance

Numbers and methodology live in [`benchmarks/results.md`](benchmarks/results.md) — the single source of truth, regenerated via `hyperfine`. (TL;DR: TS startup overhead over plain Node is a small fixed cost dominated by transpile + preload; `nub run` beats `pnpm run` on script dispatch.)

## Design corpus

All design lives under [`wiki/`](wiki/):

- [`wiki/architecture.md`](wiki/architecture.md) — augmenter vs fork, compat mode
- [`wiki/philosophy.md`](wiki/philosophy.md) — additivity, brand boundary, reversibility
- [`wiki/PLAN.md`](wiki/PLAN.md) — canonical v0.1 plan
- [`wiki/whitepaper.md`](wiki/whitepaper.md) — public framing

## License

MIT
