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
  // no user file). Replace the throwing getter with a plain `undefined` value —
  // matching Node 25+'s clean shape — so `typeof localStorage === "undefined"` is
  // true and no throw occurs; `sessionStorage`, which needs only the flag, keeps
  // working. This runs in the preload BEFORE any user code, so the throwing getter
  // is never observed. When the user passes `--localstorage-file`, the env var is
  // absent and `localStorage` works normally (we do not touch it). We deliberately
  // KEEP the env var set so it inherits to the whole process subtree: a `node`- or
  // `nub`-spawned grandchild re-inherits the webstorage flag via NODE_OPTIONS and
  // would otherwise re-install the throwing getter with no neutralize signal. It's
  // an internal `__NUB_*` plumbing var that's explicitly fine to leak to children.
  // Neutralization is idempotent — a descendant re-running this preload with the
  // var still set just re-defines `localStorage` to undefined again, which is
  // harmless. The descriptor is configurable+writable so user code can still assign
  // its own `localStorage`.
  if (process.env.__NUB_NEUTRALIZE_LOCALSTORAGE) {
    try {
      Object.defineProperty(globalThis, "localStorage", {
        value: undefined,
        configurable: true,
        writable: true,
        enumerable: false,
      });
    } catch { /* descriptor non-configurable on this runtime: leave Node's behavior */ }
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
