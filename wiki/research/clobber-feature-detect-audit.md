# Clobber feature-detect audit

**Date:** 2026-05-25. Scope: re-evaluate every userland package currently in (or proposed for) Nub's v0.1 default clobber set against a tightened bar: a clobber is justified only when the userland package does *not* already feature-detect and route to native on Nub's Node 22.15+ floor, OR when there is a specific, articulable parity benefit beyond install-size. Companion to [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md), [`polyfill-demand-audit.md`](polyfill-demand-audit.md), [`clobber-technical-followup.md`](clobber-technical-followup.md), [`clobber-perf-comparison.md`](clobber-perf-comparison.md), [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md). Triggered by pushback on 2026-05-25: "would be quite weird if [the userland package] clobbered the native implementation if it existed" — i.e., a clobber that only saves parse-cost on a polyfill that already routes to native is not buying what the legacy-polyfill doc claimed it was buying.

## TL;DR

- **The current three default-clobber entries all keep their seats** — but for a different and stronger reason than the existing doc records. `@js-temporal/polyfill`, `urlpattern-polyfill`, and `abort-controller` all clear the reframed bar under (a): the published main entry on each does NOT feature-detect to native on Nub's Node 22.15+ floor. They unconditionally load and execute their polyfill source on every import. Clobber is genuine code elimination (~125 KB Temporal, ~18 KB URLPattern, ~3 KB + `event-target-shim` for AbortController), not "a faster no-op."
- **All five "strong adds" from [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md) lose their seats under the reframed bar.** `safe-buffer`, `queue-microtask`, `buffer-from`, `setimmediate`, and `performance-now` *all* feature-detect to native on Node ≥4.5 / ≥8.5 / ≥11. On Nub's 22.15+ floor, every one of them is already a noop-substitution-by-binding: requiring `safe-buffer` returns `require('buffer')`; requiring `queue-microtask` returns `queueMicrotask.bind(globalThis)`; requiring `buffer-from` returns a wrapper that immediately delegates to `Buffer.from`; requiring `setimmediate` runs an IIFE that early-returns and produces an empty module; requiring `performance-now` returns `() => performance.now()`. Clobber for these would save parse-cost on the userland package file but would not eliminate any meaningful runtime work, because there is no runtime work to eliminate.
- **The three secondary candidates also drop**, each for a slightly different reason: `is-buffer` doesn't feature-detect but introduces a behavioral delta vs native on degenerate inputs (the existing clobber bar's "no API-parity bugs" still applies); `atob` doesn't feature-detect but the eliminated body is a single 5-line `Buffer.from(s, 'base64').toString('binary')` wrapper — clears the floor under (a) but the magnitude is below "load-bearing"; `abab`'s `lib/atob`'s spec-divergent `null`-on-invalid-input vs native's `DOMException` throw is a parity bug that disqualifies it independently of the reframed bar.
- **`web-streams-polyfill@4.x` ponyfill DOES NOT feature-detect** (verified via [`dist/ponyfill.js`](https://unpkg.com/web-streams-polyfill@4.3.0/dist/ponyfill.js) — full polyfill loads unconditionally, only the optional `/polyfill` sub-path touches `globalThis`). It clears (a) cleanly with a large elimination (~50 KB of stream-spec code). It remains **deferred** to v0.x for the same sub-path-aware-clobber-table reason as before: the existing simple-string-match table would over-match `/es5`.
- **No new "doesn't-feature-detect" candidates make it past the existing parity bar.** A sweep through the browserify-shim family (`events`, `process`, `buffer`, `assert`, `util`, `path`, `stream`, `string_decoder`, `crypto-browserify`, `os-browserify`, `tty-browserify`, `domain-browser`, `punycode`) finds that none are actually loaded on Node — Node's bare-specifier resolver picks the core module before `node_modules`, so the userland shim sits dormant on disk and clobbering would change nothing about what executes. `readable-stream@4.x` is loaded on Node and does not feature-detect, but it actively prefers its own vendored stream implementation by design (only `READABLE_STREAM=disable` env var routes to native) — clobbering would invert the package's purpose and break vendoring-on-purpose consumers. `inherits@2.0.4` feature-detects to `util.inherits` and would noop-substitute. `safer-buffer` partially feature-detects but its `safer.Buffer` is a `Buffer` minus `allocUnsafe`/`allocUnsafeSlow` by design — clobbering to native re-introduces the unsafe methods, which is a behavior change the package explicitly exists to prevent. None qualify.
- **Net effect on `wiki/runtime/package-clobbering.md`:** the v0.1 default set stays at three entries (Temporal, URLPattern, AbortController). The "strong adds" tranche from the legacy doc is rejected wholesale. The rationale field for each existing entry should be updated to lead with the (a) qualifier (real code elimination) instead of leading with install-size or Bun-parity, both of which were the weaker rationales the reframed bar now makes explicit.

## The reframed bar

A userland package is a clobber candidate only if **at least one of:**

- **(a)** it does NOT feature-detect to native on Nub's Node 22.15+ floor, so the clobber is genuine elimination of polyfill code that would otherwise execute on every import, OR
- **(b)** there is a specific, articulable parity benefit that justifies the clobber even though userland already routes to native — Bun-aliasing for ecosystem parity is the canonical example, but each invocation of (b) must name the benefit and not lean on vague "install-size" or "freshness" framings.

The reframed bar is the floor, not the ceiling: a package clearing (a) or (b) still has to pass the existing clobber bar's separate criteria (pure-spec-shim shape, no API-parity bugs, native equivalent exists). A package that clears (a) trivially — eliminating a 3-line wrapper, say — does not automatically merit a seat; the elimination should be load-bearing enough that the per-clobber surface (parity risk, support burden, debug-output noise) is worth carrying.

Install-size, parse-time, and "feels modern" are NOT articulable parity benefits under (b). They were the rationales the legacy-polyfill doc leaned on; they are now insufficient.

## Per-candidate verification table

| Package | Latest version | Feature-detects? | What runs on Node 22.15+ | Observable-equivalent to native? | Keep / drop under reframed bar | Source URL |
|---|---|---|---|---|---|---|
| `@js-temporal/polyfill` | 0.5.1 | **No** | Full ~125 KB polyfill bundle loads; exports `{ Temporal, Intl, toTemporalInstant }` from the polyfill's own namespace. Never assigns to `globalThis`. | **No** — software JSBI BigInt vs V8 native; cold-start ~1.25 ms higher; named-export `Temporal` is the polyfill class | **Keep** via (a) | [`dist/index.cjs`](https://unpkg.com/@js-temporal/polyfill@0.5.1/dist/index.cjs), [`dist/index.esm.js`](https://unpkg.com/@js-temporal/polyfill@0.5.1/dist/index.esm.js) |
| `urlpattern-polyfill` | 10.1.0 | **Partial** — global-write only | `import { URLPattern } from "./dist/urlpattern.js"` loads ~18 KB polyfill unconditionally; `export { URLPattern }` is the polyfill class; `if (!globalThis.URLPattern) globalThis.URLPattern = URLPattern` skips global write on native-supporting Node, but the named-export path is unaffected by the guard | **No** — named export is the polyfill class on every Node; ~30% slower `test()` vs native | **Keep** via (a) | [`index.js`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.js), [`index.cjs`](https://unpkg.com/urlpattern-polyfill@10.1.0/index.cjs) |
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

### `@js-temporal/polyfill@0.5.1` — KEEP

Both the ESM main entry (`dist/index.esm.js`) and the CJS main entry (`dist/index.cjs`) are the bundled polyfill source. A grep for `globalThis.Temporal` or `typeof Temporal` across either file returns zero matches. The file leads with `import e from "jsbi"` — i.e., the polyfill statically depends on the JSBI BigInt shim and unconditionally constructs its `Temporal.PlainDate`/`PlainTime`/`PlainDateTime`/etc. classes on top of JSBI. On Node 26 with native `Temporal`, `import { Temporal } from "@js-temporal/polyfill"` returns the polyfill namespace, not native, and the ~125 KB of polyfill code runs on every import regardless. Clobber to `{ Temporal: globalThis.Temporal }` is real elimination: removes one of the largest single polyfill source files in npm-popular use today. Verified against the existing claim in [`clobber-technical-followup.md`](clobber-technical-followup.md) §"`@js-temporal/polyfill@0.5.1`"; the claim still holds on `@latest`.

### `urlpattern-polyfill@10.1.0` — KEEP

The ESM and CJS main entries are identical in shape:

```js
import { URLPattern } from "./dist/urlpattern.js";
export { URLPattern };
if (!globalThis.URLPattern) {
  globalThis.URLPattern = URLPattern;
}
```

Two things to notice. First, the `import { URLPattern } from "./dist/urlpattern.js"` is unconditional — the polyfill source (~18 KB) loads on every import. Second, the `if (!globalThis.URLPattern)` guard governs only the *global write*, not the *named export*: `export { URLPattern }` always re-exports the polyfill's `URLPattern`, never the native one. Code that does `import { URLPattern } from "urlpattern-polyfill"` on Node 24+ gets the polyfill class even though `globalThis.URLPattern` is native. Clobber is real elimination of ~18 KB of polyfill source and a noticeable runtime delta (~30% faster `test()` calls). Verified against the existing claim in [`clobber-technical-followup.md`](clobber-technical-followup.md) §"`urlpattern-polyfill@10.1.0`"; still holds on `@latest`.

### `abort-controller@3.0.0` — KEEP

The current `wiki/runtime/package-clobbering.md` rationale for `abort-controller` reads "Userland API matches native exactly. Bun ships the same clobber. Negligible runtime delta — included for install-size and ecosystem-parity reasons rather than perf." Under the reframed bar that rationale is the *weaker* of two available justifications. The actual main entry, [`dist/abort-controller.mjs`](https://unpkg.com/abort-controller@3.0.0/dist/abort-controller.mjs), starts with:

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

There is no `typeof AbortController === 'function'` check, no `if (globalThis.AbortController)` guard. The package always loads `event-target-shim`, always constructs its own `AbortSignal extends EventTarget` class, always exports it. On Node 22.15+ this code is fully wasted: native `AbortController`/`AbortSignal` have been globals since Node 16. Clobber to `globalThis.AbortController`/`AbortSignal` is genuine elimination (a) — plus the additive benefit that native has `AbortSignal.timeout`, `AbortSignal.any`, `AbortSignal.abort`, and `signal.throwIfAborted()`, none of which the userland version exposes. Updating the rationale field in `package-clobbering.md` to lead with (a) is a strict improvement over leading with Bun-parity.

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

On Nub's 22.15+ floor — and any Node ≥4.5 — `module.exports = buffer` is the branch taken; `module.exports` IS the result of `require('buffer')` (i.e., `node:buffer`). The trailing `SafeBuffer` function definition and its prototype assignments still execute as dead-code parse cost, but the module's user-facing exports are bit-identical to `node:buffer`. A clobber here would save a few KB of dead-code parse and the `node_modules/safe-buffer` directory on disk; runtime behavior is unchanged because runtime behavior is already native. Under the reframed bar's (a), the elimination is dead code; under (b), the only nameable benefit is install-size, which the reframed bar explicitly disqualifies. DROP.

### `queue-microtask@1.2.3` — DROP

Full source is seven lines:

```js
let promise
module.exports = typeof queueMicrotask === 'function'
  ? queueMicrotask.bind(typeof window !== 'undefined' ? window : global)
  : cb => (promise || (promise = Promise.resolve())).then(cb).catch(err => setTimeout(() => { throw err }, 0))
```

On Node ≥11 (well below Nub's 22.15+ floor) the ternary's truthy branch is taken and `module.exports === queueMicrotask.bind(global)`. The userland default export and a hypothetical clobbed `export default queueMicrotask.bind(globalThis)` are the same bound function. There is nothing to eliminate, no behavior to fix, and the install-size win is small absolute (single-file package, no transitive deps). DROP.

### `buffer-from@1.1.2` — DROP

The top-level `isModern` constant is `typeof Buffer !== 'undefined' && typeof Buffer.alloc === 'function' && typeof Buffer.allocUnsafe === 'function' && typeof Buffer.from === 'function'`. On Nub's floor it is `true`. The exported `bufferFrom(value, encodingOrOffset, length)` reduces to: throw a custom `TypeError` if `value` is a number, else dispatch to `Buffer.from(...)` (string, ArrayBuffer, generic). The userland module is observably `Buffer.from` with a one-line preamble check that just substitutes the error message. Clobbering buys the substitution but at the cost of the error-message divergence (or matching it via a wrapper, which is silly). Not load-bearing under (a); no nameable (b). DROP.

### `setimmediate@1.0.5` — DROP

The IIFE leads with `if (global.setImmediate) { return; }`. On every Node version (the Node API has had `setImmediate` since 0.10), the IIFE returns before any code runs, no installation logic executes, and `module.exports` is never assigned — so `require('setimmediate')` returns `{}`. The userland module is observably empty on Node. Clobber to an empty module would be byte-equivalent at the export-shape level but, again, there is nothing to eliminate at runtime. The whole 187-line file parses for nothing, and parsing 6.5 KB of polyfill code is too small a parse-time win to justify a per-clobber surface. DROP.

### `performance-now@2.1.0` — DROP

The first branch of the leading `if` block on Node 22.15+ is taken:

```js
if ((typeof performance !== "undefined" && performance !== null) && performance.now) {
  module.exports = function() {
    return performance.now();
  };
}
```

Default export is `() => performance.now()`. Clobbering to `export default performance.now.bind(performance)` is a noop substitution at the binding level. Same DROP shape as `queue-microtask`.

### `is-buffer@2.0.5` — DROP

The entire module is:

```js
module.exports = function isBuffer (obj) {
  return obj != null && obj.constructor != null &&
    typeof obj.constructor.isBuffer === 'function' && obj.constructor.isBuffer(obj)
}
```

Does NOT feature-detect, so it clears (a) with a small elimination (3-line wrapper). But the wrapper has different semantics from native: it dispatches to `obj.constructor.isBuffer`, which is `Buffer.isBuffer` for real Buffer instances but can be anything for arbitrary objects. Native `Buffer.isBuffer` performs an internal brand check that ignores `.constructor`. For real Buffers both return true; for objects that lie about `.constructor.isBuffer`, userland returns true and native returns false. Whether that divergence matters depends on your view of "would a user notice." The existing clobber bar's "no API-parity bugs" criterion is conservative about exactly this kind of subtle delta. DROP under the existing bar even though (a) is technically cleared.

### `atob@2.1.2` — DROP

Full module:

```js
function atob(str) {
  return Buffer.from(str, 'base64').toString('binary');
}
module.exports = atob.atob = atob;
```

Does NOT feature-detect. Clears (a) with a tiny elimination — five lines. On valid input the userland function and `globalThis.atob` are observably identical (both are spec-defined as binary-string-from-base64). On invalid input the userland function relies on `Buffer.from`'s lenient parsing, while native `atob` throws `DOMException`. That latter divergence is mostly academic for typical use, but it is a delta. The bigger reason to drop is magnitude: a 5-line wrapper is not the kind of elimination the reframed bar's (a) was reaching for ("genuine elimination of polyfill code"). DROP for impact; close call.

### `abab@2.0.6` — DROP

The main entry is three lines (`const atob = require("./lib/atob"); const btoa = require("./lib/btoa"); module.exports = { atob, btoa };`) but loads two real files (101 + 62 lines) that do NOT feature-detect. Clears (a) with a small-but-real elimination. The disqualifier is parity: `lib/atob`'s leading docstring is *"Implementation of atob() according to the HTML and Infra specs, except that instead of throwing INVALID_CHARACTER_ERR we return null."* Returns `null` on invalid input where native throws `DOMException`. That is an explicit, documented divergence — consumer code that branches on the return value of `atob` behaves differently. The package is also deprecated upstream (npm README: "Please use your platform's native atob()/btoa() methods if possible — this package may be removed in the future.") so the install-size trend is already downward. DROP under the existing "no API-parity bug" criterion.

### `web-streams-polyfill@4.3.0` — DEFER (status unchanged)

The main entry `dist/ponyfill.js` is the full polyfill: a UMD wrapper that defines `ByteLengthQueuingStrategy`, `CountQueuingStrategy`, `ReadableByteStreamController`, `ReadableStream`, `ReadableStreamBYOBReader`, `ReadableStreamBYOBRequest`, `ReadableStreamDefaultController`, `ReadableStreamDefaultReader`, `TransformStream`, `TransformStreamDefaultController`, `WritableStream`, `WritableStreamDefaultController`, `WritableStreamDefaultWriter` and assigns them into the exports namespace. No `if (typeof ReadableStream !== 'undefined')` guard. No `globalThis` write from the main entry — that's the `/polyfill` sub-path's job. Verified: the file ends with `e.ReadableStream=ReadableStream, e.WritableStream=WritableStream, …` (UMD exports), and the only `globalThis` reference is `const fr = "undefined" != typeof globalThis ? globalThis : "undefined" != typeof self ? self : …` for the `DOMException` lookup, not a `URLPattern`-style assignment. The package clears (a) with a substantial elimination (~50 KB of stream-spec implementation). The `/polyfill` sub-path additionally installs the classes on `globalThis` (also without a feature-detect guard). Both are clobber-safe; the `/es5` and `/polyfill/es5` sub-paths are not safe to clobber (the user explicitly opted into ES5 output). The existing simple-string-match clobber table would over-match. Status: stays **deferred to v0.x** for the same machinery reason as before, NOT for any feature-detect-related reason.

## Newly-found "doesn't feature-detect" candidates

The sweep below targeted the npm-popular polyfill space, looking specifically for packages that always load their polyfill source. The headline finding is that none of them qualify under the *full* clobber bar — the (a)-clearing candidates that exist fail one or another of the existing criteria (Node never loads them, behavior changes on clobber, or by-design vendoring intent).

| Package | Loaded on Node? | Feature-detects? | Why not a candidate |
|---|---|---|---|
| `events` (3.3.0) | **No** — Node resolves core `events` before `node_modules` lookup; userland package is dormant on disk | No (browserify shim — always defines its own `EventEmitter`) | Clobbering a package Node never loads is a no-op. The package is webpack/browserify-only. |
| `process` (0.11.10) | **No** — same core-first resolution | No | Same; never loaded on Node. |
| `buffer` (npm; 6.0.3) | **No** — `node:buffer` wins | No | Same. |
| `assert` (npm; 2.1.0) | **No** — `node:assert` wins | No (relies on `object.assign/polyfill`, `object-is/polyfill`, `call-bind/callBound`) | Same. |
| `util` (npm; 0.12.5), `path` (npm; 0.12.7), `stream` (npm; 0.0.3), `string_decoder` (npm; 1.3.0), `os-browserify`, `tty-browserify`, `crypto-browserify`, `domain-browser` | **No** — all core-first | No (browserify-shim shape) | Same. The browserify-shim family is not a Node-clobber target at all; Node's resolver makes them moot. |
| `punycode` (2.3.1) | **Yes** — but `require('punycode')` still hits core `node:punycode` first (with DEP0040 warning); userland is the *migration target*, not the migration source | n/a | Wrong direction. Clobbering `punycode` would reverse the Node-official migration from core to userland. |
| `readable-stream` (4.7.0) | **Yes** — actively loaded | No — main entry prefers its own `../stream` implementation by default; only `process.env.READABLE_STREAM === 'disable'` routes to native `node:stream` | Clears (a) but the package's whole purpose is to be a vendored stream impl with cross-version-stable semantics. Clobbering inverts the package's intent and would break vendoring-on-purpose consumers. |
| `inherits` (2.0.4) | **Yes** | **Yes** — main entry is `try { var util = require('util'); if (typeof util.inherits !== 'function') throw ''; module.exports = util.inherits; } catch (e) { module.exports = require('./inherits_browser.js'); }` — on Node it always reaches `module.exports = util.inherits` | Feature-detects. Noop substitution. DROP. |
| `safer-buffer` (2.1.2) | **Yes** | Partial — copies properties from `Buffer` only if missing; `Safer.from`/`alloc` defined only if absent | Clobbering to native re-introduces `allocUnsafe`/`allocUnsafeSlow`, which the package explicitly excludes. Behavior change. DROP. |
| `fast-text-encoding` (1.0.6) | **Yes** | Partial — side-effect-only; `scope.TextEncoder = scope.TextEncoder || v` feature-detects the global install but the polyfill function definitions execute unconditionally; module export is `{}` | Module export is empty on Node, so a clobber would change nothing observable to consumers. Side-effect install is already a noop on modern Node. DROP. |

The broader meta-finding: the "doesn't feature-detect on Node 22.15+" pool, once filtered to "actually loaded on Node," is essentially the three browser-targeted polyfills already in the default set (Temporal, URLPattern, AbortController), plus the deferred web-streams-polyfill, plus a long tail of small wrappers that either fail the parity-tax check (`is-buffer`, `abab`) or whose elimination is too small to justify a clobber slot (`atob`).

## Recommendation

Proposed v0.1 default-clobber set under the reframed bar:

| Package | Clobbered to | Why this clears the reframed bar |
|---|---|---|
| `@js-temporal/polyfill` | `{ Temporal: globalThis.Temporal, Intl: globalThis.Intl, toTemporalInstant: Date.prototype.toTemporalInstant }` (native on Node 26+; Nub's `--import` polyfill on older Node) | (a). Main entry unconditionally loads ~125 KB of JSBI-backed polyfill source and exports the polyfill's own `Temporal` namespace. Never feature-detects. Clobber eliminates the entire bundle on native-supporting Node. |
| `urlpattern-polyfill` | `{ URLPattern: globalThis.URLPattern }` (native on Node 24+; Nub's polyfill on older Node) | (a). Main entry unconditionally loads ~18 KB of polyfill source; the global-write guard does not gate the named export. Clobber eliminates the polyfill bundle on native-supporting Node. |
| `abort-controller` | `{ AbortController: globalThis.AbortController, AbortSignal: globalThis.AbortSignal }` (native on Node 16+; present on Nub's 22.15+ floor) | (a). Main entry unconditionally loads `event-target-shim` and constructs its own `AbortSignal extends EventTarget`. Never feature-detects. Native additionally provides `AbortSignal.timeout`/`.any`/`.abort` and `signal.throwIfAborted()` (additive, no regression). Also clears (b) via Bun-parity. |

Deferred (clears (a) cleanly, but needs sub-path-aware clobber-table machinery before it can land):

- `web-streams-polyfill@4.x` — main entry (`./`) and `/polyfill` sub-path are clobber-safe; `/es5` and `/polyfill/es5` must be passed through. Track for v0.x.

Demoted from the [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md) recommendations under the reframed bar:

- `safe-buffer` — feature-detects to `require('buffer')` on every supported Node. No real elimination.
- `queue-microtask` — feature-detects to `queueMicrotask.bind(globalThis)` on Node ≥11.
- `buffer-from` — feature-detects to `Buffer.from`-wrapping on every supported Node.
- `setimmediate` — feature-detects via IIFE early-return; module is empty on Node.
- `performance-now` — feature-detects to `() => performance.now()` on Node ≥8.5.
- `is-buffer` — doesn't feature-detect, but trips the existing parity bar (degenerate-input divergence vs native).
- `atob` — doesn't feature-detect; clears (a) but the 5-line wrapper is below the load-bearing threshold.
- `abab` — doesn't feature-detect; clears (a) but `lib/atob`'s null-on-invalid-input vs native's `DOMException` throw is a parity bug. Also deprecated upstream.

Follow-on action items for the package-clobbering plan (deliberately not applied in this audit):

- Strengthen the rationale field on each of the three existing entries to lead with (a) — real code elimination — instead of leading with install-size or Bun-parity. The existing wording for `abort-controller` is the most outdated and benefits most from the rewrite.
- Add an explicit "the reframed clobber bar" subsection that states the (a)/(b) test verbatim so future audit cycles use the same rule.
- Do not pull in any of the five "strong adds" from [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md); the legacy doc's recommendation does not survive the reframed bar.

## Sources

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
- Existing clobber corpus: `../runtime/package-clobbering.md`, [`userland-package-clobbering-audit.md`](userland-package-clobbering-audit.md), [`polyfill-demand-audit.md`](polyfill-demand-audit.md), [`clobber-technical-followup.md`](clobber-technical-followup.md), [`clobber-perf-comparison.md`](clobber-perf-comparison.md), [`legacy-polyfill-clobber-candidates.md`](legacy-polyfill-clobber-candidates.md).

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
