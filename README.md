# Nub

An all-in-one toolkit for Node.js. One Rust binary to run your files and scripts, install dependencies, and manage Node itself — a Bun-like modern DX on top of the `node` you already have. There's no new runtime to adopt and no lock-in: every augmentation rides on Node's own public extension surfaces.

**Documentation:** https://nubjs.com/docs

```sh
# macOS / Linux
curl -fsSL https://nubjs.com/install.sh | bash

# Windows (PowerShell)
irm https://nubjs.com/install.ps1 | iex

# Or via npm (pnpm / yarn global add work too)
npm install -g --ignore-scripts=false @nubjs/nub
```

That puts `nub` and `nubx` on your `PATH`.

For GitHub Actions, use [`nubjs/setup-nub`](https://github.com/nubjs/setup-nub) in place of `actions/setup-node`:

```diff
- - uses: actions/setup-node@v4
+ - uses: nubjs/setup-nub@v0
```

It installs Nub, can pre-provision the project's Node, and can cache Nub's store.

## Quickstart

```sh
nub index.ts             # run a TypeScript file on stock Node
nub run dev              # run a package.json script (~24× faster than pnpm run)
nubx prisma generate     # run a CLI from node_modules/.bin (~19× faster than npx)
nub install              # install dependencies (pnpm-shaped, lockfile-compatible)
nub watch src/server.ts  # restart on file changes
nub pm shim              # route bare npm/pnpm/yarn through the project's pin
nub node install 22      # provision a Node version
```

## What Nub replaces

| Nub | Instead of |
|---|---|
| `nub <file>` | `node`, `tsx`, `ts-node`, `dotenv-cli` |
| `nub run <script>` | `npm run`, `pnpm run`, `yarn run` |
| `nubx` | `npx`, `pnpm dlx`, `pnpm exec`, `yarn dlx` |
| `nub install` | `npm`, `pnpm`, `yarn` |
| `nub watch` | `nodemon`, `node --watch`, `tsx watch` |
| `nub node` | `nvm`, `fnm`, `n`, `volta` |
| `nub pm` | `corepack` |

## The toolkit

### File runner — `nub <file>`

A flag-for-flag drop-in for `node <file>` that also runs TypeScript and JSX directly — no tsconfig, no build step. A `.ts` file starts on par with plain `node`:

```sh
nub index.ts             # TypeScript, JSX, no build step
nub --watch app.ts       # same path, restart-on-change
```

```ts
import config from "./config.yaml";  // .yaml, .toml, .jsonc, .json5, .txt — parsed default import
```

- 📦 Runs the full TS surface — non-erasable syntax (`enum`, `namespace`, parameter properties) and legacy decorators with `emitDecoratorMetadata`.
- 🧭 Resolves imports the way your editor does — extensionless, `.js → .ts`, tsconfig `paths`.
- 🔐 Loads `.env*` automatically with `${VAR}` expansion.
- 🗂️ Imports data files as parsed values — `.yaml`, `.toml`, `.jsonc`, `.json5`, `.txt`.
- 🌐 Backfills modern globals per Node-version band — `Temporal`, `URLPattern`, `WebSocket`, `EventSource`, `node:sqlite`, Web Workers.
- 🗺️ Surfaces source maps in error traces.
- ⚡ Starts about 2.9× faster than `tsx`, which loads esbuild and its loader hooks on every run.

See [Runtime](https://nubjs.com/docs/runtime).

### Script runner — `nub run`

A drop-in for `npm run` and `pnpm run`. The runner is a Rust binary with no JavaScript startup of its own, so it dispatches a warm script roughly 24× faster than `pnpm run`:

```sh
nub run build
nub run -r --filter "@org/*" test     # pnpm's filter grammar, verbatim
```

```
script dispatch · warm · 50 runs · macOS
nub run     14.7 ms
node --run  32.2 ms   (2.2×)
npm run     329.9 ms  (22×)
pnpm run    442.7 ms  (30×)
```

- 🚀 Dispatches a warm script in ~14.7 ms — roughly 24× faster than `pnpm run`.
- 🔁 Runs lifecycle `pre`/`post` hooks and exposes the full `npm_*` environment.
- 🧰 Puts `node_modules/.bin` on `PATH` and forwards args without the `--` separator.
- 🗃️ Preserves the pnpm workspace surface — `-r` / `--recursive`, `--filter`, `--parallel`, `--workspace-concurrency`, `--resume-from`, `--stream`.
- 🎯 Reads `--filter` with pnpm's grammar verbatim, including graph (`...@org/web`) and changed-since (`[main]`) selectors.

See [Script runner](https://nubjs.com/docs/run).

### Package runner — `nubx` / `nub dlx`

A drop-in for `npx` and `pnpm dlx`, local-first with a registry fallback. It resolves `node_modules/.bin` in Rust and execs the binary directly, so the per-call Node bootstrap that `npx` pays disappears:

```sh
nubx eslint . --fix
nubx cowsay@1.5.0 "hi"   # fetched from the registry, then discarded
```

```
esbuild --version · macOS
nubx esbuild --version        11 ms
pnpm exec esbuild --version   191 ms  (17×)
npx esbuild --version         226 ms  (19×)
```

- ⚡ Runs a local CLI in ~11 ms — about 19× lighter than `npx`, with no Node process in the wrapper.
- 🔎 Resolves `node_modules/.bin` regardless of which package manager installed it.
- 🌐 Fetches and runs an uninstalled bin from the registry, then discards it.
- 🧩 Matches `pnpm exec` / `pnpm dlx` flags, shell mode included.
- 🪜 Walks the resolution chain — member `.bin` first, then the workspace root, then ancestors.

See [Package runner](https://nubjs.com/docs/nubx).

### Package manager — `nub install`

Nub has its own pnpm-shaped install engine (the vendored [aube](https://github.com/jdx/aube) engine, embedded as a library). The CLI follows pnpm's spellings; the lockfile stays in your project's native format, which Nub infers and mirrors:

```sh
nub install                    # alias: nub i  ·  also: nub ci, --frozen-lockfile
nub add -E -D --save-catalog react
nub remove lodash
nub update
nub dedupe
nub import                     # convert another lockfile in place
```

```
warm frozen install · create-t3-app · 222 deps · macOS
nub    1122 ms
bun    1444 ms   (29% slower)
pnpm   2847 ms   (2.5×)
npm    4163 ms   (3.7×)
```

- ⚡ Installs create-t3-app (222 deps) warm + frozen in ~1.1 s.
- 🔄 Round-trips your existing lockfile — npm, pnpm, and Bun in place; Yarn read-only.
- 🧠 Infers the incumbent package manager and mirrors it — no migration, no prompt.
- 🗄️ Dedupes through a global content-addressed store and materializes by reflink/hardlink.
- 🐍 Accepts pnpm's flags with the same spelling and semantics, down to the workspace catalog.
- 🛡️ Treats dependency build scripts as deny-by-default — a script runs only on an explicit allow or a vetted default-trust floor.
- 🦠 Queries OSV for malicious-package (`MAL-*`) advisories on every fresh resolve and hard-blocks a confirmed hit (`ERR_NUB_MALICIOUS_PACKAGE`).
- 🔻 Refuses a version whose publish trust evidence weakened against an earlier release (`trustPolicy=no-downgrade`, `ERR_NUB_TRUST_DOWNGRADE`).
- ⏳ Holds back releases younger than `minimumReleaseAge` (24h by default, matching pnpm), so a freshly-compromised version isn't pulled in.

These supply-chain defenses are on by default, no config required. Dependency build scripts run only on an explicit allow (`pnpm.onlyBuiltDependencies`, `trustedDependencies`, `nub approve-builds`) or when a curated default-trust floor vouches for the package under registry-provenance, advisory-vetting, and cooling-window gates; a skipped package is named with `WARN_NUB_IGNORED_BUILD_SCRIPTS`. An OSV malicious-package hit aborts the install, a weakened-provenance downgrade is refused, and too-new versions wait out the cooling window. See [Package manager](https://nubjs.com/docs/install).

### Package meta-manager — `nub pm`

Corepack's job, in native Rust: provision and run the exact pnpm / npm / yarn your project pins:

```sh
nub pm use pnpm@9.15.4   # declare the project's PM, align the lockfile
nub pm shim              # bare npm/pnpm/yarn route through the pin
```

- 🎯 Reads the pin from `packageManager`, `devEngines`, or Yarn Berry's `yarnPath`.
- 📥 Fetches that version integrity-verified, caches it, and runs it on the project's Node.
- 🚫 Needs no `corepack enable` and no baked version table.

See [Package meta-manager](https://nubjs.com/docs/pm).

### Node version manager — `nub node`

Pin a version and the matching stock Node is fetched from nodejs.org, SHA-256-verified, cached, and run — in the same breath as your code, no second command:

```sh
echo 22 > .node-version
nub hello.ts
```

```
Using Node.js 22.15.0 (resolved from .node-version)
Installed in 9.8s
Hello world!
```

```sh
nub node install 22     # also: ls, uninstall, pin, which
nub node pin lts
```

- 📌 Pins from `.node-version`, `.nvmrc`, or `engines.node`.
- 📥 Fetches stock Node from nodejs.org, SHA-256-verified and cached.
- 🤝 Provisions on demand, in the same command that runs your code.
- 🧭 Adopts whatever `node` is on your `PATH` when there's no pin.

See [Node manager](https://nubjs.com/docs/node).

### Watch mode — `nub watch`

Restart-on-change driven by the resolved dependency graph plus the off-graph files that still invalidate a run — no glob list to maintain:

```sh
nub watch src/server.ts
nub --watch src/server.ts   # same path
```

- 👀 Tracks the resolved dependency graph automatically.
- 🧷 Also watches the off-graph invalidators — `.env*`, the `tsconfig.json` extends chain, `package.json`.
- ⚙️ Runs on Node's own `--watch` engine, preserving output by default.

See [Watch mode](https://nubjs.com/docs/watch).

### Self-update — `nub upgrade`

```sh
nub upgrade             # update Nub itself to the latest release
```

- ⬆️ Updates the `nub` binary in place from the release channel.

## Modern APIs, on the Node you have

The file runner backfills modern web-platform and TC39 APIs per Node-version band — each polyfilled (feature-detected, native wins) or flag-injected. Every row maps to a band in nub's [feature matrix](crates/nub-core/src/node/feature_matrix.rs); the right column states where Nub's mitigation applies.

| API | Mitigation | Where Nub backfills it |
|---|---|---|
| `Temporal` | Polyfilled (lazy global) | Every supported Node — no Node ships it |
| `Worker` (browser-shape) | Polyfilled over `worker_threads` | Every supported Node — no Node ships it |
| `reportError` | Polyfilled | Every supported Node — no Node ships it |
| `URLPattern` | Polyfilled; native on Node 24+ | Below Node 24 |
| `RegExp.escape` | Polyfilled; native on Node 24+ | Below Node 24 |
| `Promise.try` | Polyfilled; native on Node 24+ | Below Node 24 |
| `Float16Array` | Polyfilled; native on Node 24+ | Below Node 24 |
| `Error.isError` | Polyfilled; native on Node 24+ | Below Node 24 |
| `navigator.locks` | Polyfilled; native on Node 24.5+ | Below Node 24.5 |
| `WebSocket` | Flag-injected; default-on Node 22+ | Node 20.10–21.x |
| `EventSource` | Flag-injected | Node 20.18+ and 22.3+ |
| `node:sqlite` | Flag-injected; unflagged 22.13+ / 23.4+ | Node 22.5–22.12 and 23.x |
| `sessionStorage` / `localStorage` | Flag-injected (`--experimental-webstorage`) | Node 22.4–24.x |
| `vm.Module` | Flag-injected (`--experimental-vm-modules`) | Across the supported floor |
| `using` / `await using` | Transpiled | Every supported Node |

Polyfills feature-detect and bow out when Node ships the API natively, so a backfill never shadows a real global. `sessionStorage` works out of the box; persistent `localStorage` is opt-in, since it needs a backing file. See [Runtime](https://nubjs.com/docs/runtime).

## How it works

Nub is **not a Node fork**. It's a Rust CLI that orchestrates your installed Node through Node's own extension surfaces — `module.registerHooks()` for TS transpilation and resolution, `--import` preloads for polyfills, V8 flag injection for unflagging experimental features, an oxc-based N-API addon for fast transpilation, and a per-invocation `PATH` shim so subprocesses stay augmented. Code targeting Node runs on Nub byte-for-byte.

The `--node` flag is the escape hatch: it runs with zero augmentation — no load hook, no preload, no flag injection, no `.env` loading — on the project's *pinned* Node, which makes it the tool for differential debugging.

```sh
nub --node script.js     # the project's pinned Node, vanilla
```

There are no Nub-specific APIs to import or call, no `nub:*` module namespace, no `@nub/*` scope, and no `"nub"` config field you author. Drop Nub in, drop it out — your code never references it.

## Requirements

- **Node 18.19+.** 18.19–22.14 use the compatibility tier (async `module.register()` loader-worker); 22.15+ use the fast path (sync `module.registerHooks()`).
- macOS (arm64, x64), Linux (x64, arm64), Windows (x64, arm64).

Ambient TypeScript declarations for the modern globals ship via `@types/node` 25; `reportError` lives in `@nubjs/types`.

## License

MIT
