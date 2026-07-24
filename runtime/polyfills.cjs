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
// Node 22.15+ (our floor) already has: navigator, navigator.locks,
// navigator.hardwareConcurrency, WebSocket. No polyfills needed.
//
// Node 24+ adds: URLPattern, RegExp.escape, Error.isError, Promise.try.
// We polyfill those on Node 22.x only.
//
// No Node version ships: Temporal, reportError, browser-shape Worker.
// These need polyfills on all supported versions. (Temporal is a lazy global
// installed by the preload entry, NOT here — see preload.cjs / preload.mjs.)

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
  // already proved that), so `delete` removes it cleanly. This file is sloppy-mode
  // CJS, where `delete` never throws (a non-configurable property would just make
  // it return false and leave Node's getter in place); the try/catch is belt-and-
  // suspenders for any future strict/ESM move.
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
  // `isFloat16Array`. See wiki/runtime/float16array-polyfill.md.
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
