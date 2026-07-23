<p align="center">
  <img src="https://nubjs.com/icon-border.svg" width="200px" align="center" alt="Nub logo" />
  <h1 align="center">Nub</h1>
  <p align="center">
    A fast all-in-one toolkit that augments Node.js instead of replacing it
  </p>
</p>

<div align="center">
  <a href="https://nubjs.com/docs">Docs</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="https://github.com/nubjs/nub">GitHub</a>
  <span>&nbsp;&nbsp;•&nbsp;&nbsp;</span>
  <a href="https://x.com/colinhacks">𝕏</a>
  <br/><br/>
</div>

<p align="center">
<a href="https://github.com/nubjs/nub" rel="nofollow"><img src="https://nubjs.com/stars.svg" alt="stars"></a>
</p>



<br/>
<br/>

A Bun-like DX on top of stock `node`, written in Rust.


```sh
nub index.ts             # TypeScript-first Node.js runtime
nub run dev              # 24× faster pnpm run
nubx prisma generate     # 19× faster npx
nub install              # 18× faster pnpm install
nub watch src/server.ts  # native watch mode
nub pm shim              # built-in Corepack-style shims
nub node install 26      # Node version manager
nub upgrade              # self update
```

One tool to run your files and scripts, install dependencies, and manage Node itself. No new runtime, no vendor-specific API surface, no lock-in.

| Nub | Instead of |
|---|---|
| `nub <file>` | `node`, `tsx`, `ts-node`, `dotenv-cli` |
| `nub run <script>` | `npm run`, `pnpm run` |
| `nubx` | `npx`, `pnpm dlx / exec` |
| `nub install` | `npm`, `pnpm` |
| `nub watch` | `nodemon`, `node --watch`, `tsx watch` |
| `nub node` | `nvm`, `fnm`, `n`, `volta` |
| `nub pm` | `corepack` |

<br/>

## Install

```sh
# macOS / Linux
curl -fsSL https://nubjs.com/install.sh | bash

# Windows (PowerShell)
irm https://nubjs.com/install.ps1 | iex

# Homebrew (macOS / Linux)
brew install nubjs/tap/nub

# Nix (flakes)
nix run github:nubjs/nub

# mise
mise use -g nub

# Or via npm (pnpm / yarn global add work too)
npm install -g --ignore-scripts=false @nubjs/nub
```

For GitHub Actions, use [`nubjs/setup-nub`](https://github.com/nubjs/setup-nub) in place of `actions/setup-node`. It's one-to-one compatible.

```diff
- - uses: actions/setup-node@v4
+ - uses: nubjs/setup-nub@v0
```

<br/>

## File runner — `nub <file>`

Run a file. Supports `.js`, `.ts`, `.mjs`, `.cjs`, `.mts`, `.cts`, `.jsx`, and `.tsx`. Flag-for-flag and var-for-var drop-in compatible with `node` (mostly via passthrough).

```sh
nub index.ts             # TypeScript, JSX, no build step
nub --watch app.ts       # same path, restart-on-change
```

It augments stock Node with some of Bun/Deno's best features:

- 🦆 Full TypeScript support, including `enum`, `namespace`
- 🧭 TypeScript-friendly resolution: extensionless imports, `tsconfig.json#paths`
- ⚛️ JSX / TSX
- 🎂 Decorators and `emitDecoratorMetadata`
- 🆕 Modern syntax like `using` (downleveled in transpiler when needed)
- 🔐 Automatic `.env*` loading — Next.js/Vite parity
- 🗂️ Built-in loaders for common data formats — `.yaml`, `.toml`, `.jsonc`, `.json5`, `.txt`
- 🌐 Polyfills for `Temporal`, `Worker`, `URLPattern` (when needed)
- 🔥 Unflags experimental features like `node:sqlite`, `vm.Module`, `localStorage`, `WebSocket`, `EventSource`
- ⚡ 2.9× faster startup than `tsx`

> **How it works** — Nub takes advantage of Node extension surfaces that mostly didn't exist when Deno and Bun were built: 
> 
> - [`--import`](https://nodejs.org/api/cli.html#--importmodule)/[`--require`](https://nodejs.org/api/cli.html#-r---require-module) preloads
> - [`module.registerHooks()`](https://nodejs.org/api/module.html#moduleregisterhooksoptions) for transpilation and resolution 
> - [N-API native addons](https://nodejs.org/api/n-api.html): Nub embeds [oxc](https://oxc.rs/) for pre-transpilation


### Node provisioning

When you run a file with nub, it infers the version of Node your project expects and auto-installs it if needed. It respects (in precedence order):

- `NODE_EXECUTABLE` (override)
- `package.json#devEngines`
- `.node-version`
- `.nvmrc`
- `package.json#engines`

This resolved version of Node is installed and your file is executed with it (with Nub's augmentations).

```sh
$ echo 26 > .node-version
$ nub hello.ts
Using Node.js 26.3.0 (resolved from .node-version)
Installed in 9.8s
Hello world!
```

### Modern APIs

Modern API work out of the box under Nub. Node.js experimental APIs are unflagged, others are auto-polyfilled (e.g. `Temporal` on Node 25 and earlier), and others are downleveled in the transpiler (`using`).

| API | How |
|---|---|
| [`Temporal`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Temporal) | polyfilled below Node 26, native above |
| [`URLPattern`](https://developer.mozilla.org/en-US/docs/Web/API/URLPattern) | polyfilled below Node 24, native above |
| [`RegExp.escape`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/RegExp/escape) | polyfilled below Node 24, native above |
| [`Error.isError`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Error/isError) | polyfilled below Node 24, native above |
| [`Promise.try`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise/try) | polyfilled below Node 24, native above |
| [`Float16Array`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Float16Array) | polyfilled below Node 24, native above |
| [`navigator.locks`](https://developer.mozilla.org/en-US/docs/Web/API/Web_Locks_API) | polyfilled below Node 24.5, native above |
| [`reportError`](https://developer.mozilla.org/en-US/docs/Web/API/Window/reportError) | polyfilled |
| [`vm.Module`](https://nodejs.org/api/vm.html#class-vmmodule) | unflagged |
| [`Wasm module imports`](https://nodejs.org/api/esm.html#wasm-modules) | unflagged below Node 24.5 (22.19 on the 22.x line), native above |
| [`WebSocket`](https://developer.mozilla.org/en-US/docs/Web/API/WebSocket) | unflagged from Node 20.10, native from Node 22 |
| [`EventSource`](https://developer.mozilla.org/en-US/docs/Web/API/EventSource) | unflagged from Node 20.18, native above |
| [`node:sqlite`](https://nodejs.org/api/sqlite.html) | unflagged from Node 22.5, native from Node 22.13 |
| [`addon imports`](https://nodejs.org/api/esm.html#node-addons) | unflagged from Node 22.20, never native |

### Watch mode

Restart-on-change driven by the resolved dependency graph plus the off-graph files that still invalidate a run — no glob list to maintain:

```sh
nub watch src/server.ts
nub --watch src/server.ts   # same path
```

- 👀 Tracks the resolved dependency graph automatically
- 🧷 Also watches the off-graph invalidators — `.env*`, the `tsconfig.json` extends chain, `package.json`
- ⚙️ Runs on Node's own `--watch` engine, preserving output by default

View the [full runtime docs  👉](https://nubjs.com/docs/runtime).

<br/>

## Script runner — `nub run`

A drop-in for `npm run` and `pnpm run`. The runner is a Rust binary with no JavaScript startup of its own, so it dispatches a warm script roughly 24× faster than `pnpm run`:

```sh
nub run build
nub run -r --filter "@org/*" test     # supports --filter
```

It's fast compared to existing JavaScript-based script runners.

| Command | Time | Relative |
|---|---|---|
| `nub run` | 14.7 ms | — |
| `npm run` | 329.9 ms | 22× |
| `pnpm run` | 442.7 ms | 30× |

> script dispatch · warm · 50 runs · macOS — [view benchmark](https://github.com/nubjs/nub/tree/main/tests/bench/script-runner)

- 🚀 Feels instantaneous — 14ms vs a detectable 300ms+ lag for npm/pnpm
- 🔁 Full lifecycle support — `pre`/`post` hooks and the complete `npm_*` environment
- 🧰 Local `node_modules/.bin` on `PATH`, with args forwarded without the `--` separator
- 🗃️ The full pnpm workspace surface — `-r`, `--filter`, `--parallel`, `--workspace-concurrency`, `--resume-from`, `--stream`
- 🎯 pnpm's `--filter` grammar verbatim — graph (`...@org/web`) and changed-since (`[main]`) selectors

View the [full script runner docs 👉](https://nubjs.com/docs/runner/run).

<br/>

## Package runner — `nubx` / `nub dlx`

A drop-in for `npx` and `pnpm dlx`. Local-first with a download-and-execute registry fallback (same as `npx`). Eliminating the double-Node.js-spawn performance penalty paid by JavaScript-based tools like `npx` and `pnpm`.

```sh
nubx eslint . --fix
nubx -y cowsay@1.5.0 "hi"   # fetched from the registry (auto-approved via -y)
```

| Command | Time | Relative |
|---|---|---|
| `nubx esbuild --version` | 11 ms | — |
| `pnpm exec esbuild --version` | 191 ms | 17× |
| `npx esbuild --version` | 226 ms | 19× |

> esbuild --version · macOS — [view benchmark](https://github.com/nubjs/nub/tree/main/tests/bench/bin-runner)

- ⚡ Runs a local bin ~19× faster than `npx`, with no Node in the wrapper
- 🔎 Resolves `node_modules/.bin` regardless of which package manager installed it
- 🌐 Registry fallback for uninstalled bins — fetched, run, then discarded
- 🧩 Full `pnpm exec` / `pnpm dlx` flag parity, shell mode included
- 🪜 Walks the resolution chain — member `.bin`, then workspace root, then ancestors

View the [full package runner docs 👉](https://nubjs.com/docs/runner).

<br/>

## Package manager — `nub install`

Nub is a package manager powered by the [Aube](https://github.com/jdx/aube) engine. The CLI is flag-for-flag compatible with `pnpm` for muscle memory, but 

```sh
nub install                    
nub ci
nub add -E -D --save-catalog react
nub remove lodash
nub update
nub dedupe
```

It's fast — avoids the per-command Node.js bootstrap lag incurred by JS-based package managers.

| Tool | Time | Relative |
|---|---|---|
| `nub` | 171 ms | — |
| `bun` | 686 ms | 4.0× |
| `pnpm` | 3193 ms | 18.7× |
| `npm` | 5316 ms | 31.1× |

> warm frozen install · TanStack Start · 313 deps · macOS — [view benchmark](https://github.com/nubjs/nub/tree/main/tests/bench/install)

### Security

- 🛡️ Blocks postinstall by default
- 🦠 Checks [osv.dev](https://osv.dev) for known-malicious package versions during resolution by default
- 🔻 Refuses provenance downgrades by default
- ⏳ 24-hour `minimumReleaseAge` by default

### Compatibility

When you run `nub install` inside a project, it detects the *incumbent* package manager (based on your `package.json#packageManager` or any detected lockfiles). It then runs in **compat-mode**, respecting the config files and environment variables for that package manager.

Under each incumbent, Nub reads that tool's branded config and no other's; the neutral `.npmrc` cascade and `npm_config_*` are read under every one.

| Incumbent | Config it reads |
|---|---|
| **npm** | `package-lock.json`, `.npmrc`, `overrides`, `workspaces`, `engines`/`os`/`cpu`/`libc` |
| **pnpm** | `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `.pnpmfile.cjs`, `package.json#pnpm`, `resolutions`, `catalog:`, `.npmrc` |
| **Yarn** (read-only) | `yarn.lock`, a `.yarnrc.yml` / `.yarnrc` subset, `YARN_*`, `resolutions`, `packageExtensions`, `.npmrc` |
| **Bun** | `bun.lock`, `bunfig.toml` `[install]`, `trustedDependencies`, `overrides`, `patchedDependencies`, `catalog:`, `.npmrc` |
| **Nub** | neutral only — `.npmrc`, `npm_config_*`, `overrides` / `resolutions` / `catalog` / `workspaces` |

View the [full package manager docs 👉](https://nubjs.com/docs/install#config-it-reads).

<br/>

## Package meta-manager — `nub pm`

Corepack's job, in native Rust: provision and run the exact pnpm / npm / yarn your project pins:

```sh
nub pm shim              # registers global shims (Corepack-style)
```

Like `corepack enable`, this registers global shims for `npm`, `yarn`, and `pnpm`. When you run a command using one of these shim aliases anywhere on your file system, the shim will:

- Detect the version used in your project
- Install that version if needed
- Run the command using the proper version

Nub provides this functionality as a convenience for users who prefer to keep their current package manager. Corepack itself was [unbundled from Node itself](https://github.com/nodejs/nodejs.org/issues/7555) in v25.

View the [full `nub pm` docs 👉](https://nubjs.com/docs/pm).

<br/>

## Node version manager — `nub node`

Though Node.js versions will generally be auto-installed and cached as needed, you can manage versions manually as well.

```sh
$ nub node -h 
nub node — manage Node versions

Usage: nub node <command>

Commands:
  which                    print the resolved Node binary path (why → stderr)
  install [<version>...]   provision version(s) into nub's cache
  ls                       list versions in nub's cache
  uninstall <version>      remove a version from nub's cache
  pin <version>            write the project's Node pin
```

View the [full `nub node` docs 👉](https://nubjs.com/docs/node).

<br/>

## License

MIT

> <p align="center">
>  <em>⭐️ If you read this far, consider starring the repo :) ⭐️</em>
> </p>
