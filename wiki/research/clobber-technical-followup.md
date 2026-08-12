# Clobber technical follow-up

**Date:** 2026-05-24. Scope: the four open questions left by [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md), each tightened from a "maybe" into a recommendation grounded in current source and bug history.

## TL;DR

Three of the four questions resolve to no change: `ws` is deferred, `node-fetch` and `eventsource` stay rejected. The fourth reverses the prior audit — polyfill clobber eliminates real code rather than a no-op.

- **Selective `ws` clobber:** feasible via `module.registerHooks()` + data-URL synthetic modules + `nextResolve` (which skips the calling hook, so no sentinel is needed). **Defer.** The hazard the prior audit named is unchanged: `import { WebSocket } from 'ws'` consumers overwhelmingly use the `EventEmitter`-shaped `.on('open', …)` API, which native `WebSocket` (EventTarget) doesn't support.
- **`node-fetch` / `cross-fetch`:** divergences from native fetch are concrete (Node `Readable` body, `res.buffer()`, no forbidden-headers restriction, `agent`, `size`, custom error classes). Bun's existing clobber shipped multiple multipart-corruption bugs in 2026 — [#26225](https://github.com/oven-sh/bun/issues/26225), [#26638](https://github.com/oven-sh/bun/issues/26638), [#21467](https://github.com/oven-sh/bun/issues/21467). **Keep rejected.**
- **`eventsource`:** userland v3 (current major) already dropped `{ headers, https, proxy, agent }`; the residual gap is `{ fetch }` for header injection. LLM-streaming code that uses `EventSource` relies on it for `Authorization`; native doesn't accept it. **Keep rejected.**
- **Polyfill verification:** prior audit's "imports are already a no-op on native-supporting Node" is **wrong** on the named-export path. `@js-temporal/polyfill@0.5.1` main entry deliberately does **not** assign to `globalThis`; `urlpattern-polyfill@10.1.0` feature-detects the global write but always loads the polyfill source and returns the polyfill class from `import { URLPattern }`. Clobber is a genuine code-elimination win, not a "faster no-op."

## 1. Selective `ws` clobber feasibility

Selective clobber is feasible with `module.registerHooks()` on Node 22.15+: a hook re-resolves the same specifier through `nextResolve` to obtain the real file URL without re-entering itself.

Each `resolve` hook receives a `nextResolve` that calls "the subsequent resolve hook in the chain, or the Node.js default resolve hook after the last user-supplied resolve hook" ([docs](https://nodejs.org/api/module.html#moduleregisterhooksoptions)) — explicitly **not** the calling hook. No sentinel condition (`'__nub-bypass-ws-clobber__'`, etc.) is needed.

Pattern (brand-boundary clean — `data:` URLs, no `nub:*` scheme):

```js
import { registerHooks } from 'node:module';
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier !== 'ws') return nextResolve(specifier, context);
    const { url } = nextResolve(specifier, context);  // file:// URL of real ws
    const src =
      `export * from ${JSON.stringify(url)};\n` +
      `export { default } from ${JSON.stringify(url)};\n` +
      `export const WebSocket = globalThis.WebSocket;\n`;
    return {
      url: 'data:text/javascript;base64,' + Buffer.from(src).toString('base64'),
      format: 'module', shortCircuit: true,
    };
  },
});
```

`export *` plus a `WebSocket`-named export wins the name conflict by ECMAScript binding rules. The synthetic module's `import` of `<real-ws-file-url>` is an absolute URL, not a bare specifier; the hook's specifier check doesn't match, so no loop. This is the same technique `tsx` uses for transform pipelines. Stable on Node 22.15+.

**Why defer anyway.** The hazard isn't load-failure of `ws`; it's the EventEmitter-vs-EventTarget shape mismatch on the client class. `ws.WebSocket` extends Node `EventEmitter`, exposes `.on('open' | 'message' | …)`, `_socket`, `.terminate()`, per-message-deflate, `protocols`/`headers`/`agent` constructor options. Native `WebSocket` is EventTarget with `addEventListener` and accepts `(url, protocols)`. Substituting native for the named `WebSocket` export breaks the exact subset of consumers we'd be trying to help — `ws`'s README itself documents the EventEmitter API. `WebSocketServer` resolves `WebSocket` via `require('./websocket')` internally, so the server side is unaffected; that part works. It just doesn't pay off.

**Recommendation: defer.** ~30 LOC to add later if demand surfaces.

## 2. `node-fetch` / `cross-fetch` re-evaluation

`node-fetch` v3 [`docs/v3-LIMITS.md`](https://github.com/node-fetch/node-fetch/blob/main/docs/v3-LIMITS.md) lists its own divergences from spec; cross-checking against Node ≥18 native fetch (undici-backed):

| Divergence | node-fetch v3 | Native fetch | Severity |
|---|---|---|---|
| `res.body` type | Node `Readable` stream | WHATWG `ReadableStream` | High — `pipeline(res.body, fs.createWriteStream(…))` works on node-fetch, throws on native |
| Extra body method | `res.buffer()` | (absent) | Medium — common in older codebases |
| `req.body` accepts | Buffer / Node `Readable` directly | WHATWG `ReadableStream` / `Blob` / `FormData` / string | High — Node-stream upload is the common pattern |
| Forbidden headers | None — can set `Cookie`, `User-Agent`, `Host` | Restricted per spec | Medium |
| Constructor option `agent` | Yes (`https.Agent`) | No (use `dispatcher` only on undici-import) | High — proxies, mTLS, custom DNS |
| Option `size` (max response bytes) | Yes | No | Low |
| Option `highWaterMark` | Yes | No | Low |
| Error classes | `FetchError` with `.type`/`.code` | `TypeError` (generic) | Medium — error-handling code branches on `.code === 'ECONNRESET'` etc. |
| `bodyUsed` after `new Response(consumedStream)` | Doesn't update | Updates | Edge |

Four high-severity divergences hit realistic Node-shaped code. The typical `node-fetch` consumer is not writing spec-shape — they're writing Node-shape (Readable streams, `agent: httpsAgent`, `.buffer()`). Clobbering breaks that. `cross-fetch`'s Node entry is a thin re-export of `node-fetch`; same divergences apply.

**Bun's clobber bug history (2026):** [#26225](https://github.com/oven-sh/bun/issues/26225) — `form-data` + `node-fetch@2` + `fs.createReadStream` multipart upload truncated (`Content-Length: 17` on a 10 MB body, missing-end-boundary on the server), fixed in PR [#26226](https://github.com/oven-sh/bun/pull/26226) "convert old-style Node.js streams to Web streams"; [#26638](https://github.com/oven-sh/bun/issues/26638) follow-on multipart corruption over HTTPS, fixed in PR [#26639](https://github.com/oven-sh/bun/pull/26639); duplicates [#21467](https://github.com/oven-sh/bun/issues/21467), [#21788](https://github.com/oven-sh/bun/issues/21788), [#19097](https://github.com/oven-sh/bun/issues/19097); related [#11621](https://github.com/oven-sh/bun/issues/11621). All variations of "the Node-stream-body shim doesn't faithfully reproduce node-fetch." This is the parity-tax bill, paid by Bun's users.

**Recommendation: keep rejected.** Anyone wanting the install-size win can `import { fetch } from 'undici'` themselves. No userbase Nub should be solving for.

## 3. `eventsource` native vs userland gap

**Native (undici, [`eventsource.js`](https://github.com/nodejs/undici/blob/main/lib/web/eventsource/eventsource.js)) accepts:** `{ withCredentials, dispatcher, node: { reconnectionTime, dispatcher } }`. The `dispatcher`/`node` keys are undici-only; the WHATWG spec defines only `withCredentials`.

Bun's native `EventSource` (since Bun 1.1.23) is strict spec — `withCredentials` only.

**Userland `eventsource@v3` ([README](https://github.com/EventSource/eventsource/blob/main/README.md), [MIGRATION.md](https://github.com/EventSource/eventsource/blob/main/MIGRATION.md)):** v3 (current major) **dropped** `{ headers, https, proxy, agent }`. The replacement extension is a `{ fetch }` option that injects a custom fetch (so users wrap fetch to add `Authorization`), plus `Symbol.for('eventsource.supports-fetch-override')` for feature detection, plus `code`/`message` props on `error` events. The prior audit's "userland accepts `headers`/`proxy`/`agent`" was correct for v1/v2; v3 changed it.

**LLM-streaming reality.** OpenAI's and Anthropic's official Node SDKs implement streaming via `fetch` + manual SSE line-parsing on `response.body`, not via `EventSource`. Code that uses `EventSource` for LLM streaming today is typically either v2-pinned with `{ headers: { Authorization: 'Bearer …' } }` or v3 with the `{ fetch }` override. Both break under a native clobber.

**Recommendation: keep rejected.** The *reason* updates from "v2 headers option" to "v3 `{ fetch }` option," but the conclusion is the same. Spec-shape `EventSource` doesn't accept auth; auth is the dominant use; users who care will install the package.

## 4. Polyfill-package feature-detect verification

Two polyfill packages were checked against the prior audit's claim that importing them is already a no-op on a Node with native support. Neither is.

### `@js-temporal/polyfill@0.5.1`

Main entry is [`lib/index.ts`](https://unpkg.com/@js-temporal/polyfill@0.5.1/lib/index.ts), which exports `{ Temporal, Intl, toTemporalInstant }` and **does not assign anything to `globalThis`**.

The file's lead comment is explicit: *"This entry point treats Temporal as a library, and does not polyfill it onto the global object. This is in order to avoid breaking the web in the future, if the polyfill gains wide adoption before the API is finalized."* README repeats it. A separate `lib/init.ts` does set `globalThis.Temporal`, but is "only for the browser playground and the test262 tests" — not the published main export.

**Implication:** on Node 26 with native `Temporal`, `import { Temporal } from '@js-temporal/polyfill'` returns the polyfill's namespace, **not** native, and **not** a no-op. The ~125 KB CJS/ESM bundle parses on every import. A Nub clobber resolving the specifier to `{ Temporal: globalThis.Temporal }` is real elimination, not "a faster no-op." Behavior-change risk: code depending on polyfill-specific bug-compat would shift to native; under v0.x opt-in, that's the user's choice.

### `urlpattern-polyfill@10.1.0`

Both [`index.js`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.js) and [`index.cjs`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.cjs) have the same shape:

```js
import { URLPattern } from "./dist/urlpattern.js";
export { URLPattern };
if (!globalThis.URLPattern) {
  globalThis.URLPattern = URLPattern;
}
```

The global assignment **is** feature-detected. But the polyfill source (`./dist/urlpattern.js`, ~18 KB) loads unconditionally to satisfy the named export, and `import { URLPattern } from "urlpattern-polyfill"` returns the polyfill's class even on Node where `globalThis.URLPattern` is native. So again: clobber on native-supporting Node is genuine code elimination, not a no-op.

### Net implication

Both packages on the named-import path **load and execute their full polyfill source** regardless of native availability. Clobber removes ~125 KB (Temporal) + ~18 KB (URLPattern).

Both remain **opt-in clobber, v0.x**, per the prior audit; the reasoning strengthens.

## Sources

Primary evidence behind each verdict: Node's hook documentation, the fetch and SSE packages' own limitation and migration docs, Bun's multipart bug threads, and the two polyfills' published entry points.

- Node `module.registerHooks()`: [docs](https://nodejs.org/api/module.html#moduleregisterhooksoptions). `nextResolve` skips the calling hook — no sentinel needed.
- `node-fetch` v3 known limits: [`docs/v3-LIMITS.md`](https://github.com/node-fetch/node-fetch/blob/main/docs/v3-LIMITS.md); README "Difference from client-side fetch": [README](https://github.com/node-fetch/node-fetch/blob/main/README.md).
- Bun `node-fetch` clobber bug history: [#26225](https://github.com/oven-sh/bun/issues/26225), PR [#26226](https://github.com/oven-sh/bun/pull/26226); [#26638](https://github.com/oven-sh/bun/issues/26638), PR [#26639](https://github.com/oven-sh/bun/pull/26639); [#21467](https://github.com/oven-sh/bun/issues/21467); [#21788](https://github.com/oven-sh/bun/issues/21788); [#19097](https://github.com/oven-sh/bun/issues/19097); [#11621](https://github.com/oven-sh/bun/issues/11621).
- Undici `EventSource`: [`lib/web/eventsource/eventsource.js`](https://github.com/nodejs/undici/blob/main/lib/web/eventsource/eventsource.js); accepts `withCredentials`, `dispatcher` (undici-only), `node.reconnectionTime`, `node.dispatcher`.
- Userland `eventsource` v3: [README](https://github.com/EventSource/eventsource/blob/main/README.md), [MIGRATION.md](https://github.com/EventSource/eventsource/blob/main/MIGRATION.md). v3 dropped `{ headers, https, proxy, agent }`; added `{ fetch }` and `Symbol.for('eventsource.supports-fetch-override')`.
- `@js-temporal/polyfill@0.5.1`: main entry [`lib/index.ts`](https://unpkg.com/@js-temporal/polyfill@0.5.1/lib/index.ts) (no global) vs [`lib/init.ts`](https://unpkg.com/@js-temporal/polyfill@0.5.1/lib/init.ts) (global, non-main).
- `urlpattern-polyfill@10.1.0`: [`index.js`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.js), [`index.cjs`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.cjs).
- Prior audit: [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md). Brand boundary: [`../../AGENTS.md`](../../AGENTS.md).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
