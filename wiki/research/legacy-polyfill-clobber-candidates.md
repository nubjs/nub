# Legacy-polyfill clobber candidates (npm-wide sweep)

An npm-wide sweep for packages that polyfill APIs Node has shipped natively for years, scored against the spec-shim bar the existing default clobber set already sets.

**Date:** 2026-05-25. Scope: broad sweep of high-download npm packages that polyfill APIs Node and V8 shipped natively years ago, looking for additions to Nub's v0.1 default clobber set beyond the three already chosen (`@js-temporal/polyfill`, `urlpattern-polyfill`, `abort-controller`). Companion to [[research/userland-package-clobbering-audit]], [[research/polyfill-demand-audit]], [[research/clobber-technical-followup]], [[research/clobber-perf-comparison]]. Question this doc answers: **which legacy-inertia packages (think `safe-buffer` — 100M+ weekly downloads of pure transitive cruft) clear the spec-shim, identical-shape, no-non-spec-exports bar that the existing clobber rule defines?**

## TL;DR

Five recommended additions, three secondary candidates, web-streams deferred until the clobber table is sub-path aware, and the whole es-shims family rejected on export shape.

- **Recommended additions to the v0.1 default clobber set: five.** `safe-buffer` (259M/wk), `queue-microtask` (116M/wk), `buffer-from` (86M/wk), `setimmediate` (51M/wk), `performance-now` (33M/wk). Combined: ~545M weekly downloads of packages that are either already no-ops on modern Node (the first three) or single-function spec-shims that map 1:1 to a native global (the last two). All five published main entries are pure spec-shape with no extra exports — exactly the "would-the-user-on-plain-Node-plus-`module.register()`-get-the-same-result" test the existing clobber doc enforces.
- **Three secondary candidates** that are clobber-safe but lower-impact: `is-buffer` (46M/wk; native is more correct on degenerate inputs), `atob` (15M/wk; default export is `(s) => Buffer.from(s, 'base64').toString('binary')`, identical to native), `abab` (26M/wk; named `{ atob, btoa }`, but the package is deprecated upstream so install-size pressure is already trending down). Add if the v0.1 default-clobber machinery is cheap to extend; otherwise these are the next tranche.
- **Web Streams is a special case.** `web-streams-polyfill@4.x` (55M/wk) has multiple entry points. The main entry (`./` = the ponyfill) is clobber-safe — it exports `{ ReadableStream, WritableStream, TransformStream, … }`, all spec classes that map 1:1 to globals. The `/polyfill` sub-path is also safe (it installs the same classes on `globalThis`, which is a no-op when native is present). The `/es5` and `/polyfill/es5` sub-paths must **not** be clobbered — they're for users targeting ES5, which Nub's Node 22.15+ floor makes moot, but clobbering would still misrepresent what the package promises. Recommend deferring web-streams to v0.x until the sub-path-aware clobber-table machinery is in place; the existing simple-string-match clobber table will accidentally clobber `/es5` if asked to handle the main entry.
- **The whole es-shims family is a clean reject.** `globalthis` (69M), `object-is` (25M), `object.hasown` (5.3M), `array-includes` (58M), `array.prototype.flat` (56M), `array.prototype.flatmap` (55M), `string.prototype.matchall` (47M), `object.assign` (78M), `object.fromentries` (54M), `object.entries` (51M), `object.values` (58M), `regexp.prototype.flags` (74M), `function.prototype.name` (67M), `string.prototype.trimstart` (69M), `string.prototype.trimend` (70M) — every one of these decorates the polyfill function with non-spec `.shim()`, `.getPolyfill()`, `.implementation` extras (verified by reading source for `object-is`, `object.hasown`, `globalthis`; the rest of the family follows the same template). Users who reach for these packages disproportionately want the `.shim()` method to install the polyfill globally, exactly the surface clobbering destroys. **Don't clobber any of them.** Combined demand ~900M weekly downloads, but it's the wrong shape regardless of the headline number.
- **The biggest surprises are `@ungap/structured-clone` and `whatwg-url`** — both look like clean polyfills from the outside, both have non-spec API on inspection. `@ungap/structured-clone` ships `serialize`/`deserialize` named exports plus accepts non-spec `{ json, lossy }` options on the default function (verified by reading [`cjs/index.js`](https://unpkg.com/@ungap/structured-clone@1.3.1/cjs/index.js)); `whatwg-url` is the spec reference implementation jsdom and npm use directly, exporting `parseURL`, `serializeURL`, `serializeHost`, and a dozen other spec-internal helpers no native global exposes. Both REJECT despite the dl-count temptation.

## Demand table (top legacy-polyfill candidates by 2026-05-18 → 2026-05-24 weekly downloads)

Thirty-four candidate packages ranked by weekly downloads, each with its native equivalent, the Node version that shipped it, Bun parity, and the clobber verdict.

| Package | DL/wk | Native equivalent + Node version | Bun parity | Safe to clobber? | Notes |
|---|---:|---|---|---|---|
| `safe-buffer` | 259,669,194 | `node:buffer` (`Buffer.from`/`alloc`/`allocUnsafe` since Node 4.5, 2016) | No | **Yes** | Modern-Node branch is literally `module.exports = require('buffer')` — clobber is bit-identical. |
| `punycode` | 182,208,028 | `node:punycode` (built-in, deprecated since Node 7) | No | **No** | Userland v2 is the actively-maintained fork; built-in is being phased out. Clobbering reverses the migration direction. |
| `whatwg-url` | 173,869,312 | `globalThis.URL` / `URLSearchParams` (Node 10+) | No | **No** | Full WHATWG URL reference impl. Exports `parseURL`, `serializeURL`, `serializeHost`, `setTheUsername`, etc. — no native equivalent for the spec-internal helpers. |
| `queue-microtask` | 116,566,283 | `globalThis.queueMicrotask` (Node 11+) | No | **Yes** | Modern-Node default export is `queueMicrotask.bind(globalThis)` — clobber is identical. |
| `base64-js` | 106,287,052 | n/a (not a Node-API polyfill; arbitrary `Uint8Array`↔base64) | No | **No** | Not a polyfill of any Node global; provides `byteLength`/`toByteArray`/`fromByteArray` over `Uint8Array`. No native target. |
| `buffer-from` | 86,449,201 | `Buffer.from` (Node 4.5+) | No | **Yes** | Default export wraps `Buffer.from`; only behavioral diff is the error message thrown on `typeof value === 'number'`. |
| `object.assign` | 78,932,309 | `Object.assign` (forever) | No | **No** | es-shims family — `.shim()`/`.getPolyfill()`/`.implementation` non-spec extras. |
| `string.prototype.trimend` | 70,412,639 | `String.prototype.trimEnd` (Node 10+) | No | **No** | es-shims family. |
| `string.prototype.trimstart` | 69,323,242 | `String.prototype.trimStart` (Node 10+) | No | **No** | es-shims family. |
| `globalthis` | 69,024,828 | `globalThis` (Node 12+) | No | **No** | Callable-default form (`require('globalthis')()`) plus `.shim()`/`.getPolyfill()`/`.implementation` extras. |
| `function.prototype.name` | 67,806,007 | `Function.prototype.name` (forever) | No | **No** | es-shims family. |
| `@ungap/structured-clone` | 62,758,913 | `globalThis.structuredClone` (Node 17+) | No | **No** | Non-spec `serialize`/`deserialize` named exports plus non-spec `{ json, lossy }` options on the default function. |
| `object.values` | 58,262,336 | `Object.values` (Node 7+) | No | **No** | es-shims family. |
| `array-includes` | 58,291,072 | `Array.prototype.includes` (Node 6+) | No | **No** | es-shims family. |
| `array.prototype.flat` | 56,347,110 | `Array.prototype.flat` (Node 11+) | No | **No** | es-shims family. |
| `array.prototype.flatmap` | 55,777,767 | `Array.prototype.flatMap` (Node 11+) | No | **No** | es-shims family. |
| `web-streams-polyfill` | 55,098,177 | `globalThis.ReadableStream` / `WritableStream` / `TransformStream` (Node 18+) | No | **Partial** | Main entry (ponyfill) and `/polyfill` are clobber-safe; `/es5` and `/polyfill/es5` are not, and the existing string-match clobber table would over-match. Defer to v0.x. |
| `object.fromentries` | 54,456,003 | `Object.fromEntries` (Node 12+) | No | **No** | es-shims family. |
| `event-target-shim` | 51,121,568 | `globalThis.EventTarget` (Node 15+) | No | **No** | Class export plus extension hooks (custom event types, attached-listener handling). Used as a base class by `abort-controller@1.x` and friends; clobbering breaks downstream subclassing. |
| `object.entries` | 51,727,037 | `Object.entries` (Node 7+) | No | **No** | es-shims family. |
| `setimmediate` | 51,234,228 | `globalThis.setImmediate` (Node forever) | No | **Yes** | Side-effect-only; on Node the IIFE early-returns and the module is empty. Clobber to empty module. |
| `string.prototype.matchall` | 47,761,812 | `String.prototype.matchAll` (Node 12+) | No | **No** | es-shims family. |
| `is-buffer` | 46,604,253 | `Buffer.isBuffer` (Node forever) | No | **Yes** (secondary) | Single-function default export. Native is stricter on degenerate inputs (objects with a fake `constructor.isBuffer`). |
| `performance-now` | 33,217,302 | `performance.now()` (Node 8.5+) | No | **Yes** | Default export is `() => performance.now()` on modern-Node branch — identical. |
| `promise` | 27,929,229 | `globalThis.Promise` (forever) | No | **No** | Default export is a different `Promise` constructor (not the global) — `instanceof` checks against the global break. Sub-paths (`/lib/es6-extensions`, `/setimmediate`, `/polyfill`) add monkey-patches. |
| `abab` | 26,879,958 | `globalThis.atob` / `btoa` (Node 16+) | No | **Yes** (secondary) | Named `{ atob, btoa }` exports map 1:1. Note: deprecated upstream as of [npm/abab@v2.0.6 README](https://www.npmjs.com/package/abab). |
| `object-is` | 25,834,301 | `Object.is` (forever) | No | **No** | es-shims family. |
| `whatwg-fetch` | 22,899,569 | `globalThis.fetch` (Node 18+) | No | **No** | XHR-based browser polyfill; named `fetch` export is the XHR impl, not the global. Useful in jsdom test envs, not on Node. |
| `es6-promise` | 17,387,027 | `globalThis.Promise` (forever) | No | **No** | Has explicit `.polyfill()` method that monkey-patches the global. Non-pure shape. |
| `core-js-pure` | 16,831,364 | various | No | **No** | Multi-hundred-export bundle consumed via transform, not a single clobber target. |
| `atob` | 15,489,899 | `globalThis.atob` (Node 16+) | No | **Yes** (secondary) | Default export `(s) => Buffer.from(s, 'base64').toString('binary')` — identical to native. |
| `fast-text-encoding` | 6,459,313 | `globalThis.TextEncoder` / `TextDecoder` (Node 11+) | No | **Maybe** | Side-effect global install; default export not the focus. Low priority below the 5M/wk cut. |
| `object.hasown` | 5,335,023 | `Object.hasOwn` (Node 16.9+) | No | **No** | es-shims family. |
| `text-encoding` | 1,885,782 | `globalThis.TextEncoder` / `TextDecoder` (Node 11+) | No | **No** | Includes a giant encodings table for legacy charsets the native pair doesn't cover (Big5, EUC-KR, etc.) — not a strict subset. |

All download counts from `https://api.npmjs.org/downloads/point/last-week/<pkg>` on 2026-05-25, window 2026-05-18 → 2026-05-24. Bun parity confirmed by reading [`src/resolve_builtins/HardcodedModule.zig`](https://github.com/oven-sh/bun/blob/main/src/resolve_builtins/HardcodedModule.zig) (local clone at `bun/src/resolve_builtins/HardcodedModule.zig`) — none of the candidates in this table appear in Bun's alias table. Bun's set remains the seven from [[research/userland-package-clobbering-audit]] (`ws`, `undici`, `node-fetch`, `isomorphic-fetch`, `@vercel/fetch`, `utf-8-validate`, `abort-controller`); legacy-polyfill cruft is not on Bun's radar at all.

## Per-candidate brief (top recommendations)

Published source for each recommended package, the modern-Node branch it reduces to, and the exact clobber synthetic proposed for it.

### `safe-buffer@5.2.1` — STRONG ADD

[Main entry `index.js`](https://unpkg.com/safe-buffer@5.2.1/index.js) — 22 lines on the modern-Node path:

```js
var buffer = require('buffer')
var Buffer = buffer.Buffer
…
if (Buffer.from && Buffer.alloc && Buffer.allocUnsafe && Buffer.allocUnsafeSlow) {
  module.exports = buffer
} else {
  // legacy fallback with SafeBuffer subclass
}
```

On any Node ≥ 4.5 — and Nub's floor is 22.15 — the entire user-visible module is `require('buffer')` re-exported. The `SafeBuffer` subclass only appears in the legacy fallback branch, so `import { SafeBuffer } from 'safe-buffer'` is already `undefined` on modern Node; a clobber that omits `SafeBuffer` (or aliases it to `Buffer`) is observably identical to the userland behavior. Clobber synthetic should re-export `node:buffer` directly. **Largest single line item in this audit by demand.** Almost all 259M weekly downloads are transitive — `safe-buffer` is depended on by `tar`, `string_decoder`, `readable-stream`, `webpack-sources`, and several thousand other long-tail packages that haven't bumped past their Node-4-era minimum.

### `queue-microtask@1.2.3` — STRONG ADD

[Full source](https://unpkg.com/queue-microtask@1.2.3/index.js) — 7 lines:

```js
let promise
module.exports = typeof queueMicrotask === 'function'
  ? queueMicrotask.bind(typeof window !== 'undefined' ? window : global)
  : cb => (promise || (promise = Promise.resolve())).then(cb).catch(err => setTimeout(() => { throw err }, 0))
```

On Node 11+ (where `queueMicrotask` is global), the default export is literally `queueMicrotask.bind(globalThis)`. Clobber to `export default queueMicrotask.bind(globalThis)` is a noop substitution at the binding level. No other exports. 116M weekly downloads, ~entirely transitive (heavily depended on by `run-parallel`, `readable-stream`, the webpack ecosystem).

### `buffer-from@1.1.2` — STRONG ADD

[Full source](https://unpkg.com/buffer-from@1.1.2/index.js) — 70 lines, default export is a single function `bufferFrom(value, encodingOrOffset, length)`.

On modern Node where `isModern` is `true`, the body reduces to: throw a custom `TypeError` if `value` is a number, else dispatch to `Buffer.from(...)`. The clobber synthetic is `export default (...args) => { if (typeof args[0] === 'number') throw new TypeError('"value" argument must not be a number'); return Buffer.from(...args); }` — preserves the package's specific error message verbatim. The clobber doesn't have to do this; the simpler `export default Buffer.from.bind(Buffer)` is also defensible if we accept that calling `bufferFrom(123)` now throws Node's standard `Buffer.from` error (`"The first argument must be of type string..."`) instead of the package's `"value" argument must not be a number`. Recommend the error-preserving version for byte-for-byte parity. 86M weekly downloads, ~all transitive.

### `setimmediate@1.0.5` — STRONG ADD

[Source](https://unpkg.com/setimmediate@1.0.5/setImmediate.js) — wrapped in `(function (global, undefined) { "use strict"; if (global.setImmediate) { return; } …`.

The IIFE returns immediately on every Node version (Node has had `setImmediate` natively forever); nothing assigns to `module.exports`. The module's observable effect on Node is **nothing** — empty module load. Clobber synthetic is `export {};` (or equivalently a `data:` URL with empty body). 51M weekly downloads of code that does nothing on Node, so there is no behavior to preserve.

### `performance-now@2.1.0` — STRONG ADD

[Source](https://unpkg.com/performance-now@2.1.0/lib/performance-now.js) — branches on `performance.now` availability. On modern Node the branch taken is:

```js
module.exports = function() {
  return performance.now();
};
```

Default export is a zero-arg function returning `performance.now()`. Clobber synthetic is `export default () => performance.now()` or `export default performance.now.bind(performance)`. No other exports. 33M weekly downloads.

### `is-buffer@2.0.5` — SECONDARY ADD

[Full source](https://unpkg.com/is-buffer@2.0.5/index.js) — 4 lines:

```js
module.exports = function isBuffer (obj) {
  return obj != null && obj.constructor != null &&
    typeof obj.constructor.isBuffer === 'function' && obj.constructor.isBuffer(obj)
}
```

The userland check is "object has a constructor with an `isBuffer` static method that returns true." Native `Buffer.isBuffer` uses Node's internal Buffer-instance brand check, which is stricter — it returns false for objects that lie about their constructor. **In every realistic case where `obj` is in fact a Buffer, both return true; in degenerate cases (an object with a fake `constructor.isBuffer = () => true`), userland returns true and native returns false.** That's a correctness win for native, not a regression. Default export is a single function; no extras. Clobber synthetic: `export default Buffer.isBuffer.bind(Buffer)`. 46M weekly downloads, ~entirely transitive (most direct consumers migrated to `Buffer.isBuffer` years ago).

### `atob@2.1.2` — SECONDARY ADD

[Full source](https://unpkg.com/atob@2.1.2/node-atob.js) — 5 lines:

```js
function atob(str) {
  return Buffer.from(str, 'base64').toString('binary');
}
module.exports = atob.atob = atob;
```

Default export is a function; `.atob` property on the function points at the same function. Modern Node's native `globalThis.atob` is observably identical (same binary-string-from-base64 behavior). Clobber synthetic: `const fn = (s) => globalThis.atob(s); fn.atob = fn; export default fn;` — preserves the unusual `require('atob').atob` access pattern some legacy code uses. 15M weekly downloads.

### `abab@2.0.6` — SECONDARY ADD

[Full source](https://unpkg.com/abab@2.0.6/index.js):

```js
const atob = require("./lib/atob");
const btoa = require("./lib/btoa");
module.exports = { atob, btoa };
```

Named exports `{ atob, btoa }` map 1:1 to globals on Node 16+. Clobber synthetic: `export const atob = globalThis.atob; export const btoa = globalThis.btoa;`. **Caveat:** the package is deprecated upstream; the README's first line is "Please use your platform's native `atob()`/`btoa()` methods if possible — this package may be removed in the future." The 26M weekly downloads are mostly transitive via `jsdom` (which dropped its dependency in v22+); the install-size pressure is already trending down without our help. Add if the clobber-table machinery is cheap to extend; otherwise let it die naturally.

### `web-streams-polyfill@4.3.0` — DEFER

Two of the package's four entry points are clobber-safe and two are not, which is why it waits for sub-path-aware machinery.

Main entry `dist/ponyfill.js` is a pure ponyfill — it exports `{ ByteLengthQueuingStrategy, CountQueuingStrategy, ReadableByteStreamController, ReadableStream, ReadableStreamBYOBReader, ReadableStreamBYOBRequest, ReadableStreamDefaultController, ReadableStreamDefaultReader, TransformStream, TransformStreamDefaultController, WritableStream, WritableStreamDefaultController, WritableStreamDefaultWriter }` and does **not** touch `globalThis`. Verified by reading [`dist/ponyfill.js`](https://unpkg.com/web-streams-polyfill@4.3.0/dist/ponyfill.js): the file ends with `e.ReadableStream=ReadableStream,…` assigning into the UMD exports namespace, no `globalThis` writes. Every named export maps 1:1 to a global on Node 18+. The package.json `exports` map advertises four entry points: `"."` (ponyfill), `"./polyfill"` (installs the classes on globalThis), `"./es5"` (ES5-compiled ponyfill), `"./polyfill/es5"` (ES5-compiled polyfill). The first two are clobber-safe; the last two would be misrepresented by a clobber (the user opted into ES5 output for a reason that Nub's Node-22.15 floor doesn't honor — but a user wrote `import …/es5` deliberately, and a silent substitution to the modern code would surprise them). **Defer to v0.x** until the clobber-table mechanism supports per-sub-path entries; the existing simple-string-match table would either over-clobber `/es5` or refuse to handle the main entry without conditional logic.

## Hazards encountered

Seven traps found while checking export shapes: es-shims decorations, sub-path exports that differ from the main entry, an installer method as the documented use, a distinct Promise constructor, and a URL parser used as a library.

- **Description-of-export-shape ≠ actual export shape.** Several es-shims packages (`globalthis`, `object-is`, `object.hasown`, `array-includes`, etc.) are billed as "polyfills" but the shipped main entry is a function with `.shim()/.getPolyfill()/.implementation` decorations specifically designed for consumption by `core-js`-style polyfill installers. Treating "polyfill" as a signal for clobber-safety is wrong; verify by reading the published source.
- **Re-export-of-native packages already are no-ops at runtime.** `safe-buffer`, `setimmediate`, `queue-microtask`, and `performance-now` are all already executing zero-effect code on modern Node. The clobber win is install-size (the package on disk and in `node_modules/.package-lock.json`) and parse-time (the file still has to be read, parsed, and executed even if the execution is a no-op). The runtime win is small; the install-size win is what the legacy-cruft category is about.
- **`@ungap/structured-clone`'s `/json` sub-export is a different function entirely.** Per the package.json `exports` map: `"./json": { "import": "./esm/json.js", "default": "./cjs/json.js" }`. That sub-export ships `parse`/`stringify` over the structured-clone representation, not the spec `structuredClone`. Any clobber of `@ungap/structured-clone` would need to leave `/json` alone; combined with the non-spec `serialize`/`deserialize`/`{ json, lossy }` on the main export, the package is a reject. Mentioned because the package is in the top tier by downloads and looks superficially safe.
- **`whatwg-fetch`'s named `fetch` export is the XHR-based polyfill, not the global.** On Node, `import { fetch } from 'whatwg-fetch'` returns a function that tries to construct `XMLHttpRequest` and throws. Anyone importing from `whatwg-fetch` on Node is either (a) running under jsdom test infrastructure that provides XHR, or (b) doing so by accident via transitive deps. Either way, clobbering to native fetch would silently change behavior in test environments that explicitly want the XHR-shaped fetch. REJECT despite the surface appeal.
- **`promise@8.x` returns a different `Promise` constructor.** `module.exports = require('./lib')` → `module.exports = Promise` where `Promise` is the package's own constructor function. `instanceof require('promise')` does not match global Promise instances; some legacy code relies on this differentiation to choose between native and userland implementations. Clobbering to `globalThis.Promise` breaks the discriminator. Plus the package has sub-paths (`promise/setimmediate`, `promise/polyfill`, `promise/lib/es6-extensions`) that add monkey-patches; the main-entry default export is intertwined with them. REJECT.
- **`es6-promise.polyfill()` is the documented use pattern.** Per [README](https://www.npmjs.com/package/es6-promise): "If you want to use this polyfill, you can do so via `require('es6-promise').polyfill();` or `require('es6-promise/auto');`" — i.e., the package's documented use is its `.polyfill()` method, not its default export. Clobbering the package would substitute the *constructor* for what users expect to be an *installer*. REJECT.
- **`whatwg-url` is library-grade, not a polyfill.** Used by jsdom, npm, yarn, and pnpm as a URL parser they consume directly (not as a global). Exports `parseURL`, `serializeURL`, `serializeHost`, `serializeURLOrigin`, `setTheUsername`, `setThePassword`, `cannotHaveAUsernamePasswordPort`, `percentDecodeString`, `percentDecodeBytes`, plus `URL` and `URLSearchParams`. The spec-internal helpers have no native equivalent. REJECT.

## Recommendation: proposed additions to the default-clobber table

Append the following rows to the v0.1 default clobber set, after the existing three:

| Package | Clobbered to | Reason |
|---|---|---|
| `safe-buffer` | `node:buffer` re-export (`export { Buffer, SlowBuffer, kMaxLength, INSPECT_MAX_BYTES, Blob, File, atob, btoa, isAscii, isUtf8, constants, kStringMaxLength, transcode, resolveObjectURL } from 'node:buffer'`) | Pure spec-shim: on Node ≥ 4.5 the package's main entry is literally `module.exports = require('buffer')`. Clobber is bit-identical to the userland module on every supported Node version. 259M weekly downloads, ~entirely transitive. Install-size win is the headline; runtime is already a no-op. |
| `queue-microtask` | `export default queueMicrotask.bind(globalThis)` | Single-function default export. On Node ≥ 11 (well below Nub's 22.15 floor) the userland module's default export is `queueMicrotask.bind(globalThis)` — clobber is the same binding. 116M weekly downloads, ~entirely transitive. |
| `buffer-from` | `export default (value, encodingOrOffset, length) => { if (typeof value === 'number') throw new TypeError('"value" argument must not be a number'); return Buffer.from(value, encodingOrOffset, length); }` | Single-function default export wrapping `Buffer.from`. Clobber preserves the package's specific `TypeError` message verbatim, matching userland byte-for-byte. 86M weekly downloads. |
| `setimmediate` | empty module (`export {};`) | Side-effect-only module that early-returns on any Node (`if (global.setImmediate) return;`). The userland module is observably empty on Node. Clobber is identical. 51M weekly downloads. |
| `performance-now` | `export default () => performance.now()` | Single-function default export. On Node ≥ 8.5 (where `performance.now` is available) the userland module reduces to `function() { return performance.now(); }`. Clobber is identical. 33M weekly downloads. |

Optional secondary tier (add now or in a follow-up batch — same shape-safety bar, lower demand or minor caveats):

| Package | Clobbered to | Reason |
|---|---|---|
| `is-buffer` | `export default Buffer.isBuffer.bind(Buffer)` | Single-function default export checking buffer-hood. Native is strictly more correct on degenerate inputs (objects with a fake `constructor.isBuffer`); for all real Buffers, both return true. 46M weekly downloads. |
| `atob` | `const fn = (s) => globalThis.atob(s); fn.atob = fn; export default fn;` | Single-function default export that's also accessible as `.atob` (`module.exports = atob.atob = atob;` in userland). On Node ≥ 16 the userland body `Buffer.from(str, 'base64').toString('binary')` is identical to native `globalThis.atob`. 15M weekly downloads. |
| `abab` | `export const atob = globalThis.atob; export const btoa = globalThis.btoa;` | Named `{ atob, btoa }` exports. Userland package is deprecated upstream (per its npm README); 26M weekly downloads are trending down without intervention. Worth a passive clobber if the table machinery is cheap. |

Plus a deferred entry, tracked for v0.x once sub-path-aware clobber-table machinery exists:

- **`web-streams-polyfill@4.x`** main entry (ponyfill) and `/polyfill` sub-path are clobber-safe (all named exports map 1:1 to globals); `/es5` and `/polyfill/es5` must be passed through. 55M weekly downloads.

## Sources

Every download count, published source file, and Node API-history reference this audit rests on.

- npm download counts (all 2026-05-25, window 2026-05-18 → 2026-05-24): `https://api.npmjs.org/downloads/point/last-week/<pkg>` for each package in the demand table.
- `safe-buffer@5.2.1` source: https://unpkg.com/safe-buffer@5.2.1/index.js
- `queue-microtask@1.2.3` source: https://unpkg.com/queue-microtask@1.2.3/index.js
- `buffer-from@1.1.2` source: https://unpkg.com/buffer-from@1.1.2/index.js
- `setimmediate@1.0.5` source: https://unpkg.com/setimmediate@1.0.5/setImmediate.js
- `performance-now@2.1.0` source: https://unpkg.com/performance-now@2.1.0/lib/performance-now.js
- `is-buffer@2.0.5` source: https://unpkg.com/is-buffer@2.0.5/index.js
- `atob@2.1.2` source: https://unpkg.com/atob@2.1.2/node-atob.js
- `abab@2.0.6` source: https://unpkg.com/abab@2.0.6/index.js; deprecation notice on npm: https://www.npmjs.com/package/abab
- `web-streams-polyfill@4.3.0` source: https://unpkg.com/web-streams-polyfill@4.3.0/dist/ponyfill.js; package.json `exports` map: `https://registry.npmjs.org/web-streams-polyfill/latest`
- `globalthis@1.0.4` source: https://unpkg.com/globalthis@1.0.4/index.js
- `object-is@1.1.6` source: https://unpkg.com/object-is@1.1.6/index.js
- `object.hasown@1.1.4` source: https://unpkg.com/object.hasown@1.1.4/index.js
- `@ungap/structured-clone@1.3.1` cjs entry: https://unpkg.com/@ungap/structured-clone@1.3.1/cjs/index.js; package.json `exports` map: `https://registry.npmjs.org/@ungap/structured-clone/latest`
- `whatwg-fetch@3.6.20` source: https://unpkg.com/whatwg-fetch@3.6.20/fetch.js
- `whatwg-url@16.0.1` package metadata: `https://registry.npmjs.org/whatwg-url/latest`
- `promise@8.3.0` and `es6-promise@4.2.8` metadata via `https://registry.npmjs.org/<pkg>/latest`
- Bun alias table — local clone at `bun/src/resolve_builtins/HardcodedModule.zig`; no matches for any candidate package in this audit
- Node API history: `Buffer.from` since [Node 4.5](https://nodejs.org/api/buffer.html#static-method-bufferfromarray) / [5.10](https://nodejs.org/en/blog/release/v5.10.0); `queueMicrotask` since [Node 11.0](https://nodejs.org/en/blog/release/v11.0.0/); `globalThis` since [Node 12.0](https://nodejs.org/en/blog/release/v12.0.0/); `structuredClone` since [Node 17.0](https://nodejs.org/en/blog/release/v17.0.0/); `atob`/`btoa` since [Node 16.0](https://nodejs.org/en/blog/release/v16.0.0/); `Object.hasOwn` since [Node 16.9](https://nodejs.org/en/blog/release/v16.9.0/); `performance.now` global since [Node 8.5](https://nodejs.org/api/perf_hooks.html); `setImmediate` since Node 0.10
- Existing clobber docs: [[research/userland-package-clobbering-audit]], [[research/polyfill-demand-audit]], [[research/clobber-technical-followup]], [[research/clobber-perf-comparison]]

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
