# Polyfill demand audit (v0.1 globals)

**Date:** 2026-05-24. Scope: the 12 globals Nub plans to ship in v0.1 under "Modern APIs" and "WinterTC compatible". Two questions: (1) what is the npm download demand for each one's userland polyfill equivalent, and (2) does Bun ship it natively?

## TL;DR

Download demand splits the twelve globals into a keep list, a quieter mid-tier, and a trim list.

- **Headline number:** four of the twelve have real userland demand — `ws` (223M/week), `eventsource` (36M/week), `urlpattern-polyfill` (18M/week), `better-sqlite3` (6.9M/week). Everything else is ≤1.2M/week, and three (`node-self`, `report-error`, and any `PromiseRejectionEvent` package) round to zero.
- **Trim list:** `reportError`, `self`, and `PromiseRejectionEvent` total ~2k downloads/week between them, and Bun ships only one of the three. Cut from v0.1 ship copy; demote to WinterTC-parity-later or skip.
- **Recommendation:** keep the eight surfaces with real signal (Temporal, URLPattern, WebSocket, Worker, EventSource, localStorage/sessionStorage, vm.Module, node:sqlite). Drop the three WinterTC-gap globals from prominent v0.1 marketing; keep them as silent compliance if free.

## Matrix

Per global: the userland polyfill developers install today, its weekly npm download count, and whether Bun ships the surface natively.

| Global | Userland polyfill | Weekly npm downloads (2026-05-17 → 2026-05-23) | Bun native? | Source |
|---|---|---|---|---|
| `Temporal` | `@js-temporal/polyfill` | 1,216,585 | **No** — tracked in [oven-sh/bun#15853](https://github.com/oven-sh/bun/issues/15853); JSC implementation only available behind `BUN_JSC_useTemporal=1` env flag, incomplete | [npm](https://api.npmjs.org/downloads/point/last-week/@js-temporal/polyfill) |
| `URLPattern` | `urlpattern-polyfill` | 18,484,400 | **Yes** — Bun v1.3.4 (Nov 2025), native via WebKit impl | [npm](https://api.npmjs.org/downloads/point/last-week/urlpattern-polyfill), [Bun blog](https://bun.com/blog/bun-v1.3.4) |
| `WebSocket` | `ws` | 223,383,960 | **Yes** — client `new WebSocket()` long-shipped; server via `Bun.serve` | [npm](https://api.npmjs.org/downloads/point/last-week/ws), [Bun docs](https://bun.com/docs/runtime/http/websockets) |
| `Worker` (browser-shape) | `web-worker` | 4,695,060 | **Yes** — `new Worker()` listed in [Bun APIs table](https://bun.com/docs/runtime/bun-apis) | [npm](https://api.npmjs.org/downloads/point/last-week/web-worker) |
| `reportError` | `report-error` (only candidate) | 23 | **Yes** — listed in [Bun globals](https://bun.com/docs/runtime/globals) | [npm](https://api.npmjs.org/downloads/point/last-week/report-error) |
| `self` | `node-self` | 2,072 | **No** — not in Bun's globals table; historically polyfilled as `globalThis.self = globalThis` ([oven-sh/bun#250](https://github.com/oven-sh/bun/issues/250)) | [npm](https://api.npmjs.org/downloads/point/last-week/node-self) |
| `PromiseRejectionEvent` (constructor) | none popular — no `promise-rejection-event` or `reporterror` package exists on npm | 0 (no package) | **No** — not listed in Bun globals; Bun emits `unhandledrejection`/`rejectionhandled` events on `globalThis` but does not expose the constructor | [npm 404](https://api.npmjs.org/downloads/point/last-week/promise-rejection-event), [Bun globals](https://bun.com/docs/runtime/globals) |
| `EventSource` | `eventsource` | 36,388,627 | **Yes** — landed in [oven-sh/bun#29960](https://github.com/oven-sh/bun/pull/29960) as `globalThis.EventSource` | [npm](https://api.npmjs.org/downloads/point/last-week/eventsource), [Bun ref](https://bun.sh/reference/globals/EventSource) |
| `localStorage` / `sessionStorage` | `node-localstorage` | 548,693 | **No** — tracked in [oven-sh/bun#19115](https://github.com/oven-sh/bun/issues/19115), throws `ReferenceError`; userland alternatives are `bun-storage` and `@thai/storage` | [npm](https://api.npmjs.org/downloads/point/last-week/node-localstorage) |
| `vm.Module` / `SourceTextModule` | n/a (V8-level, no userland polyfill possible) | — | **No** — explicitly "not implemented" per [Bun vm docs](https://bun.sh/reference/node/vm/SourceTextModule); core `vm.Script` works but not the ES Modules variants | Bun docs |
| `node:sqlite` | `better-sqlite3` | 6,890,284 | **Yes** — Bun has long-shipped `bun:sqlite` (same API shape as `better-sqlite3`); `node:sqlite` is also supported | [npm](https://api.npmjs.org/downloads/point/last-week/better-sqlite3), [Bun docs](https://bun.com/reference/node/url/URLPattern) |

All download counts pulled from `https://api.npmjs.org/downloads/point/last-week/<pkg>` on 2026-05-24, covering the most recent fully-elapsed UTC week (2026-05-17 → 2026-05-23 inclusive).

## Findings

Four tiers, ordered from strongest userland signal to none, plus one surface no userland polyfill can reach.

### Real signal (keep)

The four strongest surfaces, each with a userland equivalent above 6M weekly downloads.

- **`ws` at 223M/week** — the `WebSocket`-shaped polyfill people install. Nub does not displace it for server-side WebSocket: `ws` is the server-acceptor, and Nub's offering is the client shape `new WebSocket(url)`. Native in Node 22.5+, so Nub's window is 22.0–22.4, and the mechanism is a `--experimental-websocket` auto-inject rather than a third-party polyfill.
- **`eventsource` at 36M/week** — LLM-streaming clients and SSE consumers pull this. Nub auto-injects `--experimental-eventsource` instead of requiring `npm i eventsource`. Bun ships it natively.
- **`urlpattern-polyfill` at 18M/week** — native in Node 24+, so Nub's polyfill window is Node 22.x. Bun shipped it in v1.3.4.
- **`better-sqlite3` at 6.9M/week** — established embedded-SQLite demand. Nub auto-injects the `node:sqlite` flag, so users who would have reached for `better-sqlite3` get the same ergonomics out of the box.

### Mid-tier (keep, but quieter)

Three surfaces between 549k and 4.7M weekly downloads — enough demand to ship, not enough to lead with.

- **`@js-temporal/polyfill` at 1.2M/week** — a fraction of what URLPattern or EventSource pull, and most consumption is via libraries (226 dependents) rather than direct application use. Bun has no native Temporal. Worth keeping in v0.1: Temporal is the largest TC39 proposal in years, and exposing it without ceremony differentiates Nub from both Node and Bun.
- **`web-worker` at 4.7M/week** — the browser-shape `Worker` over `worker_threads` is a recurring portability ask. Bun ships it.
- **`node-localstorage` at 549k/week** — modest but not negligible. Bun does not ship it natively (gap since 2025); Deno does.

### No signal (the trim list)

Three WinterTC globals with roughly 2,100 weekly downloads between them.

- **`reportError` at 23/week** (`report-error` package). Bun ships it natively. WinterTC spec compliance and nothing else.
- **`self` at 2,072/week** (`node-self`) — an earlier note flagged this as ~50/week; the current measurement is ~2k/week, one order of magnitude below `node-localstorage` and four below `eventsource`. Bun does not ship it as a documented global.
- **`PromiseRejectionEvent` — no package exists.** No popular npm polyfill, so no measurable userland demand. Bun dispatches the event names on `globalThis` but does not expose the constructor.

### V8-level (not a polyfill question)

One surface where npm download counts cannot measure demand, because no userland package can implement it.

- **`vm.Module` / `SourceTextModule`** — V8-internal machinery, so no userland polyfill is possible, and Bun does not implement it. Nub auto-injects `--experimental-vm-modules`, which unblocks Jest's and Vitest's ESM modes; those paths do not work under Bun.

## Recommendation

**Keep eight, demote three.** The eight for front-of-house v0.1 marketing are Temporal, URLPattern, WebSocket, Worker, EventSource, localStorage/sessionStorage, vm.Module, and node:sqlite.

Each of the eight is a clear ecosystem hunger (≥500k/week userland equivalent), a Jest/Vitest unblock with no other path (vm.Module), or a deliberate parity claim against Bun. Drop `reportError`, `self`, and `PromiseRejectionEvent` from prominent v0.1 messaging: ~2,100 downloads/week across all three, Bun ships only one of them, and they appear in no v0.1-relevant developer story.

The whitepaper's "WinterTC compatible" section reads as claiming a clean checkmark on min-common-api.proposal.wintertc.org, which works as silent compliance but does not deserve real estate in the headline modern-APIs pitch. If shipping them costs a few lines of preload, keep the implementation and let the conformance table speak for itself; if it costs more, move them to a WinterTC-conformance sub-page or defer to v0.2.

## Changelog

The single entry records this doc's move out of the internal research corpus.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
