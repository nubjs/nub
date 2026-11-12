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

const {
  SetPrototypeValues,
  SetIteratorPrototypeNext,
  SetPrototypeAdd,
  SetPrototypeDelete,
  SetPrototypeHas,
  Object,
  Symbol,
  Uint8Array,
  Int8Array,
  Int16Array,
  SyntaxError,
  ArrayFrom,
  ArrayPrototypeReverse,
  ArrayPrototypeSplice,
  MapPrototypeGet,
  MapPrototypeSet,
  MapPrototypeHas,
  WeakMapPrototypeHas,
  StringPrototypeSlice,
  StringPrototypePadStart,
  StringPrototypeCharCodeAt,
  StringPrototypeIncludes,
  NumberPrototypeToString,
  TypedArrayPrototypeSet,
  TypedArrayPrototypeSort,
  TypedArrayPrototypeReverse,
  JSONStringify,
  parseInt,
  Array,
  ArrayBuffer,
  DataView,
  BigInt,
  Number,
  String,
  Map,
  Set,
  Error,
  ArrayPrototypePush,
  ArrayPrototypeSlice,
  ArrayPrototypeEvery,
  StringPrototypeSplit,
  StringPrototypeToLowerCase,
  RegExpPrototypeExec,
  PromiseResolve,
  ReferenceError,
  ArrayPrototypeShift,
  BigIntPrototypeToString,
  DataViewPrototypeSetFloat64,
  DataViewPrototypeGetUint32,
  ArrayIsArray,
  ArrayPrototypeSort,
  MathMax,
  MathMin,
  MathTrunc,
  NumberIsInteger,
  NumberIsNaN,
  ObjectCreate,
  ObjectDefineProperty,
  ObjectFreeze,
  ObjectGetOwnPropertyDescriptor,
  ObjectGetPrototypeOf,
  ObjectIs,
  ObjectIsFrozen,
  ObjectKeys,
  Proxy,
  RangeError,
  ReflectApply,
  ReflectConstruct,
  ReflectDefineProperty,
  ReflectGet,
  ReflectGetOwnPropertyDescriptor,
  ReflectOwnKeys,
  ReflectSet,
  TypeError,
  WeakMap,
  WeakMapPrototypeGet,
  WeakMapPrototypeSet
} = require("./primordials.cjs");

const compileBootstrap = process[Symbol.for("nub.compile.bootstrap")];
const getBuiltin = typeof compileBootstrap?.getBuiltin === "function"
  ? compileBootstrap.getBuiltin
  : require;
const { installUtilRateShims } = require("./util/rate.cjs");

// Install every globalThis/prototype polyfill that doesn't depend on loading the
// ESM side-effect modules (worker-polyfill, navigator-locks). Synchronous and
// idempotent — safe to call once per realm. `preloaded` carries the CJS-required
// polyfill packages the preload entry stashed (urlpattern, float16), since the
// resolve hook would otherwise clobber a later import of them.
function installSyncPolyfills(preloaded) {
  preloaded = preloaded || {};
  installUtilRateShims();

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
    ObjectDefineProperty(globalThis, "reportError", {
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
  //
  // Probe with `in`, never `typeof`: from Node 20 `File` is a lazy global, and
  // reading its value loads `internal/file` into every augmented startup. `in`
  // sees the property without materializing it.
  if (!("File" in globalThis) || !("Blob" in globalThis)) {
    const origEmitWarning = process.emitWarning;
    process.emitWarning = function (warning, ...rest) {
      const opt = rest[0];
      const type = opt && typeof opt === "object" ? opt.type : opt;
      const msg = typeof warning === "string" ? warning : (warning && warning.message) || "";
      if (type === "ExperimentalWarning" && /buffer\.(File|Blob)/.test(msg)) return;
      return ReflectApply(origEmitWarning, this, [warning, ...rest]);
    };
    try {
      const buffer = getBuiltin("node:buffer");
      const sampleArgs = { File: [[], ""], Blob: [[]] };
      for (const name of ["File", "Blob"]) {
        const Ctor = buffer[name];
        if (typeof globalThis[name] === "undefined" && typeof Ctor === "function") {
          ObjectDefineProperty(globalThis, name, {
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
  // The spec mandates `MessageEvent.ports` be a read-only (frozen) array. Below Node
  // 22.3 the global is `internal/worker/io`'s MessageEvent, whose `ports` getter
  // returns a MUTABLE array, so wrap the configurable prototype getter to freeze
  // every read — for both a native MessageChannel's delivery and nub's worker-side
  // MessageEvents. Idempotent (the wrapper is marked so a re-run in the same realm
  // doesn't double-wrap).
  //
  // NEVER lift this version gate into a `typeof globalThis.MessageEvent` check.
  // Node 22.3 replaced the global with undici's (nodejs/node#52370), which already
  // freezes in its own `get ports()` — so the wrapper is a strict no-op there — and
  // installed it as a V8 LAZY DATA PROPERTY. Any property operation that reads that
  // slot — `typeof`, a bare read, `getOwnPropertyDescriptor`, even `defineProperty`
  // — materializes it, pulling `internal/deps/undici/undici` and ~112 further
  // builtins (node:http, net, the whole internal/streams tree, worker_threads) into
  // EVERY augmented startup. There is no lazy wrapper to write instead: the accessor
  // cannot be read or shadowed without materializing, and the one free operation
  // (plain assignment) discards the native constructor irrecoverably. So the
  // decision must be made from the version alone, without touching the global.
  const [__meMajor = 0, __meMinor = 0] = process.versions.node
    .split(".")
    .map((n) => parseInt(n, 10));
  const nativePortsFrozen = __meMajor > 22 || (__meMajor === 22 && __meMinor >= 3);
  if (!nativePortsFrozen && typeof globalThis.MessageEvent === "function") {
    const proto = globalThis.MessageEvent.prototype;
    const desc = ObjectGetOwnPropertyDescriptor(proto, "ports");
    if (desc && typeof desc.get === "function" && desc.configurable && !desc.get.__nubFreezesPorts) {
      const origGet = desc.get;
      // Getter syntax, so the wrapper is an accessor function like the one it replaces.
      const { get } = ObjectGetOwnPropertyDescriptor(
        {
          get ports() {
            const ports = ReflectApply(origGet, this, []);
            return ArrayIsArray(ports) ? ObjectFreeze(ports) : ports;
          },
        },
        "ports",
      );
      get.__nubFreezesPorts = true;
      ObjectDefineProperty(proto, "ports", { ...desc, get });
    }
  }

  // ── URLPattern (native on Node 24+, missing on 22.x) ───────────────
  // The package's own entry assigns the global when it loads, which leaves it
  // enumerable, so it is defined here in that case too.
  const URLPattern = preloaded.urlpattern?.URLPattern;
  if (
    URLPattern &&
    (typeof globalThis.URLPattern === "undefined" || globalThis.URLPattern === URLPattern)
  ) {
    defBuiltin(globalThis, "URLPattern", URLPattern);
  }

  // Temporal (in no Node version) is installed as a LAZY global by the preload
  // entry after this runs — see preload.cjs / preload.mjs (A37). Touching
  // globalThis.Temporal here would defeat that laziness, so we must not.

  // `--frozen-intrinsics`: every installer below adds a member to a built-in, and
  // defining one on a frozen object throws, which would stop the program starting.
  // The globals above are unaffected, because globalThis itself stays extensible.
  // Math is the signal because a hardening preload that freezes only prototypes,
  // Object.prototype above all, leaves it alone and these installers still work.
  if (ObjectIsFrozen(Math)) return;

  installLocaleVariants();

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
    // A lone surrogate is escaped too (EncodeForRegExpEscape), so the result
    // stays well-formed and a `u`/`v` pattern built from it still parses.
    const isSurrogate = (cp) => cp >= 0xd800 && cp <= 0xdfff;
    const encode = (ch, cp) =>
      SYNTAX.has(ch)
        ? "\\" + ch
        : CONTROL[ch] ?? ((OTHER.has(ch) || isWhiteSpace(cp) || isSurrogate(cp)) ? hexEscape(cp) : ch);
    defBuiltin(RegExp, "escape", { escape(s) {
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
    } }.escape);
  }

  // Error.isError (~95% fidelity — cross-realm internal-slot unreachable)
  if (typeof Error.isError !== "function") {
    defBuiltin(Error, "isError", { isError(value) {
      if (value == null || typeof value !== "object") return false;
      return value instanceof Error;
    } }.isError);
  }

  // Promise.try (native on Node 23+). Generic on `this` like the other statics: the
  // capability comes from NewPromiseCapability(this), so a subclass drives the result
  // and a non-constructor `this` throws synchronously.
  if (typeof Promise.try !== "function") {
    const promiseResolve = PromiseResolve;
    defBuiltin(Promise, "try", {
      try(callbackfn, ...args) {
        if (!(this !== null && (typeof this === "object" || typeof this === "function"))) {
          throw new TypeError("Promise.try called on a non-object");
        }
        let value;
        try {
          value = callbackfn(...args);
        } catch (e) {
          const { promise, reject } = newPromiseCapability(this, "try");
          reject(e);
          return promise;
        }
        // PromiseResolve preserves an existing promise of this constructor,
        // including across realms. Do not consult a user-defined ctor.resolve.
        return ReflectApply(promiseResolve, this, [value]);
      },
    }.try);
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
      // Defined the way the engine defines them: non-enumerable, and the three
      // functions as methods, with `littleEndian` left out of `length`.
      defBuiltin(globalThis, "Float16Array", f16.Float16Array);

      if (typeof DataView.prototype.getFloat16 !== "function") {
        defBuiltin(DataView.prototype, "getFloat16", {
          getFloat16(offset) {
            return f16.getFloat16(this, offset, arguments[1]);
          },
        }.getFloat16);
      }
      if (typeof DataView.prototype.setFloat16 !== "function") {
        defBuiltin(DataView.prototype, "setFloat16", {
          setFloat16(offset, value) {
            f16.setFloat16(this, offset, value, arguments[2]);
          },
        }.setFloat16);
      }

      if (typeof Math.f16round !== "function") {
        defBuiltin(Math, "f16round", {
          f16round(x) {
            return f16.f16round(x);
          },
        }.f16round);
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
  installJsonParseOptions();
  installIteratorSurface();
  installSymbolMetadata();
}

// ── The Iterator surface: constructor, Iterator.from, the 11 sync helpers, and
//    the newer statics (concat / zip / zipKeyed) plus chunks/windows/includes/join ──
// Node ships this in layers — helpers and `from` at 22.0, `concat` only at 26, and
// zip/zipKeyed/includes/join at 27, and chunks/windows are still polyfilled. It is
// deliberately built as INDEPENDENT per-name guards rather than one all-or-nothing
// branch. A runtime with native helpers keeps every native method untouched and
// gains only the names it lacks.
//
// When the host has native helpers, map(identity) supplies their intrinsic
// prototype and internal slots while delegating iteration and closure to the
// fallback. Older hosts use the shared userland helper prototype below.
function installIteratorSurface() {
  // %IteratorPrototype% is reachable from any built-in iterator: array iterator →
  // %ArrayIteratorPrototype% → %IteratorPrototype%.
  const IteratorProto = ObjectGetPrototypeOf(ObjectGetPrototypeOf([][Symbol.iterator]()));
  const nativeMap = IteratorProto.map;
  const isObject = (v) => v !== null && (typeof v === "object" || typeof v === "function");

  if (typeof globalThis.Iterator !== "function") {
    // The Iterator constructor is abstract: calling it directly throws, and
    // subclassing is the only legitimate use. Its .prototype IS %IteratorPrototype%.
    const Iterator = function Iterator() {
      if (new.target === undefined || new.target === Iterator) {
        throw new TypeError("Abstract class Iterator not directly constructable");
      }
    };
    ObjectDefineProperty(Iterator, "prototype", {
      value: IteratorProto,
      writable: false,
      enumerable: false,
      configurable: false,
    });
    // `constructor` and Symbol.toStringTag are ACCESSORS in the spec, so that
    // assigning either on an object inheriting from %IteratorPrototype% (a
    // user's iterator class prototype) creates an own property there instead of
    // failing or rewriting the shared one (SetterThatIgnoresPrototypeProperties).
    const setIgnoringPrototype = (obj, key, v) => {
      if (!isObject(obj)) throw new TypeError("setter called on a non-object");
      if (obj === IteratorProto) throw new TypeError(`Cannot assign to Iterator.prototype[${String(key)}]`);
      const ok = ObjectGetOwnPropertyDescriptor(obj, key) === undefined
        ? ReflectDefineProperty(obj, key, { value: v, writable: true, enumerable: true, configurable: true })
        : ReflectSet(obj, key, v, obj);
      if (!ok) throw new TypeError(`Cannot assign to ${String(key)}`);
    };
    const accessors = {
      get constructor() {
        return Iterator;
      },
      set constructor(v) {
        setIgnoringPrototype(this, "constructor", v);
      },
      get [Symbol.toStringTag]() {
        return "Iterator";
      },
      set [Symbol.toStringTag](v) {
        setIgnoringPrototype(this, Symbol.toStringTag, v);
      },
    };
    for (const key of ["constructor", Symbol.toStringTag]) {
      const { get, set } = ObjectGetOwnPropertyDescriptor(accessors, key);
      ObjectDefineProperty(IteratorProto, key, { get, set, enumerable: false, configurable: true });
    }
    ObjectDefineProperty(globalThis, "Iterator", {
      value: Iterator,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }
  const Iterator = globalThis.Iterator;

  // %IteratorPrototype%[Symbol.dispose] (explicit resource management; native on
  // Node 24+): lets `using` close any iterator through its `return()`.
  if (typeof IteratorProto[Symbol.dispose] !== "function") {
    const dispose = {
      [Symbol.dispose]() {
        const ret = this.return;
        if (ret !== undefined && ret !== null) ReflectApply(ret, this, []);
      },
    }[Symbol.dispose];
    // Node 18-22 define Symbol.dispose themselves with the description
    // "nodejs.dispose", which would otherwise name this "[nodejs.dispose]".
    ObjectDefineProperty(dispose, "name", { value: "[Symbol.dispose]" });
    defBuiltin(IteratorProto, Symbol.dispose, dispose);
  }

  const AsyncIteratorProto = ObjectGetPrototypeOf(ObjectGetPrototypeOf(
    ObjectGetPrototypeOf((async function* () {})())
  ));
  if (typeof AsyncIteratorProto[Symbol.asyncDispose] !== "function") {
    const asyncDispose = {
      async [Symbol.asyncDispose]() {
        const ret = this.return;
        if (ret !== undefined && ret !== null) await ReflectApply(ret, this, []);
      },
    }[Symbol.asyncDispose];
    ObjectDefineProperty(asyncDispose, "name", { value: "[Symbol.asyncDispose]" });
    defBuiltin(AsyncIteratorProto, Symbol.asyncDispose, asyncDispose);
  }

  const iterOf = (obj) => {
    if (!isObject(obj)) throw new TypeError("not an object");
    return obj;
  };
  // GetIteratorDirect: helpers operate on the object's OWN `next`, read ONCE here
  // and never again, without re-invoking Symbol.iterator — so a partially-consumed
  // iterator keeps its place. Every method validates its arguments BEFORE this
  // runs, and a validation failure closes the receiver (`closeOnError`).
  const getIteratorDirect = (obj) => ({ iterator: obj, next: obj.next });
  // IteratorClose for a NORMAL completion: a throw from reading or calling
  // `return`, or a non-object result, propagates.
  const closeIterator = (iterator) => {
    const ret = iterator.return;
    if (ret === undefined || ret === null) return;
    if (!isObject(ReflectApply(ret, iterator, []))) {
      throw new TypeError("iterator.return() did not return an object");
    }
  };
  // IteratorClose for a THROW completion: the original error always wins.
  const closeOnError = (iterator, error) => {
    try {
      const ret = iterator.return;
      if (ret !== undefined && ret !== null) ReflectApply(ret, iterator, []);
    } catch { /* masked by `error`, per IteratorClose */ }
    throw error;
  };
  // Run a method's argument validation; on failure close the receiver, which has
  // not had its `next` read yet.
  const validate = (obj, check) => {
    try {
      return check();
    } catch (e) {
      closeOnError(obj, e);
    }
  };
  // Drive an iterator record with for..of semantics while forwarding closure.
  // The `finally` is load-bearing: when a consumer abandons a helper early (a
  // `break`, a `return`, a throw), native helpers call the SOURCE iterator's
  // `return()`, and without this the source would be left open. It must NOT fire
  // once the source is exhausted or has itself thrown (a throwing `next`, a
  // non-object result, a throwing `done`/`value` getter): IteratorStepValue marks
  // the record done in all of those, so native never calls `return()` on them.
  // A throw from `return()` propagates here; when the exit was itself a throw,
  // for..of's own IteratorClose discards it in favor of the original error.
  function* drain(rec) {
    let open = true;
    try {
      while (true) {
        let value;
        try {
          const r = ReflectApply(rec.next, rec.iterator, []);
          if (!isObject(r)) throw new TypeError("iterator result is not an object");
          if (r.done) {
            open = false;
            return;
          }
          value = r.value;
        } catch (e) {
          open = false;
          throw e;
        }
        yield value;
      }
    } finally {
      if (open) closeIterator(rec.iterator);
    }
  }
  const asIterable = (rec) => ({ [Symbol.iterator]: () => drain(rec) });

  // %IteratorHelperPrototype% stand-in. The helper body is a generator (which gets
  // laziness, re-entrancy errors and return-forwarding mid-iteration right), but a
  // bare generator misses one case: `return()` BEFORE the first `next()` must
  // still close the underlying iterator, and a never-started generator runs no
  // `finally`. This wrapper closes it directly in that state.
  const helperState = new WeakMap();
  const stateOf = (h) => {
    const s = WeakMapPrototypeGet(helperState, h);
    if (s === undefined) throw new TypeError("not an Iterator Helper");
    return s;
  };
  const HelperProto = ObjectCreate(IteratorProto);
  defBuiltin(HelperProto, "next", {
    next() {
      const s = stateOf(this);
      s.started = true;
      return s.gen.next();
    },
  }.next);
  defBuiltin(HelperProto, "return", {
    return() {
      const s = stateOf(this);
      if (!s.started) {
        s.started = true;
        s.gen.return();
        closeIterator(s.rec.iterator);
        return { value: undefined, done: true };
      }
      return s.gen.return();
    },
  }.return);
  ObjectDefineProperty(HelperProto, Symbol.toStringTag, {
    value: "Iterator Helper",
    writable: false,
    enumerable: false,
    configurable: true,
  });
  const nativeHelperState = new WeakMap();
  const guardedNativeCall = (receiver, method, returning) => {
    const state = WeakMapPrototypeGet(nativeHelperState, receiver);
    if (!state || state.completed) return ReflectApply(method, receiver, []);
    if (state.executing) throw new TypeError("Iterator Helper is already executing");
    // Suspended-start return is completed BEFORE closing its inputs. Reentrant
    // calls then return done; suspended-yield cleanup instead stays executing.
    if (returning && !state.started) {
      state.completed = true;
      return ReflectApply(method, receiver, []);
    }
    state.started = true;
    state.executing = true;
    try {
      const result = ReflectApply(method, receiver, []);
      if (result.done) state.completed = true;
      return result;
    } catch (error) {
      state.completed = true;
      throw error;
    } finally { state.executing = false; }
  };
  const makeHelper = (rec, body) => {
    const h = ObjectCreate(HelperProto);
    WeakMapPrototypeSet(helperState, h, { gen: body(), rec, started: false });
    if (typeof nativeMap !== "function") return h;
    const native = ReflectApply(nativeMap, h, [(value) => value]);
    const next = native.next;
    const close = native.return;
    WeakMapPrototypeSet(nativeHelperState, native, { started: false, completed: false, executing: false });
    // Older V8 map helpers mark themselves completed before closing their input.
    // Keep the executing guard around that cleanup too, without changing a native
    // prototype. Borrowing the host's intrinsic methods directly retains its host
    // semantics; the new helper's ordinary methods enforce the current spec.
    defBuiltin(native, "next", { next() { return guardedNativeCall(this, next, false); } }.next);
    defBuiltin(native, "return", { return() { return guardedNativeCall(this, close, true); } }.return);
    return native;
  };

  // take/drop limit: ToNumber, NaN is a RangeError, then ToIntegerOrInfinity and
  // negative is a RangeError — which is where a naive `Number(x) | 0` diverges.
  // Node 27's spec rejects finite limits above 2^53-1. Existing native helpers
  // are left alone, including older versions with different limit semantics.
  const toLimit = (v) => {
    const n = +v;
    if (NumberIsNaN(n)) throw new RangeError("limit must not be NaN");
    const i = n === Infinity ? Infinity : MathTrunc(n) || 0;
    if (i < 0 || (i !== Infinity && i > Number.MAX_SAFE_INTEGER)) {
      throw new RangeError("limit must be in 0..2^53 - 1 or Infinity");
    }
    return i;
  };
  const requireFn = (f, what) => {
    if (typeof f !== "function") throw new TypeError(`${what} is not a function`);
  };
  // chunks/windows size: an integral Number with NO coercion, in 1..2^32-1.
  const requireSize = (v, what) => {
    if (!NumberIsInteger(v)) throw new TypeError(`${what} must be an integral Number`);
    if (v < 1 || v > 2 ** 32 - 1) throw new RangeError(`${what} must be between 1 and 2^32 - 1`);
    return v;
  };

  const protoHelpers = {
    map(mapper) {
      const O = iterOf(this);
      validate(O, () => requireFn(mapper, "mapper"));
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        let i = 0;
        for (const v of asIterable(rec)) yield mapper(v, i++);
      });
    },
    filter(predicate) {
      const O = iterOf(this);
      validate(O, () => requireFn(predicate, "predicate"));
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        let i = 0;
        for (const v of asIterable(rec)) if (predicate(v, i++)) yield v;
      });
    },
    take(limit) {
      const O = iterOf(this);
      const n = validate(O, () => toLimit(limit));
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        // A spent limit closes the source on the NEXT pull, never eagerly.
        if (n === 0) return closeIterator(rec.iterator);
        let left = n;
        for (const v of asIterable(rec)) {
          yield v;
          if (--left === 0) return;
        }
      });
    },
    drop(limit) {
      const O = iterOf(this);
      const n = validate(O, () => toLimit(limit));
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        // IteratorStep, not IteratorStepValue: a skipped value is never read. A
        // throwing `next` or `done` leaves the source done, so nothing closes it here.
        for (let left = n; left > 0; left--) {
          const r = ReflectApply(rec.next, rec.iterator, []);
          if (!isObject(r)) throw new TypeError("iterator result is not an object");
          if (r.done) return;
        }
        for (const v of asIterable(rec)) yield v;
      });
    },
    flatMap(mapper) {
      const O = iterOf(this);
      validate(O, () => requireFn(mapper, "mapper"));
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        let i = 0;
        for (const v of asIterable(rec)) {
          // GetIteratorFlattenable(reject-primitives): a primitive — a string
          // included — is a TypeError, and an object with no Symbol.iterator is
          // taken as an iterator itself.
          const mapped = mapper(v, i++);
          if (!isObject(mapped)) throw new TypeError("flatMap mapper did not return an object");
          const method = mapped[Symbol.iterator];
          let inner = mapped;
          if (method !== undefined && method !== null) {
            inner = ReflectApply(method, mapped, []);
            if (!isObject(inner)) throw new TypeError("Symbol.iterator did not return an object");
          }
          for (const x of asIterable(getIteratorDirect(inner))) yield x;
        }
      });
    },
    reduce(reducer) {
      const O = iterOf(this);
      validate(O, () => requireFn(reducer, "reducer"));
      const it = drain(getIteratorDirect(O));
      let acc;
      let i = 0;
      if (arguments.length > 1) {
        acc = arguments[1];
      } else {
        const first = it.next();
        if (first.done) {
          throw new TypeError("reduce of empty iterator with no initial value");
        }
        acc = first.value;
        i = 1;
      }
      for (const v of it) acc = reducer(acc, v, i++);
      return acc;
    },
    toArray() {
      const rec = getIteratorDirect(iterOf(this));
      const out = [];
      for (const v of asIterable(rec)) ArrayPrototypePush(out, v);
      return out;
    },
    some(predicate) {
      const O = iterOf(this);
      validate(O, () => requireFn(predicate, "predicate"));
      let i = 0;
      for (const v of asIterable(getIteratorDirect(O))) if (predicate(v, i++)) return true;
      return false;
    },
    every(predicate) {
      const O = iterOf(this);
      validate(O, () => requireFn(predicate, "predicate"));
      let i = 0;
      for (const v of asIterable(getIteratorDirect(O))) if (!predicate(v, i++)) return false;
      return true;
    },
    find(predicate) {
      const O = iterOf(this);
      validate(O, () => requireFn(predicate, "predicate"));
      let i = 0;
      for (const v of asIterable(getIteratorDirect(O))) if (predicate(v, i++)) return v;
      return undefined;
    },
    forEach(fn) {
      const O = iterOf(this);
      validate(O, () => requireFn(fn, "fn"));
      let i = 0;
      for (const v of asIterable(getIteratorDirect(O))) fn(v, i++);
      return undefined;
    },
    // ── Stage 4 (2026-09), in no Node release yet ──
    // iterator-chunking: chunks() partitions into non-overlapping arrays of n and
    // yields a SHORT final chunk; windows() slides by one and, by default, yields
    // nothing when the source is shorter than n ("allow-partial" yields the short
    // buffer instead). Unlike take/drop, the size is not coerced.
    chunks(chunkSize) {
      const O = iterOf(this);
      const n = validate(O, () => requireSize(chunkSize, "chunkSize"));
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        let buf = [];
        for (const v of asIterable(rec)) {
          ArrayPrototypePush(buf, v);
          if (buf.length === n) {
            yield buf;
            buf = [];
          }
        }
        if (buf.length > 0) yield buf;
      });
    },
    windows(windowSize) {
      const O = iterOf(this);
      const undersized = arguments.length > 1 ? arguments[1] : undefined;
      const n = validate(O, () => {
        requireSize(windowSize, "windowSize");
        if (undersized !== undefined && undersized !== "only-full" && undersized !== "allow-partial") {
          throw new TypeError('undersized must be "only-full" or "allow-partial"');
        }
        return windowSize;
      });
      const rec = getIteratorDirect(O);
      return makeHelper(rec, function* () {
        const buf = [];
        for (const v of asIterable(rec)) {
          ArrayPrototypePush(buf, v);
          if (buf.length > n) ArrayPrototypeShift(buf);
          if (buf.length === n) yield ArrayPrototypeSlice(buf);
        }
        if (undersized === "allow-partial" && buf.length > 0 && buf.length < n) yield buf;
      });
    },
    // iterator-includes: SameValueZero, so NaN is found and -0 matches +0. The
    // optional skip count is not coerced; +Infinity skips everything.
    includes(searchElement) {
      const O = iterOf(this);
      const skippedElements = arguments.length > 1 ? arguments[1] : undefined;
      const toSkip = validate(O, () => {
        if (skippedElements === undefined) return 0;
        if (
          typeof skippedElements !== "number" ||
          !(NumberIsInteger(skippedElements) || skippedElements === Infinity || skippedElements === -Infinity)
        ) {
          throw new TypeError("skippedElements must be an integral Number or ±Infinity");
        }
        if (skippedElements < 0) throw new RangeError("skippedElements must not be negative");
        if (skippedElements !== Infinity && skippedElements > Number.MAX_SAFE_INTEGER) {
          throw new RangeError("skippedElements must not exceed 2^53 - 1");
        }
        return skippedElements;
      });
      let skipped = 0;
      for (const v of asIterable(getIteratorDirect(O))) {
        if (skipped < toSkip) skipped++;
        else if (v === searchElement || (v !== v && searchElement !== searchElement)) return true;
      }
      return false;
    },
    // iterator-join: like Array.prototype.join — default separator ",", and
    // null/undefined elements become the empty string. ToString (a template
    // literal), not String(), so a Symbol separator or element is a TypeError.
    join(separator) {
      const O = iterOf(this);
      const sep = separator === undefined ? "," : validate(O, () => `${separator}`);
      let out = "";
      let first = true;
      for (const v of asIterable(getIteratorDirect(O))) {
        if (!first) out += sep;
        first = false;
        if (v !== null && v !== undefined) out += `${v}`;
      }
      return out;
    },
  };
  // Literal guards are the feature-matrix anchors. chunks/windows remain
  // polyfilled on Node 27, while includes/join are native there.
  if (typeof Iterator.prototype.chunks !== "function") {
    defBuiltin(IteratorProto, "chunks", protoHelpers.chunks);
  }
  if (typeof Iterator.prototype.includes !== "function") {
    defBuiltin(IteratorProto, "includes", protoHelpers.includes);
  }
  if (typeof Iterator.prototype.join !== "function") {
    defBuiltin(IteratorProto, "join", protoHelpers.join);
  }
  for (const name of ObjectKeys(protoHelpers)) {
    if (!["chunks", "includes", "join"].includes(name) && typeof IteratorProto[name] !== "function") {
      defBuiltin(IteratorProto, name, protoHelpers[name]);
    }
  }

  // Iterator.from: wraps an iterable OR a bare iterator-like object. An object that
  // already inherits from %IteratorPrototype% is returned AS IS.
  if (typeof Iterator.from !== "function") {
    defBuiltin(Iterator, "from", {
      from(O) {
        if (typeof O === "string") {
          return (function* () {
            yield* O;
          })();
        }
        iterOf(O);
        const method = O[Symbol.iterator];
        let rec;
        if (typeof method === "function") {
          rec = ReflectApply(method, O, []);
          if (IteratorProto.isPrototypeOf(rec)) return rec;
        } else {
          if (typeof O.next !== "function") throw new TypeError("not iterable and has no next");
          if (IteratorProto.isPrototypeOf(O)) return O;
          rec = O;
        }
        const direct = getIteratorDirect(rec);
        return (function* () {
          yield* asIterable(direct);
        })();
      },
    }.from);
  }

  // Iterator.concat (Stage 4, native only on Node 26+): each argument must be an
  // iterable OBJECT — a string is a TypeError — and its Symbol.iterator method is
  // read up front but called LAZILY, once the previous source is exhausted. Sources
  // are stepped like flatMap's inner iterators, with a bare `next()`: `yield*` would
  // forward the consumer's arguments.
  if (typeof Iterator.concat !== "function") {
    defBuiltin(Iterator, "concat", {
      concat(...items) {
        const sources = [];
        for (let i = 0; i < items.length; i++) {
          const item = items[i];
          if (!isObject(item)) throw new TypeError("Iterator.concat arguments must be iterable objects");
          const method = item[Symbol.iterator];
          if (typeof method !== "function") throw new TypeError("Iterator.concat arguments must be iterable objects");
          ArrayPrototypePush(sources, { item, method });
        }
        // Nothing is open before the first `next()`, so an early `return()` closes nothing.
        return makeHelper({ iterator: {} }, function* () {
          for (let i = 0; i < sources.length; i++) {
            const inner = ReflectApply(sources[i].method, sources[i].item, []);
            if (!isObject(inner)) throw new TypeError("Symbol.iterator did not return an object");
            for (const v of asIterable(getIteratorDirect(inner))) yield v;
          }
        });
      },
    }.concat);
  }

  // Joint iteration (native on Node 27+). Capture each next method once and
  // close all still-open inputs, in reverse order, on early return or failure.
  const closeAll = (records, throwing = false, error) => {
    for (let i = records.length - 1; i >= 0; i--) {
      const rec = records[i];
      if (rec.done) continue;
      rec.done = true;
      try { closeIterator(rec.iterator); } catch (e) {
        if (!throwing) { throwing = true; error = e; }
      }
    }
    if (throwing) throw error;
  };
  const zipOptions = (options) => {
    if (options === undefined) options = ObjectCreate(null);
    iterOf(options);
    const suppliedMode = options.mode;
    const mode = suppliedMode === undefined ? "shortest" : suppliedMode;
    if (mode !== "shortest" && mode !== "longest" && mode !== "strict") {
      throw new TypeError('mode must be "shortest", "longest", or "strict"');
    }
    const padding = mode === "longest" ? options.padding : undefined;
    if (padding !== undefined) iterOf(padding);
    return { mode, padding };
  };
  const zipRecord = (value, flattenable = false) => {
    iterOf(value);
    const method = value[Symbol.iterator];
    const iterator = flattenable && (method === undefined || method === null)
      ? value : ReflectApply(method, value, []);
    iterOf(iterator);
    return { ...getIteratorDirect(iterator), done: false };
  };
  const zipStep = (rec, readValue = true) => {
    try {
      const result = ReflectApply(rec.next, rec.iterator, []);
      iterOf(result);
      if (result.done) { rec.done = true; return { done: true }; }
      return { done: false, value: readValue ? result.value : undefined };
    } catch (e) {
      // An iterator that throws from next/done/value is already considered done.
      rec.done = true;
      throw e;
    }
  };
  const zipHelper = (records, mode, padding, keys) => {
    // makeHelper forwards return() even before the first next(). Its synthetic
    // underlying iterator closes every input instead of just one.
    const closer = { iterator: { return() { closeAll(records); return {}; } } };
    return makeHelper(closer, function* () {
      try {
        if (records.length === 0) return;
        while (true) {
          const row = [];
          for (let i = 0; i < records.length; i++) {
            const rec = records[i];
            if (rec.done) { ArrayPrototypePush(row, padding[i]); continue; }
            const result = zipStep(rec);
            if (result.done) {
              if (mode === "shortest") return;
              if (mode === "strict") {
                if (i !== 0) throw new TypeError("zip inputs have different lengths");
                for (let j = 1; j < records.length; j++) {
                  if (!zipStep(records[j], false).done) {
                    throw new TypeError("zip inputs have different lengths");
                  }
                }
                return;
              }
              if (ArrayPrototypeEvery(records, (r) => r.done)) return;
              ArrayPrototypePush(row, padding[i]);
            } else {
              ArrayPrototypePush(row, result.value);
            }
          }
          if (keys === undefined) yield row;
          else {
            const result = ObjectCreate(null);
            for (let i = 0; i < keys.length; i++) result[keys[i]] = row[i];
            yield result;
          }
        }
      } catch (e) {
        closeAll(records, true, e);
      } finally {
        closeAll(records);
      }
    });
  };
  if (typeof Iterator.zip !== "function") {
    defBuiltin(Iterator, "zip", { zip(iterables) {
      iterOf(iterables);
      const { mode, padding } = zipOptions(arguments[1]);
      const records = [];
      const pads = [];
      const input = zipRecord(iterables);
      try {
        while (true) {
          const result = zipStep(input);
          if (result.done) break;
          try { ArrayPrototypePush(records, zipRecord(result.value, true)); }
          catch (e) { closeAll([input, ...records], true, e); }
        }
        if (padding !== undefined) {
          const padIter = zipRecord(padding);
          for (let i = 0; i < records.length; i++) {
            ArrayPrototypePush(pads, padIter.done ? undefined : zipStep(padIter).value);
          }
          closeAll([padIter]);
        }
      } catch (e) { closeAll(records, true, e); }
      return zipHelper(records, mode, pads);
    }}.zip);
  }
  if (typeof Iterator.zipKeyed !== "function") {
    defBuiltin(Iterator, "zipKeyed", { zipKeyed(iterables) {
      iterOf(iterables);
      const { mode, padding } = zipOptions(arguments[1]);
      const keys = [];
      const records = [];
      const pads = [];
      try {
        const ownKeys = ReflectOwnKeys(iterables);
        for (let k = 0; k < ownKeys.length; k++) {
          const key = ownKeys[k];
          const desc = ReflectGetOwnPropertyDescriptor(iterables, key);
          if (desc === undefined || !desc.enumerable) continue;
          const value = iterables[key];
          if (value === undefined) continue;
          ArrayPrototypePush(keys, key);
          ArrayPrototypePush(records, zipRecord(value, true));
        }
        if (padding !== undefined) {
          for (let k = 0; k < keys.length; k++) ArrayPrototypePush(pads, padding[keys[k]]);
        }
      } catch (e) { closeAll(records, true, e); }
      return zipHelper(records, mode, pads, keys);
    }}.zipKeyed);
  }
}

// ── Symbol.metadata (TC39 Stage 3, decorator metadata; in no engine) ──
// Only the well-known symbol is provided. POPULATING `klass[Symbol.metadata]` is
// the decorator transform's job, and the spec's own answer for an undecorated class
// is `undefined` — so defining the symbol makes `Symbol.metadata` referenceable
// without inventing metadata that isn't there.
function installSymbolMetadata() {
  if (typeof Symbol.metadata === "symbol") return;
  ObjectDefineProperty(Symbol, "metadata", {
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
  ObjectDefineProperty(target, name, {
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
  const sizeGetter = ObjectGetOwnPropertyDescriptor(Set.prototype, "size").get;
  const requireSet = (o) => {
    ReflectApply(sizeGetter, o, []);
  };
  // GetSetRecord: the argument-validation order is observable, so it is preserved
  // exactly — size read and coerced first (NaN is a TypeError, negative a
  // RangeError), then `has`, then `keys`.
  const setRecord = (obj) => {
    if (obj === null || (typeof obj !== "object" && typeof obj !== "function")) {
      throw new TypeError("argument is not an object");
    }
    const numSize = Number(obj.size);
    if (NumberIsNaN(numSize)) throw new TypeError("size is NaN");
    const intSize = MathTrunc(numSize) || 0;
    if (intSize < 0) throw new RangeError("size is negative");
    const has = obj.has;
    if (typeof has !== "function") throw new TypeError("has is not callable");
    const keys = obj.keys;
    if (typeof keys !== "function") throw new TypeError("keys is not callable");
    return { obj, size: intSize, has, keys };
  };
  // -0 is normalized to +0 on the way into a result set, matching SameValueZero.
  const norm = (v) => (v === 0 ? 0 : v);
  // GetIteratorFromMethod: `keys()` must return an object, which is then stepped
  // through the `next` read once here. It is never asked for Symbol.iterator, so a
  // bare `{ next }` iterator is valid and a string result is a TypeError. `return`
  // is read only when a method stops early, as IteratorClose does.
  const otherKeys = (rec) => {
    const iterator = ReflectApply(rec.keys, rec.obj, []);
    if (iterator === null || (typeof iterator !== "object" && typeof iterator !== "function")) {
      throw new TypeError("keys() did not return an object");
    }
    const next = iterator.next;
    return {
      [Symbol.iterator]() { return this; },
      next() { return ReflectApply(next, iterator, []); },
      return() {
        const close = iterator.return;
        return close === undefined || close === null ? {} : ReflectApply(close, iterator, []);
      },
    };
  };

  const ownValues = (set) => {
    const iterator = SetPrototypeValues(set);
    return {
      [Symbol.iterator]() { return this; },
      next() { return SetIteratorPrototypeNext(iterator); },
    };
  };
  const copySet = (set) => {
    const result = new Set();
    for (const value of ownValues(set)) SetPrototypeAdd(result, value);
    return result;
  };
  const methods = {
    union(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = copySet(this);
      for (const k of otherKeys(rec)) SetPrototypeAdd(out, norm(k));
      return out;
    },
    intersection(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = new Set();
      // Iterate the SMALLER side, but membership is always tested against the
      // other — the spec picks by size for complexity, and the observable
      // difference is which side's iteration order the result follows.
      if (ReflectApply(sizeGetter, this, []) <= rec.size) {
        for (const k of ownValues(this)) if (ReflectApply(rec.has, rec.obj, [k])) SetPrototypeAdd(out, k);
      } else {
        for (const k of otherKeys(rec)) if (SetPrototypeHas(this, k)) SetPrototypeAdd(out, norm(k));
      }
      return out;
    },
    difference(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = copySet(this);
      if (ReflectApply(sizeGetter, this, []) <= rec.size) {
        for (const k of ownValues(this)) if (ReflectApply(rec.has, rec.obj, [k])) SetPrototypeDelete(out, k);
      } else {
        for (const k of otherKeys(rec)) SetPrototypeDelete(out, norm(k));
      }
      return out;
    },
    symmetricDifference(other) {
      requireSet(this);
      const rec = setRecord(other);
      const out = copySet(this);
      for (const k of otherKeys(rec)) {
        const key = norm(k);
        if (SetPrototypeHas(this, key)) SetPrototypeDelete(out, key);
        else SetPrototypeAdd(out, key);
      }
      return out;
    },
    isSubsetOf(other) {
      requireSet(this);
      const rec = setRecord(other);
      if (ReflectApply(sizeGetter, this, []) > rec.size) return false;
      for (const k of ownValues(this)) if (!ReflectApply(rec.has, rec.obj, [k])) return false;
      return true;
    },
    isSupersetOf(other) {
      requireSet(this);
      const rec = setRecord(other);
      if (ReflectApply(sizeGetter, this, []) < rec.size) return false;
      for (const k of otherKeys(rec)) if (!SetPrototypeHas(this, norm(k))) return false;
      return true;
    },
    isDisjointFrom(other) {
      requireSet(this);
      const rec = setRecord(other);
      if (ReflectApply(sizeGetter, this, []) <= rec.size) {
        for (const k of ownValues(this)) if (ReflectApply(rec.has, rec.obj, [k])) return false;
      } else {
        for (const k of otherKeys(rec)) if (SetPrototypeHas(this, norm(k))) return false;
      }
      return true;
    },
  };
  // `union` is written out literally as the feature-matrix row's detect anchor; the
  // rest loop, and every method is still guarded on its own so a runtime shipping
  // only part of the family keeps what it has.
  if (typeof Set.prototype.union !== "function") defBuiltin(Set.prototype, "union", methods.union);
  for (const name of ObjectKeys(methods)) {
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
        out[i] = mapfn === undefined ? v : await ReflectApply(mapfn, thisArg, [v, i]);
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
    const len = MathMin(MathMax(MathTrunc(Number(arrayLike.length)) || 0, 0), 2 ** 53 - 1);
    const out = new C(len);
    for (let i = 0; i < len; i++) {
      const v = await arrayLike[i];
      out[i] = mapfn === undefined ? v : await ReflectApply(mapfn, thisArg, [v, i]);
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
    const has = Ctor === Map ? MapPrototypeHas : WeakMapPrototypeHas;
    const get = Ctor === Map ? MapPrototypeGet : WeakMapPrototypeGet;
    const set = Ctor === Map ? MapPrototypeSet : WeakMapPrototypeSet;
    if (Ctor === Map && typeof Map.prototype.getOrInsert === "function"
        && typeof Map.prototype.getOrInsertComputed === "function") {
      continue;
    }
    if (typeof Ctor.prototype.getOrInsert !== "function") {
      defBuiltin(Ctor.prototype, "getOrInsert", {
        getOrInsert(key, value) {
          if (has(this, key)) return get(this, key);
          set(this, key, value);
          return value;
        },
      }.getOrInsert);
    }
    if (typeof Ctor.prototype.getOrInsertComputed !== "function") {
      defBuiltin(
        Ctor.prototype,
        "getOrInsertComputed",
        {
          getOrInsertComputed(key, callbackfn) {
            if (typeof callbackfn !== "function") {
              throw new TypeError("callbackfn is not a function");
            }
            if (has(this, key)) return get(this, key);
            const value = callbackfn(key);
            // The callback may have inserted `key` itself; the spec's final write
            // wins, so set unconditionally rather than re-checking `has`.
            set(this, key, value);
            return value;
          },
        }.getOrInsertComputed,
      );
    }
  }
}

// ── Intl.Locale.prototype.variants and the `variants` option (native on Node 27+) ──
function installLocaleVariants() {
  // A Node built without ICU has no Intl at all.
  if (typeof Intl !== "object" || typeof Intl.Locale !== "function") return;
  if ("variants" in Intl.Locale.prototype) return;
  const NativeLocale = Intl.Locale;
  const proto = NativeLocale.prototype;
  // The intrinsic getter checks [[InitializedLocale]] even across realms and
  // ignores user overrides of baseName/toString. Its result omits extensions.
  const baseName = ObjectGetOwnPropertyDescriptor(proto, "baseName").get;
  const localeToString = proto.toString;
  const isVariant = (part) => RegExpPrototypeExec(/^(?:[a-z0-9]{5,8}|[0-9][a-z0-9]{3})$/i, part) !== null;
  const getter = {
    get variants() {
      const parts = StringPrototypeSplit(ReflectApply(baseName, this, []), "-");
      let result;
      for (let i = 1; i < parts.length; i++) {
        if (isVariant(parts[i])) result = result === undefined ? parts[i] : `${result}-${parts[i]}`;
      }
      return result;
    },
  };
  ObjectDefineProperty(proto, "variants", {
    get: ObjectGetOwnPropertyDescriptor(getter, "variants").get,
    enumerable: false,
    configurable: true,
  });
  // Without this wrapper the option is silently ignored. UpdateLanguageId reads it
  // between region and calendar, so the proxy reads it there and hides it from the
  // engine. The tag is then rebuilt, because the engine sorts variants and rejects
  // duplicates only when parsing a tag.
  const variantList = /^(?:[a-z0-9]{5,8}|[0-9][a-z0-9]{3})(?:-(?:[a-z0-9]{5,8}|[0-9][a-z0-9]{3}))*$/i;
  const Locale = new Proxy(NativeLocale, {
    __proto__: null,
    construct(target, args, newTarget) {
      const tag = args[0];
      const options = args[1];
      if (options === undefined || options === null) {
        return ReflectConstruct(target, args, newTarget);
      }
      let variants;
      const hooked = new Proxy(Object(options), {
        __proto__: null,
        get(target, key) {
          if (key === "variants") return undefined;
          if (key === "calendar") {
            const value = target.variants;
            if (value !== undefined) {
              variants = `${value}`;
              if (RegExpPrototypeExec(variantList, variants) === null) {
                throw new RangeError(`Invalid variants: ${variants}`);
              }
              const parts = StringPrototypeSplit(StringPrototypeToLowerCase(variants), "-");
              const seen = new Set();
              for (let i = 0; i < parts.length; i++) {
                const part = parts[i];
                if (SetPrototypeHas(seen, part)) throw new RangeError(`Duplicate variants: ${variants}`);
                SetPrototypeAdd(seen, part);
              }
            }
          }
          return ReflectGet(target, key);
        }
      });
      const locale = ReflectConstruct(target, [tag, hooked], newTarget);
      if (variants === undefined) return locale;

      const text = ReflectApply(localeToString, locale, []);
      const base = ReflectApply(baseName, locale, []);
      const parts = StringPrototypeSplit(base, "-");
      let id = parts[0];
      for (let i = 1; i < parts.length; i++) {
        if (!isVariant(parts[i])) id = `${id}-${parts[i]}`;
      }
      const selectedPrototype = ObjectGetPrototypeOf(locale);
      const surrogate = function LocaleWithVariants() {};
      ObjectDefineProperty(surrogate, "prototype", { value: selectedPrototype });
      return ReflectConstruct(target, [`${id}-${StringPrototypeToLowerCase(variants)}${StringPrototypeSlice(text, base.length)}`], surrogate);
    },
  });
  ObjectDefineProperty(proto, "constructor", { value: Locale });
  ObjectDefineProperty(Intl, "Locale", { value: Locale });
}

// ── Math.sumPrecise (native on Node 27+) ──
function installMathSumPrecise() {
  if (typeof Math.sumPrecise === "function") return;
  defBuiltin(Math, "sumPrecise", { sumPrecise(items) {
    // Every finite double is an integer multiple of 2^-1074. Accumulate those
    // integers exactly, then round once. Floating-point partial sums can lose
    // subnormals when an intermediate overflow forces them to be rescaled.
    const bits = new DataView(new ArrayBuffer(8));
    let total = 0n;
    let allMinusZero = true;
    let hasNaN = false;
    let posInf = false;
    let negInf = false;
    for (const x of items) {
      if (typeof x !== "number") throw new TypeError("Math.sumPrecise accepts only Numbers");
      if (!ObjectIs(x, -0)) allMinusZero = false;
      if (NumberIsNaN(x)) { hasNaN = true; continue; }
      if (x === Infinity) { posInf = true; continue; }
      if (x === -Infinity) { negInf = true; continue; }
      if (hasNaN || posInf || negInf || x === 0) continue;
      DataViewPrototypeSetFloat64(bits, 0, x);
      const hi = DataViewPrototypeGetUint32(bits, 0);
      const exponent = (hi >>> 20) & 0x7ff;
      let significand = (BigInt(hi & 0xfffff) << 32n) | BigInt(DataViewPrototypeGetUint32(bits, 4));
      if (exponent !== 0) significand = (significand | (1n << 52n)) << BigInt(exponent - 1);
      total += hi >>> 31 ? -significand : significand;
    }
    if (hasNaN || (posInf && negInf)) return NaN;
    if (posInf) return Infinity;
    if (negInf) return -Infinity;
    if (total === 0n) return allMinusZero ? -0 : 0;
    const negative = total < 0n;
    if (negative) total = -total;
    const shift = MathMax(0, BigIntPrototypeToString(total, 2).length - 53);
    let rounded = total >> BigInt(shift);
    if (shift > 0) {
      const remainder = total - (rounded << BigInt(shift));
      const halfway = 1n << BigInt(shift - 1);
      if (remainder > halfway || (remainder === halfway && (rounded & 1n))) rounded++;
    }
    // The exponent is >= -1074, so this scale is representable. A carry at the
    // upper boundary correctly produces Infinity on the final multiplication.
    const value = Number(rounded) * 2 ** (shift - 1074);
    return negative ? -value : value;
  }}.sumPrecise);
}

// ── JSON.parse options (TC39 Stage 2.7, 2026-09; proposal-json-parseimmutable,
//    shipped by no engine) ──
// `JSON.parse(text, { freeze, preferNullPrototype })`. The spec text, not the
// README, is the source: the option is `preferNullPrototype` (the README's
// `nullPrototype` is stale), neither option is coerced (a non-Boolean is a
// TypeError), and `preferNullPrototype` defaults to the value of `freeze`. A
// callable second argument is a reviver exactly as before, and an options object
// never reaches the reviver path, so the two never combine.
//
// JSON.parse exists everywhere, so the detect PROBES behavior: a native
// implementation returns a frozen, null-prototype object for `{ freeze: true }`.
//
// Implementation: the engine parses, then one iterative walk over the result sets
// the prototype and freezes. Every object in the tree was created by that parse
// and is unaliased, so no user code can observe it before the walk finishes, which
// makes this equivalent to the spec's per-object freeze at creation. The walk
// keeps an explicit stack because V8's parser accepts nesting far deeper than the
// call stack. Arrays keep Array.prototype, as in the spec. The one fidelity cost
// of wrapping a builtin: an error thrown through it carries one extra stack frame.
function installJsonParseOptions() {
  const nativeParse = JSON.parse;
  if (typeof JSON.parse === "function") {
    // A parser installed before this preload (Yarn PnP's `.pnp.cjs` loads first)
    // may reject a non-callable second argument, so a throw reads as "no support"
    // rather than aborting startup.
    let probe;
    try {
      probe = nativeParse("{}", { freeze: true });
    } catch {}
    if (typeof probe === "object" && probe !== null && ObjectIsFrozen(probe)
        && ObjectGetPrototypeOf(probe) === null) {
      return;
    }
  }
  const { freeze, keys, setPrototypeOf } = Object;
  const { isArray } = Array;
  const optionsError = (name) =>
    new TypeError(`JSON.parse option "${name}" must be a boolean`);
  // Method syntax so the replacement stays a non-constructor, like the builtin.
  const { parse } = {
    parse(text, reviverOrOptions) {
      if (typeof reviverOrOptions !== "object" || reviverOrOptions === null) {
        return nativeParse(text, reviverOrOptions);
      }
      // Spec order: ToString(text), then the two option reads, then the parse.
      const jsonString = `${text}`;
      let freezeOpt = reviverOrOptions.freeze;
      if (freezeOpt === undefined) freezeOpt = false;
      if (typeof freezeOpt !== "boolean") throw optionsError("freeze");
      let nullProto = reviverOrOptions.preferNullPrototype;
      if (nullProto === undefined) nullProto = freezeOpt;
      if (typeof nullProto !== "boolean") throw optionsError("preferNullPrototype");
      const result = nativeParse(jsonString);
      if ((!freezeOpt && !nullProto) || typeof result !== "object" || result === null) {
        return result;
      }
      // Indexed with a null prototype so no Array.prototype method or indexed
      // accessor that user code installs after the preload can reach the walk.
      const stack = setPrototypeOf([result], null);
      let top = 1;
      while (top > 0) {
        const node = stack[--top];
        if (isArray(node)) {
          for (let i = 0; i < node.length; i++) {
            const v = node[i];
            if (typeof v === "object" && v !== null) stack[top++] = v;
          }
        } else {
          const ks = keys(node);
          for (let i = 0; i < ks.length; i++) {
            const v = node[ks[i]];
            if (typeof v === "object" && v !== null) stack[top++] = v;
          }
          if (nullProto) setPrototypeOf(node, null);
        }
        if (freezeOpt) freeze(node);
      }
      return result;
    },
  };
  defBuiltin(JSON, "parse", parse);
}

// ── Shipped-standard ECMAScript builtins missing below their Node line ──
// Every one of these is Stage 4 and native on a
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
    ObjectDefineProperty(target, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  // ── URL.parse (native Node 22.1 and 20.19; absent on all of 21.x) ──
  // Only URL parsing failures become null. WebIDL string conversion happens
  // first, so user-defined coercion errors (including Symbols) still propagate.
  if (typeof URL.parse !== "function") {
    // `base` is read from `arguments` rather than declared: WebIDL counts only
    // REQUIRED arguments toward `length`, so native URL.parse.length is 1.
    def(URL, "parse", {
      parse(url) {
        if (arguments.length === 0) throw new TypeError("URL.parse requires a URL argument");
        const input = `${url}`;
        const base = arguments.length > 1 && arguments[1] !== undefined ? `${arguments[1]}` : undefined;
        try {
          return base === undefined ? new URL(input) : new URL(input, base);
        } catch {
          return null;
        }
      },
    }.parse);
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
    def(String.prototype, "isWellFormed", {
      isWellFormed() {
        return !loneSurrogate.test(thisStr(this));
      },
    }.isWellFormed);
    def(String.prototype, "toWellFormed", {
      toWellFormed() {
        // Replace each lone surrogate with U+FFFD. The `u` flag makes a well-formed
        // pair a single match unit, so only unpaired units are hit.
        return thisStr(this).replace(/[\uD800-\uDFFF]/gu, "�");
      },
    }.toWellFormed);
  }

  // ── Object.groupBy / Map.groupBy (native Node 21; absent 18.19–20.x) ──
  // The two differ in more than their container: Object.groupBy coerces each key
  // with ToPropertyKey and returns a NULL-PROTOTYPE object (so a "toString" group
  // can't collide with Object.prototype), while Map.groupBy keys on the raw value
  // under SameValueZero — which a Map already implements, including -0 → +0.
  if (typeof Object.groupBy !== "function") {
    def(Object, "groupBy", {
      groupBy(items, callback) {
        if (typeof callback !== "function") throw new TypeError("callback is not a function");
        const obj = ObjectCreate(null);
        let i = 0;
        for (const item of items) {
          const key = callback(item, i++);
          const k = typeof key === "symbol" ? key : String(key);
          if (obj[k] === undefined && !(k in obj)) obj[k] = [];
          ArrayPrototypePush(obj[k], item);
        }
        return obj;
      },
    }.groupBy);
  }
  if (typeof Map.groupBy !== "function") {
    def(Map, "groupBy", {
      groupBy(items, callback) {
        if (typeof callback !== "function") throw new TypeError("callback is not a function");
        const map = new Map();
        let i = 0;
        for (const item of items) {
          // Map.prototype.get/set already key under SameValueZero, so -0 collapses
          // to +0 exactly as the spec's normalization requires.
          const key = callback(item, i++);
          const group = MapPrototypeGet(map, key);
          if (group === undefined) MapPrototypeSet(map, key, [item]);
          else ArrayPrototypePush(group, item);
        }
        return map;
      },
    }.groupBy);
  }

  // ── Change-array-by-copy (native Node 20; absent 18.19) ──
  // Every method returns a plain Array regardless of the receiver's constructor —
  // deliberately NOT species-aware, unlike map/filter. The ArrayFrom(this) hop
  // both realizes an array-like receiver and gives the fresh array to mutate.
  if (typeof Array.prototype.toSorted !== "function") {
    def(Array.prototype, "toSorted", {
      toSorted(compareFn) {
        if (compareFn !== undefined && typeof compareFn !== "function") {
          throw new TypeError("comparefn must be a function");
        }
        return ArrayPrototypeSort(ArrayFrom(this), compareFn);
      },
    }.toSorted);
    def(Array.prototype, "toReversed", {
      toReversed() {
        return ArrayPrototypeReverse(ArrayFrom(this));
      },
    }.toReversed);
    def(Array.prototype, "toSpliced", {
      toSpliced(start, skipCount, ...items) {
        const arr = ArrayFrom(this);
        // An OMITTED deleteCount deletes through the end, but a declared parameter
        // forwards an explicit `undefined`, which splice coerces to 0. Branch on the
        // real argument count so `toSpliced(1)` truncates the way native does.
        if (arguments.length <= 1) ArrayPrototypeSplice(arr, start);
        else ArrayPrototypeSplice(arr, start, skipCount, ...items);
        return arr;
      },
    }.toSpliced);
    // `with` is a reserved word, so a named function EXPRESSION is a SyntaxError.
    // An object-method shorthand accepts the reserved name and infers `name` as
    // "with", which a `function with_` would have gotten wrong.
    def(Array.prototype, "with", {
      with(index, value) {
        const arr = ArrayFrom(this);
        const i = MathTrunc(Number(index)) || 0;
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
  const TypedArrayProto = ObjectGetPrototypeOf(Int8Array.prototype);
  if (typeof TypedArrayProto.toSorted !== "function") {
    def(TypedArrayProto, "toSorted", {
      toSorted(compareFn) {
        if (compareFn !== undefined && typeof compareFn !== "function") {
          throw new TypeError("comparefn must be a function");
        }
        const copy = new this.constructor(this);
        return TypedArrayPrototypeSort(copy, compareFn);
      },
    }.toSorted);
    def(TypedArrayProto, "toReversed", {
      toReversed() {
        return TypedArrayPrototypeReverse(new this.constructor(this));
      },
    }.toReversed);
    def(TypedArrayProto, "with", {
      with(index, value) {
        const copy = new this.constructor(this);
        const i = MathTrunc(Number(index)) || 0;
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
      const len = newLength === undefined ? oldLen : MathTrunc(Number(newLength)) || 0;
      if (len < 0) throw new RangeError("invalid length");
      // Copy BEFORE detaching; the copy is truncated or zero-padded to `len`.
      // `transfer` PRESERVES a resizable source's resizability while
      // `transferToFixedLength` always yields a fixed buffer — the two entry points
      // genuinely differ, verified against native (resizable=true vs false).
      const keepResizable = !fixedLength && buf.resizable === true;
      const out = keepResizable
        ? new ArrayBuffer(len, { maxByteLength: buf.maxByteLength })
        : new ArrayBuffer(len);
      TypedArrayPrototypeSet(new Uint8Array(out), new Uint8Array(buf, 0, MathMin(oldLen, len)));
      // Detach the source. The clone takes ownership of the original memory and
      // is immediately discarded, so this moves rather than copies.
      structuredClone(buf, { transfer: [buf] });
      return out;
    };
    // newLength is optional, so it rides `arguments`: native length is 0 for both.
    def(ArrayBuffer.prototype, "transfer", {
      transfer() {
        return transferImpl(this, arguments.length > 0 ? arguments[0] : undefined, false);
      },
    }.transfer);
    def(ArrayBuffer.prototype, "transferToFixedLength", {
      transferToFixedLength() {
        return transferImpl(this, arguments.length > 0 ? arguments[0] : undefined, true);
      },
    }.transferToFixedLength);
    ObjectDefineProperty(ArrayBuffer.prototype, "detached", {
      get: ObjectGetOwnPropertyDescriptor(
        {
          get detached() {
            return isDetached(this);
          },
        },
        "detached",
      ).get,
      enumerable: false,
      configurable: true,
    });
  }

  // ── Atomics.pause (TC39 Stage 4, proposal-atomics-microwait; native on Node 24+) ──
  // A pure micro-architectural HINT with no observable effect beyond argument
  // validation, so returning undefined is a fully faithful implementation — the
  // spec permits an implementation to do nothing. Validation is the observable
  // part: the optional argument must be an integral Number.
  if (typeof Atomics !== "undefined" && typeof Atomics.pause !== "function") {
    def(Atomics, "pause", {
      pause() {
        const iterationNumber = arguments.length > 0 ? arguments[0] : undefined;
        if (iterationNumber !== undefined) {
          if (typeof iterationNumber !== "number" || !NumberIsInteger(iterationNumber)) {
            throw new TypeError("Atomics.pause iterationNumber must be an integral Number");
          }
        }
        return undefined;
      },
    }.pause);
  }
}

// ── Uint8Array base64/hex (TC39 Stage 4; native Node 25+, absent below) ──
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
  for (let i = 0; i < B64.length; i++) DECODE[StringPrototypeCharCodeAt(B64, i)] = i;

  // %TypedArray%.prototype[@@toStringTag] getter — the brand check native uses: it
  // accepts a Uint8Array (and Buffer, a Uint8Array subclass) and rejects any other
  // TypedArray or non-typed-array with a TypeError.
  const tagGet = ObjectGetOwnPropertyDescriptor(
    ObjectGetPrototypeOf(Uint8Array.prototype),
    Symbol.toStringTag,
  ).get;
  const checkU8 = (arg) => {
    let kind;
    try {
      kind = ReflectApply(tagGet, arg, []);
    } catch {
      throw new TypeError("not a Uint8Array");
    }
    if (kind !== "Uint8Array") throw new TypeError("not a Uint8Array");
  };
  const getOptions = (options) => {
    if (typeof options === "undefined") return ObjectCreate(null);
    if (options && typeof options === "object") return options;
    throw new TypeError("options is not object");
  };
  const isDetached = (arr) => "detached" in arr.buffer && arr.buffer.detached;
  const isWs = (cc) =>
    cc === 0x09 || cc === 0x0a || cc === 0x0c || cc === 0x0d || cc === 0x20;
  const skipWs = (s, i) => {
    while (i < s.length && isWs(StringPrototypeCharCodeAt(s, i))) i++;
    return i;
  };

  // chunk is 2–4 standard-alphabet chars; pads to 4 then emits 1–3 bytes. In strict
  // mode the unused low bits of a 2/3-char chunk must be zero.
  const decodeChunk = (chunk, throwOnExtraBits) => {
    const n = chunk.length;
    const padded = n < 4 ? chunk + (n === 2 ? "AA" : "A") : chunk;
    const triplet =
      (DECODE[StringPrototypeCharCodeAt(padded, 0)] << 18) +
      (DECODE[StringPrototypeCharCodeAt(padded, 1)] << 12) +
      (DECODE[StringPrototypeCharCodeAt(padded, 2)] << 6) +
      DECODE[StringPrototypeCharCodeAt(padded, 3)];
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
            ArrayPrototypePush(bytes, ...decodeChunk(chunk, false));
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
        ArrayPrototypePush(bytes, ...decodeChunk(chunk, lastChunkHandling === "strict"));
        return { bytes, read: string.length, error: null };
      }
      if (alphabet === "base64url") {
        if (char === "+" || char === "/") {
          return { bytes, read, error: new SyntaxError("unexpected character " + JSONStringify(char)) };
        } else if (char === "-") {
          char = "+";
        } else if (char === "_") {
          char = "/";
        }
      }
      if (!StringPrototypeIncludes(B64, char)) {
        return { bytes, read, error: new SyntaxError("unexpected character " + JSONStringify(char)) };
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
        ArrayPrototypePush(bytes, ...decodeChunk(chunk, false));
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
    if (into && bytes.length > 0) TypedArrayPrototypeSet(into, bytes);
    if (error) throw error;
    return { read, bytes };
  };

  const u8ToHex = (arr) => {
    checkU8(arr);
    if (isDetached(arr)) {
      throw new TypeError("toHex called on array backed by detached buffer");
    }
    let out = "";
    for (let i = 0; i < arr.length; ++i) out += StringPrototypePadStart(NumberPrototypeToString(arr[i], 16), 2, "0");
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
        const hexits = StringPrototypeSlice(string, read, read + 2);
        if (RegExpPrototypeExec(/[^0-9a-fA-F]/, hexits)) {
          error = new SyntaxError("string should only contain hex characters");
          break;
        }
        ArrayPrototypePush(bytesArr, parseInt(hexits, 16));
        read += 2;
        if (bytesArr.length === maxLength) break;
      }
    }
    if (error && !into) throw error;
    const bytes = new Uint8Array(bytesArr);
    if (into && bytes.length > 0) TypedArrayPrototypeSet(into, bytes);
    if (error) throw error;
    return { read, bytes };
  };

  const def = (target, name, fn) => {
    ObjectDefineProperty(target, name, {
      value: fn,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };
  def(Uint8Array.prototype, "toBase64", {
    toBase64(options) {
      return u8ToBase64(this, options);
    },
  }.toBase64);
  def(Uint8Array, "fromBase64", {
    fromBase64(string, options) {
      return b64ToU8(string, options, undefined).bytes;
    },
  }.fromBase64);
  def(Uint8Array.prototype, "setFromBase64", {
    setFromBase64(string, options) {
      checkU8(this);
      const { read, bytes } = b64ToU8(string, options, this);
      return { read, written: bytes.length };
    },
  }.setFromBase64);
  def(Uint8Array.prototype, "toHex", {
    toHex() {
      return u8ToHex(this);
    },
  }.toHex);
  def(Uint8Array, "fromHex", {
    fromHex(string) {
      return hexToU8(string, undefined).bytes;
    },
  }.fromHex);
  def(Uint8Array.prototype, "setFromHex", {
    setFromHex(string) {
      checkU8(this);
      const { read, bytes } = hexToU8(string, this);
      return { read, written: bytes.length };
    },
  }.setFromHex);
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
    ObjectDefineProperty(Symbol, "dispose", { value: Symbol("Symbol.dispose") });
  }
  if (typeof Symbol.asyncDispose === "undefined") {
    ObjectDefineProperty(Symbol, "asyncDispose", { value: Symbol("Symbol.asyncDispose") });
  }

  const defGlobal = (name, value) => {
    ObjectDefineProperty(globalThis, name, {
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
        ObjectDefineProperty(this, "error", {
          value: error,
          writable: true,
          enumerable: false,
          configurable: true,
        });
        ObjectDefineProperty(this, "suppressed", {
          value: suppressed,
          writable: true,
          enumerable: false,
          configurable: true,
        });
      }
    }
    ObjectDefineProperty(SuppressedError.prototype, "name", {
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
          ArrayPrototypePush(this.#stack, () => ReflectApply(method, value, []));
        }
        return value;
      }
      adopt(value, onDispose) {
        if (this.#disposed) throw new ReferenceError("DisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        ArrayPrototypePush(this.#stack, () => onDispose(value));
        return value;
      }
      defer(onDispose) {
        if (this.#disposed) throw new ReferenceError("DisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        ArrayPrototypePush(this.#stack, () => onDispose());
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
    ObjectDefineProperty(DisposableStack.prototype, Symbol.dispose, {
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
            ArrayPrototypePush(this.#stack, () => ReflectApply(sync, value, []));
          } else {
            if (typeof method !== "function") {
              throw new TypeError("value is not async disposable");
            }
            ArrayPrototypePush(this.#stack, () => ReflectApply(method, value, []));
          }
        }
        return value;
      }
      adopt(value, onDispose) {
        if (this.#disposed) throw new ReferenceError("AsyncDisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        ArrayPrototypePush(this.#stack, () => onDispose(value));
        return value;
      }
      defer(onDispose) {
        if (this.#disposed) throw new ReferenceError("AsyncDisposableStack already disposed");
        if (typeof onDispose !== "function") throw new TypeError("onDispose is not callable");
        ArrayPrototypePush(this.#stack, () => onDispose());
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
    ObjectDefineProperty(AsyncDisposableStack.prototype, Symbol.asyncDispose, {
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
  ObjectDefineProperty(Promise, "withResolvers", {
    // Method syntax so it is a non-constructor with no `prototype`, like the builtin.
    value: {
      withResolvers() {
        const { promise, resolve, reject } = newPromiseCapability(this, "withResolvers");
        return { promise, resolve, reject };
      },
    }.withResolvers,
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
    const obj = ObjectCreate(null);
    for (let i = 0; i < entries.length; i++) obj[entries[i].key] = entries[i].value;
    return obj;
  };

  // PerformPromiseAllKeyed. `remaining` starts at 1 and is decremented only once
  // the loop is done, so a dictionary of already-settled promises cannot resolve
  // the combinator before every key has been visited.
  const perform = (isSettled, promises, ctor, capability, promiseResolve) => {
    const entries = [];
    const remaining = { value: 1 };
    const ownKeys = ReflectOwnKeys(promises);
    for (let k = 0; k < ownKeys.length; k++) {
      const key = ownKeys[k];
      const desc = ReflectGetOwnPropertyDescriptor(promises, key);
      if (desc === undefined || !desc.enumerable) continue;
      const value = promises[key];
      const index = ArrayPrototypePush(entries, { key, value: undefined }) - 1;
      const nextPromise = ReflectApply(promiseResolve, ctor, [value]);
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
    // Method syntax: a non-constructor with no `prototype`, like the builtin, and
    // the computed key gives it the builtin's `name`.
    const fn = {
      [name](promises) {
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
      },
    }[name];
    ObjectDefineProperty(Promise, name, {
      value: fn,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  if (needAll) install("allKeyed", false);
  if (needAllSettled) install("allSettledKeyed", true);
}

module.exports = { installSyncPolyfills };
