# Polyfill demand audit (v0.1 globals)

**Date:** 2026-05-24. Scope: the 12 globals Nub plans to ship in v0.1 per `wiki/whitepaper.md` §"Modern APIs" and §"WinterTC compatible". Two questions only: (1) what's the npm download demand for the userland polyfill equivalent of each, and (2) does Bun ship it natively?

## TL;DR

- **Headline number:** four of the twelve have real userland demand — `ws` (223M/week), `urlpattern-polyfill` (18M/week), `eventsource` (36M/week), `better-sqlite3` (6.9M/week). Everything else is ≤1.2M/week, and three (`node-self`, `report-error`, and any `PromiseRejectionEvent` package) round to zero — there is no userland market for these polyfills because there is no userland demand.
- **Trim list:** `reportError`, `self`, and `PromiseRejectionEvent` are polishing-something-nobody-needs. Total userland demand: ~2k downloads/week combined. Bun doesn't even ship two of the three. Cut from v0.1 ship copy; demote to "WinterTC parity, ship later" or skip entirely.
- **Recommendation:** keep the eight surfaces with real signal (Temporal, URLPattern, WebSocket, Worker, EventSource, localStorage/sessionStorage, vm.Module, node:sqlite). Drop the three WinterTC-gap globals from prominent v0.1 marketing — they're real spec compliance but no one is asking. Keep them as silent compliance if free.

## Matrix

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

All download counts pulled directly from `https://api.npmjs.org/downloads/point/last-week/<pkg>` on 2026-05-24. Window covers the most recent fully-elapsed UTC week (2026-05-17 → 2026-05-23 inclusive).

## Findings

### Real signal (keep, market hard)

Four polyfills are clearly load-bearing in the Node ecosystem today:

- **`ws` at 223M/week** — the gold standard. `WebSocket`-shaped polyfill is *the* polyfill people install. Note that Nub isn't going to displace `ws` for server-side WebSocket — `ws` is the server-acceptor; Nub's offering is the *client* shape `new WebSocket(url)`. But the headline number proves there is intense ecosystem hunger for the W3C shape on Node. (Native in Node 22.5+ anyway, so Nub's polyfill window is just 22.0–22.4. Per the whitepaper this is a `--experimental-websocket` flag auto-inject, not a third-party polyfill.)
- **`eventsource` at 36M/week** — huge demand. Every LLM-streaming client, every SSE consumer, every server-sent-events demo pulls this. Nub's value-add is auto-flag-injecting `--experimental-eventsource` instead of forcing users to `npm i eventsource`. Bun ships it natively; we'd be matching Bun, closing a real Node gap.
- **`urlpattern-polyfill` at 18M/week** — huge. Native in Node 24+; Nub's polyfill window is Node 22.x. Bun shipped this in v1.3.4. Clear keep.
- **`better-sqlite3` at 6.9M/week** — well-established embedded-SQLite hunger. Nub's `node:sqlite` flag auto-inject is the right move; users who would have reached for `better-sqlite3` get the same ergonomics out of the box.

### Mid-tier (keep, but quieter)

- **`@js-temporal/polyfill` at 1.2M/week** — meaningful but a fraction of what URLPattern or EventSource pull. Most consumption is via libraries (226 dependents), not direct application use. Bun still doesn't have it natively. Worth keeping in v0.1 because Temporal *is* the biggest TC39 proposal in years and being the first runtime to expose it without ceremony is a real differentiator vs Node and Bun both.
- **`web-worker` at 4.7M/week** — solid mid-tier demand. The browser-shape `Worker` over `worker_threads` is a recurring portability ask. Bun ships it; matching Bun here is table stakes.
- **`node-localstorage` at 549k/week** — modest but not negligible. Bun *doesn't* ship this natively (gap since 2025); Deno does. Nub shipping it would beat Bun on a small-but-real surface.

### No signal (the trim list)

- **`reportError` at 23/week** (`report-error` package) — twenty-three. Bun ships it natively, which is the only thing keeping it from being a pure curio. WinterTC spec compliance and that's it.
- **`self` at 2,072/week** (`node-self`) — an earlier note flagged this as ~50/week; current measurement is ~2k/week, still tiny in context (one order of magnitude below `node-localstorage`, four orders below `eventsource`). Bun doesn't even ship it as a documented global.
- **`PromiseRejectionEvent` — no package exists.** No popular npm polyfill, no userland demand we can measure. Bun doesn't expose the constructor either (it dispatches the event names on `globalThis` but doesn't ship the class).

The three WinterTC-gap globals together have essentially zero market validation. They're spec-compliance polish, not user-felt features.

### V8-level (not a polyfill question)

- **`vm.Module` / `SourceTextModule`** — no userland polyfill is possible; this is V8-internal machinery. Bun explicitly does not implement it. Nub's value here is auto-flag-injecting `--experimental-vm-modules`, which unblocks Jest and Vitest's ESM modes. That's a real differentiator vs Bun (where Jest/Vitest's ESM paths simply don't work).

## Recommendation

**Keep eight, demote three, leave one as-is.** The eight to keep as front-of-house v0.1 marketing are Temporal, URLPattern, WebSocket, Worker, EventSource, localStorage/sessionStorage, vm.Module, and node:sqlite — each is either a clear ecosystem hunger (≥500k/week userland equivalent), a Jest/Vitest unblock with no other path (vm.Module), or a deliberate parity claim against Bun where Nub can credibly say "and we have this too." Drop `reportError`, `self`, and `PromiseRejectionEvent` from the prominent v0.1 messaging — combined demand is ~2,100 downloads/week across all three, Bun ships only one of them, and they don't show up in any v0.1-relevant developer story. The whitepaper's current §"WinterTC compatible" section reads like Nub trying to claim a clean checkmark on min-common-api.proposal.wintertc.org, which is fine as silent compliance but doesn't deserve real estate in the headline modern-APIs pitch. If shipping them is free (a few lines of preload) keep the implementation and let the conformance table speak for itself; if it costs more than that, defer to v0.2 and move on. The trim list: drop `reportError`, `self`, `PromiseRejectionEvent` from the front-page modern-APIs section of the whitepaper; either move them to a "WinterTC conformance, full details" sub-page or scope them out of v0.1 entirely.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
