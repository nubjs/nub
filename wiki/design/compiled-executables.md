# Compiled executables

The `nub compile` command turns an entry file into a single executable that runs with no Node installed and no `node_modules` on the target machine.

This document covers how the artifact is put together: what goes inside it, what deliberately stays as ordinary files, and how the two are told apart.

## Self-extracting, not virtual

A Rust launcher carries an embedded payload, extracts real files into a content-addressed cache on first run, and executes the user's Node against them. Subsequent runs find the cache already populated.

The alternative — a virtual filesystem, so nothing touches disk — is not available to Nub. Node's module resolver reaches the filesystem through native bindings rather than the JavaScript `fs` module, so a JavaScript-level patch cannot intercept `require()`. Bun and Deno can intercept beneath JavaScript because they own the runtime; Nub runs the user's stock Node and applies no patches to it. Deno itself shipped a self-extracting mode after several years of maintaining a virtual filesystem.

A virtual filesystem does not reach a native addon in any case, because the platform dynamic loader opens a real path. Measured against Bun 1.3.14 on macOS arm64, which owns its runtime and does have one:

| what the program uses | what reaches disk |
| --- | --- |
| pure JavaScript | nothing — the code is served from a virtual root at `/$bunfs/root`, which does not exist on disk |
| a native addon | the addon is written to the temporary directory, `dlopen`ed, and **unlinked immediately** |

The unlinked file is absent from a directory listing while the process is still running, so only the open-file table shows it, as a mapped region. Two runs produced two different names, so the write repeats rather than caching. Owning the runtime therefore buys a virtual filesystem for code and not for addons — which is the same boundary that makes Nub ship an addon-bearing package as real files.

Two shapes:

- **embed** (default) — carries a compressed Node binary, so the artifact is self-contained.
- **`--smol`** — carries no Node, and discovers or provisions one at run time.

## The payload goes in a section, not on the end

Every executable format already has somewhere to put arbitrary bytes: a Mach-O section, an ELF section, a Windows resource. The payload goes there, and the launcher reads it back the way the loader would.

The obvious alternative is to append the payload past the end of the file and search backwards for a marker. It is less code and it works — until the binary has to be signed. A signature covers the file, so appending invalidates it, and macOS refuses to execute a binary whose signature does not match. Tools that took that route spent years on the consequences: `pkg` never resolved it, and `nexe` closed the problem as not planned.

A section avoids the whole class. The bytes are part of the image the linker describes, so the artifact can be signed afterwards and stays signed. On macOS Nub signs it, and `codesign -v` passes on a compiled binary and again after re-signing it yourself.

Nub uses [libsui](https://github.com/denoland/sui) for this, vendored under `vendor/libsui`, with one change: it wrote a section for arm64 but appended for x86_64, so Intel artifacts carried the fragile shape and Nub's verification did not recognise them at all. Both architectures now take the section path.

## Bundle everything possible, and no more

Startup cost is dominated by file count, not file size. Measured on synthetic trees holding total bytes constant while varying the number of modules:

| what is loaded | cost per file |
| --- | --- |
| a plain relative module | ~60 µs (CommonJS), ~79 µs (ESM) |
| a package in `node_modules` | ~160 µs |

The extra cost of a package is its `package.json` open and parse. The relationship is linear out to a thousand files, and per-byte cost is roughly 18 ns — so splitting a bundle into more files is close to byte-neutral and costs only in file count.

Hence the rule: every module that can be bundled is bundled. A package that cannot be ships **exactly as it sits on disk**, under `node_modules/` beside the bundle, at the path it already occupied.

The question is asked of **every package in the ejected package's dependency closure, one at a time**. It used to be asked once, at the root, and the rest of the closure inherited the answer — an inheritance nothing justified. `pdfkit` has to ship as files because it reads its fonts through `__dirname`; `fontkit` never did, and `@swc/helpers` was shipping 438 files so that two of them could be loaded. On a `pdfkit` program the payload goes from **1204 files to 249**, and on `geoip-lite` from **166 to 27**, with byte-identical output.

The packages that come back clean are bundled into chunks written where Node will look for them — `node_modules/fontkit/__nub/0.js`, beside a small `package.json` whose `exports` map names every specifier the closure answers. Two refusals keep that honest, and both give up the whole closure rather than half of it:

- A package that **computes** a specifier gets its closure shipped verbatim. A stub answers the specifiers a static scan found; it cannot answer one it never saw, and the failure would be a clean build that dies on the user's machine.
- A specifier naming something a JavaScript chunk cannot be — a stylesheet, or JSON reached through `import`, which Node validates by the resolved file's extension — does the same.

Where two closures share a helper, it moves into a shared chunk at the payload root, and there the file **extension is the only thing that says how to read it**. It must therefore be `.cjs` or `.mjs`, never `.js`: `.js` is the one extension that does not decide, so Node resolves it against the nearest `package.json` — and the payload has none, so the answer comes from whatever directory happens to sit above the extraction cache. Point the cache inside a `"type": "module"` project and every shared chunk is read as ESM, so the first `require` in it throws and the binary dies after a build that reported success.

An application using express, helmet, morgan, winston, axios, lodash, uuid, ws, jsonwebtoken and bcryptjs installs 122 packages and compiles to **six files, with nothing ejected** — about 330 KB more than a program that only parses a schema. Dependency count is not what a compiled artifact pays for.

Nothing is relocated, flattened, or rewritten for a package that stays. That is the point. A package left in place keeps everything its author relied on:

- `__dirname` resolves where they expected.
- A sibling package it reaches by walking up the tree is still there.
- An addon it locates by building a path at run time is at the end of that path.

Node's ordinary resolution does the rest.

### Two install shapes, one of which cannot be copied verbatim

That holds exactly as written for a flat `node_modules`, where every package already sits somewhere its dependents reach by walking up.

An isolated install — what `nub install` itself produces — is shaped differently. `node_modules/sharp` is a symlink, the real package sits at `node_modules/.store/sharp@0.35.3/node_modules/sharp`, and its dependencies sit beside it there rather than hoisted. Node copes because it resolves symlinks: code loaded through the link runs from the real directory and finds its dependencies as neighbours.

A payload cannot reproduce that. It carries files, not symlinks — the launcher treats a symlink in an extracted tree as tampering — so the link would become a copy, and a copy is not where its neighbours are.

So a dependency is placed where its dependent can actually reach it. A flat install already satisfies that and its packages keep the exact paths they occupy on disk. Under an isolated install a dependency nests directly under the package that declared it, which resolves from wherever that package lands.

Something two packages both depend on is placed once, not under each of them. That needs the walk to go breadth-first — a shallower placement is on more dependents' lookup paths, so putting it down first is what lets the deeper one be dropped — and it needs packages identified by their real directory, since a store reaches one package through several symlinks. For sharp this is 18 MB of libvips that no longer lands on disk twice.

The distinction is easy to miss because a flat tree hides it: npm hoists transitive dependencies to the top level, which is exactly where a lookup through the symlink would search. A resolver reading the wrong path therefore works on every npm-installed fixture and ships a package with none of its dependencies from an isolated one — a binary that builds clean and fails at run time with `Cannot find module`.

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
| depends on `node-gyp-build`, `bindings`, `node-pre-gyp`, `@mapbox/node-pre-gyp`, `prebuild-install`, `node-addon-api`, or `nan` | the package locates or compiles native code |
| two or more optional dependencies named after itself plus a platform | a napi-rs package with per-platform binaries |
| `gypfile`, a `binary` block, or an install-phase script | the package builds a native artifact on install |

Checked against published manifests, these select `bcrypt`, `sqlite3`, `canvas`, `better-sqlite3`, `cpu-features`, `isolated-vm`, `fsevents`, `sharp`, and `@node-rs/argon2`, and leave `express`, `keyv`, `pino`, and `zod` to be bundled.

Precision matters as much as recall here, and it fails silently: a package ejected for no reason still works, it just loses its tree-shaking and adds files nobody notices. Across a 73-package tree exercising twenty-two ordinary libraries — `lodash`, `date-fns`, `zod`, `ajv`, `handlebars`, `ejs`, `marked`, `validator`, `commander`, `uuid`, `semver`, `qs`, `mime-types` among them — nothing was ejected and the artifact carried six files.

The napi-rs signal earns its place. Such a package is a JavaScript-only wrapper whose per-platform sidecar holds the addon — `sharp` contains no `.node` file of its own — so following dependencies forward from the addon misses the package the application actually imports. Reading the platform list off the wrapper's manifest answers that directly.

A reference to `__dirname` is deliberately **not** treated as a signal. Many published bundles mention it in code paths that never touch disk, and no comparable tool uses it to decide.

## What manifests cannot see

Some packages are pure JavaScript and still cannot be bundled. They declare nothing that sets them apart — the behaviour that defeats bundling only appears at run time:

| package | what it does |
| --- | --- |
| `pino`, `thread-stream` | start a worker from a path built at run time |
| `jsdom` | reads its default stylesheet from a path built at run time |
| `pdfkit` | reads its built-in fonts from a path built at run time |
| `geoip-lite` | reads its address database from a path built at run time |
| `sql.js` | reads its WebAssembly module from a path built at run time |
| `pino-pretty`, `pino-roll` | are named as a string and required inside that worker |
| `keyv` | requires a storage backend chosen from a connection string |
| `config` | requires a dependency it does not declare |
| `import-in-the-middle`, `require-in-the-middle` | patch the module loader, which a bundle has already resolved past |

No rule that reads DECLARATIONS can reach these, so Nub carries a list — but a rule that reads the package TREE reaches some of them, and the four data-file entries below are now detected rather than merely listed (see the next section). A list is where the ecosystem lands when analysis runs out: Next.js still maintains 79 default entries after years of investment in static analysis, in `packages/next/src/lib/server-external-packages.jsonc`, read as `EXTERNAL_PACKAGES` by its webpack config. A list is not where it has to stay, though — the target is one that shrinks as detection improves.

Importing those 79 entries wholesale was measured and rejected. Roughly half are native packages the manifest rules already catch; a third are build tools Next externalizes to keep a dev server's rebuilds fast rather than for correctness; and the list names `express`, which bundles correctly and whose eject cost a compiled server 0.5 MB and its tree-shaking for nothing. An entry that is never imported is free, because the classifier only runs on a package the bundler actually resolved — but an entry that IS imported is not, so entries arrive one at a time with a reproduction.

The list matches exact names. A prefix or substring match would quietly unbundle `pino-http` and `keyv-redis`, which are ordinary packages, and that failure is silent: the package loses its tree-shaking and nothing breaks.

### Files a package reads beside its own source

A package that ships a data file next to its source — a lookup table, a WebAssembly module, a font — declares nothing unusual, so it is bundled. Whether that works depends on how it names the file, not on what the file is:

| in the package's source | outcome |
| --- | --- |
| `new URL('./table.txt', import.meta.url)` | the file is emitted into the payload and the reference rewritten |
| `path.join(__dirname, 'table.txt')` | bundled, and `__dirname` is now the payload root — the read fails |

The first form is a static reference the bundler resolves; the second is a path computed at run time, the same thing that defeats detection for native addons. Verified with the identical text file in both forms, and a WebAssembly module through the first.

The failure is at run time, on a clean build:

```
Error: ENOENT: no such file or directory, open '.../compile-app/64119dd9/table.txt'
```

Bun has the same limitation and fails less usefully: its `__dirname` still points at the build machine's `node_modules`, so the artifact works where it was built and fails everywhere else.

`--unbundled` is the remedy — the package ships in its installed layout with its files beside it, and `__dirname` points where its author expected.

This form IS detected, and it is the one case where reading the package tree earns its cost. The rule convicts on two facts together: the package builds a path at run time, AND it ships a file that is not code. Either alone is ordinary — plenty of packages mention `__dirname` without reading anything, and every package ships a README — so neither is a signal by itself.

Requiring both to meet is what keeps it precise. Measured: `openai` (2710 files, 1210 of them not code) and `rxjs` (2277 / 1022) are bundled untouched, and a three-way control confirms the discrimination — a package that only mentions `__dirname` is left alone, a package that only ships a data file is left alone, and only the one doing both is ejected.

It costs nothing when it is not needed. The manifest rules run first because they are free; the tree is read only for a package no declaration has already settled. The diagnostic names both halves, so a wrong verdict says what convinced it: ``index.js`` builds a path at run time and it ships ``table.dat``, which is not code.

The older reasoning against this — that it would mean the source analysis the manifest rules exist to avoid — measured the wrong thing. Measured against an 83-package tree, six packages mention `__dirname` at all and exactly one reaches the filesystem with it — `pino`, already on the list above.

Four packages found this way — `jsdom` reading its default stylesheet, `pdfkit` its built-in fonts, `geoip-lite` its address database, `sql.js` its WebAssembly module — are also named on the list above. They are kept there deliberately: the list is the belt to the detector's braces while the detector is young, and a name costs nothing to carry.

Run over 473 installed packages, the rule ejects 8 that genuinely read a shipped file and 1 that does not. The 8 include six on no list anywhere — `tiktoken`, `esbuild-wasm`, `web-tree-sitter`, `yoga-wasm-web`, `tesseract.js-core` and `@jimp/plugin-print` — which is the argument for detecting over listing: each of those compiles clean today and dies on first use, and none was going to be found except by someone hitting it. `tiktoken` was confirmed end to end: before, `Error: Missing tiktoken_bg.wasm`; after, output identical to plain Node.

The one false positive was `fontkit`, and closing it is what the scan's reachability rule exists for. Its `src/opentype/shapers/*.js` build paths from `__dirname`, but `main` and every `exports` entry name `dist/`, which inlines those tries — so Node never loads the convicting files. A top-level directory is now skipped when **nothing published names it and no entry file so much as mentions it**; the second condition is what makes this safe, since a package that requires `./src/x` from its `dist/` bundle keeps `src/` scanned. The check is textual on purpose: a mention inside a comment keeps a directory scanned, which is the direction to be wrong in, because over-scanning costs one package's tree-shaking while under-scanning ships a binary that fails on a user's machine. Measured on the case that motivated it, `fontkit` goes from 131 scanned files to 3, while date-fns `_lib`, restructure `src`, iconv-lite `encodings` and better-sqlite3 `prebuilds` are each mentioned and stay in. Across a 200-package tree the set of packages ejected is **identical** before and after: it changes how much of a package ships, never which packages ship.

Three narrowings each removed a package that works fine, and all three are load-bearing. The base expression must be handed to something that BUILDS a path, or `ejs` ejects on the `__filename` token it concatenates into template source it is compiling. A dependency's own `bin/` and `scripts/` are skipped, or `ejs` and `mathjs` eject on a CLI reading its `usage.txt` — which an importing application never loads. And license and notice files are not payload, or one `ThirdPartyNotices.txt` ejects a package that reads nothing.

What it still cannot see is the other half of the list: a package that names a module by a string it computes. `keyv` picking a backend from a connection string, `config` requiring a dependency it never declares, `thread-stream` starting a worker from a path it is handed — none reads a shipped file, so no amount of asset detection reaches them. Those stay curated.

### A dynamic import carrying import attributes

`import("./data.json", { with: { type: "json" } })` is the worst shape available, and is refused at build time. It is the one case where the bundler does half the job and the half it skips is invisible until the binary runs on another machine.

The bundler FOLLOWS the import and emits the module, then leaves the original specifier in the output. The payload gains an orphan chunk nothing names, the call resolves against the extraction directory at run time, and the file is not there — after a build that reported success. The static form of the same import is bundled correctly, which is what the diagnostic points at. `--allow-dynamic-import` keeps it as written, and the binary then resolves it from the directory it is started in.

### Data formats the runtime and the compiler both read

Nub's runtime loads `.jsonc`, `.json5`, `.toml`, `.yaml`, `.yml` and `.txt` as data imports, and the compiler reads the same table rather than restating it.

The five the bundler does not handle natively are parsed at build time and inlined as a JSON literal, so no parser reaches the artifact. How that stayed one implementation rather than two is [One parser, not two](#one-parser-not-two).

**Superseded.** These five formerly failed the build, and this section recorded that as an open gap — a program that ran under `nub` could not be compiled.

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

The rule is about the whole dependency closure, not one package. Where `better-sqlite3` keeps every platform's prebuild in a single directory, a napi-rs package puts each platform in its own sidecar — so `@img/sharp-linux-arm64` contains only a Linux addon and is simply not the sidecar a macOS build uses. Judging each package alone made the failure exactly backwards: installing the target's sidecars, which is what a cross-build requires, then failed every build including one for the host.

A sidecar that rules the target out is dropped whole rather than merely stripped of its addon, because most of one is the shared library beside the addon. `os`, `cpu` and `libc` are the same fields npm and pnpm read to decide whether to install an optional dependency, so the package states the answer itself: `@img/sharp-darwin-arm64` declares `os: ["darwin"]`, and the musl build adds `libc: ["musl"]`. Absent fields mean it runs anywhere. For sharp this is the difference between 52 MB and 38 MB.

Where pnpm or yarn is the incumbent, its `supportedArchitectures` setting installs a foreign dependency tree. Other projects can select the target for one install:

```bash
nub install --os linux --cpu arm64 --libc musl
```

If no prebuilt exists for the target, install on that platform, usually in a container.

Verified by building on macOS and running the result on Linux and on Alpine. A package shipping prebuilts for eight platforms contributed only the ones matching each target, and the artifact ran there unmodified. The 122-package application above cross-compiles the same way and runs on a Debian image with no Node installed, and on Alpine.

A glibc build and a musl build of the same addon are indistinguishable from the ELF header, which records machine and operating system but not which C library. Both therefore satisfy a platform check, both travel, and the loader picks whichever it finds first — on Alpine that was the glibc one, which cannot load. Nub tells them apart by the symbols they carry: a glibc build has versioned symbols such as `__cxa_finalize@GLIBC_2.17`, a musl build names `libc.musl-<arch>.so.1`. An addon carrying neither marker still travels, since refusing something merely unclassifiable would reject working packages.

### System libraries on the target

The binary carries its own Node, but Node and native addons link a few system libraries, and a minimal container image can lack them:

| target | typically needed |
| --- | --- |
| Debian, Ubuntu | `libatomic1` |
| Alpine | `libgcc`, `libstdc++` |

When Node itself cannot start for this reason, Nub names the missing library and the package that provides it rather than passing the loader's message through. An addon that fails the same way is reported by Node, which names the library but not the package to install — the failure happens inside a process Nub has already handed control to.

## Verification

A compiled artifact is exercised by deleting `node_modules`, running the binary from an unrelated directory, and checking its output:

```console
$ rm -rf node_modules
$ cd /tmp && ./app
ok
```

`sharp` is the demanding case. Its addon lives in one package and the shared library it links against lives in another, so it only works if both are present at the paths they were installed to — which is what shipping packages in place provides. Compiled on macOS for Linux, with the Linux sidecars installed, the artifact runs on a Debian image with no Node present.

Two harnesses under `tests/compile-corpus/` do this continuously. One varies **which package** is compiled, across pure JavaScript, node-gyp, node-pre-gyp and napi-rs packages. The other varies **the shape of the tree** it sits in — a nested duplicate version, a scoped package, a symlinked workspace member, an isolated install, a peer dependency, and a package that reads a data file. The second axis is the one that finds path defects, because it inspects the payload rather than only running the artifact: a tree shape can produce a collision that no ordinary package would, and the binary still exits 0 printing the right answer.

### Platform coverage

Nub publishes eight targets. Each is verified by building an artifact and running it, rather than inferred from a sibling that shares an operating system or an architecture.

Two gates do this, and they are not the same gate. The table below is the **development loop** — what a macOS host can reach itself. The **release gate** is separate and stricter: `release.yml` compiles and runs a fixture for all eight targets, each on a runner of that architecture, before anything publishes. There is no Rosetta and no cross-built leg in it — `darwin-x64` runs on `macos-15-intel`, `linux-arm64` on `ubuntu-24.04-arm`, and the two musl targets in Alpine containers on the matching host. The fixture loads a real `.node` addon, and the macOS legs assert `Signature=adhoc` through `codesign`.

Between releases, `compile-native.yml` covers five of the eight on every push — `darwin-arm64`, `linux-x64`, `win32-x64`, `win32-arm64`, and `linux-x64-musl` through a container. The remaining three are gated at release only.

| target | how the development loop verifies it |
| --- | --- |
| darwin-arm64 | native, plus the native-islands gate |
| darwin-x64 | cross-built, run under Rosetta, signature checked |
| linux-x64, linux-arm64 | cross-built, run on an image with no Node installed |
| linux-x64-musl, linux-arm64-musl | cross-built, run on Alpine |
| win32-x64 | native on CI, including a renamed host binary, an ejected addon and an ejected data file |
| win32-arm64 | native on a Windows-on-ARM runner |

Two of these cannot be reached from a macOS development host, which is why they are on CI rather than in the local loop. A Windows-on-ARM launcher needs a toolchain that host does not have, and the Intel macOS artifact is signed by shelling out to `codesign`, which only exists on macOS.

The arm64 Windows job asserts the architecture it landed on before it does anything else. A runner labelled for one architecture and provisioned as another would otherwise report a pass for a target that was never exercised, which is the failure this whole section exists to rule out.

### The two shapes, and what `--smol` trades away

Both shapes are exercised across the same four kinds of package — one loading a native addon, one reading a data file, one reading a WebAssembly module, and one pure JavaScript. Each artifact must reproduce the program's output on plain Node.

| what the program uses | embed | `--smol` |
| --- | --- | --- |
| a native addon | 30.6 MB | 3.6 MB |
| a data file | 33.0 MB | 5.9 MB |
| a WebAssembly module | 30.9 MB | 3.9 MB |
| pure JavaScript | 28.1 MB | 1.1 MB |

The difference is the compressed Node, so it is roughly constant and dominates every artifact that is not itself large.

**Those are the sizes of the file you ship, and they are not the size of what the artifact occupies once it runs.** The embedded Node is stripped and compressed at about 5:1, so an embed artifact that is 27.9 MB on disk expands to roughly **102 MB** in the extraction cache on its first run — almost all of it the decompressed Node, and the program itself a rounding error beside it. Quote the two numbers together. A runtime that carries its interpreter uncompressed has a larger artifact and no expansion, and comparing only the shipped file flatters the shape that defers the cost. `--smol` is the shape with no such asterisk: **under 1 MB** shipped, because the Node is neither carried nor expanded — it is the machine's own.

What `--smol` trades away is only visible on a machine that has no Node, which is also the machine the shape exists for. Built on macOS for Linux and run on an image with no Node installed:

- With `curl` or `wget` present it provisions a Node and runs, native addon and all.
- With neither, it refuses and says so, naming the two commands and pointing at the embed shape, which needs no download.

That is the whole trade: a binary an order of magnitude smaller, in exchange for a first run that reaches the network. An embed artifact carries everything and never does.

## Carrying Nub's own augmentations into the artifact

A compiled binary has to reproduce what `nub <file>` does, not merely what `node <file>` does — otherwise compiling a program silently removes the runtime it was written against.

The augmentations reach an artifact through **three** mechanisms, chosen per augmentation by when the information is available.

| kind | examples | how it travels |
| --- | --- | --- |
| **Transform-time** | TypeScript, JSX, decorators + metadata, data-format imports | the bundler applies it; the result is in the payload |
| **Polyfill** | Temporal, URLPattern, float16, browser-shape Worker, navigator + locks, the `abort-controller` clobber | `runtime/compile-preamble.mjs` is bundled in; `__nub_compile_bootstrap.cjs` hands it the builtin accessors it needs, as a `--require` preload when the payload reaches `child_process` or a worker and otherwise from a path the launcher passes in the environment. A polyfill the target has natively is stripped from the bundle; one it lacks (Temporal, URLPattern, Float16Array) is installed as a lazy global whose package evaluates on the first read, never at start. On a target with Node's compile cache (22.1+) the chunk is shaped so the cache holds the runtime's startup functions after the first run, rather than their being compiled from source on every start |
| **Node flag** | `--experimental-sqlite`, `--experimental-websocket`, `--experimental-eventsource`, `--experimental-webstorage`, `--experimental-wasm-modules`, `--enable-source-maps` | **computed at RUN time by the launcher**, through the same `flags::compute_inject_flags` the `nub` CLI uses |

The third row is the one worth stating plainly, because baking it would be the obvious wrong answer. Which flags a Node needs is a function of its VERSION, and for `--smol` the version is not known until the machine that runs the binary is known. So the launcher links `nub-core` and asks the same policy at startup, against the Node it is about to spawn. A build-time snapshot would be wrong for `--smol` and would rot for embed the moment the flag bands moved.

`--node-options` is a separate thing from all of this and is not an alternative to the `NODE_OPTIONS` environment variable. The two serve different people: the variable belongs to whoever RUNS the binary and is already honored at startup; the flag belongs to whoever BUILT it, for a program that cannot work without an option its users cannot be expected to set. All three sets apply, in order — computed, then baked, then the runner's.

### The syntax level travels too

A transform is not only about what the source *is*; it is also about what the target can PARSE. Getting that wrong produced this failure class in its purest form: a green build, and a `SyntaxError` on the user's machine.

`nub <file>` transpiles to `es2022`, which down-levels `using` and `await using` into the `usingCtx` helper because Node 22 cannot parse the declaration at all. The bundler was originally given no syntax target, so it emitted whatever the source used and a Node 22 artifact died — an augmentation the runtime made and the compiler did not.

The target names an **ES year**, not the target's Node version, and that is not the weaker choice — it is the only one that works. Oxc's `EngineTargets::from_target` inserts a default ES engine beside whatever you name, and its feature lookup returns on the ES entry as soon as it encounters one, so a bare `node22.15` parses, is accepted, and lowers nothing.

It is also not gated on the target version. Matching the runtime unconditionally is what makes the guarantee hold in the direction that matters: there is no Node for which the compiler lowers *less* than `nub <file>` would. The cost is one small helper on a target new enough not to have needed it.

### Verified by differential, not by reading

Each augmentation is a fixture run twice — once as `nub <fixture>`, once as the compiled artifact — and the two outputs compared.

Locally it is run against Node 26.5 and again against 22.15, the band where the flag injection is load-bearing rather than redundant. The 22.15 leg needs its own positive control: on plain Node there, `EventSource`, `vm` modules and Web Storage are all absent, which is what makes the artifact reproducing them evidence rather than coincidence.

Continuous integration runs one version, not the band — the harness takes the runner's own Node and writes it into `.node-version`, so the plain-Node control, the reference run and the artifact are the same build with nothing to provision. It runs both shapes there, since `--smol` computes its flag set against a version the build never saw. The two-version sweep therefore remains a manual step, and the lower band is the half that goes unexercised between releases.

Named fixtures can only test what someone thought to name, and `compile-preamble.mjs` is a second implementation of what the run-time preload installs — so the failure to expect is it falling behind quietly, one global at a time. One fixture therefore names nothing: it digests every own property of `globalThis` plus the members of the builtins Nub patches rather than replaces, and the harness compares that digest against the reference run. A polyfill added to the preload and not the preamble fails it without anyone having predicted the polyfill. Measured today the two agree exactly — 22 additions over plain Node on 22.15, 9 on 26.5, none missing and none extra.

### One parser, not two

Data-format imports are transform-time, and the compiler must not grow a second implementation of them. Two parsers behind one syntax would let a document mean different things depending on whether it was run or compiled.

The runtime already parses YAML, TOML, JSON5 and JSONC **in Rust**, through `nub-native`'s `parseYaml`/`parseToml`/`parseJson5`/`parseJsonc`, with the JavaScript libraries only as a fallback when the addon is missing.

So the four parsers and their depth and alias-expansion guards move into a small crate both sides depend on: `nub-native` keeps its `#[napi]` wrappers, and the compiler's loader plugin calls the same functions. `nub-native` is its own workspace, but the shared crate is a sibling under `crates/` rather than beneath it, so an ordinary path dependency reaches it from both. The extension table stays the runtime's — the compiler reads it rather than restating it, so the two surfaces cannot disagree about what `.yaml` means.

## What deliberately does not survive

Erasability is a claim about the program's own behavior, not about every surface the `nub` command has. Four things are absent from an artifact by construction, and naming them is the point — an unlisted absence is indistinguishable from a defect.

**The `nub` command itself.** The package manager, `nub node` version management, `nub upgrade`, `nubx`. An artifact is the user's program, not a copy of the CLI that built it.

**Watch-mode surfaces.** `import.meta.hot` is a committed type-only shape that is `undefined` unless `nub watch --hot` is active. An artifact is never in watch mode — but neither is an ordinary `nub app.ts`, so the artifact matches the reference run exactly and the documented `if (import.meta.hot)` guard behaves identically either way. This is an absence in the same sense that it is absent from any non-watch run, not a compile-specific gap.

**Configuration read at run time.** Covered in full by strict compilation below: every config surface is honored when the artifact is BUILT and read by nothing when it runs.

**A child process started from a path the compiler was not told is code.** A `Worker` is a second entry point and is treated as one — `new Worker(new URL(…, import.meta.url))` becomes a real chunk behind a generated wrapper, so the worker thread gets its TypeScript transpiled and the preamble installed inside the thread. Verified on Node 22.15, where a raw worker source would not run at all: `enum` erased, `Temporal` and `reportError` both present in the thread. Bun documents the same case as one it does not yet handle.

`child_process.fork(…)` is the shape that does not get this, and the difference is information rather than effort: `new Worker` names its argument as code, while a path handed to `fork` is a string that the compiler cannot distinguish from a path to data. Such a file is embedded as a **data asset** — shipped verbatim, never transpiled, its own imports resolving against an extraction directory with no `node_modules` — so reading it works and executing it does not, and a forked child runs without the augmentations its parent has. The build says so, naming the file and pointing at the `Worker` shape that is bundled properly. Chasing the general case means statically resolving arbitrary spawn arguments, which is unbounded; the warning is the deliberate stopping point.

**Node version selection.** The version is decided at build time — from the project's pin — and an artifact does not reconsider it. That is worth stating for `--smol` specifically, since it is the shape that finds a Node at run time and could plausibly have consulted the ambient project: a `--smol` binary built against 26.5.0 and then run inside a directory pinning 24.18.1 still reports `26.5.0`. The pin is an input to the build, never to the run.

### The self-identification marker is withheld

`process.versions.nub` means "Nub's augmentation is active in this process" — that is the contract the runtime's own tests pin, and it is why `--node` and `NODE_COMPAT` both withhold it. A compiled artifact withholds it too.

That is a genuine fork rather than an oversight, because the artifact's augmentations really are active: the fixtures reproduce 22 polyfilled globals on Node 22. A library feature-gating on the marker therefore takes the plain-Node branch inside a compiled binary and gives up capability that is actually present. The other reading still wins — a compiled artifact is a standalone program running on stock Node, not a `nub` process, and the marker exists to detect the latter. Anything gating on it should feature-detect the capability instead, which gives the same answer either way. A fixture pins the difference so it cannot drift back silently.

### Binary asset imports stay compile-only

The compiler gives twenty-three extensions a default the runtime does not accept, so `import icon from "./icon.png"` runs inside a binary and throws `ERR_UNKNOWN_FILE_EXTENSION` under `nub app.ts`. The asymmetry is deliberate, and looks like a gap.

The twenty-three are `.md` as text, plus twenty-two binary ones — images, fonts, media, wasm, `.zip`/`.bin`/`.pdf` — as a path.

Teaching the runtime the same defaults was considered and declined for three reasons. There is already a portable way to reach these files — `readFile(new URL("./icon.png", import.meta.url))` returns the same bytes under `nub` and inside a compiled binary, verified including with the source tree deleted, because the URL form is what the compiler embeds. Adding an import spelling would be a second mechanism for something that has one. And the two sides are not symmetric in what they owe: a bundler must decide something for every import it meets, so a default beats an error, while a runtime that resolves `./icon.png` to a path string is inventing module semantics no specification describes.

What that leaves is a real cost — the failure lands during development, before anyone reaches a build — so the compile docs say plainly that these extensions are compile-only and point at the URL form. Reversing this is a small change if the ergonomics turn out to matter more than the duplication.

### No public "am I a compiled binary?" API

Bun ships `Bun.isStandaloneExecutable`, and the equivalent was considered and declined. The reason is architectural rather than a matter of taste: what makes a program need to ask is usually a virtual filesystem, and Nub does not have one.

A VFS is where a path that worked in development stops resolving once bundled. Nub extracts real files and sets `process.execPath` to the binary, so the branch a Bun program writes here mostly has nothing to select between.

Anyone who genuinely needs the answer already has it, and the correct form is an identity test rather than a presence check:

```js
const standalone = process.env.__NUB_COMPILED_EXEC_PATH === process.execPath;
```

Presence alone is wrong because the variable is inherited: a `child_process.fork` child sees the parent's value while its own `execPath` is the extracted Node, so it would claim to be a standalone executable. Comparing the two is `true` only in the binary's own process. The variable stays underscore-prefixed and undocumented on purpose — it is the launcher's private identity channel, carrying semantics beyond a boolean (an empty string means explicitly-not-compiled, and a `Worker` recovers it from cloned state instead), and publishing it would freeze all of that as a compatibility contract to serve a need nobody has yet reported.

### Strict compilation

The policy in one line: **every configuration surface is honored when the artifact is BUILT, and read by nothing when it runs.** A compiled program's behavior is a property of the binary, not of the directory someone happens to launch it from.

This is worth stating as a policy rather than leaving as an accident, because the obvious alternative is what a neighbouring tool shipped and then regretted. Bun's compiled binaries autoload `.env` and `bunfig.toml` at run time, with `tsconfig.json` and `package.json` behind opt-in flags, and its own documentation says the first two "may also be disabled by default for more deterministic behavior" in a future version. Nub takes that endpoint directly and offers no opt-ins in either direction — a flag that reintroduces run-directory dependence is the thing being avoided, so there is no flag.

Verified by running an artifact in a directory carrying a hostile `.env`, `tsconfig.json`, `package.json`, `nub.jsonc`, `.npmrc` and `.node-version` at once: a `.env` that would inject a variable, a tsconfig that would change the JSX and target settings, a package.json redirecting `main`/`exports`/`imports`, and a Node pin naming a different version. The output is identical to the same binary run in an empty directory. The `nub` CLI in that same directory does read `nub.jsonc` and fails on it, which is what makes the artifact's silence evidence rather than coincidence.

One caveat belongs here, because it is a real way to make a binary fail to start and it is Node's rather than Nub's. An artifact whose payload reaches `child_process` or a worker bootstraps through `--require`, and Node synthesizes that preload's parent module at the current directory — so a **malformed** `package.json` sitting there fails resolution with `ERR_INVALID_PACKAGE_CONFIG` before any user code runs. Plain `node --require <anything>` in the same directory fails identically with no Nub involved, and a *valid* `package.json` of any shape has no effect. Inherited Node behavior with a narrow trigger, not configuration being read. An artifact that bootstraps from the preamble instead has no preload and starts normally there (verified with a `{` for a `package.json`: the `Worker`-mentioning binary dies with that code, the plain one prints its output).

## Startup

Compare a compiled artifact against running the same bundle on an installed Node, rather than against an empty script: an empty script measures Node's floor and charges Nub for work the application would pay under any bundler.

Measured on a hello-world program, warm, taking the minimum of 120 runs:

| | |
| --- | --- |
| `node -e 0` | 31.3 ms |
| a neutral bundle on installed Node | 33.3 ms |
| the same program's bundle on installed Node | 38.0 ms |
| the compiled artifact | 45.3 ms |

The artifact costs 12.0 ms more than the neutral-bundle row, and 7.3 ms more than the same program's own bundle — the launcher's cache verification plus loading the Node image it hands control to. Two costs that used to sit in that gap are gone: on Unix the launcher replaces itself with Node via `exec` instead of starting a second process and forwarding signals to it, and an artifact whose module graph is fully bundled — no `--external`, no retained computed `import()`, no verbatim payload file — skips the V8 syntax flags, which when non-default at startup make Node reject its embedded builtin code cache for every internal module compiled afterwards. (Measured while `--js-defer-import-eval` still rode argv; it has since moved to a runtime flip that a program without the syntax never triggers, so on the run path the cost is gone for such programs too.) Node's own startup accounts for most of the remainder and is not something Nub can reduce: the artifact runs stock Node.

That overhead is close to fixed, so hello-world is the worst case for it. The 122-package application starts in about 88 ms, where the extra work is the application's own modules rather than anything the launcher adds. Read the table as a floor the artifact pays once, not as a proportional cost.

### Build time

Compressing the embedded Node dominates a build — around twenty seconds for a ~113 MB Node. The input does not change for a given Node version and target, so the first build pays it and later ones do not.

The compressed bytes are cached under the hash already computed for them, and a cached build produces a byte-identical artifact.

Lowering the compression level is the obvious alternative and is not taken. It would compress several times faster for about a tenth more artifact, and decompression is level-independent so it would cost nothing at run time — but artifact size is what this design is best at, and caching wins the build time without spending any of it.

### The first run is the expensive one

Those figures are warm — the cache is already populated. The first run of an **embed** artifact also decompresses its Node, writes it to disk, and then executes that freshly written file for the first time.

On Linux the whole first run costs a few hundred milliseconds; on macOS it is closer to a second, because the system validates a newly written executable before running it. `--smol` carries no Node, so its first run is close to its warm one.

That matters wherever the cache is not reused. A developer pays it once; continuous integration, containers and short-lived serverless instances start from an empty cache every time and pay it on every run. `--smol` is the shape for those, provided a compatible Node is present or can be provisioned.

Decompressing and writing the Node accounts for about 150 ms of it. The rest is the first execution of that file, which is paid by whichever code runs it first — so moving work around inside the launcher does not reduce it.

V8 startup snapshots do not help. An empty snapshot measured slower than no snapshot, because Node already applies its own built-in snapshot and the extra blob is another large file to read.
