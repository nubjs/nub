# Research: Rust-from-JS interop on stock Node

**Status:** v1, 2026-05-16. Per-call benchmarks verified against the napi-rs overhead suite.

**Related:** [[research/augmentation-layers]] covers where new APIs *enter* the system (resolver hooks, prelude `--import`, globals); this doc covers how their implementations reach JS. [[research/forking-node]] weighs staying external against modifying the runtime.

## Question

Nub adds globals, built-in modules, and resolver-served virtual specifiers under the API additivity policy. Implementations sit on a spectrum from "pure JS shipped as a package" to "Rust compiled into Nub."

For the Rust end: **how does Rust code get invoked from JS running inside Node, and what does each option cost per call?**

The design questions that depend on the answer:

- Should `<hypothetical Nub module>.hash(buf)` be Rust or JS? The answer differs by how it is called.
- Is per-byte-from-JS Rust viable, or only per-buffer-from-JS?
- Does anything shipped as a built-in *have* to be Rust, or is JS always sufficient?

On naming: the Nub rule is no `globalThis.nub` namespace — every Nub built-in is also an npm package (e.g. `import { hash } from "@nub/hash"`). The npm package is the API; whether its Nub implementation is Rust or JS is an internal decision.

## TL;DR

On stock Node, three real options:

| Option | Trivial-call cost | Best for | Worst for |
|---|---|---|---|
| **N-API native addon** (napi-rs) | ~26 ns / ~230 ns w/ object | Coarse-grained ops, native dep wrapping | Per-token hot loops |
| **WebAssembly** | ~5–10 ns numeric, much more w/ strings | Pure-compute helpers, universal binary | OS/native interop |
| **Rust sidecar over IPC** | ~µs+ | Build orchestration, install, watch | Anything per-call |

A fourth option — direct V8 binding inside a modified runtime — would drop per-call cost to JS-call levels but is out of scope per [[research/forking-node]].

**Recommendation:** N-API is the default. Design the Rust surface coarse-grained: batch work into single calls; never put Rust on the inside of a hot JS loop.

## Option 1 — Node-API (napi-rs) native addons

The documented stable Rust↔Node boundary. Nub ships a `.node` shared library (per-platform prebuilds) and loads it from a prelude module injected via `--import` at spawn time.

### How it plumbs in

1. `nub` spawns `node --import nub-prelude.mjs <user script>`.
2. `nub-prelude.mjs` does `import { hash, ... } from "@nub/internal-addon"`, which `require`s the `.node` file via napi-rs's loader stub.
3. The prelude either:
   - Exposes the Rust functions on `globalThis`, if Nub-shape globals are ever added (currently policy-prohibited), or
   - Stays in scope as a module reached only via the resolver-hook redirect — when user code writes `import { hash } from "@nub/hash"`, the resolve hook short-circuits the disk lookup and returns the prelude-loaded module.

The second pattern is the right one for Nub: the same `import` works on plain Node (resolving to the published `@nub/hash` package) and on Nub (resolving to the in-process Rust addon). Reversibility by construction.

### Call overhead — concrete numbers

Source: `github.com/Brooooooklyn/rust-to-nodejs-overhead-benchmark` (napi-rs maintainer, M1 Max).

| Operation | napi-rs (ops/s) | ~ns/call |
|---|---|---|
| `sum(a, b)` numbers | 37.99M | ~26 ns |
| String concat | 9.96M | ~100 ns |
| Object return `{w, h}` | 4.33M | ~231 ns |

Compare:

- Same-realm monomorphic JS call (JIT-inlined): ~1–3 ns.
- N-API boundary tax is ~10–30× a JS call for trivial work, ~80–100× if the result is an object.
- Historical baseline (kenny-y, [nodejs/node#21072](https://github.com/nodejs/node/pull/21072), 2018): NAPI fast path 36 ns → 20 ns on Haswell. Floor has barely moved since.

### V8 Fast API status

V8 has had "Fast API Calls" since ~2020, used heavily inside Node itself (Buffer, URL, crypto) to bypass HandleScope setup and reach near-native call cost for primitives.

As of mid-2026 it is not exposed through Node-API as a stable user-facing entry; discussion lives on [napi-rs#1973](https://github.com/napi-rs/napi-rs/issues/1973) and nothing has shipped. Plan against the ~26 ns floor.

### Distribution

The napi-rs pipeline is mature:

- `@napi-rs/cli new <pkg>` scaffolds a per-platform GitHub Actions matrix build.
- Prebuilds for Linux (x64, arm64, glibc + musl), macOS (x64, arm64), Windows (x64, arm64) ship as `@scope/<pkg>-<triple>` packages.
- The installer picks the right one via `optionalDependencies` and falls back to building from source.

So the published `@nub/<name>` npm packages carry their own native addons through this pipeline on plain Node; on Nub the addon is statically inside the prelude, with no separate dependency.

### The coarse-grained design rule

The 26 ns floor means never putting Rust on the inside of a JS hot loop. The correct shape is one Rust call per logical operation, not one per token, byte, or row.

- ✅ `hash(buffer)` — single call processes the whole buffer in Rust.
- ❌ `hashUpdate(byte)` called from a JS `for` loop — every byte pays 26 ns of boundary tax, dwarfing the hash itself.
- ✅ `parseTsconfig(path)` — read + parse + return one object.
- ❌ `parser.next()` returning one token at a time — N-API tax per token.
- ✅ `glob(pattern, opts) → string[]` — Rust walks the FS, returns all matches as one array.
- ⚠️ `glob(pattern, { onMatch(path) { ... } })` — N-API tax on every callback. Acceptable if N is small; problematic for huge trees.

This rule shapes API design more than it shapes the implementation choice: batch on the Rust side and return a single result.

## Option 2 — WebAssembly

Rust compiled to WASM (via `wasm-bindgen` or `wasi-sdk`), loaded as an ES module or `WebAssembly.instantiate`d from the prelude.

### Pros

WASM's advantages are numeric call cost and one binary for every platform.

- **Lower call cost for primitives:** ~5–10 ns for an `i32 → i32` call inside an already-instantiated module in V8, beating N-API for numerics.
- **Universal binary:** one `.wasm` for every platform, no prebuild matrix.
- **Sandboxed by default** — useful only if untrusted Rust plugins are ever executed, which Nub internals do not need.

### Cons

The costs land on everything that is not a number: typed data, OS access, and threads.

- **Typed-data marshalling is expensive.** Strings, buffers, and objects cross the boundary by copying into and out of linear memory, so anything beyond `i32`/`f64` drops below N-API performance.
- **No native OS interop.** Filesystem, networking, and native dependencies all go back through JS shims or WASI, spending the saved call-boundary latency on marshalling.
- **No threading without `wasm32-wasi-threads`,** which Node supports only partially. Pacquet, swc, and lightningcss all use real OS threads.

### When it makes sense

A self-contained pure-compute helper with hot per-call paths and primitive args and returns — a hash function, a fast checksum, a small text classifier. Otherwise default to N-API.

## Option 3 — Rust sidecar over IPC

Rust runs in a separate process; Node IPCs to it over a Unix socket, named pipe, or shared memory.

### Per-call cost

Order microseconds — orders of magnitude worse than N-API or WASM, with round-trip latency dominating.

### When it makes sense

Coarse-grained operations where the per-call latency is amortized:

- **Package install** — `nub install` calls the Rust pacquet engine once for "install this lockfile."
- **Build orchestration** — a bundler invoked once per build, not per module.
- **File watching** — start watchers in Rust and push notifications to Node when something changes. The Rust side aggregates, so the IPC is rare and therefore cheap.

### Relation to the `nub daemon` model

The planned `nub daemon` (a warm Node worker for `nubx` warm starts) is a Node-side daemon: a pre-bootstrapped Node process, with IPC from `nubx` into Node.

A Rust sidecar is a separate pattern — a long-lived Rust process that Node talks to. Both may end up existing (Node daemon for warm JS execution, Rust sidecar for the install / watch / build engines), and they should not be conflated.

## Out of scope — direct V8 binding via a modified runtime

A fourth option exists in principle: modify Node to bind Rust against V8 directly, with no Node-API tax — `v8::Local<v8::Value>` semantics, fast-call qualifier, the works. It would also be the only way to add entries to the closed `node:*` namespace.

That path is out of scope per Nub's additivity policy and the trade-off analysis in [[research/forking-node]]. The design consequence: a Nub built-in whose value depends on per-call latency below ~26 ns cannot ship. Redesign the API to amortize the boundary cost, or drop the feature.

## How this shapes Nub's design

The 26 ns N-API floor and the cost of shipping prebuilt addons make JS the default, with Rust reserved for coarse-grained calls.

1. **Default new APIs to JS** unless Rust offers something concrete: speed (with a coarse-grained API shape), access to an already-vendored Rust crate (swc, lightningcss, pacquet), or correctness guarantees only Rust provides. Every built-in ships as an npm package, JS is the path of least resistance for that, and addon distribution is a real tax.
2. **When Rust wins, ship via napi-rs N-API addons.** WASM is a special case for self-contained pure-compute helpers; a sidecar is for coarse engine-level work.
3. **Coarse-grained API surface is mandatory.** Design Rust-backed APIs around one call per operation, not one call per element. Inversion of control via JS callbacks paid into Rust is suspect for high-N cases.
4. **Do not promise sub-N-API performance.** APIs whose value depends on latency below the 26 ns floor get redesigned to amortize, or get dropped.
5. **No `node:*` injection.** Even with the sync-hook fix for `node:*` interception ([[research/augmentation-layers#Augmentation layer B: per-file loader hooks (current plan)|`augmentation-layers.md`]]), intercepting `node:fs` is not the same as adding `node:postgres`. New entries in that namespace require modifying the runtime, out of scope per [[research/forking-node]].

## Open follow-ups

Unmeasured on current Node: the real N-API floor, whether V8 Fast API reaches Node-API, how the alternative bindings compare, and how Nub's own prebuilds are produced.

- **Bench the 26 ns claim against the actual N-API floor under Node 24 / 26.** napi-rs's published numbers are from the ~Node 20 era.
- **Track V8 Fast API exposure through Node-API.** If [napi-rs#1973](https://github.com/napi-rs/napi-rs/issues/1973) resolves with a stable user-facing entry, the floor drops and per-token Rust calls re-enter the design space.
- **Investigate `node-bindgen` and `neon`** as alternative Rust↔Node bindings. Initial data: napi-rs is the fastest of the three on the trivial number op (37.99M vs 23.98M neon vs 19.62M node-bindgen).
- **Prebuild matrix logistics for Nub's own addons.** Decide whether each `@nub/*` package owns its own matrix or one consolidated pipeline produces them all.

## Sources

The per-call figures come from the napi-rs overhead benchmark; the rest cover N-API cost history, WASM call cost in V8, and the prebuild model.

- napi-rs overhead benchmark: `github.com/Brooooooklyn/rust-to-nodejs-overhead-benchmark`
- N-API call cost history: [`nodejs/node#21072`](https://github.com/nodejs/node/pull/21072)
- V8 Fast API exposure discussion: [`napi-rs/napi-rs#1973`](https://github.com/napi-rs/napi-rs/issues/1973)
- napi-rs prebuild model: `napi.rs/docs/deep-dive/release`
- WASM call cost in V8: V8 blog, "Faster JS-to-Wasm calls" (2021)
- Node-API docs: `nodejs.org/api/n-api.html`

## Changelog

Revision history. Both entries record the 2026-07-30 migration out of the internal corpus; no measured value changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
- 2026-07-30 — Staleness note: the build-orchestration example referred to a build subcommand Nub does not ship. The per-call overhead figures are unaffected.
