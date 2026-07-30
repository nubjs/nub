# `nub compile` — system design

How a Nub project becomes a single self-contained executable, and why it is built this way.
This document is the orientation for anyone — human or agent — changing the compile subsystem.
Keep it current: a change that invalidates a claim here should update it in the same commit.

The code it describes spans three places: `crates/nub-cli/src/compile/` (the build side),
`crates/nub-launcher/` (the runtime side, a separate minimal binary), and
`crates/nub-core/src/compile.rs` (the payload format both agree on).

## The premise: augmentation is resolved at COMPILE time

This is the load-bearing idea, and almost every design choice below follows from it.

Nub normally augments the user's Node through Node's own extension surfaces — a `--import`
preload, `module.registerHooks`, injected V8 flags. A compiled binary does none of that at
runtime. **The launcher spawns bit-exact official Node with NO preload**, passing only
version-appropriate flags computed by `flags::compute_inject_flags`. Everything the preload
would have done is already done: TypeScript and JSX are transpiled by Rolldown during the
build, `process.platform`/`process.arch` are substituted as defines, module resolution is
settled by bundling.

So "a compiled binary gets no runtime augmentation" is the design working, not a bug. It is
precisely what lets the standalone executable ship **without a transpiler** — the single
largest reason the binary is as small as it is, and why startup is a plain Node process spawn.

**The one thing this cannot cover is a preload-provided GLOBAL.** `Worker`, `sessionStorage`
and friends exist because the preload defines them; there is no compile-time equivalent yet.
Code using them compiles clean and fails at runtime. The intended fix is a **polyfill
preamble** — a feature-detecting banner injected into the bundle at build time, version-gated
the same way the runtime tier logic is. Not built. This is the known gap; do not rediscover it
as a mystery.

## Two shapes

| Shape | Size | Contains | Node comes from |
| --- | --- | --- | --- |
| default (embed) | ~26 MB | launcher + manifest + bundled JS + assets + a stripped, zstd-19 Node | inside the binary |
| `--smol` | ~0.6 MB | launcher + manifest + bundled JS + assets | discovered on PATH, else provisioned |

`--smol` provisioning is implemented only for hosts served by `tar.xz` — **on Windows it is not
implemented**, so a `--smol` Windows binary requires a pre-installed Node and fails loudly
(exit 1, legible message) without one. That is a real limitation, not a defect; the embed shape
is the answer there.

## Anatomy of a compiled binary

A compiled binary *is* the `nub-launcher` executable with one extra section injected into it.
The section holds a JSON manifest plus the payload it describes. The launcher is a separate,
deliberately minimal crate — it must not grow a dependency on the CLI, because every byte of it
ships in every compiled binary.

Injection is per-format, via `libsui`:

- **Mach-O** — a new section; the `__probe` self-check exists because an under-padded injection
  corrupts into a SIGILL trap, and it is cheaper to catch that at build time.
- **ELF** — `.note.sui` plus `.sui.phdrs`.
- **PE** — a resource.

`compile/inject.rs::verify_template` validates the template's **architecture**, not merely its
format — Mach-O cputype, ELF `e_machine`, PE COFF Machine. That gate exists because a checksum
and a format check both pass for a darwin-arm64 template used against `--platform darwin-x64`,
and the result was a binary labelled x64 containing arm64 code.

## The build pipeline

1. **Resolve the target.** `--target` picks the Node version (default: latest major, resolved
   through the same `resolve_pin_chain` `nub run` uses). `--platform` picks the triple.
2. **Bundle.** Rolldown runs **in-process** as a Rust library — there is no bundler subprocess.
   Plugins are registered in `compile/bundle.rs`; ordering matters and is commented there.
3. **Collect assets.** `--include` globs, loader-claimed extensions (`--loader`, `text`,
   `file`, `.wasm`), `new URL(..., import.meta.url)` references, and `.node` native addons all
   funnel into one shared `Assets` collector, which is what makes an asset reached twice ship
   once and gives everything the same content-hashed flat naming.
4. **Acquire the launcher template.** `NUB_LAUNCHER_TEMPLATE` → a sibling of the running `nub`
   → the local cache → fetched from the release for the **exact** version (never "latest") and
   verified against its `.sha256` sidecar.
5. **Inject** the manifest and payload into a copy of the template.

## The runtime path

`nub-launcher/src/main.rs::launch` is short and the order is deliberate:

1. `cache::resolve()` — pick a writable, exec-capable cache root, once, and thread it through.
   Candidates in order: `NUB_COMPILE_CACHE_DIR` (used verbatim, the documented escape hatch)
   → `XDG_CACHE_HOME` → `HOME` → a per-uid dir under `TMPDIR`.
2. `acquire_node` — use the embedded Node, or discover/provision one for `--smol`.
3. `ensure_app` — extract the bundled JS and assets into the app dir.
4. `compute_inject_flags` — version-appropriate flags only.
5. Spawn Node: own process group, signal forwarding, TTY foreground handoff, `argv0` set to
   `node` so `process.argv0` matches what a user expects. `NODE_OPTIONS` is inherited untouched.

## Why extract to real files instead of a virtual filesystem

Bun's compiled binaries serve embedded files from a virtual `/$bunfs/`. That cannot `dlopen`,
so native addons do not work there. Nub extracts to real paths on disk, which means the
platform's dynamic loader gets a real file at a real path — exactly what it gets from
`node_modules`. **This is a structural advantage over Bun, not a workaround.** Do not "optimize"
it into a VFS.

Extracted addons are mode `644`: `dlopen` needs read, not exec. No `chmod` is required.

## What is settled at build time vs. at run time

**Build time:** TypeScript/JSX transpilation, `--define` substitution, `process.platform` and
`process.arch` (which is how a multi-platform addon package picks the target's variant while
building), tree-shaking, chunk layout, asset naming, worker entry chunking.

**Run time, by deliberate exception:** `--external` packages and computed `import(expr)` sites
allowed through by `--allow-dynamic-import` are resolved by a `module.registerHooks` shim the
build installs only when something actually needs it. Resolution order is shape-dependent and
each direction guards a different silent wrong answer — path-like specifiers try the artifact
first (chunk-to-chunk imports are indistinguishable at run time), bare specifiers try the launch
directory first (the app dir has no `node_modules`, so Node's walk would climb out of the cache).

## Invariants worth knowing before you change something

- **`CjsPathGlobals` runs last among transforms that rewrite user source**, so scanners ahead of
  it see the module as authored and report line:column a user can find. It splices `__dirname`
  and `__filename` from the virtual `\0nub-path-globals` module and never emits `createRequire`
  — which is why `NativeAddons`, whose transform is gated on the source containing
  `createRequire`, is safe to push after it.
- **Refuse loudly rather than emit something that dies at run time.** The recurring failure mode
  in this subsystem is the *silent wrong answer*: a build that succeeds and produces a binary
  that resolves the wrong module, or embeds a `.ts` file as inert data. Where a case cannot be
  handled, fail the build with a diagnostic naming the file and the reason.
- **Emitted chunks are excluded from entry detection.** Rolldown marks them `is_entry`, so
  without that guard a bundled worker is mistaken for the program's entry.
- **Verification means running the produced binary**, from a foreign cwd, with the source tree
  absent, asserting *which* file answered — not merely that something did. A reviewer reading
  the diff cannot see any of the above.

## Known gaps

- The polyfill preamble (preload-provided globals) — see the premise section.
- `--smol` provisioning on Windows.
- Node's license redistribution obligations for the embedded runtime.
- Never executed on: musl (either variant — the `is_musl()` / `read_elf_interp_libc()` store-reuse
  gate has never run), linux-arm64, win32-arm64, darwin-x64.
