// Polyfill preloads for Nub v0.1 — the shared implementation for BOTH tiers.
//
// This is a CommonJS module with ZERO top-level await so the fast tier
// (Node 22.15+, `--require` CJS preload) can `require()` it synchronously: a
// `require()`-loaded preload keeps Node's synchronous `Module.runMain` CJS entry
// path (top-level `executionAsyncId()===1`, sync exception origin), which the old
// `--import` ESM preload broke (R1). The compat tier (`--import` preload.mjs)
// reuses this same logic via the `installSyncPolyfills` export, then loads the two
// ESM side-effect modules (worker-polyfill, navigator-locks) with dynamic
// `import()` — on the < 22.15 floor `require()` of an ES module is unreliable.
//
// All polyfills feature-detect and bow out if the global is already present.
//
// nub's SUPPORT floor is Node 18.19; 22.15 is the FAST-TIER boundary, NOT the
// floor. Judging "do we need a polyfill?" against 22.15 silently skips the
// 18.19–21.x compat tier — which is how Promise.withResolvers (native since Node
// 22.0) went unpolyfilled here until 2026-07, on a "native on our floor" verdict
// that was written when the floor really was 22.15.
//
// Node 22.15+ already has: navigator, navigator.locks,
// navigator.hardwareConcurrency, WebSocket.
//
// Node 22+ adds: Promise.withResolvers. Node 24+ adds: URLPattern,
// RegExp.escape, Error.isError, Promise.try. Each is polyfilled below its line.
//
// No Node version ships: Temporal, reportError, browser-shape Worker,
// Promise.allKeyed / Promise.allSettledKeyed.
// These need polyfills on all supported versions. (Temporal is a lazy global
// installed by the preload entry, NOT here — see preload.cjs / preload.mjs.)

// STRICT MODE IS LOAD-BEARING, not hygiene. In sloppy mode a method invoked as
// `fn.call(null)` has its `this` COERCED to the global object, so every
// RequireObjectCoercible / ToObject(this) check a polyfilled method makes would
// silently pass where the real builtin throws a TypeError — verified against native
// for both String.prototype.isWellFormed and Array.prototype.toSorted. Strict mode
// leaves `this` as null and restores the spec behavior for the whole file at once,
// instead of guarding method by method.
"use strict";

const { createRequire } = require("node:module");
const __require = createRequire(__filename);

// Install every globalThis/prototype polyfill that doesn't depend on loading the
// ESM side-effect modules (worker-polyfill, navigator-locks). Synchronous and
// idempotent — safe to call once per realm. `preloaded` carries the CJS-required
// polyfill packages the preload entry stashed (urlpattern, float16), since the
// resolve hook would otherwise clobber a later import of them.
function installSyncPolyfills(preloaded) {
  preloaded = preloaded || {};

  // ── Web Storage: neutralize the throwing localStorage getter ────────
  // When nub injects `--experimental-webstorage` on the 22.4–24 band AND the user
  // did NOT pass `--localstorage-file`, Node installs a `localStorage` global that
  // is a getter THROWING `ERR_INVALID_ARG_VALUE` on ANY access — even
  // `typeof localStorage` throws, so feature-detection is impossible and the throw
  // can surface before user code expects it. The spawn layer signals this case via
  // the internal `__NUB_NEUTRALIZE_LOCALSTORAGE` env var (set iff unflagged ∧
  // no user file). DELETE the throwing getter so the global becomes ABSENT —
  // matching vanilla Node 24's shape on this band (`'localStorage' in globalThis
  // === false`), not present-but-undefined. Absent is the additive choice: a bare
  // `localStorage` read throws ReferenceError exactly as on vanilla Node 24, and
  // `typeof localStorage === "undefined"` stays true with no throw. The earlier
  // present-undefined define matched Node 25+'s native shape, but that broke isomorphic
  // libraries that gate on `'localStorage' in window/globalThis` (e.g. vitest's
  // happy-dom `getWindowKeys`): a present property made them SKIP installing their
  // own store, so user code then read nub's `undefined` and crashed (#166). This
  // runs in the preload BEFORE any user code, so the throwing getter is never
  // observed. When the user passes `--localstorage-file`, the env var is absent and
  // `localStorage` works normally (we do not touch it). We deliberately KEEP the
  // env var set so it inherits to the whole process subtree: a `node`- or
  // `nub`-spawned grandchild re-inherits the webstorage flag via NODE_OPTIONS and
  // would otherwise re-install the throwing getter with no neutralize signal. It's
  // an internal `__NUB_*` plumbing var that's explicitly fine to leak to children.
  // Neutralization is idempotent — a descendant re-running this preload deletes an
  // already-absent or re-installed `localStorage` again, which is harmless. The
  // property is a configurable own accessor (the define that replaced it before
  // already proved that), so `delete` removes it cleanly. This file is strict-mode
  // CJS. The file is now STRICT (see the header), so `delete` on a
  // non-configurable property THROWS rather than returning false — which is exactly
  // what the try/catch below was already written to absorb.
  if (process.env.__NUB_NEUTRALIZE_LOCALSTORAGE) {
    try {
      delete globalThis.localStorage;
    } catch { /* non-configurable on this runtime: leave Node's behavior */ }
  }

  // ── reportError (WinterTC min-common-API, not in any Node) ──────────
  // Defined NON-ENUMERABLE so it is invisible to `Object.keys(globalThis)` /
  // for-in / structured-clone-of-keys — that invisibility-to-enumeration IS the
  // additive contract: code written for vanilla Node must not observe nub's
  // injected globals when it enumerates the global object. Node defines its own
  // globals non-enumerably for the same reason. Kept writable+configurable so
  // user code can still override or delete it, matching Node's global descriptors.
  if (typeof globalThis.reportError !== "function") {
    Object.defineProperty(globalThis, "reportError", {
      value: (err) => {
        queueMicrotask(() => {
          throw err;
        });
      },
      enumerable: false,
      writable: true,
      configurable: true,
    });
  }

  // ── File (global on Node 20+, missing on the 18.x compat floor) ─────
  // Node exposes the WHATWG `File` as a global from Node 20; on 18.13–18.x it
  // exists only as `node:buffer`'s `File` export. Backfill the global from there
  // so worker/messaging code that constructs `new File(...)` works down to the
  // floor (polyfill-all-the-way-down). Identity is preserved (same constructor as
  // `node:buffer`), so `instanceof` and undici's webidl brand checks hold. Blob is
  // already global on 18.x, but the same backfill guards it for completeness.
  // Non-enumerable to match Node's own global descriptors (the additive contract:
  // invisible to global enumeration).
  //
  // Node 18 emits a one-time `ExperimentalWarning: buffer.File …` on the FIRST
  // `new File(...)` (the constructor, NOT the property read). Without nub the floor
  // simply has no `File` global, so backfilling it would newly surface that warning
  // when user code first constructs a File. To keep the floor backfill silent we
  // force one throwaway construction INSIDE a suppression window: that consumes
  // Node's once-per-feature guard (the warning is dropped here) so the user's later
  // `new File(...)` is silent.
  if (typeof globalThis.File === "undefined" || typeof globalThis.Blob === "undefined") {
    const origEmitWarning = process.emitWarning;
    process.emitWarning = function (warning, ...rest) {
      const opt = rest[0];
      const type = opt && typeof opt === "object" ? opt.type : opt;
      const msg = typeof warning === "string" ? warning : (warning && warning.message) || "";
      if (type === "ExperimentalWarning" && /buffer\.(File|Blob)/.test(msg)) return;
      return origEmitWarning.call(this, warning, ...rest);
    };
    try {
      const buffer = require("node:buffer");
      const sampleArgs = { File: [[], ""], Blob: [[]] };
      for (const name of ["File", "Blob"]) {
        const Ctor = buffer[name];
        if (typeof globalThis[name] === "undefined" && typeof Ctor === "function") {
          Object.defineProperty(globalThis, name, {
            value: Ctor,
            enumerable: false,
            writable: true,
            configurable: true,
          });
          // Trip (and suppress) the experimental-feature warning now, so user code
          // never sees it.
          try { new Ctor(...sampleArgs[name]); } catch { /* construction shape varies; the warning fires regardless */ }
        }
      }
    } finally {
      process.emitWarning = origEmitWarning;
    }
  }

  // ── MessageEvent.ports → frozen array (WHATWG read-only requirement) ─
  // The spec mandates `MessageEvent.ports` be a read-only (frozen) array; Node's
  // native MessageEvent returns a mutable array. Wrap the configurable prototype
  // getter so every read yields a frozen array, for both a native MessageChannel's
  // delivery and nub's worker-side MessageEvents. Idempotent (the wrapper is marked
  // so a re-run in the same realm doesn't double-wrap).
  if (typeof globalThis.MessageEvent === "function") {
    const proto = globalThis.MessageEvent.prototype;
    const desc = Object.getOwnPropertyDescriptor(proto, "ports");
    if (desc && typeof desc.get === "function" && desc.configurable && !desc.get.__nubFreezesPorts) {
      const origGet = desc.get;
      const get = function () {
        const ports = origGet.call(this);
        return Array.isArray(ports) ? Object.freeze(ports) : ports;
      };
      get.__nubFreezesPorts = true;
      Object.defineProperty(proto, "ports", { ...desc, get });
    }
  }

  // ── URLPattern (native on Node 24+, missing on 22.x) ───────────────
  if (typeof globalThis.URLPattern === "undefined") {
    const mod = preloaded.urlpattern;
    const URLPattern = mod?.URLPattern;
    if (URLPattern) globalThis.URLPattern = URLPattern;
  }

  // Temporal (in no Node version) is installed as a LAZY global by the preload
  // entry after this runs — see preload.cjs / preload.mjs (A37). Touching
  // globalThis.Temporal here would defeat that laziness, so we must not.

  // ── Stage 4 polyfills (native on Node 24+, missing on 22.x) ────────

  // RegExp.escape — spec-faithful port of the TC39 proposal (native on Node 24+),
  // so the 22.x floor behaves byte-for-byte like native: a leading digit/letter is
  // control-escaped, syntax chars are backslashed, control chars use \t\n\v\f\r, and
  // the "other punctuators" + whitespace set is hex-escaped. Verified byte-identical
  // to Node's native RegExp.escape across every ASCII char + leading/whitespace/
  // astral cases (so a concatenated `escape(s)` is safe too, not just
  // `new RegExp(escape(s))`). The earlier reduced-fidelity version only escaped the
  // syntax chars.
  if (typeof RegExp.escape !== "function") {
    const SYNTAX = new Set(["^", "$", "\\", ".", "*", "+", "?", "(", ")", "[", "]", "{", "}", "|", "/"]);
    const CONTROL = { "\t": "\\t", "\n": "\\n", "\v": "\\v", "\f": "\\f", "\r": "\\r" };
    // ASCII "other punctuators" the spec escapes by code, plus SPACE.
    const OTHER = new Set([..." ,-=<>#&!%:;@~'\"`"]);
    const isWhiteSpace = (cp) =>
      cp === 0x09 || cp === 0x0a || cp === 0x0b || cp === 0x0c || cp === 0x0d ||
      cp === 0x20 || cp === 0xa0 || cp === 0x1680 || (cp >= 0x2000 && cp <= 0x200a) ||
      cp === 0x2028 || cp === 0x2029 || cp === 0x202f || cp === 0x205f || cp === 0x3000 ||
      cp === 0xfeff;
    const hexEscape = (cp) => {
      if (cp <= 0xff) return "\\x" + cp.toString(16).padStart(2, "0");
      if (cp <= 0xffff) return "\\u" + cp.toString(16).padStart(4, "0");
      const h = cp - 0x10000;
      const hi = 0xd800 + (h >> 10);
      const lo = 0xdc00 + (h & 0x3ff);
      return "\\u" + hi.toString(16).padStart(4, "0") + "\\u" + lo.toString(16).padStart(4, "0");
    };
    const encode = (ch, cp) =>
      SYNTAX.has(ch)
        ? "\\" + ch
        : CONTROL[ch] ?? ((OTHER.has(ch) || isWhiteSpace(cp)) ? hexEscape(cp) : ch);
    RegExp.escape = (s) => {
      if (typeof s !== "string") throw new TypeError("RegExp.escape argument must be a string");
      const cps = [...s]; // iterate by code point (astral-safe)
      let out = "";
      for (let i = 0; i < cps.length; i++) {
        const ch = cps[i];
        const cp = ch.codePointAt(0);
        // A leading decimal-digit/ASCII-letter is control-escaped so a preceding `\`
        // in a concatenated pattern can't form an escape sequence.
        if (i === 0 && ((cp >= 0x30 && cp <= 0x39) || (cp >= 0x41 && cp <= 0x5a) || (cp >= 0x61 && cp <= 0x7a))) {
          out += "\\x" + cp.toString(16).padStart(2, "0");
        } else {
          out += encode(ch, cp);
        }
      }
      return out;
    };
  }

  // Error.isError (~95% fidelity — cross-realm internal-slot unreachable)
  if (typeof Error.isError !== "function") {
    Error.isError = (value) => {
      if (value == null || typeof value !== "object") return false;
      return value instanceof Error;
    };
  }

  // Promise.try
  if (typeof Promise.try !== "function") {
    Promise.try = (fn, ...args) => {
      return new Promise((resolve) => resolve(fn(...args)));
    };
  }

  // Float16Array (TC39 Stage 4, native on Node 24+; absent on our 22.x floor).
  // Installed from the spec-compliant @petamoriken/float16 polyfill (vendored,
  // preloaded by the preload entry). It provides the full TypedArray method
  // surface (map/filter/subarray/set/reduce/…) and correct round-to-nearest-even,
  // including subnormals — unlike the prior hand-rolled Proxy shim, which had
  // ~30 methods missing and truncating/denormal-flushing conversion.
  //
  // INHERENT userland limitation (not fixable by any JS polyfill): a polyfilled
  // Float16Array isn't recognized by `ArrayBuffer.isView()` (it has no V8 internal
  // [[TypedArrayName]] slot). Code needing that check should use the polyfill's
  // `isFloat16Array`. See internal/runtime/float16array-polyfill.md.
  if (typeof globalThis.Float16Array === "undefined") {
    const f16 = preloaded.float16;
    if (f16?.Float16Array) {
      globalThis.Float16Array = f16.Float16Array;

      if (typeof DataView.prototype.getFloat16 !== "function") {
        DataView.prototype.getFloat16 = function (offset, littleEndian) {
          return f16.getFloat16(this, offset, littleEndian);
        };
        DataView.prototype.setFloat16 = function (offset, value, littleEndian) {
          f16.setFloat16(this, offset, value, littleEndian);
        };
      }

      if (typeof Math.f16round !== "function") {
        Math.f16round = f16.f16round;
      }
    }
  }

  installUint8ArrayBase64();
  installDisposableStacks();
  installKeyedPromiseCombinators();
  installPromiseWithResolvers();
  installFloorBuiltins();
  installSetMethods();
  installArrayFromAsync();
  installMapGetOrInsert();
  installMathSumPrecise();
  installIteratorSurface();
  installSymbolMetadata();
}

// ── The Iterator surface: constructor, Iterator.from, the 11 sync helpers, and
//    the newer statics (concat / zip / zipKeyed) plus chunks/windows/includes/join ──
// Node ships this in layers — helpers and `from` at 22.0, `concat` only at 26, and
// zip/zipKeyed/chunks/windows/includes/join nowhere yet — so this installer is
// deliberately built as INDEPENDENT per-name guards rather than one all-or-nothing
// branch. A runtime with native helpers keeps every native method untouched and
// gains only the names it lacks.
//
// FIDELITY LIMIT (inherent, same class as the polyfilled Float16Array not being an
// ArrayBuffer.isView): the helper objects here are generator objects, so their
// prototype is not the spec's %IteratorHelperPrototype% and their
// Symbol.toStringTag reads "Generator" rather than "Iterator Helper". Iteration,
// laziness, argument validation and underlying-iterator closing all behave
// correctly — only the internal identity differs, which no realistic consumer
// inspects. Generators were chosen precisely because they get the hard part right:
// an early `return()` propagates to the source iterator.
function installIteratorSurface() {
  // %IteratorPrototype% is reachable from any built-in iterator: array iterator →
  // %ArrayIteratorPrototype% → %IteratorPrototype%.
  const IteratorProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));

  if (typeof globalThis.Iterator !== "function") {
    // The Iterator constructor is abstract: calling it directly throws, and
    // subclassing is the only legitimate use. Its .prototype IS %IteratorPrototype%.
    const Iterator = function Iterator() {
      if (new.target === undefined || new.target === Iterator) {
        throw new TypeError("Abstract class Iterator not directly constructable");
      }
    };
    Object.defineProperty(Iterator, "prototype", {
      value: IteratorProto,
      writable: false,
      enumerable: false,
      configurable: false,
    });
    defBuiltin(IteratorProto, "constructor", Iterator);
    Object.defineProperty(globalThis, "Iterator", {
      value: Iterator,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }
  const Iterator = globalThis.Iterator;

  // GetIteratorDirect: helpers operate on the object's OWN `next`, without
  // re-invoking Symbol.iterator — so a partially-consumed iterator keeps its place.
  const iterOf = (obj) => {
    if (obj === null || (typeof obj !== "object" && typeof obj !== "function")) {
      throw new TypeError("not an object");
    }
    return obj;
  };
  // Drive an iterator record with for..of semantics while forwarding closure.
  // The `finally` is load-bearing: when a consumer abandons a helper early (a
  // `break`, a `return`, a throw), native helpers call the SOURCE iterator's
  // `return()`, and without this the source would be left open — verified against
  // native, which is how the omission was caught. It must fire ONLY on early exit,
  // since native does not call `return()` on an already-exhausted iterator.
  function* drain(rec) {
    let exhausted = false;
    try {
      while (true) {
        const r = rec.next();
        if (r.done) {
          exhausted = true;
          return;
        }
        yield r.value;
      }
    } finally {
      if (!exhausted) {
        const ret = rec.return;
        if (typeof ret === "function") {
          // A throw from `return()` must not mask the completion in flight.
          try {
            ret.call(rec);
          } catch { /* swallow, matching IteratorClose's error handling here */ }
        }
      }
    }
  }
  const asIterable = (rec) => ({ [Symbol.iterator]: () => drain(rec) });
  // ToIntegerOrInfinity plus the helpers' shared limit validation: NaN and negative
  // are RangeErrors, which is where a naive `Number(x) | 0` diverges.
  const toLimit = (v) => {
    const n = Number(v);
    if (Number.isNaN(n)) throw new RangeError("limit must not be NaN");
    const i = n === Infinity ? Infinity : Math.trunc(n) || 0;
    if (i < 0) throw new RangeError("limit must not be negative");
    return i;
  };
  const requireFn = (f, what) => {
    if (typeof f !== "function") throw new TypeError(`${what} is not a function`);
  };

  const protoHelpers = {
    map(mapper) {
      const rec = iterOf(this);
      requireFn(mapper, "mapper");
      return (function* () {
        let i = 0;
        for (const v of asIterable(rec)) yield mapper(v, i++);
      })();
    },
    filter(predicate) {
      const rec = iterOf(this);
      requireFn(predicate, "predicate");
      return (function* () {
        let i = 0;
        for (const v of asIterable(rec)) if (predicate(v, i++)) yield v;
      })();
    },
    take(limit) {
      const rec = iterOf(this);
      const n = toLimit(limit);
      return (function* () {
        if (n === 0) return;
        let left = n;
        for (const v of asIterable(rec)) {
          yield v;
          if (--left === 0) return;
        }
      })();
    },
    drop(limit) {
      const rec = iterOf(this);
      const n = toLimit(limit);
      return (function* () {
        let left = n;
        for (const v of asIterable(rec)) {
          if (left > 0) {
            left--;
            continue;
          }
          yield v;
        }
      })();
    },
    flatMap(mapper) {
      const rec = iterOf(this);
      requireFn(mapper, "mapper");
      return (function* () {
        let i = 0;
        for (const v of asIterable(rec)) {
          const inner = mapper(v, i++);
          // The mapper must return something iterable; a bare value is a TypeError
          // rather than being wrapped.
          if (inner === null || inner === undefined || typeof inner[Symbol.iterator] !== "function") {
            throw new TypeError("flatMap mapper did not return an iterable");
          }
          yield* inner;
        }
      })();
    },
    reduce(reducer) {
      const rec = iterOf(this);
      requireFn(reducer, "reducer");
      let acc;
      let i = 0;
      if (arguments.length > 1) {
        acc = arguments[1];
      } else {
        const first = rec.next();
        if (first.done) {
          throw new TypeError("reduce of empty iterator with no initial value");
        }
        acc = first.value;
        i = 1;
      }
      for (const v of asIterable(rec)) acc = reducer(acc, v, i++);
      return acc;
    },
    toArray() {
      const rec = iterOf(this);
      const out = [];
      for (const v of asIterable(rec)) out.push(v);
      return out;
    },
    some(predicate) {
      const rec = iterOf(this);
      requireFn(predicate, "predicate");
      let i = 0;
      for (const v of asIterable(rec)) if (predicate(v, i++)) return true;
      return false;
    },
    every(predicate) {
      const rec = iterOf(this);
      requireFn(predicate, "predicate");
      let i = 0;
      for (const v of asIterable(rec)) if (!predicate(v, i++)) return false;
      return true;
    },
    find(predicate) {
      const rec = iterOf(this);
      requireFn(predicate, "predicate");
      let i = 0;
      for (const v of asIterable(rec)) if (predicate(v, i++)) return v;
      return undefined;
    },
    forEach(fn) {
      const rec = iterOf(this);
      requireFn(fn, "fn");
      let i = 0;
      for (const v of asIterable(rec)) fn(v, i++);
      return undefined;
    },
    // ── Stage 3 additions, in no engine yet ──
    // iterator-chunking: chunks() partitions into non-overlapping arrays of n and
    // yields a SHORT final chunk; windows() slides by one and yields nothing at all
    // when the source is shorter than n. Both reject n < 1, unlike take/drop.
    chunks(chunkSize) {
      const rec = iterOf(this);
      const n = toLimit(chunkSize);
      if (n < 1 || n === Infinity) throw new RangeError("chunkSize must be a positive integer");
      return (function* () {
        let buf = [];
        for (const v of asIterable(rec)) {
          buf.push(v);
          if (buf.length === n) {
            yield buf;
            buf = [];
          }
        }
        if (buf.length > 0) yield buf;
      })();
    },
    windows(windowSize) {
      const rec = iterOf(this);
      const n = toLimit(windowSize);
      if (n < 1 || n === Infinity) throw new RangeError("windowSize must be a positive integer");
      return (function* () {
        const buf = [];
        for (const v of asIterable(rec)) {
          buf.push(v);
          if (buf.length > n) buf.shift();
          if (buf.length === n) yield buf.slice();
        }
      })();
    },
    // iterator-includes: SameValueZero, so NaN is found and -0 matches +0.
    includes(searchElement) {
      const rec = iterOf(this);
      for (const v of asIterable(rec)) {
        if (v === searchElement || (Number.isNaN(v) && Number.isNaN(searchElement))) return true;
      }
      return false;
    },
    // iterator-join: like Array.prototype.join — default separator ",", and
    // null/undefined elements become the empty string.
    join(separator) {
      const rec = iterOf(this);
      const sep = separator === undefined ? "," : String(separator);
      let out = "";
      let first = true;
      for (const v of asIterable(rec)) {
        if (!first) out += sep;
        first = false;
        if (v !== null && v !== undefined) out += String(v);
      }
      return out;
    },
  };
  // `chunks` is written out literally as the matrix anchor for the Stage 3 additions
  // (chunks/windows/includes/join), which install even where the ES2025 helpers are
  // already native. Every name is still guarded individually.
  if (typeof Iterator.prototype.chunks !== "function") {
    defBuiltin(IteratorProto, "chunks", protoHelpers.chunks);
  }
  for (const name of Object.keys(protoHelpers)) {
    if (name !== "chunks" && typeof IteratorProto[name] !== "function") {
      defBuiltin(IteratorProto, name, protoHelpers[name]);
    }
  }

  // Iterator.from: wraps an iterable OR a bare iterator-like object. An object that
  // already inherits from %IteratorPrototype% is returned AS IS.
  if (typeof Iterator.from !== "function") {
    defBuiltin(Iterator, "from", function from(O) {
      if (typeof O === "string") {
        return (function* () {
          yield* O;
        })();
      }
      iterOf(O);
      const method = O[Symbol.iterator];
      let rec;
      if (typeof method === "function") {
        rec = method.call(O);
        if (IteratorProto.isPrototypeOf(rec)) return rec;
      } else {
        if (typeof O.next !== "function") throw new TypeError("not iterable and has no next");
        if (IteratorProto.isPrototypeOf(O)) return O;
        rec = O;
      }
      return (function* () {
        yield* asIterable(rec);
      })();
    });
  }

  // Iterator.concat (Stage 4, native only on Node 26+): each argument must be
  // iterable, and each one's Symbol.iterator is called LAZILY, only when the
  // previous source is exhausted.
  if (typeof Iterator.concat !== "function") {
    defBuiltin(Iterator, "concat", function concat(...items) {
      for (const it of items) {
        if (it === null || it === undefined || typeof it[Symbol.iterator] !== "function") {
          throw new TypeError("Iterator.concat arguments must be iterable");
        }
      }
      return (function* () {
        for (const it of items) yield* it;
      })();
    });
  }

  // Iterator.zip / Iterator.zipKeyed (Stage 4, proposal-joint-iteration — in no
  // engine). zipKeyed is the ITERATOR twin of Promise.allKeyed: same own-enumerable
  // key treatment, same dictionary-in/dictionary-out shape. `mode` selects what
  // happens on unequal lengths: "shortest" (default) stops at the first exhausted
  // source, "longest" pads with `padding`/undefined, "strict" throws.
  const zipMode = (options) => {
    if (options === undefined) return { mode: "shortest", padding: undefined };
    if (options === null || typeof options !== "object") {
      throw new TypeError("options must be an object");
    }
    const mode = options.mode === undefined ? "shortest" : options.mode;
    if (mode !== "shortest" && mode !== "longest" && mode !== "strict") {
      throw new TypeError('mode must be "shortest", "longest", or "strict"');
    }
    return { mode, padding: options.padding };
  };
  if (typeof Iterator.zip !== "function") {
    defBuiltin(Iterator, "zip", function zip(iterables) {
      // Step 1 is an OBJECT check, which precedes any iterability test — so a string
      // primitive is a TypeError even though strings are iterable.
      iterOf(iterables);
      if (typeof iterables[Symbol.iterator] !== "function") {
        throw new TypeError("Iterator.zip requires an iterable of iterables");
      }
      const { mode, padding } = zipMode(arguments.length > 1 ? arguments[1] : undefined);
      const sources = [...iterables].map((it) => {
        if (it === null || it === undefined || typeof it[Symbol.iterator] !== "function") {
          throw new TypeError("Iterator.zip sources must be iterable");
        }
        return it[Symbol.iterator]();
      });
      // Padding is stepped EXACTLY once per source and then closed, never drained:
      // spreading it would hang on an infinite padding iterator and over-consume a
      // finite one.
      const pads = [];
      if (padding !== undefined) {
        if (padding === null || typeof padding[Symbol.iterator] !== "function") {
          throw new TypeError("padding must be iterable");
        }
        const padIter = padding[Symbol.iterator]();
        for (let i = 0; i < sources.length; i++) {
          const r = padIter.next();
          if (r.done) break;
          pads.push(r.value);
        }
        if (typeof padIter.return === "function") {
          try {
            padIter.return();
          } catch { /* closing must not mask the caller's completion */ }
        }
      }
      return (function* () {
        if (sources.length === 0) return;
        const live = sources.map(() => true);
        while (true) {
          const row = [];
          if (mode === "strict") {
            // Strict requires every source to end on the SAME round, and must throw
            // AT the diverging index — without stepping the sources after it.
            let firstDone;
            for (let i = 0; i < sources.length; i++) {
              const r = sources[i].next();
              if (i === 0) firstDone = !!r.done;
              else if (!!r.done !== firstDone) {
                throw new TypeError("Iterator.zip strict mode requires equal-length inputs");
              }
              if (!r.done) row.push(r.value);
            }
            if (firstDone) return;
            yield row;
            continue;
          }
          let anyLive = false;
          let anyDone = false;
          for (let i = 0; i < sources.length; i++) {
            if (!live[i]) {
              row.push(pads[i]);
              anyDone = true;
              continue;
            }
            const r = sources[i].next();
            if (r.done) {
              live[i] = false;
              anyDone = true;
              row.push(pads[i]);
            } else {
              anyLive = true;
              row.push(r.value);
            }
          }
          if (mode === "shortest" && anyDone) return;
          if (mode === "longest" && !anyLive) return;
          yield row;
        }
      })();
    });
  }
  if (typeof Iterator.zipKeyed !== "function") {
    defBuiltin(Iterator, "zipKeyed", function zipKeyed(iterables) {
      iterOf(iterables);
      const { mode, padding } = zipMode(arguments.length > 1 ? arguments[1] : undefined);
      // Own ENUMERABLE keys including symbols, matching Promise.allKeyed.
      const keys = Reflect.ownKeys(iterables).filter((k) => {
        const d = Reflect.getOwnPropertyDescriptor(iterables, k);
        return d !== undefined && d.enumerable;
      });
      const sources = keys.map((k) => {
        const it = iterables[k];
        if (it === null || it === undefined || typeof it[Symbol.iterator] !== "function") {
          throw new TypeError("Iterator.zipKeyed sources must be iterable");
        }
        return it[Symbol.iterator]();
      });
      const padFor = (k) =>
        padding === null || padding === undefined ? undefined : padding[k];
      return (function* () {
        const live = sources.map(() => true);
        while (true) {
          if (sources.length === 0) return;
          const row = Object.create(null);
          let anyLive = false;
          let anyDone = false;
          for (let i = 0; i < sources.length; i++) {
            if (!live[i]) {
              row[keys[i]] = padFor(keys[i]);
              anyDone = true;
              continue;
            }
            const r = sources[i].next();
            if (r.done) {
              live[i] = false;
              anyDone = true;
              row[keys[i]] = padFor(keys[i]);
            } else {
              anyLive = true;
              row[keys[i]] = r.value;
            }
          }
          if (mode === "shortest" && anyDone) return;
          if (mode === "strict" && anyDone) {
            if (!anyLive) return;
            throw new TypeError("Iterator.zipKeyed strict mode requires equal-length inputs");
          }
          if (mode === "longest" && !anyLive) return;
          yield row;
        }
      })();
    });
  }
}

// ── Symbol.metadata (TC39 Stage 3, decorator metadata; in no engine) ──
// Only the well-known symbol is provided. POPULATING `klass[Symbol.metadata]` is
// the decorator transform's job, and the spec's own answer for an undecorated class
// is `undefined` — so defining the symbol makes `Symbol.metadata` referenceable
// without inventing metadata that isn't there.
function installSymbolMetadata() {
  if (typeof Symbol.metadata === "symbol") return;
  Object.defineProperty(Symbol, "metadata", {
    value: Symbol("Symbol.metadata"),
    writable: false,
    enumerable: false,
    configurable: false,
  });
}

// Shared `def`: non-enumerable + writable + configurable, matching how the engine
// defines its own builtins (and this file's enumeration-invisibility contract).
// EVERY installer below guards per-METHOD on its own feature-detect, so a runtime
// that ships some of a family natively keeps those and only gains the rest — no
// polyfill ever overwrites a native implementation.
function defBuiltin(target, name, value) {
  Object.defineProperty(target, name, {
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
}

// ── New Set methods (TC39 Stage 4 / ES2025; native Node 22+, absent 18.19–21.x) ──
// The subtlety that makes these more than one-liners is that the ARGUMENT is not
// required to be a Set: the spec defines a "set-like" protocol (a `size` number
// plus callable `has` and `keys`), and reads it through GetSetRecord in a fixed
// order with specific error types. A naive `new Set(other)` implementation would
// accept the wrong inputs, reject the right ones, and iterate in the wrong order.
function installSetMethods() {
  // Real brand check: the `size` getter throws for anything without [[SetData]],
  // which is exactly the receiver check the spec performs first.
  const sizeGetter = Object.getOwnPropertyDescriptor(Set.prototype, "size").get;
  const requireSet = (o) => {
    sizeGetter.call(o);
  };
  // GetSetRecord: the argument-validation order is observable, so it is preserved
  // exactly — size read and coerced first (NaN is a TypeError, negative a
  // RangeError), then `has`, then `keys`.
  const setRecord = (obj) => {
    if (obj === null || (typeof obj !== "object" && typeof obj !== "function")) {
      throw new TypeError("argument is not an object");
    }
    const numSize = Number(obj.size);
    if (Number.isNaN(numSize)) throw new TypeError("size is NaN");
    const intSize = Math.trunc(numSize) || 0;
    if (intSize < 0) throw new RangeError("size is negative");
    const has = obj.has;
    if (typeof has !== "function") throw new TypeError("has is not callable");
    const keys = obj.keys;
    if (typeof keys !== "function") throw new TypeError("keys is not callable");
    return { obj, size: intSize, has, keys };
  };
  // -0 is normalized to +0 on the way into a result set, matching SameValueZero.
  const norm = (v) => (v === 0 ? 0 : v);
  const otherKeys = (rec) => rec.keys.call(rec.obj);

  const methods = {
    union(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = new Set(this);
      for (const k of otherKeys(rec)) out.add(norm(k));
      return out;
    },
    intersection(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = new Set();
      // Iterate the SMALLER side, but membership is always tested against the
      // other — the spec picks by size for complexity, and the observable
      // difference is which side's iteration order the result follows.
      if (this.size <= rec.size) {
        for (const k of this) if (rec.has.call(rec.obj, k)) out.add(k);
      } else {
        for (const k of otherKeys(rec)) if (this.has(k)) out.add(norm(k));
      }
      return out;
    },
    difference(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = new Set(this);
      if (this.size <= rec.size) {
        for (const k of this) if (rec.has.call(rec.obj, k)) out.delete(k);
      } else {
        for (const k of otherKeys(rec)) out.delete(norm(k));
      }
      return out;
    },
    symmetricDifference(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = new Set(this);
      for (const k of otherKeys(rec)) {
        const key = norm(k);
        if (this.has(key)) out.delete(key);
        else out.add(key);
      }
      return out;
    },
    isSubsetOf(other) {
      requireSet(this);
      const rec = setRecord(other);
      if (this.size > rec.size) return false;
      for (const k of this) if (!rec.has.call(rec.obj, k)) return false;
      return true;
    },
    isSupersetOf(other) {
      requireSet(this);
      const rec = setRecord(other);
      if (this.size < rec.size) return false;
      for (const k of otherKeys(rec)) if (!this.has(norm(k))) return false;
      return true;
    },
    isDisjointFrom(other) {
      requireSet(this);
      const rec = setRecord(other);
      if (this.size <= rec.size) {
        for (const k of this) if (rec.has.call(rec.obj, k)) return false;
      } else {
        for (const k of otherKeys(rec)) if (this.has(norm(k))) return false;
      }
      return true;
    },
  };
  // `union` is written out literally as the feature-matrix row's detect anchor; the
  // rest loop, and every method is still guarded on its own so a runtime shipping
  // only part of the family keeps what it has.
  if (typeof Set.prototype.union !== "function") defBuiltin(Set.prototype, "union", methods.union);
  for (const name of Object.keys(methods)) {
    if (name !== "union" && typeof Set.prototype[name] !== "function") {
      defBuiltin(Set.prototype, name, methods[name]);
    }
  }
}

// ── Array.fromAsync (TC39 Stage 4 / ES2025; native Node 22+, absent 18.19–21.x) ──
// Async-iterable OR sync-iterable OR array-like, in that precedence, with an
// optional mapfn whose result is AWAITED. `this` is the constructor when callable,
// so a subclass drives the result — matching Array.from.
function installArrayFromAsync() {
  if (typeof Array.fromAsync === "function") return;
  // mapfn/thisArg ride `arguments`: they are optional, so native length is 1.
  defBuiltin(Array, "fromAsync", async function fromAsync(asyncItems) {
    const mapfn = arguments.length > 1 ? arguments[1] : undefined;
    const thisArg = arguments.length > 2 ? arguments[2] : undefined;
    const C = typeof this === "function" ? this : Array;
    if (mapfn !== undefined && typeof mapfn !== "function") {
      throw new TypeError("mapfn is not a function");
    }
    const usingAsync = asyncItems != null && asyncItems[Symbol.asyncIterator] !== undefined;
    const usingSync = asyncItems != null && asyncItems[Symbol.iterator] !== undefined;
    if (usingAsync || usingSync) {
      const out = new C();
      let i = 0;
      // A sync iterator's YIELDED VALUES are awaited too, which is what makes
      // `Array.fromAsync([promise])` resolve rather than collect promises.
      for await (const v of asyncItems) {
        out[i] = mapfn === undefined ? v : await mapfn.call(thisArg, v, i);
        i++;
      }
      out.length = i;
      return out;
    }
    // Array-like fallback: read `length`, then each index, awaiting both the
    // element and the mapfn result.
    const arrayLike = Object(asyncItems);
    // ToLength clamps to [0, 2^53-1], so a NEGATIVE length yields an empty result
    // rather than the RangeError `new C(-5)` would throw.
    const len = Math.min(Math.max(Math.trunc(Number(arrayLike.length)) || 0, 0), 2 ** 53 - 1);
    const out = new C(len);
    for (let i = 0; i < len; i++) {
      const v = await arrayLike[i];
      out[i] = mapfn === undefined ? v : await mapfn.call(thisArg, v, i);
    }
    out.length = len;
    return out;
  });
}

// ── Map/WeakMap getOrInsert + getOrInsertComputed ──
//    (TC39 Stage 4 / ES2026 "upsert"; native Node 26+, absent below)
// Shipped under the name getOrInsert, NOT the proposal's original `upsert` — no
// runtime has ever exposed `upsert`, so only these two names are installed.
// getOrInsertComputed calls the callback ONLY on a miss, and re-reads the map
// afterwards because the callback can insert the same key reentrantly.
function installMapGetOrInsert() {
  // `Map.prototype.getOrInsert` is spelled out below as the feature-matrix row's
  // detect anchor; WeakMap rides the same guards through the loop.
  for (const Ctor of [Map, WeakMap]) {
    if (Ctor === Map && typeof Map.prototype.getOrInsert === "function"
        && typeof Map.prototype.getOrInsertComputed === "function") {
      continue;
    }
    if (typeof Ctor.prototype.getOrInsert !== "function") {
      defBuiltin(Ctor.prototype, "getOrInsert", function getOrInsert(key, value) {
        if (this.has(key)) return this.get(key);
        this.set(key, value);
        return value;
      });
    }
    if (typeof Ctor.prototype.getOrInsertComputed !== "function") {
      defBuiltin(
        Ctor.prototype,
        "getOrInsertComputed",
        function getOrInsertComputed(key, callbackfn) {
          if (typeof callbackfn !== "function") {
            throw new TypeError("callbackfn is not a function");
          }
          if (this.has(key)) return this.get(key);
          const value = callbackfn(key);
          // The callback may have inserted `key` itself; the spec's final write
          // wins, so set unconditionally rather than re-checking `has`.
          this.set(key, value);
          return value;
        },
      );
    }
  }
}

// ── Math.sumPrecise (TC39 Stage 3; in no Node — bun ships it, so bun is the
//    differential oracle used to verify this) ──
// Returns the CORRECTLY ROUNDED sum, not a left-to-right accumulation, so naive
// `reduce((a, b) => a + b)` is wrong: it loses low bits at every step. This is
// Shewchuk's exact-expansion algorithm — maintain a set of non-overlapping partial
// sums whose total is exact, then round once at the end.
function installMathSumPrecise() {
  if (typeof Math.sumPrecise === "function") return;
  defBuiltin(Math, "sumPrecise", function sumPrecise(items) {
    const partials = [];
    // Power-of-two factor the expansion is held in, so an intermediate overflow
    // can be escaped without losing exactness. Undone before returning.
    let scale = 1;
    let count = 0;
    // Tracks whether every finite contribution was -0, which the spec keeps in a
    // ~minus-zero~ state and must preserve in the result.
    let allMinusZero = true;
    let hasNaN = false;
    let posInf = false;
    let negInf = false;
    for (const x of items) {
      count++;
      if (typeof x !== "number") throw new TypeError("Math.sumPrecise accepts only Numbers");
      if (Number.isNaN(x)) {
        hasNaN = true;
        continue;
      }
      if (x === Infinity) {
        posInf = true;
        continue;
      }
      if (x === -Infinity) {
        negInf = true;
        allMinusZero = false;
        continue;
      }
      if (!Object.is(x, -0)) allMinusZero = false;
      if (hasNaN || (posInf && negInf)) continue;
      // Two-sum each new value against every existing partial, keeping the exact
      // low-order remainders as new partials.
      //
      // OVERFLOW: two partials can each be near MAX_VALUE while the TRUE sum is
      // finite (e.g. [1e308, 1e308, -1e308, -1e308] sums to 0), and a plain
      // expansion returns NaN there — caught by differential-testing against
      // bun's native implementation. When an intermediate goes non-finite,
      // HALVE the whole expansion and the incoming value and retry, tracking the
      // power-of-two `scale` to undo at the end. Halving a double is exact until
      // it goes subnormal, and a partial that subnormalizes here was already
      // ~2^-1074 relative to a ~2^1023 magnitude — far below the ULP of any
      // representable result, so no bit that could affect the rounding is lost.
      let xi = x * scale;
      for (;;) {
        let used = 0;
        let overflowed = false;
        for (let j = 0; j < partials.length; j++) {
          let y = partials[j];
          if (Math.abs(xi) < Math.abs(y)) {
            const t = xi;
            xi = y;
            y = t;
          }
          const hi = xi + y;
          if (!Number.isFinite(hi)) {
            overflowed = true;
            break;
          }
          const lo = y - (hi - xi);
          if (lo !== 0) partials[used++] = lo;
          xi = hi;
        }
        if (!overflowed) {
          partials.length = used;
          partials.push(xi);
          break;
        }
        // Rescale everything down by 2 and start this value over.
        for (let j = 0; j < partials.length; j++) partials[j] *= 0.5;
        scale *= 0.5;
        xi = x * scale;
      }
    }
    if (hasNaN) return NaN;
    if (posInf && negInf) return NaN;
    if (posInf) return Infinity;
    if (negInf) return -Infinity;
    // The additive identity here is -0: summing nothing, or only -0s, gives -0.
    // The all-(-0) case must be caught HERE, because the final reduction seeds
    // `total` with the literal +0 and `0 + -0` is +0, which would collapse the sign
    // (bun, the oracle, returns -0 for [-0] and [-0, -0]).
    if (count === 0 || partials.length === 0 || allMinusZero) return -0;
    // Add smallest-to-largest so the single final rounding is correct, then undo
    // the scale. Dividing by `scale` (a power of two) reintroduces no error.
    let total = 0;
    for (let j = partials.length - 1; j >= 0; j--) total += partials[j];
    return total / scale;
  });
}

// ── Shipped-standard ECMAScript builtins missing below their Node line ──
// Every one of these is Stage 4 (except Atomics.pause, Stage 3) and native on a
// NEWER Node than nub's 18.19 support floor, so each is a hole only the compat
// tier sees. They all went unpolyfilled until 2026-07 for the same reason
// Promise.withResolvers did: the 2026-05 candidates survey judged them "native on
// Nub's floor" while the floor was 22.15, and nothing revisited the verdicts when
// it moved to 18.19. Bands are from measurement across 18.19/20.10/21.0/22.15, not
// release notes — which is how URL.parse's 21.x hole turned up.
//
// Each guard is the method's own feature-detect, so every one is an independent
// no-op where the engine ships it. Installed non-enumerable, per this file's
// additive contract.
function installFloorBuiltins() {
  const def = (target, name, value) => {
    Object.defineProperty(target, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  // ── URL.parse (native Node 22.1 and 20.19; absent on all of 21.x) ──
  // The whole API is "new URL but null instead of throwing", so the try/catch IS
  // the spec. `base` is forwarded only when supplied: passing an explicit
  // undefined to the URL constructor is NOT the same as omitting it.
  if (typeof URL.parse !== "function") {
    // `base` is read from `arguments` rather than declared: WebIDL counts only
    // REQUIRED arguments toward `length`, so native URL.parse.length is 1.
    def(URL, "parse", function parse(url) {
      const base = arguments.length > 1 ? arguments[1] : undefined;
      try {
        return base === undefined ? new URL(url) : new URL(url, base);
      } catch {
        return null;
      }
    });
  }

  // ── String.prototype.isWellFormed / toWellFormed (native Node 20; absent 18.19) ──
  // "Well formed" means no LONE surrogate. Iterating with a code-point regex is
  // the cheapest faithful test: a paired surrogate matches as one astral code
  // point, so anything left in the 0xD800–0xDFFF range is unpaired.
  if (typeof String.prototype.isWellFormed !== "function") {
    const loneSurrogate = /[\uD800-\uDFFF]/u;
    // Spec step 1 is RequireObjectCoercible, so a null/undefined receiver must
    // THROW rather than stringify — a bare `String(this)` silently succeeds, which
    // native does not (verified: isWellFormed.call(null) is a TypeError).
    const thisStr = (v) => {
      if (v === null || v === undefined) {
        throw new TypeError("String.prototype method called on null or undefined");
      }
      return String(v);
    };
    def(String.prototype, "isWellFormed", function isWellFormed() {
      return !loneSurrogate.test(thisStr(this));
    });
    def(String.prototype, "toWellFormed", function toWellFormed() {
      // Replace each lone surrogate with U+FFFD. The `u` flag makes a well-formed
      // pair a single match unit, so only unpaired units are hit.
      return thisStr(this).replace(/[\uD800-\uDFFF]/gu, "�");
    });
  }

  // ── Object.groupBy / Map.groupBy (native Node 21; absent 18.19–20.x) ──
  // The two differ in more than their container: Object.groupBy coerces each key
  // with ToPropertyKey and returns a NULL-PROTOTYPE object (so a "toString" group
  // can't collide with Object.prototype), while Map.groupBy keys on the raw value
  // under SameValueZero — which a Map already implements, including -0 → +0.
  if (typeof Object.groupBy !== "function") {
    def(Object, "groupBy", function groupBy(items, callback) {
      if (typeof callback !== "function") throw new TypeError("callback is not a function");
      const obj = Object.create(null);
      let i = 0;
      for (const item of items) {
        const key = callback(item, i++);
        const k = typeof key === "symbol" ? key : String(key);
        if (obj[k] === undefined && !(k in obj)) obj[k] = [];
        obj[k].push(item);
      }
      return obj;
    });
  }
  if (typeof Map.groupBy !== "function") {
    def(Map, "groupBy", function groupBy(items, callback) {
      if (typeof callback !== "function") throw new TypeError("callback is not a function");
      const map = new Map();
      let i = 0;
      for (const item of items) {
        // Map.prototype.get/set already key under SameValueZero, so -0 collapses
        // to +0 exactly as the spec's normalization requires.
        const key = callback(item, i++);
        const group = map.get(key);
        if (group === undefined) map.set(key, [item]);
        else group.push(item);
      }
      return map;
    });
  }

  // ── Change-array-by-copy (native Node 20; absent 18.19) ──
  // Every method returns a plain Array regardless of the receiver's constructor —
  // deliberately NOT species-aware, unlike map/filter. The Array.from(this) hop
  // both realizes an array-like receiver and gives the fresh array to mutate.
  if (typeof Array.prototype.toSorted !== "function") {
    def(Array.prototype, "toSorted", function toSorted(compareFn) {
      if (compareFn !== undefined && typeof compareFn !== "function") {
        throw new TypeError("comparefn must be a function");
      }
      return Array.prototype.sort.call(Array.from(this), compareFn);
    });
    def(Array.prototype, "toReversed", function toReversed() {
      return Array.from(this).reverse();
    });
    def(Array.prototype, "toSpliced", function toSpliced(start, skipCount, ...items) {
      const arr = Array.from(this);
      // An OMITTED deleteCount deletes through the end, but a declared parameter
      // forwards an explicit `undefined`, which splice coerces to 0. Branch on the
      // real argument count so `toSpliced(1)` truncates the way native does.
      if (arguments.length <= 1) arr.splice(start);
      else arr.splice(start, skipCount, ...items);
      return arr;
    });
    // `with` is a reserved word, so a named function EXPRESSION is a SyntaxError.
    // An object-method shorthand accepts the reserved name and infers `name` as
    // "with", which a `function with_` would have gotten wrong.
    def(Array.prototype, "with", {
      with(index, value) {
        const arr = Array.from(this);
        const i = Math.trunc(Number(index)) || 0;
        const actual = i < 0 ? arr.length + i : i;
        if (actual < 0 || actual >= arr.length) throw new RangeError("invalid index");
        arr[actual] = value;
        return arr;
      },
    }.with);
  }

  // %TypedArray% gets toSorted/toReversed/with but NOT toSpliced (a typed array
  // cannot change length), and unlike the Array forms these return the SAME
  // typed-array type as the receiver.
  const TypedArrayProto = Object.getPrototypeOf(Int8Array.prototype);
  if (typeof TypedArrayProto.toSorted !== "function") {
    def(TypedArrayProto, "toSorted", function toSorted(compareFn) {
      if (compareFn !== undefined && typeof compareFn !== "function") {
        throw new TypeError("comparefn must be a function");
      }
      const copy = new this.constructor(this);
      return copy.sort(compareFn);
    });
    def(TypedArrayProto, "toReversed", function toReversed() {
      return new this.constructor(this).reverse();
    });
    def(TypedArrayProto, "with", {
      with(index, value) {
        const copy = new this.constructor(this);
        const i = Math.trunc(Number(index)) || 0;
        const actual = i < 0 ? copy.length + i : i;
        if (actual < 0 || actual >= copy.length) throw new RangeError("invalid index");
        copy[actual] = value;
        return copy;
      },
    }.with);
  }

  // ── ArrayBuffer.prototype.transfer / transferToFixedLength / detached ──
  //    (native Node 21; absent 18.19–20.x)
  // Faithful only because structuredClone's transfer list performs a REAL detach
  // on the floor — verified on 18.19: the source drops to 0 bytes and any view
  // construction on it throws, which is genuine detachment rather than a zeroed
  // buffer. Userland cannot otherwise detach an ArrayBuffer, so without that
  // primitive this pair would be unpolyfillable.
  if (typeof ArrayBuffer.prototype.transfer !== "function") {
    // A detached buffer is exactly one that rejects view construction. That is
    // what distinguishes it from a legitimately zero-length buffer, which accepts
    // a view fine — so byteLength alone cannot be the test.
    const isDetached = (buf) => {
      try {
        new Uint8Array(buf);
        return false;
      } catch {
        return true;
      }
    };
    const transferImpl = (buf, newLength, fixedLength) => {
      if (isDetached(buf)) throw new TypeError("ArrayBuffer is detached");
      const oldLen = buf.byteLength;
      const len = newLength === undefined ? oldLen : Math.trunc(Number(newLength)) || 0;
      if (len < 0) throw new RangeError("invalid length");
      // Copy BEFORE detaching; the copy is truncated or zero-padded to `len`.
      // `transfer` PRESERVES a resizable source's resizability while
      // `transferToFixedLength` always yields a fixed buffer — the two entry points
      // genuinely differ, verified against native (resizable=true vs false).
      const keepResizable = !fixedLength && buf.resizable === true;
      const out = keepResizable
        ? new ArrayBuffer(len, { maxByteLength: buf.maxByteLength })
        : new ArrayBuffer(len);
      new Uint8Array(out).set(new Uint8Array(buf, 0, Math.min(oldLen, len)));
      // Detach the source. The clone takes ownership of the original memory and
      // is immediately discarded, so this moves rather than copies.
      structuredClone(buf, { transfer: [buf] });
      return out;
    };
    // newLength is optional, so it rides `arguments`: native length is 0 for both.
    def(ArrayBuffer.prototype, "transfer", function transfer() {
      return transferImpl(this, arguments.length > 0 ? arguments[0] : undefined, false);
    });
    def(ArrayBuffer.prototype, "transferToFixedLength", function transferToFixedLength() {
      return transferImpl(this, arguments.length > 0 ? arguments[0] : undefined, true);
    });
    Object.defineProperty(ArrayBuffer.prototype, "detached", {
      get: function detached() {
        return isDetached(this);
      },
      enumerable: false,
      configurable: true,
    });
  }

  // ── Atomics.pause (TC39 Stage 3, proposal-atomics-microwait; in no Node) ──
  // A pure micro-architectural HINT with no observable effect beyond argument
  // validation, so returning undefined is a fully faithful implementation — the
  // spec permits an implementation to do nothing. Validation is the observable
  // part: the optional argument must be an integral Number.
  if (typeof Atomics !== "undefined" && typeof Atomics.pause !== "function") {
    def(Atomics, "pause", function pause() {
      const iterationNumber = arguments.length > 0 ? arguments[0] : undefined;
      if (iterationNumber !== undefined) {
        if (typeof iterationNumber !== "number" || !Number.isInteger(iterationNumber)) {
          throw new TypeError("Atomics.pause iterationNumber must be an integral Number");
        }
      }
      return undefined;
    });
  }
}

// ── Uint8Array base64/hex (TC39 Stage 3; native Node 25+, absent below) ──
// Spec-faithful port of the TC39 proposal-arraybuffer-base64 reference polyfill,
// so the < 25 floor behaves byte-for-byte like native: toBase64/fromBase64 honor
// the {alphabet, omitPadding} / {alphabet, lastChunkHandling} options,
// setFromBase64/setFromHex report {read, written} and write the valid prefix before
// throwing on a malformed tail, and toHex/fromHex round-trip. The methods are
// defined non-enumerable (the additive contract: invisible to enumeration of the
// prototype) and feature-detect off `Uint8Array.prototype.toBase64`, so they are a
// strict no-op where the runtime ships them natively. Verified differentially
// against Node native across the encode/decode/whitespace/padding/maxLength matrix.
function installUint8ArrayBase64() {
  if (typeof Uint8Array.prototype.toBase64 === "function") return;

  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  // char code → 6-bit value for the standard alphabet; url chars are remapped to
  // standard before lookup, so a single decode table covers both alphabets.
  const DECODE = new Int16Array(128).fill(-1);
  for (let i = 0; i < B64.length; i++) DECODE[B64.charCodeAt(i)] = i;

  // %TypedArray%.prototype[@@toStringTag] getter — the brand check native uses: it
  // accepts a Uint8Array (and Buffer, a Uint8Array subclass) and rejects any other
  // TypedArray or non-typed-array with a TypeError.
  const tagGet = Object.getOwnPropertyDescriptor(
    Object.getPrototypeOf(Uint8Array.prototype),
    Symbol.toStringTag,
  ).get;
  const checkU8 = (arg) => {
    let kind;
    try {
      kind = tagGet.call(arg);
    } catch {
      throw new TypeError("not a Uint8Array");
    }
    if (kind !== "Uint8Array") throw new TypeError("not a Uint8Array");
  };
  const getOptions = (options) => {
    if (typeof options === "undefined") return Object.create(null);
    if (options && typeof options === "object") return options;
    throw new TypeError("options is not object");
  };
  const isDetached = (arr) => "detached" in arr.buffer && arr.buffer.detached;
  const isWs = (cc) =>
    cc === 0x09 || cc === 0x0a || cc === 0x0c || cc === 0x0d || cc === 0x20;
  const skipWs = (s, i) => {
    while (i < s.length && isWs(s.charCodeAt(i))) i++;
    return i;
  };

  // chunk is 2–4 standard-alphabet chars; pads to 4 then emits 1–3 bytes. In strict
  // mode the unused low bits of a 2/3-char chunk must be zero.
  const decodeChunk = (chunk, throwOnExtraBits) => {
    const n = chunk.length;
    const padded = n < 4 ? chunk + (n === 2 ? "AA" : "A") : chunk;
    const triplet =
      (DECODE[padded.charCodeAt(0)] << 18) +
      (DECODE[padded.charCodeAt(1)] << 12) +
      (DECODE[padded.charCodeAt(2)] << 6) +
      DECODE[padded.charCodeAt(3)];
    const b0 = (triplet >> 16) & 255;
    const b1 = (triplet >> 8) & 255;
    const b2 = triplet & 255;
    if (n === 2) {
      if (throwOnExtraBits && b1 !== 0) throw new SyntaxError("extra bits");
      return [b0];
    }
    if (n === 3) {
      if (throwOnExtraBits && b2 !== 0) throw new SyntaxError("extra bits");
      return [b0, b1];
    }
    return [b0, b1, b2];
  };

  const u8ToBase64 = (arr, options) => {
    checkU8(arr);
    const opts = getOptions(options);
    let alphabet = opts.alphabet;
    if (typeof alphabet === "undefined") alphabet = "base64";
    if (alphabet !== "base64" && alphabet !== "base64url") {
      throw new TypeError('expected alphabet to be either "base64" or "base64url"');
    }
    const omitPadding = !!opts.omitPadding;
    if (isDetached(arr)) {
      throw new TypeError("toBase64 called on array backed by detached buffer");
    }
    const lookup =
      alphabet === "base64url"
        ? "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        : B64;
    let result = "";
    let i = 0;
    for (; i + 2 < arr.length; i += 3) {
      const triplet = (arr[i] << 16) + (arr[i + 1] << 8) + arr[i + 2];
      result +=
        lookup[(triplet >> 18) & 63] +
        lookup[(triplet >> 12) & 63] +
        lookup[(triplet >> 6) & 63] +
        lookup[triplet & 63];
    }
    if (i + 2 === arr.length) {
      const triplet = (arr[i] << 16) + (arr[i + 1] << 8);
      result +=
        lookup[(triplet >> 18) & 63] +
        lookup[(triplet >> 12) & 63] +
        lookup[(triplet >> 6) & 63] +
        (omitPadding ? "" : "=");
    } else if (i + 1 === arr.length) {
      const triplet = arr[i] << 16;
      result +=
        lookup[(triplet >> 18) & 63] +
        lookup[(triplet >> 12) & 63] +
        (omitPadding ? "" : "==");
    }
    return result;
  };

  // Core decode shared by fromBase64 and setFromBase64. Returns {bytes, read,
  // error}: a non-null `error` is thrown by the callers AFTER the valid prefix is
  // written (so setFromBase64 partial-writes then throws, matching native).
  const fromBase64 = (string, alphabet, lastChunkHandling, maxLength) => {
    if (maxLength === 0) return { read: 0, bytes: [], error: null };
    let read = 0;
    const bytes = [];
    let chunk = "";
    let index = 0;
    while (true) {
      index = skipWs(string, index);
      if (index === string.length) {
        if (chunk.length > 0) {
          if (lastChunkHandling === "stop-before-partial") {
            return { bytes, read, error: null };
          } else if (lastChunkHandling === "loose") {
            if (chunk.length === 1) {
              return {
                bytes,
                read,
                error: new SyntaxError("malformed padding: exactly one additional character"),
              };
            }
            bytes.push(...decodeChunk(chunk, false));
          } else {
            return { bytes, read, error: new SyntaxError("missing padding") };
          }
        }
        return { bytes, read: string.length, error: null };
      }
      let char = string[index];
      ++index;
      if (char === "=") {
        if (chunk.length < 2) {
          return { bytes, read, error: new SyntaxError("padding is too early") };
        }
        index = skipWs(string, index);
        if (chunk.length === 2) {
          if (index === string.length) {
            if (lastChunkHandling === "stop-before-partial") {
              return { bytes, read, error: null };
            }
            return { bytes, read, error: new SyntaxError("malformed padding - only one =") };
          }
          if (string[index] === "=") {
            ++index;
            index = skipWs(string, index);
          }
        }
        if (index < string.length) {
          return { bytes, read, error: new SyntaxError("unexpected character after padding") };
        }
        bytes.push(...decodeChunk(chunk, lastChunkHandling === "strict"));
        return { bytes, read: string.length, error: null };
      }
      if (alphabet === "base64url") {
        if (char === "+" || char === "/") {
          return { bytes, read, error: new SyntaxError("unexpected character " + JSON.stringify(char)) };
        } else if (char === "-") {
          char = "+";
        } else if (char === "_") {
          char = "/";
        }
      }
      if (!B64.includes(char)) {
        return { bytes, read, error: new SyntaxError("unexpected character " + JSON.stringify(char)) };
      }
      const remainingBytes = maxLength - bytes.length;
      if (
        (remainingBytes === 1 && chunk.length === 2) ||
        (remainingBytes === 2 && chunk.length === 3)
      ) {
        // The chunk-in-progress already represents exactly `remainingBytes` bytes;
        // the char we just read would start a group we have no room for. Stop.
        return { bytes, read, error: null };
      }
      chunk += char;
      if (chunk.length === 4) {
        bytes.push(...decodeChunk(chunk, false));
        chunk = "";
        read = index;
        if (bytes.length === maxLength) {
          // maxLength hit (setFromBase64 with a short target): native advances
          // `read` past trailing whitespace only when it runs to end-of-input —
          // if real content follows, `read` stays at the quad boundary.
          const after = skipWs(string, index);
          if (after === string.length) read = after;
          return { bytes, read, error: null };
        }
      }
    }
  };

  const b64ToU8 = (string, options, into) => {
    if (typeof string !== "string") throw new TypeError("expected input to be a string");
    const opts = getOptions(options);
    let alphabet = opts.alphabet;
    if (typeof alphabet === "undefined") alphabet = "base64";
    if (alphabet !== "base64" && alphabet !== "base64url") {
      throw new TypeError('expected alphabet to be either "base64" or "base64url"');
    }
    let lastChunkHandling = opts.lastChunkHandling;
    if (typeof lastChunkHandling === "undefined") lastChunkHandling = "loose";
    if (
      lastChunkHandling !== "loose" &&
      lastChunkHandling !== "strict" &&
      lastChunkHandling !== "stop-before-partial"
    ) {
      throw new TypeError(
        'expected lastChunkHandling to be either "loose", "strict", or "stop-before-partial"',
      );
    }
    if (into && isDetached(into)) {
      throw new TypeError("setFromBase64 called on array backed by detached buffer");
    }
    const maxLength = into ? into.length : 2 ** 53 - 1;
    let { bytes, read, error } = fromBase64(string, alphabet, lastChunkHandling, maxLength);
    if (error && !into) throw error;
    bytes = new Uint8Array(bytes);
    if (into && bytes.length > 0) into.set(bytes);
    if (error) throw error;
    return { read, bytes };
  };

  const u8ToHex = (arr) => {
    checkU8(arr);
    if (isDetached(arr)) {
      throw new TypeError("toHex called on array backed by detached buffer");
    }
    let out = "";
    for (let i = 0; i < arr.length; ++i) out += arr[i].toString(16).padStart(2, "0");
    return out;
  };

  const hexToU8 = (string, into) => {
    if (typeof string !== "string") throw new TypeError("expected string to be a string");
    if (into && isDetached(into)) {
      throw new TypeError("setFromHex called on array backed by detached buffer");
    }
    // Odd-length input is rejected unconditionally — even with an `into` and even
    // when maxLength would cut before the lone trailing hexit (matches native).
    if (string.length % 2 !== 0) {
      throw new SyntaxError("string should be an even number of characters");
    }
    const maxLength = into ? into.length : 2 ** 53 - 1;
    const bytesArr = [];
    let read = 0;
    let error = null;
    if (maxLength > 0) {
      while (read < string.length) {
        const hexits = string.slice(read, read + 2);
        if (/[^0-9a-fA-F]/.test(hexits)) {
          error = new SyntaxError("string should only contain hex characters");
          break;
        }
        bytesArr.push(parseInt(hexits, 16));
        read += 2;
        if (bytesArr.length === maxLength) break;
      }
    }
    if (error && !into) throw error;
    const bytes = new Uint8Array(bytesArr);
    if (into && bytes.length > 0) into.set(bytes);
    if (error) throw error;
    return { read, bytes };
  };

  const def = (target, name, fn) => {
    Object.defineProperty(target, name, {
      value: fn,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };
  def(Uint8Array.prototype, "toBase64", function toBase64(options) {
    return u8ToBase64(this, options);
  });
  def(Uint8Array, "fromBase64", function fromBase64(string, options) {
    return b64ToU8(string, options, undefined).bytes;
  });
  def(Uint8Array.prototype, "setFromBase64", function setFromBase64(string, options) {
    checkU8(this);
    const { read, bytes } = b64ToU8(string, options, this);
    return { read, written: bytes.length };
  });
  def(Uint8Array.prototype, "toHex", function toHex() {
    return u8ToHex(this);
  });
  def(Uint8Array, "fromHex", function fromHex(string) {
    return hexToU8(string, undefined).bytes;
  });
  def(Uint8Array.prototype, "setFromHex", function setFromHex(string) {
    checkU8(this);
    const { read, bytes } = hexToU8(string, this);
    return { read, written: bytes.length };
  });
}

// ── DisposableStack / AsyncDisposableStack (TC39 Stage 4 Explicit Resource
//    Management; native Node 24+, absent below) ──
// nub already down-levels the `using` / `await using` SYNTAX; this fills the
// runtime-CLASS gap so code that references the classes directly (or output from a
// toolchain that targets the native classes) works across the floor. Disposal is
// LIFO; a throwing disposer is aggregated into a SuppressedError chain per spec.
// Symbol.dispose / Symbol.asyncDispose are present on every Node nub supports, but
// are defined defensively if absent since the classes depend on them. Feature-detect
// off `globalThis.DisposableStack` / `globalThis.AsyncDisposableStack` — a strict
// no-op where native.
function installDisposableStacks() {
  if (typeof Symbol.dispose === "undefined") {
    Object.defineProperty(Symbol, "dispose", { value: Symbol("Symbol.dispose") });
  }
  if (typeof Symbol.asyncDispose === "undefined") {
    Object.defineProperty(Symbol, "asyncDispose", { value: Symbol("Symbol.asyncDispose") });
  }

  const defGlobal = (name, value) => {
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  // ── SuppressedError (TC39 Stage 4, the companion to the Stacks; native Node 24+,
  //    absent below) — the error a throwing disposer is aggregated into.
  if (typeof globalThis.SuppressedError === "undefined") {
    class SuppressedError extends Error {
      constructor(error, suppressed, message) {
        super(message);
        // Spec (and native Node 24+) install .error/.suppressed as non-enumerable
        // data props — plain assignment would make them enumerable, so Object.keys()
        // / JSON.stringify() would leak them on the floor but not on native.
        Object.defineProperty(this, "error", {
          value: error,
          writable: true,
          enumerable: false,
          configurable: true,
        });
        Object.defineProperty(this, "suppressed", {
          value: suppressed,
          writable: true,
          enumerable: false,
          configurable: true,
        });
      }
    }
    Object.defineProperty(SuppressedError.prototype, "name", {
      value: "SuppressedError",
      writable: true,
      enumerable: false,
      configurable: true,
    });
    defGlobal("SuppressedError", SuppressedError);
  }

  // new SuppressedError(error, suppressed): .error is the most-recent throw, the
  // accumulated prior chain is nested under .suppressed — matching the spec's
  // DisposeResources fold. Resolved AFTER the polyfill above so the floor gets a
  // real SuppressedError instance, not a bare Error.
  const Suppressed = globalThis.SuppressedError;

  if (typeof globalThis.DisposableStack === "undefined") {
    class DisposableStack {
      #disposed = false;
      #stack = [];
      get disposed() {
        return this.#disposed;
      }
      dispose() {
        if (this.#disposed) return undefined;
        this.#disposed = true;
        let hasError = false;
        let error;
        const stack = this.#stack;
        this.#stack = [];
        for (let i = stack.length - 1; i >= 0; i--) {
          try {
            stack[i]();
          } catch (e) {
            if (hasError) error = new Suppressed(e, error);
            else {
              hasError = true;
              error = e;
            }
          }
        }
        if (hasError) throw error;
        return undefined;
      }
      use(value) {
        if (this.#disposed) throw new ReferenceError("DisposableStack already disposed");
        if (value !== null && value !== undefined) {
          const method = value[Symbol.dispose];
          if (typeof method !== "function") throw new TypeError("value is not disposable");
          this.#stack.push(() => method.call(value));
        }
        return value;
      }
      adopt(value, onDispose) {
        if (this.#disposed) throw new ReferenceError("DisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        this.#stack.push(() => onDispose(value));
        return value;
      }
      defer(onDispose) {
        if (this.#disposed) throw new ReferenceError("DisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        this.#stack.push(() => onDispose());
        return undefined;
      }
      move() {
        if (this.#disposed) throw new ReferenceError("DisposableStack already disposed");
        const next = new DisposableStack();
        next.#stack = this.#stack;
        this.#stack = [];
        this.#disposed = true;
        return next;
      }
      get [Symbol.toStringTag]() {
        return "DisposableStack";
      }
    }
    // Spec: @@dispose is the same function object as `dispose`.
    Object.defineProperty(DisposableStack.prototype, Symbol.dispose, {
      value: DisposableStack.prototype.dispose,
      writable: true,
      enumerable: false,
      configurable: true,
    });
    defGlobal("DisposableStack", DisposableStack);
  }

  if (typeof globalThis.AsyncDisposableStack === "undefined") {
    class AsyncDisposableStack {
      #disposed = false;
      #stack = [];
      get disposed() {
        return this.#disposed;
      }
      async disposeAsync() {
        if (this.#disposed) return undefined;
        this.#disposed = true;
        let hasError = false;
        let error;
        const stack = this.#stack;
        this.#stack = [];
        for (let i = stack.length - 1; i >= 0; i--) {
          try {
            await stack[i]();
          } catch (e) {
            if (hasError) error = new Suppressed(e, error);
            else {
              hasError = true;
              error = e;
            }
          }
        }
        if (hasError) throw error;
        return undefined;
      }
      use(value) {
        if (this.#disposed) throw new ReferenceError("AsyncDisposableStack already disposed");
        if (value !== null && value !== undefined) {
          let method = value[Symbol.asyncDispose];
          if (method === undefined || method === null) {
            const sync = value[Symbol.dispose];
            if (typeof sync !== "function") {
              throw new TypeError("value is not async disposable");
            }
            this.#stack.push(() => sync.call(value));
          } else {
            if (typeof method !== "function") {
              throw new TypeError("value is not async disposable");
            }
            this.#stack.push(() => method.call(value));
          }
        }
        return value;
      }
      adopt(value, onDispose) {
        if (this.#disposed) throw new ReferenceError("AsyncDisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        this.#stack.push(() => onDispose(value));
        return value;
      }
      defer(onDispose) {
        if (this.#disposed) throw new ReferenceError("AsyncDisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        this.#stack.push(() => onDispose());
        return undefined;
      }
      move() {
        if (this.#disposed) throw new ReferenceError("AsyncDisposableStack already disposed");
        const next = new AsyncDisposableStack();
        next.#stack = this.#stack;
        this.#stack = [];
        this.#disposed = true;
        return next;
      }
      get [Symbol.toStringTag]() {
        return "AsyncDisposableStack";
      }
    }
    Object.defineProperty(AsyncDisposableStack.prototype, Symbol.asyncDispose, {
      value: AsyncDisposableStack.prototype.disposeAsync,
      writable: true,
      enumerable: false,
      configurable: true,
    });
    defGlobal("AsyncDisposableStack", AsyncDisposableStack);
  }
}

// ── Promise.allKeyed / Promise.allSettledKeyed (TC39 "await dictionary",
//    Stage 3) ──
// Spec-faithful port of PerformPromiseAllKeyed. No engine and no Node ships
// these yet, so unlike the rest of this file the feature-detect is not a version
// gate — it is the step-aside for whenever V8 does, the same posture as Temporal.
// Four observable details are load-bearing and easy to get wrong:
//
//   1. The result object has a NULL prototype (OrdinaryObjectCreate(null)), so
//      `result.hasOwnProperty` is undefined and util.inspect prints it as
//      `[Object: null prototype]`. Destructuring — the entire point of the API —
//      is unaffected.
//   2. Keys are the argument's own ENUMERABLE keys in [[OwnPropertyKeys]] order,
//      symbols INCLUDED. That is the proposal's deliberate divergence from the
//      userland precedents it replaces (bluebird's Promise.props, p-props), which
//      follow Object.keys and silently drop symbol keys.
//   3. A non-object argument and a non-callable `this.resolve` REJECT the
//      returned promise; only a `this` that isn't a constructor throws
//      synchronously (spec step 2's `?` vs steps 3 and 5).
//   4. `this` is the constructor, so a Promise subclass drives the result.
//
// Both are installed non-enumerable to match how the engine will define them
// (and this file's additive contract: invisible to enumeration).
// NewPromiseCapability(C), shared by the keyed combinators and withResolvers.
// Every caller reaches it through a spec step marked `?`, so a non-constructor
// `this` throws SYNCHRONOUSLY — there is no promise yet to reject.
function newPromiseCapability(C, name) {
  if (typeof C !== "function") {
    throw new TypeError(`Promise.${name} called on a non-constructor`);
  }
  let resolve;
  let reject;
  const promise = new C((res, rej) => {
    resolve = res;
    reject = rej;
  });
  if (typeof resolve !== "function" || typeof reject !== "function") {
    throw new TypeError("promise capability functions are not callable");
  }
  return { promise, resolve, reject };
}

// ── Promise.withResolvers (TC39 Stage 4 / ES2024; native on Node 22+) ──
// Absent on the whole 18.19–21.x compat tier with no polyfill until now. The
// 2026-05 candidates survey marked it "No action — native on Nub's floor", which
// was true when the floor WAS 22.15; the verdict was never revisited after the
// floor moved down to 18.19, so the gap survived silently. Spec: a capability
// plus a %Object.prototype% record of {promise, resolve, reject}, generic on
// `this` so a Promise subclass drives the promise.
function installPromiseWithResolvers() {
  if (typeof Promise.withResolvers === "function") return;
  Object.defineProperty(Promise, "withResolvers", {
    value: function withResolvers() {
      const { promise, resolve, reject } = newPromiseCapability(this, "withResolvers");
      return { promise, resolve, reject };
    },
    writable: true,
    enumerable: false,
    configurable: true,
  });
}

function installKeyedPromiseCombinators() {
  const needAll = typeof Promise.allKeyed !== "function";
  const needAllSettled = typeof Promise.allSettledKeyed !== "function";
  if (!needAll && !needAllSettled) return;

  // CreateKeyedPromiseCombinatorResultObject. Plain assignment IS
  // CreateDataPropertyOrThrow here: with no prototype there is no inherited
  // setter to intercept (not even `__proto__`), and the keys came from
  // [[OwnPropertyKeys]] so they are already distinct.
  const resultObject = (entries) => {
    const obj = Object.create(null);
    for (let i = 0; i < entries.length; i++) obj[entries[i].key] = entries[i].value;
    return obj;
  };

  // PerformPromiseAllKeyed. `remaining` starts at 1 and is decremented only once
  // the loop is done, so a dictionary of already-settled promises cannot resolve
  // the combinator before every key has been visited.
  const perform = (isSettled, promises, ctor, capability, promiseResolve) => {
    const entries = [];
    const remaining = { value: 1 };
    for (const key of Reflect.ownKeys(promises)) {
      const desc = Reflect.getOwnPropertyDescriptor(promises, key);
      if (desc === undefined || !desc.enumerable) continue;
      const value = promises[key];
      const index = entries.push({ key, value: undefined }) - 1;
      const nextPromise = promiseResolve.call(ctor, value);
      const alreadyCalled = { value: false };
      const settle = (entryValue) => {
        if (alreadyCalled.value) return;
        alreadyCalled.value = true;
        entries[index].value = entryValue;
        remaining.value -= 1;
        if (remaining.value === 0) capability.resolve(resultObject(entries));
      };
      remaining.value += 1;
      // allKeyed: the first rejection rejects the whole combinator through the
      // capability's own reject function. allSettledKeyed: the rejection is
      // recorded, and its handler SHARES `alreadyCalled` with the fulfill
      // handler, so exactly one of the pair counts even for a thenable that
      // calls both.
      nextPromise.then(
        isSettled ? (v) => settle({ status: "fulfilled", value: v }) : settle,
        isSettled ? (reason) => settle({ status: "rejected", reason }) : capability.reject,
      );
    }
    remaining.value -= 1;
    if (remaining.value === 0) capability.resolve(resultObject(entries));
  };

  const install = (name, isSettled) => {
    const fn = function (promises) {
      const ctor = this;
      const capability = newPromiseCapability(ctor, name);
      try {
        const promiseResolve = ctor.resolve;
        if (typeof promiseResolve !== "function") {
          throw new TypeError("Promise resolve is not a function");
        }
        if (
          promises === null ||
          (typeof promises !== "object" && typeof promises !== "function")
        ) {
          throw new TypeError(`Promise.${name} argument must be an object`);
        }
        perform(isSettled, promises, ctor, capability, promiseResolve);
      } catch (err) {
        capability.reject(err);
      }
      return capability.promise;
    };
    // A builtin's `name` and `length` are observable; the anonymous function
    // expression above gives length 1 but an empty name.
    Object.defineProperty(fn, "name", { value: name, configurable: true });
    Object.defineProperty(Promise, name, {
      value: fn,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  if (needAll) install("allKeyed", false);
  if (needAllSettled) install("allSettledKeyed", true);
}

// Load the two ESM side-effect modules — Web Locks (navigator.locks) and the
// browser-shape Worker global — synchronously via `require()`. Valid on the fast
// tier ONLY (Node 22.15+), where require(esm) of these side-effecting ES modules
// works (verified). The compat tier must NOT call this; it loads them with
// dynamic `import()` from preload.mjs instead.
function installEsmPolyfillsSync() {
  // ── navigator.locks (native on Node 24+, missing on 22.x) ──────────
  if (typeof globalThis.navigator?.locks === "undefined") {
    __require("./navigator-locks.mjs");
  }
  // ── Worker (browser-shape global, not in any Node) ──────────────────
  __require("./worker-polyfill.mjs");
}

module.exports = { installSyncPolyfills, installEsmPolyfillsSync };
