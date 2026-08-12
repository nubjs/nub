# Userland-package clobbering audit

**Date:** 2026-05-24. Scope: should Nub intercept specific `import`/`require` specifiers (`"ws"`, `"node-fetch"`, `"@js-temporal/polyfill"`, …) and serve a synthesized module wrapping Nub's already-present global, the way Bun does? Companion to [`polyfill-demand-audit.md`](polyfill-demand-audit.md).

## TL;DR

- **Bun's clobber set is exactly seven third-party packages, all fetch/WebSocket/abort plumbing:** `ws`, `undici`, `node-fetch`, `isomorphic-fetch`, `@vercel/fetch`, `utf-8-validate`, `abort-controller` (plus `next/dist/compiled/*` aliases for the same three). Source: [`src/bun.js/HardcodedModule.zig`](https://github.com/oven-sh/bun/blob/1cc83768/src/bun.js/HardcodedModule.zig) lines 374–389. Bun does **not** clobber `eventsource`, `better-sqlite3`, `@js-temporal/polyfill`, `urlpattern-polyfill`, `node-localstorage`, `reflect-metadata`, `web-worker`, or `bcrypt`.
- **Recommended Nub v0.1 clobber set: empty.** Userland packages like `ws` and `node-fetch` already work on the user's Node — they are not load-failures the way `node-fetch`/`undici` are inside Bun's WebKit/JSC runtime. The install-size win is small; the API-parity-bug surface (server-side `ws`, off-spec callbacks on `eventsource`, `Database` shape on `better-sqlite3` vs `node:sqlite`) is large. Reverse Bun's default-clobber posture to **opt-in only**.
- **Load-bearing safety concern: `ws` has a server.** Bun's `ws` shim is a 1,582-line file that reimplements `WebSocketServer` from scratch on top of `Bun.serve`, stubs `Receiver`/`Sender`/`createWebSocketStream` to throw `"Not supported yet in Bun"`, and has needed multi-year patch-work to add `upgrade`/`unexpected-response` events ([oven-sh/bun#28114](https://github.com/oven-sh/bun/pull/28114), [#4568](https://github.com/oven-sh/bun/issues/4568), [withastro/astro#15926](https://github.com/withastro/astro/issues/15926)). Any clobber of `ws` inherits that maintenance liability, and plain Node plus the user's installed `ws` works.
- **Meta-rec: defer all default clobbers to v0.x; ship two opt-in clobbers in v0.x once the resolve-hook is stable.** The two safe candidates are `@js-temporal/polyfill` and `urlpattern-polyfill` — pure spec-shim packages whose entire purpose is `globalThis.Temporal = …` / `globalThis.URLPattern = …`. Serving Nub's native instead is functionally identical, and the install-size win is the whole point. Everything else either fails the parity test or does not earn the install-cost saving.
- **Most surprising finding:** Bun does *not* clobber `better-sqlite3`, despite `bun:sqlite` being explicitly `better-sqlite3`-inspired; loading instead fails with `NODE_MODULE_VERSION` ABI errors ([oven-sh/bun#16050](https://github.com/oven-sh/bun/issues/16050)). The reason is the parity hazard this audit names: `bun:sqlite` is `better-sqlite3`-*shaped*, not `better-sqlite3`-*identical* (no `.aggregate`, no `.backup`, different `pragma` return shape), and Bun does not want ownership of those divergences. That conservatism is the model Nub adopts across the board.

## What Bun clobbers

Source of truth: [`src/bun.js/HardcodedModule.zig`](https://github.com/oven-sh/bun/blob/1cc83768/src/bun.js/HardcodedModule.zig), `bun_extra_alias_kvs` block (lines ~362–390). Each entry routes the specifier to a Bun-built JS module under `src/js/thirdparty/`.

| npm specifier | Bun behavior | Shim source |
|---|---|---|
| `ws` (+ `ws/lib/websocket`, `next/dist/compiled/ws`) | Full reimpl. `WebSocket` is a Node-EventEmitter wrapper over native `globalThis.WebSocket`. `WebSocketServer`/`Server` is a from-scratch reimpl over `Bun.serve`'s WebSocket upgrade. `Receiver`, `Sender`, `createWebSocketStream` throw `"Not supported yet in Bun"`. | [`src/js/thirdparty/ws.js`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/ws.js) (1,582 lines) |
| `undici` (+ `next/dist/compiled/undici`) | Full reimpl. `fetch` is `Bun.fetch`; `Response`/`Request`/`Headers`/`FormData`/`File`/`URL`/`AbortSignal`/`URLSearchParams` come from `$cpp("Undici.cpp", …)` C++ bindings. Streams adapt via `internal/webstreams_adapters`. | [`src/js/thirdparty/undici.js`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/undici.js) |
| `node-fetch` | Full reimpl with `node-fetch`-specific quirks (extends `URLSearchParams`, etc.) on top of `Bun.fetch` and the `NodeFetch.cpp` bindings. | [`src/js/thirdparty/node-fetch.ts`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/node-fetch.ts) |
| `isomorphic-fetch` | Thin wrapper: `default`/`.fetch`/`.default` all point at `Bun.fetch`. | [`src/js/thirdparty/isomorphic-fetch.ts`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/isomorphic-fetch.ts) |
| `@vercel/fetch` | Thin wrapper factory returning a function over `Bun.fetch` with JSON-body coercion. | [`src/js/thirdparty/vercel_fetch.js`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/vercel_fetch.js) |
| `abort-controller` (+ `abort-controller/polyfill`) | Forced to native `AbortController`/`AbortSignal`. | (alias-only) |
| `utf-8-validate` | Native fast-path (the package is a perf-critical native helper for `ws`). | (native binding) |

Notable omissions — Bun does **not** intercept any of these:

| npm specifier | Bun behavior |
|---|---|
| `eventsource` | Loads the real npm package; native `globalThis.EventSource` is separate ([Bun ref](https://bun.sh/reference/globals/EventSource)). |
| `better-sqlite3` | Fails to load (`NODE_MODULE_VERSION 131` vs Bun's `127`) ([oven-sh/bun#16050](https://github.com/oven-sh/bun/issues/16050)). |
| `@js-temporal/polyfill` | Loads the real npm package. (Bun has no native Temporal; [oven-sh/bun#15853](https://github.com/oven-sh/bun/issues/15853).) |
| `urlpattern-polyfill` | Loads the real npm package. (Native `URLPattern` shipped in v1.3.4 but the polyfill specifier is not intercepted.) |
| `node-localstorage`, `reflect-metadata`, `web-worker`, `bcrypt` | Real package. |

## Per-package safety analysis for Nub

Test column legend — *Safe?* asks: would a default clobber satisfy the brand-boundary "would-this-work-on-plain-Node-plus-`module.register()`" rule **and** preserve byte-for-byte compatibility for code that runs unchanged on vanilla Node? *Yes* only when (a) the package exports only spec-equivalent shapes, (b) the userland implementation is itself just a polyfill of that shape, and (c) Nub's native is a strict spec match.

| Package | Has non-global surface? | Spec divergence? | Cross-runtime portable? | Safe to default-clobber? | Nub action |
|---|---|---|---|---|---|
| `ws` | **Yes** — `WebSocketServer`, `Receiver`, `Sender`, `createWebSocketStream`, `WebSocket.Server` static. ~70% of dependents use the server side. | Many. `ws.WebSocket` extends Node `EventEmitter` (`.on('open', …)`), exposes `_socket`, `.terminate()`, perMessageDeflate per-connection negotiation, `handleProtocols`. Spec `WebSocket` is `EventTarget` with `addEventListener`. | No — server side has no spec counterpart. | **No** | **Don't clobber.** User's `ws` works on plain Node and on Nub; no install-size pressure justifies inheriting Bun's multi-year shim debt. |
| `node-fetch` | Yes — `Response`, `Request`, `Headers`, `FormData`, `Blob`, `fetchError`, `AbortError`, `isRedirect`. Stream-as-Node-Readable rather than WHATWG `ReadableStream`. | Yes — `node-fetch` predates WHATWG fetch; subtle differences in redirect handling, `body` (Node stream vs WHATWG), error subclasses. | No — clobbering hides the Node-stream `body` quirk. | **No** | **Don't clobber.** |
| `isomorphic-fetch` | No — side-effect-only install of global `fetch`. | Trivial. | Yes. | Technically yes, but: Node ≥18 already has `globalThis.fetch`, so the package is a no-op there. Clobbering saves 0 bytes that the user wasn't already paying. | **Don't clobber.** No win. |
| `cross-fetch` | Yes — exports `fetch`, `Headers`, `Request`, `Response`. Two-condition export (`browser` vs `node`); the node entry is a `node-fetch` wrapper. | Inherits `node-fetch`'s. | No. | **No** | **Don't clobber.** |
| `@vercel/fetch` | Factory function with JSON-body coercion; not a polyfill. | n/a — wraps any fetch. | n/a. | Clobbering changes behavior (Bun's shim coerces JSON; the real package retries). | **Don't clobber.** Not a polyfill at all; Bun's clobber here is questionable. |
| `undici` | Yes, large — `Agent`, `Dispatcher`, `Pool`, `Client`, `MockAgent`, `interceptors`, HTTP/2 hooks, connection-pooling APIs that have no global equivalent. | n/a — `undici` is itself the reference impl Node ships as `node:undici`. | No — server agent / dispatcher API is undici-specific. | **No** | **Don't clobber.** Node already ships `undici` natively as `node:undici`; the userland npm package is for users on older Node who need the latest. Nub has no improvement to offer. |
| `eventsource` | Modest — `EventSource` class plus constructor options not in spec (`https.RequestOptions`-style: `headers`, `agent`, `https`, `rejectUnauthorized`, `proxy`, custom retry hooks). Used heavily by SSE / LLM-streaming code. | Yes — userland constructor accepts a second arg with options the WHATWG global ignores. | **No** — code passing `new EventSource(url, { headers })` works on `eventsource` and fails on the spec global. | **No** | **Don't clobber.** This is the most-tempting candidate (36M dl/wk) and the most dangerous: the second-arg options are exactly what LLM-streaming clients use. |
| `@js-temporal/polyfill` | No — the package's entire job is `globalThis.Temporal = Temporal`. Exports the same `Temporal` namespace shape. | Polyfill *is* the spec; once Nub ships native Temporal, they're the same surface. | Yes — vanilla Node + the package gives identical observable result. | **Yes** | **Opt-in clobber, v0.x.** Genuine install-size win (the polyfill is ~200 KB). Reversible — user adds an entry to the project config to disable. |
| `urlpattern-polyfill` | Mostly no — main export is `URLPattern`. The package also exports a `URLPattern` named export and (in some versions) `URLPatternComponentResult` types; these are spec types. | Polyfill is the spec. | Yes. | **Yes** | **Opt-in clobber, v0.x.** Same logic as Temporal. |
| `web-worker` | Yes — re-exports browser `Worker` plus `Worker.deserialize` and other portability helpers. | Constructor accepts `{ type: "module" \| "classic", name, … }`; not all options round-trip across the Node `Worker` underlying impl. | Marginal. | **No** | **Don't clobber.** Polyfill exists precisely because of the cross-runtime fiddliness; clobbering loses the value-add. |
| `node-localstorage` | **Yes** — `LocalStorage(path)` constructor with custom on-disk path, `QUOTA_BYTES`, `_get`/`_set`/`_keys` test hooks. Default export is the *class*, not an instance. | Class-not-global is the entire shape difference. The spec `localStorage` is a singleton; the package exports a factory. | No — `new LocalStorage('./scratch')` has no global equivalent. | **No** | **Don't clobber.** The class-vs-singleton shape mismatch breaks every consumer. |
| `reflect-metadata` | No — pure side-effect polyfill of `Reflect.metadata`/`getMetadata`/etc. | Polyfill is the (stage-2) spec. | Yes. | Yes-on-shape, **but no on policy.** Per [`emit-decorator-metadata.md`](emit-decorator-metadata.md), Nub explicitly does **not** auto-inject decorator-metadata semantics. Clobbering this package auto-injects them by stealth. | **Don't clobber.** Policy-driven: would silently change semantics for code that ran a stage-2 polyfill explicitly. |
| `better-sqlite3` | **Yes**, large — `new Database(path, options)`, `.prepare`, `.exec`, `.transaction`, `.aggregate`, `.function`, `.backup`, `.pragma`, `.loadExtension`, `WAL` mode helpers. `node:sqlite` is `DatabaseSync` with overlapping-but-not-identical API: different option names, different `.all()` row shape (column-named obj vs raw), no `.aggregate`, no `.function`, no `.backup`. | Major. | No. | **No** | **Don't clobber.** Already rejected as additivity-violating. |
| `bcrypt` | Yes — native binding with `hash`, `compare`, `genSalt`, sync variants. | n/a (no native Nub equivalent). | n/a. | **No** | **Don't clobber.** No Nub-side implementation to clobber to. |

## The mechanism in Nub

A brand-boundary-clean intercept rides a resolve hook installed via Node's [`module.registerHooks()`](https://nodejs.org/api/module.html#moduleregisterhookspreoptions), registered from Nub's standard `--import` preload. On each resolution the hook checks the specifier against the clobber table; if matched and enabled for the project, it short-circuits to a synthesized module exporting the wrapper, and otherwise passes through. The same hook already exists for tsconfig-path rewriting and extensionless probing, so the marginal cost is one table plus the wrappers. The augmenter-vs-fork test passes: a user on plain Node could ship the same `module.register()` hook. Compat mode (`--node` / `NODE_COMPAT=1`) skips the hook, so the real npm package resolves.

## Recommendation

**Per-package decisions:** the table above is the full set — opt-in for `@js-temporal/polyfill` and `urlpattern-polyfill`, do not clobber for everything else.

**Meta-call: do not ship any default clobbering in v0.1.** Bun's seven clobbers exist because Bun is a *replacement* runtime where `node-fetch`, `ws`, and `undici` either won't load (`undici` depends on Node internals JSC doesn't have) or are redundant with native primitives. Nub *augments* the user's actual Node, where those packages load and run. Bun pays the parity cost because it must; Nub would pay it for an install-size lottery ticket. The recurring `ws`-shim parity bugs ([#4568](https://github.com/oven-sh/bun/issues/4568), [#28114](https://github.com/oven-sh/bun/pull/28114), [withastro/astro#15926](https://github.com/withastro/astro/issues/15926)) are existence proof that this is hard to get right.

**In v0.x, ship opt-in clobbering for Temporal and URLPattern.** Both exist solely to install the spec global. The user opts in via a brand-clean config file (no `"nub"` field, no `NUB_*` env, no `nub:*` specifier); removing the entry restores the real resolve.

**General rule:** Nub clobbers a userland package iff (a) it is a pure spec-shim with no non-spec surface, (b) it is in the user's opt-in list, and (c) Nub's native passes a parity test gating the clobber. Default-clobbering — Bun's posture — is rejected.

## Sources

- Bun alias table: [`src/bun.js/HardcodedModule.zig`](https://github.com/oven-sh/bun/blob/1cc83768/src/bun.js/HardcodedModule.zig) lines 362–390 (third-party `bun_extra_alias_kvs`)
- Bun `ws` shim: [`src/js/thirdparty/ws.js`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/ws.js)
- Bun `undici` shim: [`src/js/thirdparty/undici.js`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/undici.js)
- Bun `node-fetch` shim: [`src/js/thirdparty/node-fetch.ts`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/node-fetch.ts)
- Bun `isomorphic-fetch` shim: [`src/js/thirdparty/isomorphic-fetch.ts`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/isomorphic-fetch.ts)
- Bun `@vercel/fetch` shim: [`src/js/thirdparty/vercel_fetch.js`](https://github.com/oven-sh/bun/blob/main/src/js/thirdparty/vercel_fetch.js)
- Module-loader entrypoint: [`src/bun.js/ModuleLoader.zig`](https://github.com/oven-sh/bun/blob/1cc83768/src/bun.js/ModuleLoader.zig) `Bun__resolveAndFetchBuiltinModule` (line 838: `HardcodedModule.Alias.bun_aliases.getWithEql(...)`)
- `ws` `Receiver`/`createWebSocketStream` parity bug: [oven-sh/bun#4568](https://github.com/oven-sh/bun/issues/4568)
- `ws` `upgrade`/`unexpected-response` event support added: [oven-sh/bun#28114](https://github.com/oven-sh/bun/pull/28114)
- `ws` event mismatch breaking real apps under Bun: [withastro/astro#15926](https://github.com/withastro/astro/issues/15926)
- `better-sqlite3` not clobbered, fails ABI load: [oven-sh/bun#16050](https://github.com/oven-sh/bun/issues/16050)
- Bun missing Temporal: [oven-sh/bun#15853](https://github.com/oven-sh/bun/issues/15853)
- Companion polyfill demand audit: [`polyfill-demand-audit.md`](polyfill-demand-audit.md)
- Companion decorator-metadata audit (basis for the `reflect-metadata` reject): [`emit-decorator-metadata.md`](emit-decorator-metadata.md)

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
