# Research: Rust-from-JS interop on stock Node

**Status:** v1, 2026-05-16. Sub-agent verified the per-call benchmarks against the napi-rs overhead suite. **Informs:** `PLAN.md` — Pre-processing model, `PLAN.md` — Package replacement by name. **Related:** [`augmentation-layers.md`](augmentation-layers.md) — where new APIs *enter* the system (resolver hooks, prelude `--import`, globals). This doc covers *how their implementations* reach JS. [`forking-node.md`](forking-node.md) — the broader trade-off analysis of staying external vs. modifying the runtime.

## Question

Nub adds globals, built-in modules, and resolver-served virtual specifiers (per the API additivity policy). Implementations sit somewhere on a spectrum from "pure JS shipped as a package" to "Rust compiled into Nub." For the Rust end of that spectrum: **how does Rust code actually get invoked from JS running inside Node, and what does each option cost per call?**

We need this so we can answer concrete design questions:

- Should `<hypothetical Nub module>.hash(buf)` be Rust or JS? (And the answer differs depending on how it's called.)
- Is per-byte-from-JS Rust viable, or only per-buffer-from-JS?
- Does anything we ship as a built-in *have* to be Rust, or is JS always sufficient?

Note on naming: the Nub rule is **no `globalThis.nub` namespace** — every Nub built-in is also an npm package (e.g. `import { hash } from "@nub/hash"`). The interop question below is orthogonal to that: the npm package is the API; whether its Nub implementation is Rust or JS is an internal decision.

## TL;DR

On stock Node, three real options:

| Option | Trivial-call cost | Best for | Worst for |
|---|---|---|---|
| **N-API native addon** (napi-rs) | ~26 ns / ~230 ns w/ object | Coarse-grained ops, native dep wrapping | Per-token hot loops |
| **WebAssembly** | ~5–10 ns numeric, much more w/ strings | Pure-compute helpers, universal binary | OS/native interop |
| **Rust sidecar over IPC** | ~µs+ | Build orchestration, install, watch | Anything per-call |

A fourth option — direct V8 binding inside a modified runtime — would drop per-call cost to JS-call levels but is out of scope per [`forking-node.md`](forking-node.md). The implication for Nub's design is captured in the [coarse-grained design rule](#the-coarse-grained-design-rule).

**Recommendation:** N-API is the default. Design the Rust surface **coarse-grained**: batch work into single calls; never put Rust on the inside of a hot JS loop.

## Option 1 — Node-API (napi-rs) native addons

The documented stable Rust↔Node boundary. We ship a `.node` shared library (per-platform prebuilds) and load it from a prelude module that's injected via `--import` at spawn time.

### How it plumbs in

1. `nub` spawns `node --import nub-prelude.mjs <user script>`.
2. `nub-prelude.mjs` does `import { hash, ... } from "@nub/internal-addon"`, which `require`s the `.node` file via napi-rs's loader stub.
3. The prelude either:
   - Exposes the Rust functions on `globalThis` *if* we ever add Nub-shape globals (currently policy-prohibited), or
   - Stays in scope as a module accessed only via the resolver-hook redirect — when user code writes `import { hash } from "@nub/hash"`, our resolve hook short-circuits the disk lookup and returns the prelude-loaded module instead.

The second pattern is the right one for Nub: the same `import` works on plain Node (resolving to the published `@nub/hash` package) and on Nub (resolving to our in-process Rust addon). Reversibility by construction.

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

V8 has had "Fast API Calls" since ~2020, used heavily inside Node itself (Buffer, URL, crypto) to bypass HandleScope setup and reach near-native call cost for primitives. **Not exposed through Node-API as a stable user-facing entry today** (mid-2026). Discussion lives on [napi-rs#1973](https://github.com/napi-rs/napi-rs/issues/1973); nothing has shipped. We can hope this lands during 2026 but cannot depend on it. **Plan against the ~26 ns floor.**

### Distribution

napi-rs has a mature pipeline:

- `@napi-rs/cli new <pkg>` scaffolds a per-platform GitHub Actions matrix build.
- Prebuilds for Linux (x64, arm64, glibc + musl), macOS (x64, arm64), Windows (x64, arm64) ship as `@scope/<pkg>-<triple>` packages.
- Installer picks the right one via `optionalDependencies` and falls back to building from source.

For Nub this means: the published `@nub/<name>` npm packages on plain Node carry their own native addons via this pipeline. On Nub, the addon is statically inside the prelude — no separate dependency.

### The coarse-grained design rule

The 26 ns floor implies: **never put Rust on the inside of a JS hot loop.** The correct shape is one Rust call per logical operation, not one per token / per byte / per row.

Concrete examples:

- ✅ `hash(buffer)` — single call processes the whole buffer in Rust.
- ❌ `hashUpdate(byte)` called from a JS `for` loop — every byte pays 26 ns of boundary tax, dwarfing the hash itself.
- ✅ `parseTsconfig(path)` — read + parse + return one object.
- ❌ `parser.next()` returning one token at a time — N-API tax per token.
- ✅ `glob(pattern, opts) → string[]` — Rust walks the FS, returns all matches as one array.
- ⚠️ `glob(pattern, { onMatch(path) { ... } })` — N-API tax on every callback. Acceptable if N is small; problematic for huge trees.

This rule shapes the API design more than it shapes the implementation choice. When in doubt, batch on the Rust side and return a single result.

## Option 2 — WebAssembly

Rust compiled to WASM (via `wasm-bindgen` or `wasi-sdk`), loaded as an ES module or `WebAssembly.instantiate`d from the prelude.

### Pros

- **Lower call cost for primitives:** ~5–10 ns for an `i32 → i32` call inside an already-instantiated module in V8. Beats N-API for numerics.
- **Universal binary:** one `.wasm` for every platform. No prebuild matrix.
- **Sandboxed by default.** Useful if we ever execute untrusted Rust plugins (probably not relevant to Nub internals).

### Cons

- **Typed-data marshalling is expensive.** Strings, buffers, objects all cross the boundary by copying into/out of linear memory. Anything beyond `i32`/`f64` quickly drops below N-API performance.
- **No native OS interop.** Filesystem, networking, native dependencies — all of it has to go back through JS shims or WASI. The latency saved on the call boundary is squandered on the marshalling.
- **No threading without `wasm32-wasi-threads`,** which Node has partial support for. Pacquet, swc, lightningcss all use real OS threads; we don't want to give that up.

### When it makes sense

A self-contained pure-compute helper with hot per-call paths and primitive args/returns — e.g., a hash function, a fast checksum, a small text classifier. Not the default. **Default to N-API.**

## Option 3 — Rust sidecar over IPC

Rust runs in a separate process; Node IPCs to it over a Unix socket, named pipe, or shared memory.

### Per-call cost

Order microseconds — orders of magnitude worse than N-API or WASM. Round-trip latency dominates everything else.

### When it makes sense

Coarse-grained operations where the per-call latency is amortized:

- **Package install** — `nub install` calls the Rust pacquet engine once for "install this lockfile."
- **Build orchestration** — a bundler invoked once per build, not per module.
- **File watching** — start watchers in Rust, push notifications to Node when something changes. The Rust side aggregates; the IPC is cheap because rare.

### Relation to the `nub daemon` model

The plan's `nub daemon` (warm Node worker for `nubx` warm starts) is a **Node-side** daemon — pre-bootstrapped Node process, IPC from `nubx` into Node. A Rust sidecar would be a *separate* pattern: a long-lived Rust process that Node talks to. We may end up with both (Node daemon for warm JS execution, Rust sidecar for the install / watch / build engines). They shouldn't be conflated.

## Out of scope — direct V8 binding via a modified runtime

A fourth option exists in principle: modify Node to bind Rust against V8 directly, with no Node-API tax — `v8::Local<v8::Value>` semantics, fast-call qualifier, the works. This would also be the only way to add entries to the closed `node:*` namespace.

That path is out of scope per Nub's additivity policy and the trade-off analysis in [`forking-node.md`](forking-node.md). The takeaway for design here: if a Nub built-in's value proposition depends on per-call latency lower than ~26 ns, **we cannot ship that built-in.** Either redesign the API to amortize the boundary cost, or drop the feature.

## How this shapes Nub's design

1. **Default new APIs to JS** unless Rust offers something concrete: speed (with coarse-grained API shape), access to a Rust crate we already vendor (swc, lightningcss, pacquet), or correctness guarantees only Rust provides. The Nub rule is "every built-in ships as an npm package" — JS is the path of least resistance for that, and the addon-distribution complexity is a real tax.
2. **When we go Rust, ship via napi-rs N-API addons.** WASM is a special case for self-contained pure-compute helpers; sidecar is for coarse engine-level work.
3. **Coarse-grained API surface is mandatory.** Design Rust-backed APIs around "one call per operation," not "one call per element." Inversion of control via JS callbacks paid into Rust is suspect for high-N cases.
4. **Don't promise sub-N-API performance.** The 26 ns floor is real. APIs whose value depends on lower-than-that latency get redesigned to amortize, or get dropped.
5. **No `node:*` injection.** Even with the recent sync-hook fix for `node:*` interception ([`augmentation-layers.md`](augmentation-layers.md#augmentation-layer-b-per-file-loader-hooks-current-plan)), *intercepting* `node:fs` is not the same as *adding* `node:postgres`. Adding new entries to that namespace requires modifying the runtime, which is out of scope per [`forking-node.md`](forking-node.md).

## Open follow-ups

- **Bench the 26 ns claim against our actual N-API floor under Node 24 / 26.** Worth a quick micro-bench before we hard-code design rules around it. napi-rs's published numbers are from ~Node 20 era.
- **Track V8 Fast API exposure through Node-API.** If [napi-rs#1973](https://github.com/napi-rs/napi-rs/issues/1973) resolves with a stable user-facing entry, the floor drops dramatically and per-token Rust calls re-enter the design space.
- **Investigate `node-bindgen` and `neon`** as alternative Rust↔Node bindings. napi-rs is the obvious default but worth confirming we're not missing a perf or DX advantage. Initial data: napi-rs is the fastest of the three on the trivial number op (37.99M vs 23.98M neon vs 19.62M node-bindgen).
- **Prebuild matrix logistics for Nub's own addons.** napi-rs's CLI is mature, but we need to decide whether each `@nub/*` package owns its own matrix or whether one consolidated build pipeline produces them all.

## Sources

- napi-rs overhead benchmark: `github.com/Brooooooklyn/rust-to-nodejs-overhead-benchmark`
- N-API call cost history: [`nodejs/node#21072`](https://github.com/nodejs/node/pull/21072)
- V8 Fast API exposure discussion: [`napi-rs/napi-rs#1973`](https://github.com/napi-rs/napi-rs/issues/1973)
- napi-rs prebuild model: `napi.rs/docs/deep-dive/release`
- WASM call cost in V8: V8 blog, "Faster JS-to-Wasm calls" (2021)
- Node-API docs: `nodejs.org/api/n-api.html`

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
- 2026-07-30 — Staleness note: the build-orchestration example referred to a build subcommand Nub does not ship. The per-call overhead figures are unaffected.
