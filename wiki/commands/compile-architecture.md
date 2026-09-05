# `nub compile` — system design

This document describes how a Nub project becomes a target-specific executable. Keep it synchronized with changes to the compile subsystem.

The implementation spans `crates/nub-cli/src/compile/` for the build, `crates/nub-launcher/` for the runtime launcher, and `crates/nub-core/src/compile.rs` for the shared payload format.

## Compile-time transformation and runtime support

A compiled artifact carries neither Nub's transpiler nor its resolver. What normal execution does through Node's extension surfaces, the build does ahead of time — and what is left is a small runtime the launcher still has to install.

Normal Nub execution augments the user's Node through preloads, module hooks, and injected flags. Rolldown instead transpiles TypeScript and JSX, substitutes target values such as `process.platform` and `process.arch`, and resolves the static module graph during the build.

The runtime is still more than a bare script spawn. The launcher starts official, unpatched Node with version-appropriate flags and the extracted bundled entry, and hands it a fixed internal CommonJS bootstrap that captures fixed-root builtin access before user hooks run; the bundle's preamble restores the runtime globals the compiled program needs. The bootstrap is a `--require` preload only when the payload needs it to run before the ESM graph — when the sealed graph reaches `child_process`/`cluster` (the fork identity fix-up) or `Worker`/`worker_threads`. Otherwise, on Node 22.3 and later, the launcher passes the bootstrap's path in the environment and the preamble publishes the same record itself, which saves the preload's cost on every start.

"Unpatched" describes the default and the source, not an invariant of the byte stream: the build already strips the binary and re-signs it on macOS, and `--icu` additionally rewrites its ICU data package in place. Node's own code is never altered.

The preamble is injected into the main root and each supported static worker root. It installs feature-detected polyfills, including the supported Web Storage surface for `sessionStorage`. A polyfill the target Node ships natively is stripped from the bundle before it is built; one the target lacks — Temporal, URLPattern, Float16Array — is bundled but installed as a lazy global, so its package evaluates on the first read of the global rather than at every start. That storage is process-local and does not add browser-origin sharing or unsupported Web Storage APIs.

Static workers must use a file-backed `new Worker(new URL("./worker.js", import.meta.url))` shape through the global `Worker` or a named ESM import from `node:worker_threads`. Statically recognized data URLs, blob URLs, `{ eval: true }`, and CommonJS `require("node:worker_threads")` worker bindings are refused because the compiler cannot turn them into preamble-bearing roots.

## Two shapes

The default shape embeds a Node; `--smol` does not, and finds one at startup instead. That single choice decides the artifact's size, what its cache has to hold, and how much of the target version is settled at build time.

| Shape | Size | Contains | Node comes from |
| --- | --- | --- | --- |
| default (embed) | ~29 MB | launcher + manifest + bundled JS + assets + a stripped, zstd-19 Node | inside the binary |
| `--smol` | ~1.0 MB | launcher + manifest + bundled JS + assets | discovered locally, else provisioned |

Both figures are a hello-world artifact on Node 26 for darwin-arm64. Neither floor is the bundled program: a `--smol` artifact is 851 KB of launcher template before it carries a byte of application code, and an embed artifact is that plus the compressed runtime.

### Trimming ICU out of the embedded Node

`--icu=en,de,fr` rewrites the embedded Node's ICU data package in place to hold only the named languages. The default keeps every locale, because a dropped one falls back silently rather than failing.

An official Node is built `--with-intl=full-icu`, so one linked-in package covering ~700 locales accounts for 31.6 MiB of the ~102 MiB stripped binary and 31.7% of the compressed blob. The rewrite zero-fills what it vacates, and that padding survives zstd at under a kilobyte. Measured on Node 26 for darwin-arm64, English alone takes a hello-world artifact from 29.5 MB to 24.4 MB.

Three properties make the rewrite safe to do after linking. ICU reaches the package through a bare pointer and navigates it by its own table of contents, so nothing records the length a smaller package would contradict. The package announces itself with a magic and a `CmnD` format tag that occur exactly once per binary in all three container formats, so locating it needs no symbol table — which matters because `strip` removes the ELF symbol that would otherwise name it. And only locale-shaped resources are dropped, so the charset converters, break iterators, normalization tables and supplemental resources all remain: no API changes behavior, and a dropped language falls back through ICU's normal chain to `root`.

That fallback is silent, which is why the default keeps every locale and why the trim is never inferred. Two consequences follow the artifact. A trimmed payload never dedups against an official Node in Nub's store — the dedup assumes the embedded and provisioned binaries run identically, and a trim falsifies exactly that — and a trimmed macOS Node must be re-signed, so `--icu` requires `codesign` rather than degrading to an unsigned binary. Because a broken ICU package still answers `--version` before aborting at the first format call, a host-target trim is verified by formatting a date and segmenting a string, not by launching.

The `--smol` launcher downloads through curl or wget, verifies the selected archive against `SHASUMS256.txt`, and extracts it through Nub core's capped archive reader. Unix hosts use the published `.tar.xz`; Windows hosts use the published `.zip`. The verified tree is staged and atomically published under the ordinary Node store before discovery can return it.

### Which Nodes a `--smol` artifact accepts

Selection depends on the target form. An exact version reuses only that Node. A range is enforced in full, upper bound included, but only when the version the bundle was gated on is that range's own minimum. Everything else resolves to a floor.

A range qualifies when its lower bound is representable and floor resolution returns it: `>=22 <23`, `^22`, and wildcards such as `24.x`, whose floor is `24.0.0`. A major or minor pin, an alias, and an upper-only range such as `<23` do not, and any discovered Node at or above the floor qualifies for those.

The split is not cosmetic. The bundle is stripped against the resolved gate, so a form whose gate is not the range's minimum must not have its range enforced — the artifact would otherwise accept a Node whose polyfills it already removed. Two cases miss: an upper-only range, whose gate falls back to the newest matching release, and a range whose minimum is published but carries no artifact for the target, where the gate lands above it. [[crates/nub-core/src/version_management/mod.rs#range_minimum_is]] is the single predicate both sides read.

When no installed Node qualifies, the launcher provisions the newest matching release resolved at compile time — provided that release can run the payload. A bundle carrying a `module.registerHooks` shim needs a Node that has the API, and version order does not imply it: 23.0 through 23.4 sort above a 22.15 floor and satisfy a range built on it, yet predate `registerHooks` on the 23.x line. Where the newest match fails that test the compiler records no preference at all, and the launcher provisions the floor, which the build already gated on the same capability.

## Anatomy of a compiled binary

A compiled binary is a target-specific `nub-launcher` with one injected section, holding a JSON manifest and its payload. How that section is attached is format-specific, and only one of the three formats ends up signed.

Payload V3 carries the app files, the length each one extracts to, and — for the default shape — the exact aggregate Node-root `LICENSE` as a compressed notice. The decoder remains compatible with V2 payloads, which record no extracted length, and with V1 payloads, which carry neither that nor the notice.

Injection is format-specific:

- **Mach-O** uses a new section and is ad-hoc-signed by the pure-Rust injector. The signature supplies neither a Developer ID identity nor notarization. It is also not a fixed cost: the CodeDirectory holds one SHA-256 per 4 KiB page, so it scales with the image at roughly 0.78% of it — 8 KB on a `--smol` artifact and 228 KB on a 29 MB embed one. The template's own signature is discarded, since the whole image is signed again after injection.
- **ELF** uses `.note.sui` plus `.sui.phdrs` and remains unsigned.
- **PE** uses a resource and remains unsigned. Authenticode signing is the distributor's responsibility.

The `compile/inject.rs::verify_template` function validates the template's format and architecture: Mach-O cputype, ELF `e_machine`, or PE COFF Machine. A checksum alone only proves that the downloaded bytes match their publisher; it does not prove that the asset matches the requested target.

## Build pipeline

Five stages, in order. The ordering is load-bearing at two points: the target is resolved before bundling because it decides which polyfills are stripped, and the launcher is acquired before injection because a foreign target has no host fallback.

1. **Resolve the target.** The `--target` flag selects the Node version; otherwise the compiler uses the project's pin chain and refuses when it finds no pin. The `--platform` flag selects the target triple.
2. **Bundle.** Rolldown runs in-process as a Rust library. The plugins and their ordering are defined in `compile/bundle.rs`.
3. **Collect assets.** Include globs, loader-claimed files, literal `new URL(..., import.meta.url)` references, worker roots, and native addons feed one `Assets` collector. The payload stores identical physical bytes once while retaining each logical name.
4. **Acquire the launcher.** Normal lookup checks beside the running `nub`, then the local cache, then the immutable release for this exact Nub version. Downloaded launchers and their `.sha256` sidecars are verified before the launcher is cached. A foreign target has no host-template fallback; an offline build needs the exact target launcher already cached, and an unpublished version cannot fetch a release asset that does not exist.
5. **Inject and stage.** The compiler injects the payload into a staged copy, verifies what the host can verify, then replaces the requested output. A foreign artifact cannot be executed on the build host, so cross-target verification is structural rather than an execution probe.

## Runtime path

What the launcher does between exec and handing control to Node. The cache is chosen before anything is extracted, because whether the artifact needs an executable mount depends on whether that cache will have to supply Node.

1. The launcher decodes its payload. No argument spelling is reserved at any argument count; the embedded Node notice is reached only through the private `__NUB_COMPILED_LAUNCHER_MODE=licenses` channel with no application arguments (release CI's gate), which prints it before cache resolution or Node startup.
2. A `--smol` launcher first proves whether a usable external Node exists. This determines whether the selected cache will hold app data only or must supply Node.
3. `cache::resolve()` selects one cache root and threads it through the run. Normal candidates are the XDG cache, the home cache, and a per-user temporary directory. A cold cache must be writable; it must permit execution only when it will supply Node. A proven external `--smol` Node makes a `noexec` app cache valid.
4. `acquire_node` extracts or provisions Node when no external `--smol` Node was found. `ensure_app` extracts the bundled entry and assets. Durable completion markers are written after each tree is complete, and later runs reject an incomplete publication. A published app tree is accepted when it holds exactly the payload's names, at their recorded extracted lengths, carrying the executable bits the payload marks. Lengths rather than content, for the reason the embedded Node's own check already gives: the tree's directory name is the payload's content hash, so re-reading the bytes proves no identity the path did not, while it made every start cost a decompression of the whole payload. A payload old enough to record no length compares bytes.
5. `compute_inject_flags` selects flags for the Node version. The launcher prepends the absolute compile bootstrap as a `--require` preload, or hands over its path in the environment for a payload whose preamble bootstraps itself, appends the extracted entry and application arguments, and inherits `NODE_OPTIONS` unchanged. A payload whose module graph is sealed — no `--external` packages, no retained computed `import()`, and no verbatim payload file — additionally skips the V8 syntax flags, both the argv-only rows and the runtime rows the preload turns on inside the process: those enable in-progress syntax in files Node parses at runtime, which a sealed bundle does not have, and a V8 flag that is non-default at startup makes Node reject its embedded builtin code cache for every internal module compiled afterwards.
6. On Unix the launcher replaces its own process image with Node via `exec`, so the artifact and Node are one process: the terminal delivers signals directly, the exit status is Node's own, and no per-run child process is created or torn down. Windows has no `exec`, so the launcher there starts Node as a child in its own process group, forwards signals and the exit status, and hands an interactive terminal to it.

Cache selection validates the properties it relies on before using extracted files, including ownership or access control. It checks for an executable mount where supported only when Node will run from that cache. This protects the launcher's cache handoff from other principals; it is not a sandbox, and code running as the same user can modify user-owned files.

## Process identity

The outer artifact remains the application's executable identity even though the process that ends up running is Node — on Unix the launcher's own process after `exec`, on Windows a child:

| Value | Compiled artifact |
| --- | --- |
| `process.execPath` | outer compiled executable |
| `process.argv[0]` | outer compiled executable |
| `process.argv[1]` | extracted bundled entry |
| `process.argv0` | underlying Node argv0 |
| initial `process.title` | underlying Node process title |
| `process.execArgv` | actual underlying Node CLI flags |

The compile preamble rewrites `process.execPath` and `process.argv[0]`; `process.argv0` and the initial process title retain Node's native values. Directly spawning `process.execPath` re-enters the launcher and runs the compiled application again.

The `process.execArgv` array truthfully exposes the flags passed to the underlying Node, including the version-dependent flags and, when it is preloaded, the private bootstrap. It is runtime plumbing rather than a stable API. Combining the outer `process.execPath` with those underlying Node flags is not a plain-Node re-execution recipe; a caller that needs plain Node must discover and pass a Node executable.

## Forks and workers

The compiled preamble patches both CommonJS and named ESM access to `child_process.fork()`. When `options.execPath` is omitted or falsy, the fork uses the captured underlying Node. A truthy explicit `execPath` remains authoritative.

Each fork prepends the canonical private bootstrap exactly once. Missing or falsy `execArgv` otherwise inherits the parent's actual Node flags; an explicit `execArgv` array retains its authored relative order after the bootstrap. The requested module must still exist at a runtime path because bundling does not preserve its source-tree path.

For native `node:worker_threads.Worker`, an explicit `execArgv` retains Node's replacement semantics. A compiled static worker uses a generated wrapper that installs the bootstrap independently, so replacing native worker flags does not remove compiled initialization. The global `Worker` compatibility API merges explicit flags after inherited flags and normalizes the bootstrap to one leading copy.

## Why extraction uses real files

The launcher extracts the app and runtime to real paths so native addons can load through the platform dynamic loader. Extracted addons use mode `644`; `dlopen` needs read access, not the executable bit.

The default shape stores Node under `compile-node/<version>-<hash>`, keyed by content. Matching compiled binaries can share that extraction, and a compiled artifact can adopt a matching Node from Nub's ordinary store. The app files use their own payload-keyed extraction with a completion marker.

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
