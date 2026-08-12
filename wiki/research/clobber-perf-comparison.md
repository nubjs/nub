# Clobber-candidate runtime-perf comparison (native vs userland)

**Date:** 2026-05-24. Companion to [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md). Question: per candidate, is there a runtime-perf win — beyond install-size — from serving Nub's native equivalent?

## TL;DR

- **Temporal is the only candidate with a clear, large runtime-perf win.** The polyfill is ~125 KB unpacked / ~52 KB gzipped and ships its own software BigInt (JSBI) because it must run in ancient targets; native V8 Temporal is C++ on native `bigint`. No head-to-head bench is published, but the architectural delta is decisive and the cold-start parse cost saved (~1.25 ms/process at the Cloudflare-workers ~10 µs/KB ballpark) is real on every invocation. URLPattern is a distant second by parse-cost alone (~10–15 KB saved).
- **The surprising losers are `node-fetch` and `ws`.** Both are essentially tied with their native equivalents on published benches — `node-fetch` 5945 req/s vs undici-fetch 5904 req/s on Node 22.11.0; `ws` 100.69 MiB/s vs Node's undici-backed WebSocket 102.46 MiB/s. The "JS wrapper vs C++" intuition is wrong here: Node's `fetch` is itself a Web-Streams-wrapped JS layer over undici's C++ HTTP/1.1 parser, and `ws` does the same WebSocket framing in JS that undici's `WebSocket` does. No throughput win from clobbering.
- **Load-bearing methodology caveat:** "native" doesn't mean "C++ all the way down." Native `fetch`, `WebSocket`, and `EventSource` in Node are all JS-on-top-of-undici (the same library you'd `npm i undici`), with the Web Streams adapter layer adding measurable overhead vs raw `undici.request`. For these three, clobbering trades one JS layer for an equivalent JS layer; the win is API parity and bug fixes, not throughput. The only candidates whose native is meaningfully closer to the metal are `Temporal` (V8 C++) and `AbortController` (V8 internal).
- **Re-rank for perf-magnitude clobber priority:** (1) `@js-temporal/polyfill`, (2) `urlpattern-polyfill`, (3) `abort-controller` — and nothing else clears the bar. `node-fetch`, `ws`, and `eventsource` should be removed from any perf-driven clobber list; their case is install-size or freshness, not speed. The audit's earlier "opt-in only for Temporal + URLPattern" recommendation survives this perf review and gains a third candidate.

## Per-candidate perf table

| Package | Native equivalent | Cold-start parse saved (≈10 µs/KB unpacked) | Hot-path delta (native vs userland) | Source |
|---|---|---|---|---|
| `@js-temporal/polyfill` | `Temporal` (V8 13.x, Node 26+) | **~1.25 ms** (125 KB unpacked) | Native is V8 C++ on native `bigint`; polyfill is TS on JSBI. No head-to-head bench published. **Unverified — would need benchmarking.** | [npm](https://www.npmjs.com/package/@js-temporal/polyfill), [JSBI note](https://github.com/js-temporal/temporal-polyfill/), [Bryntum Temporal vs Date](https://bryntum.com/blog/javascript-temporal-is-it-finally-here/) |
| `urlpattern-polyfill` | `URLPattern` (Node 24+) | **~0.15 ms** (~15 KB unpacked) | Native ~30 % faster on `test()` (2.32 µs vs 3.02 µs), tied on construction (16.17 µs vs 16.58 µs). Both bottlenecked by RegExp compile. | [@b9g/match-pattern bench](https://www.npmjs.com/package/@b9g/match-pattern), [nodeland.dev URLPattern routing](https://adventures.nodeland.dev/archive/you-should-not-use-urlpattern-to-route-http/) |
| `abort-controller` | `AbortController` (Node 14.7+ global) | **~0.01 ms** (~1 KB) | Native is V8/C++ internal; polyfill is JS over `EventEmitter`. Historical regression on transferable path closed 2022 ([#43160](https://github.com/nodejs/node/issues/43160)). No throughput bench; ~30–50 % polyfill speedup proposals on no-listener path imply native is comparable, not 10x faster. | [node-abort-controller readme](https://github.com/southpolesteve/node-abort-controller), [perf issue #43160](https://github.com/nodejs/node/issues/43160), [polyfill opt issue #41](https://github.com/mysticatea/abort-controller/issues/41) |
| `node-fetch` | `fetch` (Node 18+, undici-backed) | **~1.07 ms** (107 KB unpacked) | **Tied.** 5945 req/s (node-fetch) vs 5904 req/s (undici-fetch) on Node 22.11.0, 50 conns, pipeline 10. `undici.request` is ~50 % faster than either (8528 req/s) — fetch's Web-Streams layer is the bottleneck, shared by both. | [MacroscopeBenchmark](https://github.com/MacroscopeBenchmark/undici), [undici perf-parity #1203](https://github.com/nodejs/undici/issues/1203) |
| `cross-fetch` | same as `node-fetch` on Node | ~0.05 ms (~5 KB) | Same as node-fetch. Cross-fetch's own wrapper is ~50 LOC; negligible. | inherits node-fetch sources |
| `isomorphic-fetch` | `fetch` (Node 18+) | ~0.005 ms (~0.5 KB) | Already a no-op on Node ≥18 (it just assigns `globalThis.fetch` if missing). | n/a |
| `@vercel/fetch` | `fetch` + custom retry | ~0.02 ms | Not a polyfill — it adds retry/JSON-coerce semantics. Clobbering changes behavior, doesn't save perf. | n/a |
| `ws` (client) | `WebSocket` (Node 22.4+, undici-backed) | **~0.8 ms** (~80 KB w/ deps) | **Tied.** undici `WebSocket` 102.46 MiB/s vs `ws` 100.69 MiB/s on binary frames; 5849 req/10s (undici) vs 6324 req/10s (`ws`) on 262 KB messages — `ws` *marginally faster* on large payloads. | [undici PR #3203 bench](https://github.com/nodejs/undici/pull/3203) |
| `ws` (server) | none in Node core | n/a | No native equivalent; can't clobber. | [nodejs.org WebSocket guide](https://nodejs.org/learn/getting-started/websocket) |
| `eventsource` | `EventSource` (`--experimental-eventsource`, undici-backed) | **~0.2 ms** (~20 KB) | No published bench. Undici maintainer explicitly flagged native parser's `Buffer.concat`/`subarray` overhead as a known perf issue ([#2630](https://github.com/nodejs/undici/issues/2630)). Userland may be equal or faster today. **Unverified — would need benchmarking.** | [undici #2630](https://github.com/nodejs/undici/issues/2630), [discussion #2976](https://github.com/nodejs/undici/discussions/2976) |

Cold-start figure derived from Cloudflare Workers parse-cost data (~5 ms per 500 KB script, ~10 µs/KB; [source](https://towardsaws.com/aws-lambda-vs-google-cloud-run-vs-cloudflare-workers-cold-starts-and-costs-89674e69208e)) and V8's own non-linearity caveats ([v8.dev/blog/preparser](https://v8.dev/blog/preparser), [v8.dev/blog/cost-of-javascript-2019](https://v8.dev/blog/cost-of-javascript-2019)). Treat as order-of-magnitude; deeply-nested code parses worse, lazy-parsed inner functions parse much better.

## Per-candidate brief

**`@js-temporal/polyfill`.** 125.9 KB CJS, 51.9 KB gzipped, one dep (JSBI). JSBI is a software BigInt shim retained for pre-2018-V8 targets — a permanent ~2x tax on every BigInt op even on modern Node. No head-to-head bench vs V8-native Temporal is published yet (Temporal landed in V8 13.0, Chrome/Firefox 137 in 2025; Node 26+ is the first stable carrier). Bryntum's 2026 [Temporal-in-2026 piece](https://bryntum.com/blog/javascript-temporal-is-it-finally-here/) benches native Temporal vs `Date` and finds rough parity; native vs JSBI-polyfill should exceed that delta comfortably. **Verdict: strict perf win, large.** The ~1.25 ms cold-start parse saved per invocation is alone the biggest single number in this audit.

**`urlpattern-polyfill`.** [@b9g/match-pattern bench](https://www.npmjs.com/package/@b9g/match-pattern) (M1, Bun 1.3.3): native `test()` 2.32 µs vs polyfill 3.02 µs (~30 %); construction tied. Both compile to RegExp via a multi-stage lexer→parser→generator; the spec is the bottleneck. Cold-start parse saved ~0.15 ms. **Verdict: minor perf win, mostly install-size.**

**`abort-controller` (mysticatea).** Native `AbortController` shipped Node 14.7.0 (2020). Polyfill is ~1 KB over `EventEmitter`. [Issue #43160](https://github.com/nodejs/node/issues/43160) showed the native transferable path was once slower than the polyfill — fixed by removing transferability from the hot path. Polyfill-side optimization issues quote 30–50 % gains on no-listener `abort()`, implying native is comparable, not dominating. **Verdict: negligible runtime delta, tiny cold-start saving.**

**`node-fetch` v3.** MacroscopeBenchmark is decisive: 5945 req/s (node-fetch) vs 5904 req/s (undici-fetch) is statistical noise. Both pay the Web-Streams "fetch surface" cost; `undici.request` (8528 req/s) is the only escape, and clobbering doesn't get you there. **Verdict: negligible. Don't clobber for perf.**

**`cross-fetch`, `isomorphic-fetch`, `@vercel/fetch`.** Thin wrappers, no independent perf. `isomorphic-fetch` is already a no-op on Node ≥18.

**`ws` client.** Undici's [PR #3203 bench](https://github.com/nodejs/undici/pull/3203) puts `ws` and native `WebSocket` within 2 % on small messages (100.69 vs 102.46 MiB/s) and shows `ws` *marginally faster* on 262 KB messages (6324 vs 5849 req/10s). Undici's maintainer notes `ws` was the implementation guide. **Verdict: no perf win.** The audit's existing "don't clobber — `ws` has a server" verdict already disposes of `ws`.

**`eventsource` (npm).** No head-to-head bench. Undici's own [#2630](https://github.com/nodejs/undici/issues/2630) flags `Buffer.concat`/`subarray` overhead in the native parser; [discussion #2976](https://github.com/nodejs/undici/discussions/2976) notes the userland package supports headers/proxies/dispatchers the native global doesn't. **Verdict: no data; priors lean against clobbering today.**

## Re-ranked clobber priority by perf magnitude

1. **`@js-temporal/polyfill`** — strict perf win. ~1.25 ms cold-start saved plus a C++/native-bigint hot-path advantage of unknown but architecturally substantial size. Largest install-size delta *and* largest single-feature runtime delta in the audit.
2. **`urlpattern-polyfill`** — minor perf win. ~30 % faster `test()`; ~0.15 ms cold-start saved. Install-size dominates.
3. **`abort-controller`** — negligible runtime delta, ~1 KB cold-start saving. Include only if the v0.x clobber-table machinery already exists for free.
4. **`cross-fetch` / `isomorphic-fetch`** — negligible. Already no-ops on Node ≥18 or trivially small. Install-size hygiene only.
5. **`node-fetch`, `ws` (client), `eventsource`** — **remove from any perf-driven clobber list.** Userland and native are tied or userland-favored on the published benches.

The earlier audit baseline was `@js-temporal/polyfill` and `urlpattern-polyfill` plus in-flight followups on `node-fetch`, `ws`, and `eventsource`; under perf those three followups are eliminated and `abort-controller` enters as a distant third.

## Sources

- MacroscopeBenchmark, node-fetch vs undici-fetch on Node 22.11.0 — https://github.com/MacroscopeBenchmark/undici
- nodejs/undici #1203 "fetch() performance parity" — https://github.com/nodejs/undici/issues/1203
- Ethan-Arrowood/undici-fetch benchmarks (older Node) — https://github.com/Ethan-Arrowood/undici-fetch/blob/main/benchmarks.md
- nodejs/undici PR #3203 (WebSocket bench, ws vs undici) — https://github.com/nodejs/undici/pull/3203
- nodejs/undici #2630 (EventSource Buffer overhead) — https://github.com/nodejs/undici/issues/2630
- nodejs/undici discussion #2976 (eventsource npm vs native) — https://github.com/nodejs/undici/discussions/2976
- nodejs/node #43160 (AbortController transferable perf) — https://github.com/nodejs/node/issues/43160
- mysticatea/abort-controller #41 (polyfill optimization) — https://github.com/mysticatea/abort-controller/issues/41
- @b9g/match-pattern URLPattern bench (M1, Bun 1.3.3) — https://www.npmjs.com/package/@b9g/match-pattern
- nodeland.dev URLPattern routing performance — https://adventures.nodeland.dev/archive/you-should-not-use-urlpattern-to-route-http/
- @js-temporal/polyfill npm metadata — https://www.npmjs.com/package/@js-temporal/polyfill
- fullcalendar/temporal-polyfill size comparison — https://github.com/fullcalendar/temporal-polyfill
- Bryntum "JavaScript Temporal in 2026" — https://bryntum.com/blog/javascript-temporal-is-it-finally-here/
- node-fetch npm metadata (107.3 KB unpacked) — https://npmx.dev/package/node-fetch
- nodejs.org "Native WebSocket Client" — https://nodejs.org/learn/getting-started/websocket
- V8 blog "Blazingly fast parsing, part 2" — https://v8.dev/blog/preparser
- V8 blog "The cost of JavaScript in 2019" — https://v8.dev/blog/cost-of-javascript-2019
- Cloudflare Workers cold-start parse-cost — https://towardsaws.com/aws-lambda-vs-google-cloud-run-vs-cloudflare-workers-cold-starts-and-costs-89674e69208e
- Companion: [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md), [`polyfill-demand-audit.md`](polyfill-demand-audit.md)

## Changelog

- 2026-07-30 — Migrated from the internal research corpus.
