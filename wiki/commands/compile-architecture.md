# `nub compile` — system design

This document describes how a Nub project becomes a target-specific executable. Keep it synchronized with changes to the compile subsystem.

The implementation spans `crates/nub-cli/src/compile/` for the build, `crates/nub-launcher/` for the runtime launcher, and `crates/nub-core/src/compile.rs` for the shared payload format.

## Compile-time transformation and runtime support

Normal Nub execution augments the user's Node through Node's extension surfaces: preloads, module hooks, and injected flags. A compiled artifact does not carry Nub's general transpiler or resolver. Rolldown transpiles TypeScript and JSX, substitutes target values such as `process.platform` and `process.arch`, and resolves the static module graph during the build.

The runtime is still more than a bare script spawn. The launcher starts official, unpatched Node with a fixed internal CommonJS bootstrap as its first CLI argument, followed by version-appropriate flags and the extracted bundled entry. The bootstrap captures fixed-root builtin access before user hooks run, and the bundle's preamble restores the runtime globals the compiled program needs.

The preamble is injected into the main root and each supported static worker root. It installs feature-detected polyfills, including the supported Web Storage surface for `sessionStorage`. That storage is process-local and does not add browser-origin sharing or unsupported Web Storage APIs.

Static workers must use a file-backed `new Worker(new URL("./worker.js", import.meta.url))` shape through the global `Worker` or a named ESM import from `node:worker_threads`. Statically recognized data URLs, blob URLs, `{ eval: true }`, and CommonJS `require("node:worker_threads")` worker bindings are refused because the compiler cannot turn them into preamble-bearing roots.

## Two shapes

| Shape | Size | Contains | Node comes from |
| --- | --- | --- | --- |
| default (embed) | ~26 MB | launcher + manifest + bundled JS + assets + a stripped, zstd-19 Node | inside the binary |
| `--smol` | ~0.6 MB | launcher + manifest + bundled JS + assets | discovered locally, else provisioned |

The `--smol` launcher downloads through curl or wget, verifies the selected archive against `SHASUMS256.txt`, and extracts it through Nub core's capped archive reader. Unix hosts use the published `.tar.xz`; Windows hosts use the published `.zip`. The verified tree is staged and atomically published under the ordinary Node store before discovery can return it.

Runtime selection depends on the target form. An exact version reuses only that Node. An explicit semver range is enforced in full, including its upper bound. A major or minor pin or alias resolves to a floor, and any discovered Node at or above that floor qualifies. When no installed Node qualifies, the launcher provisions the newest matching release resolved at compile time.

## Anatomy of a compiled binary

A compiled binary is a target-specific `nub-launcher` with one injected section. The section holds a JSON manifest and its payload. Payload V2 carries the app files and, for the default shape, the exact aggregate Node-root `LICENSE` as a compressed notice. The decoder remains compatible with V1 payloads, which have no notice field.

Injection is format-specific:

- **Mach-O** uses a new section and is ad-hoc-signed by the pure-Rust injector. The signature supplies neither a Developer ID identity nor notarization.
- **ELF** uses `.note.sui` plus `.sui.phdrs` and remains unsigned.
- **PE** uses a resource and remains unsigned. Authenticode signing is the distributor's responsibility.

The `compile/inject.rs::verify_template` function validates the template's format and architecture: Mach-O cputype, ELF `e_machine`, or PE COFF Machine. A checksum alone only proves that the downloaded bytes match their publisher; it does not prove that the asset matches the requested target.

## Build pipeline

1. **Resolve the target.** The `--target` flag selects the Node version; otherwise the compiler uses the project's pin chain and refuses when it finds no pin. The `--platform` flag selects the target triple.
2. **Bundle.** Rolldown runs in-process as a Rust library. The plugins and their ordering are defined in `compile/bundle.rs`.
3. **Collect assets.** Include globs, loader-claimed files, literal `new URL(..., import.meta.url)` references, worker roots, and native addons feed one `Assets` collector. The payload stores identical physical bytes once while retaining each logical name.
4. **Acquire the launcher.** Normal lookup checks beside the running `nub`, then the local cache, then the immutable release for this exact Nub version. Downloaded launchers and their `.sha256` sidecars are verified before the launcher is cached. A foreign target has no host-template fallback; an offline build needs the exact target launcher already cached, and an unpublished version cannot fetch a release asset that does not exist.
5. **Inject and stage.** The compiler injects the payload into a staged copy, verifies what the host can verify, then replaces the requested output. A foreign artifact cannot be executed on the build host, so cross-target verification is structural rather than an execution probe.

## Runtime path

1. The launcher decodes its payload. No argument spelling is reserved at any argument count; the embedded Node notice is reached only through the private `__NUB_COMPILED_LAUNCHER_MODE=licenses` channel with no application arguments (release CI's gate), which prints it before cache resolution or Node startup.
2. A `--smol` launcher first proves whether a usable external Node exists. This determines whether the selected cache will hold app data only or must supply Node.
3. `cache::resolve()` selects one cache root and threads it through the run. Normal candidates are the XDG cache, the home cache, and a per-user temporary directory. A cold cache must be writable; it must permit execution only when it will supply Node. A proven external `--smol` Node makes a `noexec` app cache valid.
4. `acquire_node` extracts or provisions Node when no external `--smol` Node was found. `ensure_app` extracts the bundled entry and assets. Durable completion markers are written after each tree is complete, and later runs reject an incomplete publication.
5. `compute_inject_flags` selects flags for the Node version. The launcher prepends the absolute compile bootstrap, appends the extracted entry and application arguments, and inherits `NODE_OPTIONS` unchanged.
6. The launcher starts Node in its own process group, forwards signals and the exit status, and hands an interactive terminal to the child.

Cache selection validates the properties it relies on before using extracted files, including ownership or access control. It checks for an executable mount where supported only when Node will run from that cache. This protects the launcher's cache handoff from other principals; it is not a sandbox, and code running as the same user can modify user-owned files.

## Process identity

The outer artifact remains the application's executable identity even though the operating-system child process is Node:

| Value | Compiled artifact |
| --- | --- |
| `process.execPath` | outer compiled executable |
| `process.argv[0]` | outer compiled executable |
| `process.argv[1]` | extracted bundled entry |
| `process.argv0` | underlying Node argv0 |
| initial `process.title` | underlying Node process title |
| `process.execArgv` | actual underlying Node CLI flags |

The compile preamble rewrites `process.execPath` and `process.argv[0]`; `process.argv0` and the initial process title retain Node's native values. Directly spawning `process.execPath` re-enters the launcher and runs the compiled application again.

The `process.execArgv` array truthfully exposes the flags passed to the underlying Node, including the private bootstrap and version-dependent flags. It is runtime plumbing rather than a stable API. Combining the outer `process.execPath` with those underlying Node flags is not a plain-Node re-execution recipe; a caller that needs plain Node must discover and pass a Node executable.

## Forks and workers

The compiled preamble patches both CommonJS and named ESM access to `child_process.fork()`. When `options.execPath` is omitted or falsy, the fork uses the captured underlying Node. A truthy explicit `execPath` remains authoritative.

Each fork prepends the canonical private bootstrap exactly once. Missing or falsy `execArgv` otherwise inherits the parent's actual Node flags; an explicit `execArgv` array retains its authored relative order after the bootstrap. The requested module must still exist at a runtime path because bundling does not preserve its source-tree path.

For native `node:worker_threads.Worker`, an explicit `execArgv` retains Node's replacement semantics. A compiled static worker uses a generated wrapper that installs the bootstrap independently, so replacing native worker flags does not remove compiled initialization. The global `Worker` compatibility API merges explicit flags after inherited flags and normalizes the bootstrap to one leading copy.

## Why extraction uses real files

The launcher extracts the app and runtime to real paths so native addons can load through the platform dynamic loader. Extracted addons use mode `644`; `dlopen` needs read access, not the executable bit.

The default shape stores Node under `compile-node/<version>-<hash>`, keyed by content. Matching compiled binaries can share that extraction, and a compiled artifact can adopt a matching Node from Nub's ordinary store. The app files use their own payload-keyed extraction with a completion marker.

## Build-time and runtime resolution

Build time settles TypeScript and JSX transformation, definitions, platform and architecture branches, tree-shaking, chunk layout, asset names, and static worker roots.

Runtime resolution is limited to deliberate exceptions. Packages named by `--external` and computed imports retained by `--allow-dynamic-import` use a bundled `module.registerHooks` shim. Path-like specifiers try the artifact first, while bare package specifiers try the launch directory first.

CommonJS and ESM modules can participate in immediate and lazy cycles in the bundled output. A local function returned by `createRequire()` is not compiler syntax, however, so relative calls through it cannot be followed and must become static imports. The native-addon transform recognizes only the narrow generated-loader rebinding it can prove safe to remove.

## Implementation invariants

- The `CjsPathGlobals` transform runs after scanners that need authored source locations. It provides bundled CommonJS modules with `__dirname` and `__filename` without emitting a new `createRequire` binding.
- Worker and asset scans clean query suffixes before choosing a parser source type. A TypeScript module must not silently fall back to JavaScript parsing and lose its sites.
- Emitted chunks are excluded from entry detection even when Rolldown marks them as entries.
- Unsupported executable code paths fail the build when the compiler can identify them. A JavaScript or TypeScript file embedded deliberately as data produces a warning instead because the compiler cannot infer whether the bytes are meant to execute.
- End-to-end verification runs the produced binary from a foreign working directory with the source tree unavailable and asserts which runtime file answered.
