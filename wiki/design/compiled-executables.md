# Compiled executables

The `nub compile` command turns an entry file into a single executable that runs with no Node installed and no `node_modules` on the target machine.

This document covers how the artifact is put together: what goes inside it, what deliberately stays as ordinary files, and how the two are told apart.

## Self-extracting, not virtual

A Rust launcher carries an embedded payload, extracts real files into a content-addressed cache on first run, and executes the user's Node against them. Subsequent runs find the cache already populated.

The alternative — a virtual filesystem, so nothing touches disk — is not available to Nub. Node's module resolver reaches the filesystem through native bindings rather than the JavaScript `fs` module, so a JavaScript-level patch cannot intercept `require()`. Bun and Deno can intercept beneath JavaScript because they own the runtime; Nub runs the user's stock Node and applies no patches to it. Deno itself shipped a self-extracting mode after several years of maintaining a virtual filesystem.

Two shapes:

- **embed** (default) — carries a compressed Node binary, so the artifact is self-contained.
- **`--smol`** — carries no Node, and discovers or provisions one at run time.

## Bundle everything possible, and no more

Startup cost is dominated by file count, not file size. Measured on synthetic trees holding total bytes constant while varying the number of modules:

| what is loaded | cost per file |
| --- | --- |
| a plain relative module | ~60 µs (CommonJS), ~79 µs (ESM) |
| a package in `node_modules` | ~160 µs |

The extra cost of a package is its `package.json` open and parse. The relationship is linear out to a thousand files, and per-byte cost is roughly 18 ns — so splitting a bundle into more files is close to byte-neutral and costs only in file count.

Hence the rule: every module that can be bundled is bundled. A package that cannot be is ejected whole and ships **exactly as it sits on disk**, under `node_modules/` beside the bundle, at the path it already occupied.

Nothing is relocated, flattened, or rewritten. That is the point. A package left in place keeps everything its author relied on:

- `__dirname` resolves where they expected.
- A sibling package it reaches by walking up the tree is still there.
- An addon it locates by building a path at run time is at the end of that path.

Node's ordinary resolution does the rest.

## Telling the two apart

Native addons are loaded by computing a path at run time, so no import graph can see them:

```js
require(path.join(__dirname, 'build', 'Release', 'foo.node'))
require('node-gyp-build')(__dirname)
require('bindings')('foo.node')
```

No bundler in the ecosystem resolves these by static analysis. Vercel's `@vercel/nft` runs a partial evaluator that executes `bindings()` and `nodeGypBuild()` at build time to recover the real path, supported by per-package rewrites, and Next.js still ships a hand-maintained list of packages to leave alone.

Nub asks a cheaper question. A package that calls one of those resolvers **declares it as a dependency**, and a package built with napi-rs **advertises its per-platform binaries as optional dependencies**. Both are ordinary manifest fields, so the decision needs no source analysis and nothing is executed at build time.

| signal | what it means |
| --- | --- |
| depends on `node-gyp-build`, `bindings`, `node-pre-gyp`, `prebuild-install`, `node-addon-api`, or `nan` | the package locates or compiles native code |
| two or more optional dependencies named after itself plus a platform | a napi-rs package with per-platform binaries |
| `gypfile`, a `binary` block, or an install-phase script | the package builds a native artifact on install |

Checked against published manifests, these select `bcrypt`, `sqlite3`, `canvas`, `better-sqlite3`, `cpu-features`, `isolated-vm`, `fsevents`, `sharp`, and `@node-rs/argon2`, and leave `express`, `keyv`, `pino`, and `zod` to be bundled.

The napi-rs signal earns its place. Such a package is a JavaScript-only wrapper whose per-platform sidecar holds the addon — `sharp` contains no `.node` file of its own — so following dependencies forward from the addon misses the package the application actually imports. Reading the platform list off the wrapper's manifest answers that directly.

A reference to `__dirname` is deliberately **not** treated as a signal. Many published bundles mention it in code paths that never touch disk, and no comparable tool uses it to decide.

## What manifests cannot see

Some packages are pure JavaScript and still cannot be bundled. They declare nothing that sets them apart — the behaviour that defeats bundling only appears at run time:

| package | what it does |
| --- | --- |
| `pino`, `thread-stream` | start a worker from a path built at run time |
| `pino-pretty`, `pino-roll` | are named as a string and required inside that worker |
| `keyv` | requires a storage backend chosen from a connection string |
| `config` | requires a dependency it does not declare |
| `import-in-the-middle`, `require-in-the-middle` | patch the module loader, which a bundle has already resolved past |

No rule that reads declarations can reach these, so Nub carries a list. Every project that set out to avoid one still ships it — Next.js maintains 79 entries after years of investment in static analysis.

The list matches exact names. A prefix or substring match would quietly unbundle `pino-http` and `keyv-redis`, which are ordinary packages, and that failure is silent: the package loses its tree-shaking and nothing breaks.

## When Nub gets it wrong

Two flags override the decision, because no detector reaches every package:

```bash
nub compile app.ts --unbundled some-package   # ship it, but do not bundle it
nub compile app.ts --bundled some-package     # bundle it after all
```

Use `--unbundled` for a package that loads a file by a path it builds at run time and is not yet recognised. Use `--bundled` for the reverse — a package needlessly ejected, which costs startup and size while failing nothing.

Neither is `--external`, which leaves a package out of the binary entirely, to be resolved on the machine that runs it.

## Cross-compilation

Building for another platform is bounded by one fact: **an installed dependency tree holds the build machine's binaries.**

- A package shipping prebuilt addons for several platforms contributes the one matching the target; the rest are dropped from the artifact.
- A package with no addon for the target fails the build, naming the platform found, the platform wanted, and the `supportedArchitectures` settings that install a foreign tree.

Both halves matter. Rejecting every foreign addon fails ordinary packages, since a package carrying a Windows prebuild beside a macOS one is perfectly healthy. Skipping every foreign addon ships a package with nothing loadable and defers the failure to the user's machine.

Verified by building on macOS and running the result on Linux. A package shipping prebuilts for eight platforms contributed the two matching an arm64 Linux target, and the artifact ran there unmodified.

One caveat applies to the finished binary rather than the build: on Linux the embedded Node links `libatomic`, so a minimal container image without it fails at exec with `libatomic.so.1: cannot open shared object file`. Installing `libatomic1` resolves it.

## Verification

A compiled artifact is exercised by deleting `node_modules`, running the binary from an unrelated directory, and checking its output:

```console
$ rm -rf node_modules
$ cd /tmp && ./app
ok
```

The demanding case is `sharp`. Its addon lives in one package and the shared library it links against lives in another, so it works only if both sit at the paths they were installed to — which is what shipping packages in place provides.

## Startup

Compare a compiled artifact against running the same bundle on an installed Node, rather than against an empty script: an empty script measures Node's floor and charges Nub for work the application would pay under any bundler.

Measured on a hello-world program, warm, taking the minimum of forty runs and subtracting the harness floor:

| | |
| --- | --- |
| `node -e 0` | 27.0 ms |
| a neutral bundle on installed Node | 29.8 ms |
| the same program's bundle on installed Node | 35.0 ms |
| the compiled artifact | 41.6 ms |

The artifact costs 11.8 ms more than bundling the program and running it on an installed Node, of which 6.6 ms is the launcher and the second process it starts. Node's own startup accounts for most of the remainder and is not something Nub can reduce: the artifact runs stock Node.

V8 startup snapshots do not help. An empty snapshot measured slower than no snapshot, because Node already applies its own built-in snapshot and the extra blob is another large file to read.
