# Research: trade-offs of forking Node.js

**Status:** v1, 2026-05-16.

## Question

Nub's design is purely additive: it extends Node from outside the runtime via loader hooks, a Rust CLI, and a bundled-and-spawned `node` binary. The alternative shape is a Node.js fork — modifying the runtime directly.

The conclusion: Nub stays external. The compatibility trust contract — "Nub runs everything Node runs, byte-for-byte" — is the wedge, and a fork risks it. Most of the fork-only capabilities catalogued below have viable external approximations, and none justify trading that contract.

## What "fork Node" would mean concretely

A Node fork would be a divergent maintained tree of `nodejs/node` with:

- Its own release cycle and a corresponding sync burden against upstream Node, V8, libuv, ICU, OpenSSL, undici, llhttp, c-ares.
- Direct V8 access (Fast API qualifiers, snapshots, embedder hooks).
- The ability to add entries to the closed `node:*` namespace.
- The ability to ship native built-ins compiled into the runtime binary.
- The ability to change defaults (loader paths, flag defaults, startup behavior).

The fork would still need to pass Node's test suite, preserve the npm ecosystem's NAPI guarantees, and stay close enough to upstream that npm packages built for Node load unmodified.

## Upsides catalogue

What runtime-level control could deliver that external tooling cannot, ordered roughly by user-visible impact.

### Startup time floor

Node's nominal cold start is ~30 ms but real-world `node script.js` is ~100–200 ms once module init runs. Bun starts in single-digit ms. A fork could:

- Broaden V8 startup snapshots to include more JS-land bootstrap.
- Lazy-init built-in modules.
- Shrink the default JS bootstrap surface.
- Trim loader overhead (the `--import` chain, hook plumbing).

External approximation: a warm-worker daemon (`nub daemon`) pays cold start once per project, closing most of the gap interactively and none of it in CI or other non-interactive contexts.

### Runtime-native TS/JSX

Loader-hook pre-processing costs at minimum a content-hash cache lookup per touched module, plus the swc transform on cache miss. A fork could parse TS/JSX in V8's parser pipeline, eliminating the round-trip and the cache-management surface.

External approximation: a content-addressed disk cache (`~/.cache/nub/<hash>.js`) reduces warm runs to stat-bound; cold runs still pay swc. The remaining gap is small in practice.

### Native data clients in `node:*`

Upstream ships `node:sqlite`, but not `node:postgres`, `node:redis`, or `node:s3` — and the `node:*` namespace is closed to userland. A fork could ship native clients in core.

External approximation: npm packages with native addons (`pg`, `ioredis`, `@aws-sdk/client-s3`) already exist and are fast. The fork's value-add is "batteries-included" rather than "achievable".

### Sub-N-API call cost

Stock Node's NAPI boundary is ~26 ns/call for trivial work, ~230 ns returning an object (see [`rust-from-js.md`](rust-from-js.md)). A fork could bind Rust directly to V8 via Fast API qualifiers, dropping per-call cost into single-digit ns.

External approximation: design Rust-backed APIs coarse-grained (one call per operation, not per token). Sufficient for most cases; APIs needing true per-element Rust on a hot JS loop have no external workaround.

### Sandbox / permission model

Node 23+ ships `--permission` with `--allow-fs-read`, `--allow-fs-write`, `--allow-net`, `--allow-worker`. A fork could extend this — finer-grained capabilities, default-deny shapes, per-package permissions, structured audit logs.

External approximation: stock Node's permission model, sufficient for the common cases.

### In-runtime module replacement (real server-side HMR)

Upstream demand: [nodejs/node#49442](https://github.com/nodejs/node/issues/49442) ("Invalidate import cache", 90 reactions), [nodejs/node#61767](https://github.com/nodejs/node/pull/61767) ("`Module.clearCache()` for CJS+ESM", 32 reactions). Current server-HMR options (`hot-hook`, Vite SSR + Module Runner, Bun `--hot`) reimplement a module graph the runtime already maintains internally. A fork could expose a primitive: "replace module X's exports with these new bindings, invalidate transitively, preserve identified live state across the swap."

External approximation: the rewrite-specifier trick every userland implementation uses, which is the basis for Nub's HMR primitive. Its ceiling is monotonic memory accumulation — old module records cannot be evicted from V8's ESM map — which matters for long dev sessions but does not block shipping.

### Snapshot / cache primitives exposed to users

Upstream demand: [nodejs/node#35711](https://github.com/nodejs/node/issues/35711) (snapshot integration tracking), [nodejs/node#38905](https://github.com/nodejs/node/pull/38905) (userland `--build-snapshot`, partially landed). V8 startup snapshots are a Node-internal mechanism today. A fork could expose them as a user-facing primitive: "snapshot this module graph as initialized; subsequent runs skip parse and init."

External approximation: Node's `--build-snapshot` exists but is experimental and narrow. Nub can use it today for per-project startup snapshots; broader built-in-module coverage is a custom-Node-build (no source patches) win.

### Single-binary distribution with embedded asset linking

Node 21+ has SEA (single executable application) but it's experimental and clunky. A fork could productize: `compile script.ts -o myapp` produces a stripped binary with bundled assets, native deps, and a small runtime — competitive with `deno compile` and `bun build --compile`.

External approximation: stock Node SEA + manual asset embedding works but is rough. The gap is polish and DX, not capability.

### Default flag posture

A fork could ship with `--enable-source-maps`, `--experimental-vm-modules`, sane defaults for harmony flags, etc. on by default — defaults Node won't change for compatibility reasons.

External approximation: Nub's spawn pipeline can inject default flags into every child `node`. Already planned.

### Inspector / diagnostics surface

Upstream demand: [nodejs/node#57992](https://github.com/nodejs/node/issues/57992) ("Integrate OpenTelemetry", 106 reactions), [nodejs/node#61907](https://github.com/nodejs/node/pull/61907) (OTEL module implementation), [nodejs/node#49296](https://github.com/nodejs/node/issues/49296) ("Structured log in core", 24 reactions). A fork could ship richer built-in observability: structured diagnostic channels, per-route HTTP timings, automatic event-loop-lag reporting, GC and JIT spans on by default — events `diagnostic_channel` does not currently expose.

External approximation: stock Node's `diagnostic_channel` + AsyncLocalStorage cover the HTTP / DB / DNS / undici subsystems well, and Nub bundling auto-instrumentations on top gives zero-config observability. The remaining fork-only piece is visibility into V8-internal events (GC, JIT, scheduler hops) and eliminating the residual monkey-patching some auto-instrumentations still do at module-load time.

### Stack traces, error formatting, REPL polish

A fork could ship better default error formatting, automatic source-map resolution, inlay hints in REPL output, etc.

External approximation: a Rust-side error formatter in the spawn pipeline can post-process stderr. Crude but workable.

### Build system control

Node uses GYP. A fork could move (or supplement) with a saner build system — cargo for vendored Rust crates, ninja for everything else (workerd-style). Benefit is fork-maintainer ergonomics, not user-visible.

### Custom V8 flag exposure

Some V8 features sit behind flags Node hasn't promoted (or has promoted with caveats). A fork could surface them: `--js-shareable-objects`, harmony Temporal until Node ships it, turboshaft tunings.

External approximation: pass flags via the spawn pipeline. Works for most.

### Workers / threading primitives

Node's worker pool ergonomics are weak: every worker is its own file, there are no inline-function workers, and `postMessage` copies by default. Most of the proposed improvements are additive npm packages and need no fork. The runtime-level pieces — worker cold start, ESM loader-hook regression inside workers, compiled-code retention across worker spawns — require fork-side work or upstream patches.

### Process model

A fork could ship `Bun.spawn`-style fast process spawn, `posix_spawn` defaults, or move stdio handling closer to OS-level zero-copy paths.

External approximation: stock `child_process` is fine for most users.

### Embedded HTTP / fetch handler model

Workers-style entrypoints (`export default { fetch }`) could be served by a Rust HTTP listener inside the runtime itself, no spawn boundary.

External approximation: `nub serve` over the spawn pipeline. Already planned.

## Downsides catalogue

What a fork would cost.

### Maintenance burden, perpetual

Node releases roughly monthly (with quarterly LTS cuts). V8 alone gets a major version every 4–6 weeks. Staying in sync requires:

- Continuous rebase / merge cadence against `nodejs/node`.
- Tracking V8, libuv, ICU, OpenSSL, undici, llhttp, c-ares releases independently.
- Backporting security patches on Node's schedule.
- Running Node's full test suite, which takes hours.

This is a team commitment, not a project. Bun has spent years on this and still ships re-implementation drift, with a larger team and a longer head start.

### Trust contract erosion

Nub's wedge is "everything Node runs, Nub runs." Every divergence — even unintentional, even in service of a feature — is a potential surprise bug. Vite-on-Bun's brokenness is overwhelmingly re-implementation drift in Bun's NAPI / stdlib, not Vite reaching into exotic internals.

This is the strongest argument for staying external: Nub's compat is not "we tried hard," it is "we are Node." That property is structurally lost the moment a fork happens, however careful the divergence.

### Reversibility loss

The "code targeting Nub runs unmodified on plain Node" property collapses for any user who depends on a fork-only API. The additivity policy partly insulates against this (additions that ship as npm packages run on plain Node too), but runtime-level features (startup speed, in-runtime HMR primitives, native `node:postgres`) cannot be polyfilled into stock Node.

### Ecosystem signaling

A fork reads as "competing with Node." Maintainers and standards bodies may engage less; upstream contributions get scrutinized through a competitive lens; partnership conversations get harder. Staying additive frames Nub as "tooling on top of Node," which is collaborative posture.

### Distribution complexity

A fork would ship a substantially larger binary (V8 + libuv + ICU + OpenSSL all vendored), per-platform builds across Node's matrix, codesigning across macOS/Windows, and a real installer story. The "single static Rust binary" pitch dies.

### Team scaling

Maintaining a runtime fork requires C++/V8, build-system, and OS-platform engineers — distinct from the Rust CLI and tooling expertise the additive path needs. The fork is a separate organization, not a separate file in the repo.

### Test surface

Node's test suite is enormous (and de facto runs Vite's, Vitest's, and every framework's). A fork runs all of it on every change, which is non-trivial CI infrastructure.

### Slow controversial change

Anything controversial enough to be worth forking for is also controversial enough that upstreaming it is slow. Forks accumulate divergence; divergence accumulates maintenance cost. The interesting forks in this space (io.js, Node-ChakraCore, Bun) either rejoined or stayed permanently parallel — there isn't really a third outcome.

### Compat regressions in npm packages

The ~20k npm packages with native addons are tested against Node's NAPI. A fork's NAPI must be byte-identical. Any divergence — even an "improvement" — breaks packages silently.

### Versioning confusion

Users have to know which Node version a given fork version corresponds to, and whether a `nodejs.org` security advisory applies to it.

## What can be done without forking

Most upsides have viable external paths:

| Upside | External approximation | Gap remaining |
|---|---|---|
| Startup time | `nub daemon` warm worker | CI / non-interactive |
| TS/JSX | Loader hooks + disk cache | Marginal (warm runs are stat-bound) |
| Native data clients | npm packages with native addons | "Batteries included" framing only |
| Sub-N-API call cost | Coarse-grained API design | Per-element hot loops in JS |
| Permissions | Node's `--permission` flag | Coarseness |
| In-runtime HMR | None clean | Genuine runtime-shaped gap |
| Snapshots | `--build-snapshot` (experimental) | Polish, surface |
| Single binary | Node SEA + asset embedding | DX polish |
| Default flags | Spawn-pipeline injection | None practically |
| Workers ergonomics | Additive npm packages | Worker cold-start, hook regression |
| HTTP handler model | `nub serve` over spawn | None practically |

Three capabilities have no clean external approximation: in-runtime module replacement for server HMR, sub-N-API call latency for per-element hot paths, and entries in the closed `node:*` namespace.

## What would change the calculus

The conditions that would shift the "stay external" conclusion:

- **Vite or a Vite-class tool exposes a runtime-coupled API** that needs in-runtime module replacement to work correctly. Currently no such API exists; current server-HMR is userland workarounds.
- **A specific Nub-adopted workload** (the user base, when it exists) hits the N-API floor and there's no coarse-grained redesign that works.
- **Node upstream explicitly closes the door** on a feature Nub's users need. Currently Node is moving toward more exposure of internals (`module.registerHooks` sync variant, `vm/modules` proposal, permission model), which makes the external path stronger over time, not weaker.
- **The npm-package-as-public-API contract breaks down** for a class of capability we want to ship. Currently it holds for everything in scope.

Worth periodically re-running this analysis (annually, or when any of the above shifts).

## Caveats / gaps

- The N-API floor (26 ns trivial, 230 ns object) is from napi-rs's benchmark suite; not independently verified. Worth re-running on Node 24+.
- "Node has no clean in-runtime HMR primitive" reflects May 2026 state. The `vm/modules` proposal ([nodejs/node#62720](https://github.com/nodejs/node/issues/62720)) could approach this from outside.
- Bun's maintenance posture is the closest reference for what fork-cost looks like. Whether Bun's drift-rate is representative of any fork or specific to its language-translation history is debatable.
- The "stay external" posture is contingent on Nub's compat-first wedge holding. If the positioning shifts to "we are an alternative runtime, not a Node companion," the trade-offs flip.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
