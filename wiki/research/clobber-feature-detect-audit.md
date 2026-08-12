# Clobber feature-detect audit

**Date:** 2026-05-25. Scope: re-evaluate every userland package currently in (or proposed for) Nub's v0.1 default clobber set against a tightened bar.

A clobber is justified only when the userland package does *not* already feature-detect and route to native on Nub's Node 22.15+ floor, or when there is a specific, articulable parity benefit beyond install-size. Triggered by a 2026-05-25 objection that a clobber which only saves parse-cost on a polyfill already routing to native is not buying what the legacy-polyfill doc claimed it was.

Companion to [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md), [`polyfill-demand-audit.md`](polyfill-demand-audit.md), [`clobber-technical-followup.md`](clobber-technical-followup.md), [`clobber-perf-comparison.md`](clobber-perf-comparison.md), [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md).

## TL;DR

The reframed bar keeps the three default entries and rejects every proposed addition: the five "strong adds" and three secondary candidates all drop, and `web-streams-polyfill` stays deferred.

- **The three default-clobber entries keep their seats, for a stronger reason than the existing doc records.** The published main entry of `@js-temporal/polyfill`, `urlpattern-polyfill`, and `abort-controller` does NOT feature-detect to native on Nub's Node 22.15+ floor; each unconditionally loads and executes its polyfill source on every import. Clobber is genuine code elimination (~125 KB Temporal, ~18 KB URLPattern, ~3 KB + `event-target-shim` for AbortController), not a faster no-op.
- **All five "strong adds" from [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md) lose their seats.** `safe-buffer`, `queue-microtask`, `buffer-from`, `setimmediate`, and `performance-now` all feature-detect to native on Node ≥4.5 / ≥8.5 / ≥11. On the 22.15+ floor each is already a noop substitution by binding: `safe-buffer` returns `require('buffer')`; `queue-microtask` returns `queueMicrotask.bind(globalThis)`; `buffer-from` returns a wrapper that immediately delegates to `Buffer.from`; `setimmediate` runs an IIFE that early-returns and produces an empty module; `performance-now` returns `() => performance.now()`. Clobber would save parse-cost on the package file and eliminate no runtime work, because there is none to eliminate.
- **The three secondary candidates also drop.** `is-buffer` doesn't feature-detect but introduces a behavioral delta vs native on degenerate inputs (the existing "no API-parity bugs" criterion applies); `atob` doesn't feature-detect but the eliminated body is a single 5-line `Buffer.from(s, 'base64').toString('binary')` wrapper, below "load-bearing"; `abab`'s `lib/atob` returns `null` on invalid input where native throws `DOMException`, a parity bug that disqualifies it independently of the reframed bar.
- **`web-streams-polyfill@4.x` ponyfill does not feature-detect** (verified via [`dist/ponyfill.js`](https://unpkg.com/web-streams-polyfill@4.3.0/dist/ponyfill.js) — the full polyfill loads unconditionally, only the optional `/polyfill` sub-path touches `globalThis`). It clears (a) cleanly with a large elimination (~50 KB of stream-spec code), and stays deferred to v0.x for the same reason as before: the existing simple-string-match clobber table would over-match `/es5`.
- **No new "doesn't-feature-detect" candidates make it past the existing parity bar.** In the browserify-shim family (`events`, `process`, `buffer`, `assert`, `util`, `path`, `stream`, `string_decoder`, `crypto-browserify`, `os-browserify`, `tty-browserify`, `domain-browser`, `punycode`) none are loaded on Node — the bare-specifier resolver picks the core module before `node_modules`, so the userland shim sits dormant on disk. `readable-stream@4.x` is loaded on Node and does not feature-detect, but prefers its own vendored stream implementation by design (only `READABLE_STREAM=disable` routes to native), so clobbering would invert the package's purpose. `inherits@2.0.4` feature-detects to `util.inherits`. `safer-buffer` partially feature-detects, but its `safer.Buffer` is a `Buffer` minus `allocUnsafe`/`allocUnsafeSlow` by design, so clobbering re-introduces the methods the package exists to prevent.
- **Net effect on the package-clobbering plan:** the v0.1 default set stays at three entries (Temporal, URLPattern, AbortController). The "strong adds" tranche is rejected wholesale. Each existing entry's rationale should lead with (a) — real code elimination — instead of install-size or Bun-parity.

## The reframed bar

A userland package is a clobber candidate only if at least one of:

- **(a)** it does NOT feature-detect to native on Nub's Node 22.15+ floor, so the clobber genuinely eliminates polyfill code that would otherwise execute on every import; or
- **(b)** there is a specific, articulable parity benefit that justifies the clobber even though userland already routes to native. Bun-aliasing for ecosystem parity is the canonical example; each invocation of (b) must name the benefit rather than lean on "install-size" or "freshness".

The bar is a floor, not a ceiling: a package clearing (a) or (b) still has to pass the existing criteria (pure-spec-shim shape, no API-parity bugs, native equivalent exists). Clearing (a) trivially — eliminating a 3-line wrapper — does not merit a seat; the elimination must outweigh the per-clobber surface (parity risk, support burden, debug-output noise).

Install-size, parse-time, and "feels modern" are not articulable parity benefits under (b). They were the legacy-polyfill doc's rationales, and they are now insufficient.

## Per-candidate verification table

Twelve packages, each read at its published main entry: whether it feature-detects, what actually runs on Node 22.15+, whether the result is observably equivalent to native, and the resulting verdict.

| Package | Latest version | Feature-detects? | What runs on Node 22.15+ | Observable-equivalent to native? | Keep / drop under reframed bar | Source URL |
|---|---|---|---|---|---|---|
| `@js-temporal/polyfill` | 0.5.1 | **No** | Full ~125 KB polyfill bundle loads; exports `{ Temporal, Intl, toTemporalInstant }` from the polyfill's own namespace. Never assigns to `globalThis`. | **No** — software JSBI BigInt vs V8 native; cold-start ~1.25 ms higher; named-export `Temporal` is the polyfill class | **Keep** via (a) | [`dist/index.cjs`](https://unpkg.com/@js-temporal/polyfill@0.5.1/dist/index.cjs), [`dist/index.esm.js`](https://unpkg.com/@js-temporal/polyfill@0.5.1/dist/index.esm.js) |
| `urlpattern-polyfill` | 10.1.0 | **Partial** — global-write only | `import { URLPattern } from "./dist/urlpattern.js"` loads ~18 KB polyfill unconditionally; `export { URLPattern }` is the polyfill class; `if (!globalThis.URLPattern) globalThis.URLPattern = URLPattern` skips the global write on native-supporting Node, but the named-export path is unaffected by the guard | **No** — named export is the polyfill class on every Node; ~30% slower `test()` vs native | **Keep** via (a) | [`index.js`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.js), [`index.cjs`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.cjs) |
| `abort-controller` | 3.0.0 | **No** | `import { EventTarget, defineEventAttribute } from 'event-target-shim'` runs unconditionally; an `AbortSignal extends EventTarget` class is defined and exported; no `globalThis` check | **No** — userland `AbortSignal` lacks the static methods native adds (`AbortSignal.timeout`, `AbortSignal.any`, `AbortSignal.abort`, `signal.throwIfAborted()`); native adds them additively (no regression) | **Keep** via (a) — and additionally clears (b) via Bun-parity | [`dist/abort-controller.mjs`](https://unpkg.com/abort-controller@3.0.0/dist/abort-controller.mjs) |
| `safe-buffer` | 5.2.1 | **Yes** | `if (Buffer.from && Buffer.alloc && Buffer.allocUnsafe && Buffer.allocUnsafeSlow) module.exports = buffer` — branch taken on every supported Node; `module.exports` IS `require('buffer')` | **Yes** — bit-identical to `node:buffer` | **Drop** | [`index.js`](https://unpkg.com/safe-buffer@5.2.1/index.js) |
| `queue-microtask` | 1.2.3 | **Yes** | `module.exports = typeof queueMicrotask === 'function' ? queueMicrotask.bind(global) : fallback` — taken on Node ≥11 | **Yes** — default export IS `queueMicrotask.bind(globalThis)` | **Drop** | [`index.js`](https://unpkg.com/queue-microtask@1.2.3/index.js) |
| `buffer-from` | 1.1.2 | **Yes** | `var isModern = typeof Buffer !== 'undefined' && Buffer.alloc && Buffer.allocUnsafe && Buffer.from` is true; the dispatcher reduces to `if (typeof value === 'number') throw …; else Buffer.from(...)` | **Effectively yes** — the only behavioral residue is the package's `'"value" argument must not be a number'` error string | **Drop** | [`index.js`](https://unpkg.com/buffer-from@1.1.2/index.js) |
| `setimmediate` | 1.0.5 | **Yes** | IIFE checks `if (global.setImmediate) return;` and returns early on every Node; module exports stay at default `{}`; side effects nil | **Yes** — module is observably empty on Node | **Drop** | [`setImmediate.js`](https://unpkg.com/setimmediate@1.0.5/setImmediate.js) |
| `performance-now` | 2.1.0 | **Yes** | `if (performance && performance.now) module.exports = function() { return performance.now(); };` — branch taken on Node ≥8.5 | **Yes** — default export IS `() => performance.now()` | **Drop** | [`lib/performance-now.js`](https://unpkg.com/performance-now@2.1.0/lib/performance-now.js) |
| `is-buffer` | 2.0.5 | **No** | Three-line function `obj != null && obj.constructor != null && typeof obj.constructor.isBuffer === 'function' && obj.constructor.isBuffer(obj)` defined and exported | **Not quite** — userland reads `obj.constructor.isBuffer`; native `Buffer.isBuffer` does an internal brand check; degenerate inputs (fake `.constructor.isBuffer = () => true`) diverge | **Drop** — clears (a) but trips the existing "no API-parity bug" criterion | [`index.js`](https://unpkg.com/is-buffer@2.0.5/index.js) |
| `atob` | 2.1.2 | **No** | `function atob(str) { return Buffer.from(str, 'base64').toString('binary'); }; module.exports = atob.atob = atob;` runs every import | **Yes** on valid input — same binary-string-from-base64 behavior | **Drop** — clears (a) but the elimination is a 5-line wrapper; magnitude is below load-bearing, no parity benefit under (b) | [`node-atob.js`](https://unpkg.com/atob@2.1.2/node-atob.js) |
| `abab` | 2.0.6 | **No** | Loads `./lib/atob` (101 lines, spec-divergent in error-handling) and `./lib/btoa` (62 lines); exports `{ atob, btoa }` | **No** — userland `atob` returns `null` on invalid input; native throws `DOMException`; consumer code that branches on the return value differs | **Drop** — clears (a), but the null-vs-throw on invalid input is an API-parity bug | [`index.js`](https://unpkg.com/abab@2.0.6/index.js), [`lib/atob.js`](https://unpkg.com/abab@2.0.6/lib/atob.js) |
| `web-streams-polyfill` | 4.3.0 | **No** | Main entry `dist/ponyfill.js` (~50 KB) defines `ReadableStream`/`WritableStream`/`TransformStream`/queueing-strategy/BYOB classes; `/polyfill` subentry additionally installs to `globalThis` (also no guard) | **Yes** (named exports map 1:1 to global classes on Node 18+) | **Defer to v0.x** — clears (a) with a large elimination, but sub-path-aware clobber-table machinery is needed first | [`dist/ponyfill.js`](https://unpkg.com/web-streams-polyfill@4.3.0/dist/ponyfill.js) |

## Per-candidate brief

One entry per package in the table above, naming the source lines that decide its verdict.

### `@js-temporal/polyfill@0.5.1` — KEEP

Both the ESM main entry (`dist/index.esm.js`) and the CJS main entry (`dist/index.cjs`) are the bundled polyfill source; a grep for `globalThis.Temporal` or `typeof Temporal` across either file returns zero matches.

The file leads with `import e from "jsbi"` — the polyfill statically depends on the JSBI BigInt shim and unconditionally constructs its `Temporal.PlainDate`/`PlainTime`/`PlainDateTime` classes on top of JSBI. On Node 26 with native `Temporal`, `import { Temporal } from "@js-temporal/polyfill"` returns the polyfill namespace, and the ~125 KB of polyfill code runs on every import. Clobber to `{ Temporal: globalThis.Temporal }` removes one of the largest single polyfill source files in npm-popular use today. The claim in [`clobber-technical-followup.md`](clobber-technical-followup.md) still holds on `@latest`.

### `urlpattern-polyfill@10.1.0` — KEEP

The ESM and CJS main entries are identical in shape:

```js
import { URLPattern } from "./dist/urlpattern.js";
export { URLPattern };
if (!globalThis.URLPattern) {
  globalThis.URLPattern = URLPattern;
}
```

The import is unconditional — the polyfill source (~18 KB) loads on every import — and the `if (!globalThis.URLPattern)` guard governs only the *global write*, not the *named export*, which always re-exports the polyfill's `URLPattern`. Code doing `import { URLPattern } from "urlpattern-polyfill"` on Node 24+ gets the polyfill class even though `globalThis.URLPattern` is native. Clobber eliminates ~18 KB of polyfill source and a runtime delta (~30% faster `test()` calls). The claim in [`clobber-technical-followup.md`](clobber-technical-followup.md) still holds on `@latest`.

### `abort-controller@3.0.0` — KEEP

The package builds its own `AbortSignal` on `event-target-shim` with no guard, so the seat rests on (a) — real code elimination — rather than on the install-size and parity reasons recorded today.

The current package-clobbering rationale reads "Userland API matches native exactly. Bun ships the same clobber. Negligible runtime delta — included for install-size and ecosystem-parity reasons rather than perf." Under the reframed bar that is the weaker of two available justifications. The main entry, [`dist/abort-controller.mjs`](https://unpkg.com/abort-controller@3.0.0/dist/abort-controller.mjs), starts with:

```js
import { EventTarget, defineEventAttribute } from 'event-target-shim';

class AbortSignal extends EventTarget {
  constructor() {
    super();
    throw new TypeError("AbortSignal cannot be constructed directly");
  }
  get aborted() { … }
}
defineEventAttribute(AbortSignal.prototype, "abort");

function createAbortSignal() { … }
function abortSignal(signal) { … }
const abortedFlags = new WeakMap();

class AbortController {
  constructor() { signals.set(this, createAbortSignal()); }
  get signal() { return getSignal(this); }
  abort() { abortSignal(getSignal(this)); }
}
…
export default AbortController;
export { AbortController, AbortSignal };
```

There is no `typeof AbortController === 'function'` check and no `if (globalThis.AbortController)` guard: the package always loads `event-target-shim`, always constructs its own `AbortSignal extends EventTarget`, always exports it. On Node 22.15+ that work is wasted — native `AbortController`/`AbortSignal` have been globals since Node 16. Clobber is genuine elimination under (a), plus the additive benefit that native has `AbortSignal.timeout`, `AbortSignal.any`, `AbortSignal.abort`, and `signal.throwIfAborted()`, none of which the userland version exposes.

### `safe-buffer@5.2.1` — DROP

The main entry feature-detects:

```js
var buffer = require('buffer')
var Buffer = buffer.Buffer
…
if (Buffer.from && Buffer.alloc && Buffer.allocUnsafe && Buffer.allocUnsafeSlow) {
  module.exports = buffer
} else {
  copyProps(buffer, exports)
  exports.Buffer = SafeBuffer
}
```

On Nub's 22.15+ floor — and any Node ≥4.5 — `module.exports = buffer` is the branch taken, so `module.exports` IS the result of `require('buffer')`. The trailing `SafeBuffer` function definition and its prototype assignments still execute as dead-code parse cost, but the module's user-facing exports are bit-identical to `node:buffer`. A clobber saves a few KB of dead-code parse and the `node_modules/safe-buffer` directory on disk; runtime behavior is already native. Under (a) the elimination is dead code; under (b) the only nameable benefit is install-size, which the reframed bar disqualifies.

### `queue-microtask@1.2.3` — DROP

Full source is seven lines:

```js
let promise
module.exports = typeof queueMicrotask === 'function'
  ? queueMicrotask.bind(typeof window !== 'undefined' ? window : global)
  : cb => (promise || (promise = Promise.resolve())).then(cb).catch(err => setTimeout(() => { throw err }, 0))
```

On Node ≥11 the ternary's truthy branch is taken and `module.exports === queueMicrotask.bind(global)`. The userland default export and a hypothetical clobbed `export default queueMicrotask.bind(globalThis)` are the same bound function. Nothing to eliminate, no behavior to fix, and a small absolute install-size win (single-file package, no transitive deps).

### `buffer-from@1.1.2` — DROP

The top-level `isModern` constant is `typeof Buffer !== 'undefined' && typeof Buffer.alloc === 'function' && typeof Buffer.allocUnsafe === 'function' && typeof Buffer.from === 'function'`, which is `true` on Nub's floor.

The exported `bufferFrom(value, encodingOrOffset, length)` reduces to: throw a custom `TypeError` if `value` is a number, else dispatch to `Buffer.from(...)` (string, ArrayBuffer, generic). The module is observably `Buffer.from` with a one-line preamble check that substitutes the error message. Clobbering buys the substitution at the cost of that error-message divergence. Not load-bearing under (a); no nameable (b).

### `setimmediate@1.0.5` — DROP

The IIFE leads with `if (global.setImmediate) { return; }`.

On every Node version (the API has had `setImmediate` since 0.10) the IIFE returns before any code runs, no installation logic executes, and `module.exports` is never assigned — so `require('setimmediate')` returns `{}`. The whole 187-line file parses for nothing, and parsing 6.5 KB of polyfill code is too small a win to justify a per-clobber surface.

### `performance-now@2.1.0` — DROP

The first branch of the leading `if` block is taken on Node 22.15+:

```js
if ((typeof performance !== "undefined" && performance !== null) && performance.now) {
  module.exports = function() {
    return performance.now();
  };
}
```

Default export is `() => performance.now()`. Clobbering to `export default performance.now.bind(performance)` is a noop substitution at the binding level — the same shape as `queue-microtask`.

### `is-buffer@2.0.5` — DROP

The entire module is:

```js
module.exports = function isBuffer (obj) {
  return obj != null && obj.constructor != null &&
    typeof obj.constructor.isBuffer === 'function' && obj.constructor.isBuffer(obj)
}
```

It does not feature-detect, so it clears (a) with a small elimination (3-line wrapper). But the wrapper dispatches to `obj.constructor.isBuffer`, which is `Buffer.isBuffer` for real Buffer instances and can be anything for arbitrary objects, while native `Buffer.isBuffer` performs an internal brand check that ignores `.constructor`. For real Buffers both return true; for objects that lie about `.constructor.isBuffer`, userland returns true and native returns false. The existing "no API-parity bugs" criterion is conservative about exactly this delta.

### `atob@2.1.2` — DROP

Full module:

```js
function atob(str) {
  return Buffer.from(str, 'base64').toString('binary');
}
module.exports = atob.atob = atob;
```

It does not feature-detect, clearing (a) with a five-line elimination. On valid input the userland function and `globalThis.atob` are observably identical (both spec-defined as binary-string-from-base64). On invalid input the userland function relies on `Buffer.from`'s lenient parsing while native `atob` throws `DOMException`. The deciding reason is magnitude: a 5-line wrapper is not the elimination (a) was reaching for. Close call.

### `abab@2.0.6` — DROP

The main entry is three lines (`const atob = require("./lib/atob"); const btoa = require("./lib/btoa"); module.exports = { atob, btoa };`).

It loads two real files (101 + 62 lines) that do not feature-detect, so it clears (a) with a small-but-real elimination. The disqualifier is parity: `lib/atob`'s leading docstring is *"Implementation of atob() according to the HTML and Infra specs, except that instead of throwing INVALID_CHARACTER_ERR we return null."* Consumer code that branches on the return value behaves differently. The package is also deprecated upstream (npm README: "Please use your platform's native atob()/btoa() methods if possible — this package may be removed in the future."), so the install-size trend is already downward.

### `web-streams-polyfill@4.3.0` — DEFER (status unchanged)

The main entry `dist/ponyfill.js` is the full polyfill, and the deferral is a tooling limit rather than a feature-detect finding.

A UMD wrapper defines `ByteLengthQueuingStrategy`, `CountQueuingStrategy`, `ReadableByteStreamController`, `ReadableStream`, `ReadableStreamBYOBReader`, `ReadableStreamBYOBRequest`, `ReadableStreamDefaultController`, `ReadableStreamDefaultReader`, `TransformStream`, `TransformStreamDefaultController`, `WritableStream`, `WritableStreamDefaultController`, `WritableStreamDefaultWriter` and assigns them into the exports namespace. No `if (typeof ReadableStream !== 'undefined')` guard, and no `globalThis` write from the main entry — that is the `/polyfill` sub-path's job.

Verified: the file ends with `e.ReadableStream=ReadableStream, e.WritableStream=WritableStream, …` (UMD exports), and the only `globalThis` reference is `const fr = "undefined" != typeof globalThis ? globalThis : "undefined" != typeof self ? self : …` for the `DOMException` lookup, not a `URLPattern`-style assignment. The package clears (a) with a substantial elimination (~50 KB of stream-spec implementation). The `/polyfill` sub-path additionally installs the classes on `globalThis`, also without a feature-detect guard. Both are clobber-safe; `/es5` and `/polyfill/es5` are not (the user explicitly opted into ES5 output), and the existing simple-string-match clobber table would over-match. Deferred to v0.x for that machinery reason, not for any feature-detect reason.

## Newly-found "doesn't feature-detect" candidates

The sweep below targeted the npm-popular polyfill space, looking for packages that always load their polyfill source.

None qualify under the full clobber bar — the (a)-clearing candidates fail one of the existing criteria (Node never loads them, behavior changes on clobber, or by-design vendoring intent).

| Package | Loaded on Node? | Feature-detects? | Why not a candidate |
|---|---|---|---|
| `events` (3.3.0) | **No** — Node resolves core `events` before `node_modules` lookup; userland package is dormant on disk | No (browserify shim — always defines its own `EventEmitter`) | Clobbering a package Node never loads is a no-op. The package is webpack/browserify-only. |
| `process` (0.11.10) | **No** — same core-first resolution | No | Same; never loaded on Node. |
| `buffer` (npm; 6.0.3) | **No** — `node:buffer` wins | No | Same. |
| `assert` (npm; 2.1.0) | **No** — `node:assert` wins | No (relies on `object.assign/polyfill`, `object-is/polyfill`, `call-bind/callBound`) | Same. |
| `util` (npm; 0.12.5), `path` (npm; 0.12.7), `stream` (npm; 0.0.3), `string_decoder` (npm; 1.3.0), `os-browserify`, `tty-browserify`, `crypto-browserify`, `domain-browser` | **No** — all core-first | No (browserify-shim shape) | Same. Node's resolver makes the browserify-shim family moot as a clobber target. |
| `punycode` (2.3.1) | **Yes** — but `require('punycode')` still hits core `node:punycode` first (with DEP0040 warning); userland is the *migration target*, not the migration source | n/a | Wrong direction. Clobbering `punycode` would reverse the Node-official migration from core to userland. |
| `readable-stream` (4.7.0) | **Yes** — actively loaded | No — main entry prefers its own `../stream` implementation by default; only `process.env.READABLE_STREAM === 'disable'` routes to native `node:stream` | Clears (a) but the package exists to be a vendored stream impl with cross-version-stable semantics. Clobbering inverts its intent and breaks vendoring-on-purpose consumers. |
| `inherits` (2.0.4) | **Yes** | **Yes** — main entry is `try { var util = require('util'); if (typeof util.inherits !== 'function') throw ''; module.exports = util.inherits; } catch (e) { module.exports = require('./inherits_browser.js'); }` — on Node it always reaches `module.exports = util.inherits` | Feature-detects. Noop substitution. DROP. |
| `safer-buffer` (2.1.2) | **Yes** | Partial — copies properties from `Buffer` only if missing; `Safer.from`/`alloc` defined only if absent | Clobbering to native re-introduces `allocUnsafe`/`allocUnsafeSlow`, which the package explicitly excludes. Behavior change. DROP. |
| `fast-text-encoding` (1.0.6) | **Yes** | Partial — side-effect-only; `scope.TextEncoder = scope.TextEncoder || v` feature-detects the global install but the polyfill function definitions execute unconditionally; module export is `{}` | Module export is empty on Node, so a clobber changes nothing observable to consumers. The side-effect install is already a noop on modern Node. DROP. |

The meta-finding: the "doesn't feature-detect on Node 22.15+" pool, once filtered to "actually loaded on Node," is the three browser-targeted polyfills already in the default set (Temporal, URLPattern, AbortController), plus the deferred `web-streams-polyfill`, plus a long tail of small wrappers that either fail the parity check (`is-buffer`, `abab`) or are too small to justify a clobber slot (`atob`).

## Recommendation

Proposed v0.1 default-clobber set under the reframed bar:

| Package | Clobbered to | Why this clears the reframed bar |
|---|---|---|
| `@js-temporal/polyfill` | `{ Temporal: globalThis.Temporal, Intl: globalThis.Intl, toTemporalInstant: Date.prototype.toTemporalInstant }` (native on Node 26+; Nub's `--import` polyfill on older Node) | (a). Main entry unconditionally loads ~125 KB of JSBI-backed polyfill source and exports the polyfill's own `Temporal` namespace. Never feature-detects. Clobber eliminates the entire bundle on native-supporting Node. |
| `urlpattern-polyfill` | `{ URLPattern: globalThis.URLPattern }` (native on Node 24+; Nub's polyfill on older Node) | (a). Main entry unconditionally loads ~18 KB of polyfill source; the global-write guard does not gate the named export. Clobber eliminates the polyfill bundle on native-supporting Node. |
| `abort-controller` | `{ AbortController: globalThis.AbortController, AbortSignal: globalThis.AbortSignal }` (native on Node 16+; present on Nub's 22.15+ floor) | (a). Main entry unconditionally loads `event-target-shim` and constructs its own `AbortSignal extends EventTarget`. Never feature-detects. Native additionally provides `AbortSignal.timeout`/`.any`/`.abort` and `signal.throwIfAborted()` (additive, no regression). Also clears (b) via Bun-parity. |

Deferred (clears (a) cleanly, but needs sub-path-aware clobber-table machinery first):

- `web-streams-polyfill@4.x` — main entry (`./`) and `/polyfill` sub-path are clobber-safe; `/es5` and `/polyfill/es5` must be passed through. Track for v0.x.

Demoted from the [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md) recommendations:

- `safe-buffer` — feature-detects to `require('buffer')` on every supported Node. No real elimination.
- `queue-microtask` — feature-detects to `queueMicrotask.bind(globalThis)` on Node ≥11.
- `buffer-from` — feature-detects to `Buffer.from`-wrapping on every supported Node.
- `setimmediate` — feature-detects via IIFE early-return; module is empty on Node.
- `performance-now` — feature-detects to `() => performance.now()` on Node ≥8.5.
- `is-buffer` — doesn't feature-detect, but trips the existing parity bar (degenerate-input divergence vs native).
- `atob` — doesn't feature-detect; clears (a) but the 5-line wrapper is below the load-bearing threshold.
- `abab` — doesn't feature-detect; clears (a) but `lib/atob`'s null-on-invalid-input vs native's `DOMException` throw is a parity bug. Also deprecated upstream.

Follow-on action items for the package-clobbering plan (deliberately not applied in this audit):

- Strengthen the rationale field on each of the three existing entries to lead with (a) — real code elimination — instead of install-size or Bun-parity. The `abort-controller` wording is the most outdated.
- Add a subsection stating the (a)/(b) test verbatim so future audit cycles use the same rule.
- Do not pull in any of the five "strong adds" from [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md).

## Sources

Every verdict above traces to a main entry read from unpkg at the version named, plus registry metadata for the entry-point fields and Node's own statement that core modules win a bare specifier.

- `@js-temporal/polyfill@0.5.1` package metadata: https://registry.npmjs.org/@js-temporal/polyfill/latest — `main` is `./dist/index.cjs`, `module` is `./dist/index.esm.js`.
- `@js-temporal/polyfill@0.5.1` ESM main entry: https://unpkg.com/@js-temporal/polyfill@0.5.1/dist/index.esm.js (128868 bytes; zero matches for `globalThis.Temporal` or `typeof Temporal`).
- `@js-temporal/polyfill@0.5.1` CJS main entry: https://unpkg.com/@js-temporal/polyfill@0.5.1/dist/index.cjs.
- `urlpattern-polyfill@10.1.0` ESM main entry: https://unpkg.com/urlpattern-polyfill@10.1.0/index.js.
- `urlpattern-polyfill@10.1.0` CJS main entry: https://unpkg.com/urlpattern-polyfill@10.1.0/index.cjs.
- `abort-controller@3.0.0` ESM main entry: https://unpkg.com/abort-controller@3.0.0/dist/abort-controller.mjs.
- `abort-controller@3.0.0` CJS main entry: https://unpkg.com/abort-controller@3.0.0/dist/abort-controller.js.
- `safe-buffer@5.2.1` main entry: https://unpkg.com/safe-buffer@5.2.1/index.js.
- `queue-microtask@1.2.3` main entry: https://unpkg.com/queue-microtask@1.2.3/index.js.
- `buffer-from@1.1.2` main entry: https://unpkg.com/buffer-from@1.1.2/index.js.
- `setimmediate@1.0.5` main entry: https://unpkg.com/setimmediate@1.0.5/setImmediate.js.
- `performance-now@2.1.0` main entry: https://unpkg.com/performance-now@2.1.0/lib/performance-now.js.
- `is-buffer@2.0.5` main entry: https://unpkg.com/is-buffer@2.0.5/index.js.
- `atob@2.1.2` main entry: https://unpkg.com/atob@2.1.2/node-atob.js.
- `abab@2.0.6` main entry: https://unpkg.com/abab@2.0.6/index.js; `lib/atob`: https://unpkg.com/abab@2.0.6/lib/atob.js; deprecation notice: https://www.npmjs.com/package/abab.
- `web-streams-polyfill@4.3.0` ponyfill main entry: https://unpkg.com/web-streams-polyfill@4.3.0/dist/ponyfill.js.
- Browserify-shim family registry metadata (each via `https://registry.npmjs.org/<pkg>/latest`): `events@3.3.0`, `process@0.11.10`, `buffer@6.0.3`, `assert@2.1.0`, `util@0.12.5`, `path@0.12.7`, `stream@0.0.3`, `string_decoder@1.3.0`, `os-browserify@0.3.0`, `tty-browserify@0.0.1`, `crypto-browserify@3.12.1`, `domain-browser@5.7.0`, `punycode@2.3.1`.
- `readable-stream@4.7.0` main entry: https://unpkg.com/readable-stream@4.7.0/lib/ours/index.js.
- `inherits@2.0.4` main entry: https://unpkg.com/inherits@2.0.4/inherits.js.
- `safer-buffer@2.1.2` main entry: https://unpkg.com/safer-buffer@2.1.2/safer.js.
- `fast-text-encoding@1.0.6` main entry: https://unpkg.com/fast-text-encoding@1.0.6/text.min.js.
- Node bare-specifier resolution prefers core: https://nodejs.org/api/modules.html#core-modules — "Core modules can also be identified using the node: prefix … Without the node: prefix, Node.js will use the core module if both a core module and a third-party module of the same name are installed."
- Existing clobber corpus: [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md), [`polyfill-demand-audit.md`](polyfill-demand-audit.md), [`clobber-technical-followup.md`](clobber-technical-followup.md), [`clobber-perf-comparison.md`](clobber-perf-comparison.md), [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
