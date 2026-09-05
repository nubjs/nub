# `nub compile` — system design

This document describes how a Nub project becomes a target-specific executable. Keep it synchronized with changes to the compile subsystem.

The implementation spans `crates/nub-cli/src/compile/` for the build, `crates/nub-launcher/` for the runtime launcher, and `crates/nub-core/src/compile.rs` for the shared payload format.

## Compile-time transformation and runtime support

A compiled artifact carries neither Nub's transpiler nor its resolver. What normal execution does through Node's extension surfaces, the build does ahead of time — and what is left is a small runtime the launcher still has to install.

Normal Nub execution augments the user's Node through preloads, module hooks, and injected flags. Rolldown instead transpiles TypeScript and JSX, substitutes target values such as `process.platform` and `process.arch`, and resolves the static module graph during the build.

The runtime is still more than a bare script spawn. Official, unpatched Node runs with version-appropriate flags and the bundled entry, and is handed a fixed internal CommonJS bootstrap that captures fixed-root builtin access before user hooks run; the bundle's preamble restores the runtime globals the compiled program needs. A launcher artifact starts that Node and passes the extracted entry; a single-executable artifact is that Node, and reaches its entry through the blob. The bootstrap is a `--require` preload only when the payload needs it to run before the ESM graph — when the sealed graph reaches `child_process`/`cluster` (the fork identity fix-up) or `Worker`/`worker_threads`. Otherwise, on Node 22.3 and later, the bootstrap's path travels in the environment and the preamble publishes the same record itself, which saves the preload's cost on every start. A single-executable artifact never preloads it, because the bootstrap is already the first thing its blob main runs.

"Unpatched" describes the default and the source, not an invariant of the byte stream: the build already strips the binary and re-signs it on macOS, and `--icu` additionally rewrites its ICU data package in place. Node's own code is never altered.

The preamble is injected into the main root and each supported static worker root. It installs feature-detected polyfills, including the supported Web Storage surface for `sessionStorage`. A polyfill the target Node ships natively is stripped from the bundle before it is built; one the target lacks — Temporal, URLPattern, Float16Array — is bundled but installed as a lazy global, so its package evaluates on the first read of the global rather than at every start. That storage is process-local and does not add browser-origin sharing or unsupported Web Storage APIs.

Static workers must use a file-backed `new Worker(new URL("./worker.js", import.meta.url))` shape through the global `Worker` or a named ESM import from `node:worker_threads`. Statically recognized data URLs, blob URLs, `{ eval: true }`, and CommonJS `require("node:worker_threads")` worker bindings are refused because the compiler cannot turn them into preamble-bearing roots.

## Two shapes

The default shape embeds a Node; `--smol` does not, and finds one at startup instead. That single choice decides the artifact's size, what its cache has to hold, and how much of the target version is settled at build time.

| Shape | Size | Contains | Node comes from |
| --- | --- | --- | --- |
| default (embed) | ~108 MB | a stripped Node carrying the bundled JS and assets in a single-executable blob | it is the binary |
| `--smol` | ~1.0 MB | launcher + manifest + bundled JS + assets | discovered locally, else provisioned |

Both figures are a hello-world artifact on Node 26 for darwin-arm64. Neither floor is the bundled program: a `--smol` artifact is 851 KB of launcher template before it carries a byte of application code, and an embed artifact is a whole Node before it carries one.

### The container the default shape uses

An embedding artifact is a [Node single-executable application](https://nodejs.org/api/single-executable-applications.html): official Node carrying a serialized preparation blob, with a one-byte fuse flipped so Node's own embedder loader runs the blob's main.

Nothing is extracted and no second process is created. The artifact's `process.execPath` is Node's execPath, because the two are one file.

Nub writes that blob and that container itself rather than calling `node --experimental-sea-config`, which would require a host Node of the target's exact version for every cross-build. The format is Node's, and two of its fields move between releases: the header's `mainFormat` byte arrives in 25.7, and blob `execArgv` landed as three separate backports whose bands leave the whole 23.x line without it.

`execArgv` is what decides the container. Nub's runtime flags reach a compiled artifact through that field, and an artifact that could not carry them would diverge from `nub <file>` on the first program that uses one — so a target Node below the backport bands keeps the launcher, as does any payload that needs real filesystem paths (`--external` packages, a retained computed `import()`, `--include`d files, native addons, a traced worker chunk, a linked source map). `--smol` is never a single-executable application: one *is* a Node, and the sub-1 MB shape embeds none.

Two consequences follow the shape. An artifact's Node flags are fixed at build time, where the launcher recomputed them per run — `NODE_OPTIONS` still reaches the process, since the blob's `execArgvExtension` is Node's own `env` default, but a `--no-…` in it no longer subtracts a flag Nub injects, because argv beats the environment. And `--icu` no longer changes the size on disk: the trim rewrites ICU's package in place without shortening it, and the compression pass that used to turn that into a smaller artifact is gone. It still pays on the wire, where the zero-filled remainder compresses away.

### Trimming ICU out of the embedded Node

`--icu=en,de,fr` rewrites the embedded Node's ICU data package in place to hold only the named languages. The default keeps every locale, because a dropped one falls back silently rather than failing.

An official Node is built `--with-intl=full-icu`, so one linked-in package covering ~700 locales accounts for 31.6 MiB of the ~102 MiB stripped binary. The rewrite zero-fills what it vacates, and that padding survives zstd at under a kilobyte.

Where the saving lands depends on the shape, because the rewrite frees bytes without shortening the file. A `--smol` artifact embeds no Node and is unaffected. An embedding artifact is the Node, so its size on disk does not move at all — measured on Node 26 for darwin-arm64, a hello-world artifact is 107.8 MB either way — while compressing it for distribution falls from 28.8 MB to 24.2 MB at zstd-19, and from 39.2 MB to 33.0 MB at gzip -9.

Three properties make the rewrite safe to do after linking. ICU reaches the package through a bare pointer and navigates it by its own table of contents, so nothing records the length a smaller package would contradict. The package announces itself with a magic and a `CmnD` format tag that occur exactly once per binary in all three container formats, so locating it needs no symbol table — which matters because `strip` removes the ELF symbol that would otherwise name it. And only locale-shaped resources are dropped, so the charset converters, break iterators, normalization tables and supplemental resources all remain: no API changes behavior, and a dropped language falls back through ICU's normal chain to `root`.

That fallback is silent, which is why the default keeps every locale and why the trim is never inferred. Two consequences follow the artifact. A trimmed payload never dedups against an official Node in Nub's store — the dedup assumes the embedded and provisioned binaries run identically, and a trim falsifies exactly that — and a trimmed macOS Node must be re-signed, so `--icu` requires `codesign` rather than degrading to an unsigned binary. Because a broken ICU package still answers `--version` before aborting at the first format call, a host-target trim is verified by formatting a date and segmenting a string, not by launching.

The `--smol` launcher downloads through curl or wget, verifies the selected archive against `SHASUMS256.txt`, and extracts it through Nub core's capped archive reader. Unix hosts use the published `.tar.xz`; Windows hosts use the published `.zip`. The verified tree is staged and atomically published under the ordinary Node store before discovery can return it.

### Which Nodes a `--smol` artifact accepts

Selection depends on the target form. An exact version reuses only that Node. A range is enforced in full, upper bound included, but only when the version the bundle was gated on is that range's own minimum. Everything else resolves to a floor.

A range qualifies when its lower bound is representable and floor resolution returns it: `>=22 <23`, `^22`, and wildcards such as `24.x`, whose floor is `24.0.0`. A major or minor pin, an alias, and an upper-only range such as `<23` do not, and any discovered Node at or above the floor qualifies for those.

The split is not cosmetic. The bundle is stripped against the resolved gate, so a form whose gate is not the range's minimum must not have its range enforced — the artifact would otherwise accept a Node whose polyfills it already removed. Two cases miss: an upper-only range, whose gate falls back to the newest matching release, and a range whose minimum is published but carries no artifact for the target, where the gate lands above it. [[crates/nub-core/src/version_management/mod.rs#range_minimum_is]] is the single predicate both sides read.

When no installed Node qualifies, the launcher provisions the newest matching release resolved at compile time — provided that release can run the payload. A bundle carrying a `module.registerHooks` shim needs a Node that has the API, and version order does not imply it: 23.0 through 23.4 sort above a 22.15 floor and satisfy a range built on it, yet predate `registerHooks` on the 23.x line. Where the newest match fails that test the compiler records no preference at all, and the launcher provisions the floor, which the build already gated on the same capability.

## Anatomy of a compiled binary

Both shapes are one host binary with one injected region; which binary and which region differ.

A single-executable artifact is the target's official Node carrying a Node preparation blob. Every other artifact is a target-specific `nub-launcher` carrying a JSON manifest and its payload.

Payload V3 carries the app files, the length each one extracts to, and — for a launcher that embeds a Node — the exact aggregate Node-root `LICENSE` as a compressed notice. The decoder remains compatible with V2 payloads, which record no extracted length, and with V1 payloads, which carry neither that nor the notice. A single-executable artifact carries the same notice as a blob asset instead, reached through the same private `__NUB_COMPILED_LAUNCHER_MODE=licenses` channel.

Injection is format-specific. The blob's segment, section, note and resource names are not Nub's to choose: Node's runtime has them compiled in, and reads itself with them.

- **Mach-O** uses a new section and is ad-hoc-signed by the pure-Rust injector. The signature supplies neither a Developer ID identity nor notarization. It is also not a fixed cost: the CodeDirectory holds one SHA-256 per 4 KiB page, so it scales with the image at roughly 0.78% of it — 8 KB on a `--smol` artifact and 835 KB on a 108 MB embed one. A launcher template's own signature is discarded, since the whole image is signed again after injection; a Node template's is kept, because dropping it first makes the re-sign fail strict validation.
- **ELF** uses `.note.sui` plus `.sui.phdrs` for a payload, a `NODE_SEA_BLOB` note for a blob, and remains unsigned.
- **PE** uses a resource and remains unsigned. Authenticode signing is the distributor's responsibility.

A single-executable artifact also keeps the manifest of the Node it is built from. The injector rebuilds the PE resource directory rather than editing it, so whatever the input carried has to be handed back deliberately — and for a launcher template there is nothing to hand back, since it carries no resource directory at all. For Node there is, and it is load-bearing: Windows reports version 6.2 to any image that declares no `<supportedOS>` GUID, `IsWindows10OrGreater` is the first call in Node's `wmain`, and Node exits 216 when it returns false. Root-table entries are emitted in insertion order and the resource loader binary-searches them, which fixes the order: `RT_ICON` 3, `RT_RCDATA` 10, `RT_GROUP_ICON` 14, `RT_VERSION` 16, `RT_MANIFEST` 24.

Writing the blob into the existing header slack is also what gives darwin-x64 an artifact at all. `node --build-sea` and `postject` inject through LIEF, which grows `__TEXT` by a page and shifts every later segment including the three `__thread_*` sections; dyld computes its thread-local span across those in load-command order, so the shifted binary fails a sanity check and dies at exec before any code runs. Stock Intel Node has 208 bytes of header slack and a one-section `LC_SEGMENT_64` needs 152, so Nub's writer fits into what is already there and moves only `__LINKEDIT`.

The `compile/inject.rs::verify_template` function validates the template's format and architecture: Mach-O cputype, ELF `e_machine`, or PE COFF Machine. A checksum alone only proves that the downloaded bytes match their publisher; it does not prove that the asset matches the requested target.

## Build pipeline

Five stages, in order. The ordering is load-bearing at two points: the target is resolved before bundling, and a launcher, when one is needed, is acquired before injection.

The target decides which polyfills are stripped. A foreign target has no host launcher to fall back to.

1. **Resolve the target.** The `--target` flag selects the Node version; otherwise the compiler uses the project's pin chain and refuses when it finds no pin. The `--platform` flag selects the target triple.
2. **Bundle.** Rolldown runs in-process as a Rust library. The plugins and their ordering are defined in `compile/bundle.rs`.
3. **Collect assets.** Include globs, loader-claimed files, literal `new URL(..., import.meta.url)` references, worker roots, and native addons feed one `Assets` collector. The payload stores identical physical bytes once while retaining each logical name.
4. **Acquire the launcher, when the artifact needs one.** Normal lookup checks beside the running `nub`, then the local cache, then the immutable release for this exact Nub version. Downloaded launchers and their `.sha256` sidecars are verified before the launcher is cached. A foreign target has no host-template fallback; an offline build needs the exact target launcher already cached, and an unpublished version cannot fetch a release asset that does not exist. A build that could produce a single-executable artifact skips the lookup entirely and repeats it only if the payload turns out to need real paths, because the alternative is fetching a template nothing opens — and failing an otherwise valid build on a platform for which this release published no launcher.
5. **Inject and stage.** The compiler injects the payload or the blob into a staged copy, verifies what the host can verify, then replaces the requested output. A foreign artifact cannot be executed on the build host, so cross-target verification is structural rather than an execution probe: for a single-executable artifact that means reading the written file back, confirming the fuse byte is set, that a blob carrying Node's magic sits where that platform's Node will look for it, and on Windows that the application manifest survived the rewrite.

## Runtime path

A single-executable artifact has almost no path to describe: Node starts and reads its own blob.

Node compiles the blob's main as an ordinary CommonJS wrapper — `exports`, `require`, `module`, `__filename` and `__dirname` arrive as parameters rather than globals, so the eval-global cleanup and the IIFE the inline shape needs have no counterpart here. That main is the compile bootstrap followed by a loader, which registers `module.registerHooks` to serve each chunk from the blob at a `file:` URL under a fixed virtual root, turns on Node's on-disk compile cache, and imports the entry.

The chunks travel verbatim: at a real URL each one keeps its own `import.meta.url` and its own relative specifiers, so there is no specifier substitution pass and no re-encoding. That is also the difference that decides start time. The inline (`no-extract`) shape has to make each chunk a `data:` URL because it has nothing on disk and no hook API on its floor, and inside a blob that same choice costs 8.3 ms on a 60 KB chunk — a base64 encode, a URL parse and a base64 decode, none of which any cache covers. Serving `sea.getRawAsset` through hooks avoids the copy as well: the source is handed to Node as the ArrayBuffer the blob is already mapped at.

The remaining steps describe a launcher artifact. The cache is chosen before anything is extracted, because whether the artifact needs an executable mount depends on whether that cache will have to supply Node.

1. The launcher decodes its payload. No argument spelling is reserved at any argument count; the embedded Node notice is reached only through the private `__NUB_COMPILED_LAUNCHER_MODE=licenses` channel with no application arguments (release CI's gate), which prints it before cache resolution or Node startup.
2. A `--smol` launcher first proves whether a usable external Node exists. This determines whether the selected cache will hold app data only or must supply Node.
3. `cache::resolve()` selects one cache root and threads it through the run. Normal candidates are the XDG cache, the home cache, and a per-user temporary directory. A cold cache must be writable; it must permit execution only when it will supply Node. A proven external `--smol` Node makes a `noexec` app cache valid.
4. `acquire_node` extracts or provisions Node when no external `--smol` Node was found. `ensure_app` extracts the bundled entry and assets. Durable completion markers are written after each tree is complete, and later runs reject an incomplete publication. A published app tree is accepted when it holds exactly the payload's names, at their recorded extracted lengths, carrying the executable bits the payload marks. Lengths rather than content, for the reason the embedded Node's own check already gives: the tree's directory name is the payload's content hash, so re-reading the bytes proves no identity the path did not, while it made every start cost a decompression of the whole payload. A payload old enough to record no length compares bytes.
5. `compute_inject_flags` selects flags for the Node version. The launcher prepends the absolute compile bootstrap as a `--require` preload, or hands over its path in the environment for a payload whose preamble bootstraps itself, appends the extracted entry and application arguments, and inherits `NODE_OPTIONS` unchanged. A payload whose module graph is sealed — no `--external` packages, no retained computed `import()`, and no verbatim payload file — additionally skips the V8 syntax flags, both the argv-only rows and the runtime rows the preload turns on inside the process: those enable in-progress syntax in files Node parses at runtime, which a sealed bundle does not have, and a V8 flag that is non-default at startup makes Node reject its embedded builtin code cache for every internal module compiled afterwards.
6. On Unix the launcher replaces its own process image with Node via `exec`, so the artifact and Node are one process: the terminal delivers signals directly, the exit status is Node's own, and no per-run child process is created or torn down. Windows has no `exec`, so the launcher there starts Node as a child in its own process group, forwards signals and the exit status, and hands an interactive terminal to it.

Cache selection validates the properties it relies on before using extracted files, including ownership or access control. It checks for an executable mount where supported only when Node will run from that cache. This protects the launcher's cache handoff from other principals; it is not a sandbox, and code running as the same user can modify user-owned files.

## Process identity

The outer artifact remains the application's executable identity even though the process that ends up running is Node.

For a launcher artifact that process is the launcher's own after `exec` on Unix, a child on Windows. For a single-executable artifact there is no distinction to draw.

| Value | Launcher artifact | Single-executable artifact |
| --- | --- | --- |
| `process.execPath` | outer compiled executable | the artifact, natively |
| `process.argv[0]` | outer compiled executable | the artifact, natively |
| `process.argv[1]` | extracted bundled entry | the artifact, as Node sets it |
| `process.argv0` | underlying Node argv0 | the artifact as invoked |
| initial `process.title` | underlying Node process title | the artifact as invoked |
| `process.execArgv` | actual underlying Node CLI flags | the blob's `execArgv` |

In a launcher artifact the compile preamble rewrites `process.execPath` and `process.argv[0]`, while `process.argv0` and the initial process title retain Node's native values. A single-executable artifact needs no rewrite: Node's native values already name the artifact, because it is the Node. Directly spawning `process.execPath` runs the compiled application again either way.

The `process.execArgv` array truthfully exposes the flags passed to the underlying Node, including the version-dependent flags and, when it is preloaded, the private bootstrap. It is runtime plumbing rather than a stable API. Combining the outer `process.execPath` with those underlying Node flags is not a plain-Node re-execution recipe; a caller that needs plain Node must discover and pass a Node executable.

## Forks and workers

The compiled preamble patches both CommonJS and named ESM access to `child_process.fork()`. When `options.execPath` is omitted or falsy, the fork uses the captured underlying Node. A truthy explicit `execPath` remains authoritative.

Each fork prepends the canonical private bootstrap exactly once. Missing or falsy `execArgv` otherwise inherits the parent's actual Node flags; an explicit `execArgv` array retains its authored relative order after the bootstrap. The requested module must still exist at a runtime path because bundling does not preserve its source-tree path.

For native `node:worker_threads.Worker`, an explicit `execArgv` retains Node's replacement semantics. A compiled static worker uses a generated wrapper that installs the bootstrap independently, so replacing native worker flags does not remove compiled initialization. The global `Worker` compatibility API merges explicit flags after inherited flags and normalizes the bootstrap to one leading copy.

## Why extraction uses real files

A native addon loads through the platform dynamic loader, which reads a path, so any payload carrying one extracts — and keeps the launcher rather than becoming a single-executable application.

Extracted addons use mode `644`; `dlopen` needs read access, not the executable bit.

A launcher that embeds a Node stores it under `compile-node/<version>-<hash>`, keyed by content. Matching compiled binaries can share that extraction, and such an artifact can adopt a matching Node from Nub's ordinary store. The app files use their own payload-keyed extraction with a completion marker.

A single-executable artifact extracts nothing, and still writes Node's compile cache — exactly as `node app.js` does.

## Build-time and runtime resolution

Build time settles TypeScript and JSX transformation, definitions, platform and architecture branches, tree-shaking, chunk layout, asset names, and static worker roots.

Chunk layout is Rolldown's own: a payload with no static worker and no dynamic import is one chunk, and every extra file costs a start. CommonJS and authored ESM therefore share a chunk, and the chunk's scope declares no `require`: each CommonJS module's wrapper binds the chunk's loader as its own `require`, so a builtin or external `require()` left in a wrapped module resolves while an ESM module's `typeof require` stays `undefined`, as on plain Node. Rolldown reads a `module` or `exports` reference as CommonJS evidence even in an `.mjs` file; such a module is marked as ESM before that scan so it is neither wrapped nor given a `require` Node would not give it. A type-less `.js` root that only calls `require` gets the opposite marker, since Node runs it as CommonJS and Rolldown would otherwise leave it unwrapped.

The chunk is also shaped for Node's compile cache. Node serializes a module's V8 code cache right after compiling it, so the cache holds bytecode only for the functions V8 compiled at parse time, and everything else that runs at start is compiled again from source on every warm start. V8 compiles a parenthesized function literal eagerly, and that eagerness reaches the parenthesized literals inside it, so on a target that has the compile cache (22.1 and later) each module wrapper is bound as a parenthesized function expression and each of the runtime's own helper functions is hoisted as one. The second run of an artifact then finds the runtime's startup functions in the cache; an application's own helpers are left lazy, since most never run at start and would only inflate the cache.

Runtime resolution is limited to deliberate exceptions. Packages named by `--external` and computed imports retained by `--allow-dynamic-import` use a bundled `module.registerHooks` shim. Path-like specifiers try the artifact first, while bare package specifiers try the launch directory first.

CommonJS and ESM modules can participate in immediate and lazy cycles in the bundled output. A local function returned by `createRequire()` is not compiler syntax, however, so relative calls through it cannot be followed and must become static imports. The native-addon transform recognizes only the narrow generated-loader rebinding it can prove safe to remove.

## Implementation invariants

Properties the pipeline holds that are not obvious from any one stage, and that a change to a stage can break from a distance. Most are about ordering, or about failing loudly where the compiler can prove something will not work at runtime.

- The `CjsPathGlobals` transform runs after scanners that need authored source locations. It provides bundled CommonJS modules with `__dirname` and `__filename` without emitting a new `createRequire` binding.
- Worker and asset scans clean query suffixes before choosing a parser source type. A TypeScript module must not silently fall back to JavaScript parsing and lose its sites.
- Emitted chunks are excluded from entry detection even when Rolldown marks them as entries.
- Unsupported executable code paths fail the build when the compiler can identify them. A JavaScript or TypeScript file embedded deliberately as data produces a warning instead because the compiler cannot infer whether the bytes are meant to execute.
- End-to-end verification runs the produced binary from a foreign working directory with the source tree unavailable and asserts which runtime file answered.
