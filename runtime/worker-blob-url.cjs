// Blob-URL worker source capture — the SYNC half of WHATWG `blob:` worker support.
//
// A `new Worker(blobUrl)` must read the Blob's source SYNCHRONOUSLY in the
// constructor, but a Blob's bytes are only readable via the async Blob.text /
// arrayBuffer. We close that gap by snapshotting the source at
// `URL.createObjectURL(blob)` time — which always runs BEFORE the Worker is
// constructed — into a registry keyed by the minted URL.
//
// This lives in its OWN tiny CJS module (no node:worker_threads dependency) so the
// main-thread preload can install the wrap EAGERLY: it must be live before user
// code calls createObjectURL, whereas worker-polyfill.mjs (which pulls
// worker_threads + the whole streams/worker-io builtin set) is loaded LAZILY on
// first `new Worker` to protect cold start. The Worker class imports THIS module to
// read `blobUrlSources`. Touches only URL / Blob / Buffer — all already-realized
// core globals — so requiring it adds nothing to the main-thread bootstrap set.

// blob: URL → source text. Shared with worker-polyfill.mjs's Worker constructor.
const blobUrlSources = new Map();
// Blob construction parts, remembered so createObjectURL can assemble source sync.
const blobParts = new WeakMap();

function decode(parts) {
  let src = "";
  for (const p of parts ?? []) {
    if (typeof p === "string") src += p;
    else if (typeof Buffer !== "undefined" && Buffer.isBuffer(p)) src += p.toString("utf8");
    else if (ArrayBuffer.isView(p)) src += Buffer.from(p.buffer, p.byteOffset, p.byteLength).toString("utf8");
    else if (p instanceof ArrayBuffer) src += Buffer.from(p).toString("utf8");
    else if (typeof p === "object" && p && typeof p.size === "number") {
      const nested = blobParts.get(p); // a nested Blob made via our wrapper
      if (nested) src += decode(nested);
    }
  }
  return src;
}

// Wrap URL.createObjectURL/revokeObjectURL and Proxy Blob to remember parts.
// Idempotent + transparent for every non-worker use. No-op when URL has no
// createObjectURL (older floors) — blob: workers are then simply unavailable.
//
// The install-marker is a Symbol, not a string property: it sits on the native
// Blob constructor (a shared global), so a string key would show up in
// `Reflect.ownKeys(Blob)` / `"x" in Blob` — a (cosmetic) divergence from vanilla
// Node. A Symbol keeps the marker invisible to those reflective surfaces.
const INSTALLED = Symbol.for("nub.blobUrlSupport.installed");
function installBlobUrlSupport() {
  if (typeof URL === "undefined" || typeof URL.createObjectURL !== "function") return;
  if (URL.createObjectURL[INSTALLED]) return;

  const NativeBlob = globalThis.Blob;
  if (typeof NativeBlob === "function" && !NativeBlob[INSTALLED]) {
    // Record construction parts WITHOUT changing Blob's identity. The earlier
    // approach — a `class extends NativeBlob` swapped onto globalThis.Blob — broke
    // brand identity in two ways that a structured-clone exposes:
    //   1. A deserialized Blob (postMessage / structuredClone) is a NATIVE Blob, so
    //      `cloned instanceof globalThis.Blob` was FALSE (subclass.prototype is not
    //      in a native instance's chain) — a spec violation; native Node passes.
    //   2. WORSE, undici's webidl `is.Blob` uses the ORDINARY `[Symbol.hasInstance]`
    //      (a prototype-chain check that ignores a custom hasInstance) against
    //      `globalThis.Blob`, so `new Response(clonedBlob).arrayBuffer()` failed the
    //      brand check and stringified the Blob to "[object Blob]" (13 bytes) instead
    //      of reading its bytes. A custom `Symbol.hasInstance` can't fix this — undici
    //      bypasses it.
    // The ROOT cause was `globalThis.Blob !== node:buffer.Blob`. A Proxy whose target
    // is the native Blob keeps `globalThis.Blob.prototype === NativeBlob.prototype`,
    // so every native Blob (constructed OR deserialized) passes both `instanceof` and
    // undici's ordinary-hasInstance check exactly as on vanilla Node — while the
    // `construct` trap still records parts for sync blob: worker source assembly. File
    // (which `extends` native Blob) then needs NO re-parenting: its instances already
    // have NativeBlob.prototype in their chain.
    const BlobProxy = new Proxy(NativeBlob, {
      construct(target, args, newTarget) {
        // FORWARD newTarget so a user subclass (`class X extends Blob {}`) gets ITS
        // prototype — passing `target` here would force NativeBlob.prototype and
        // silently break `new X() instanceof X` + the subclass's methods (an
        // additivity violation vs vanilla Node). When `new Blob(...)` is called
        // directly, newTarget IS this Proxy; Reflect.construct(NativeBlob, args, Proxy)
        // resolves the new instance's proto from `Proxy.prototype`, which the Proxy
        // forwards to NativeBlob.prototype — so a direct Blob is byte-identical to a
        // native one (same brand, passes instanceof + undici's webidl check).
        const inst = Reflect.construct(target, args, newTarget);
        if (args[0] != null) blobParts.set(inst, args[0]);
        return inst;
      },
    });
    Object.defineProperty(NativeBlob, INSTALLED, { value: true });
    Object.defineProperty(globalThis, "Blob", {
      value: BlobProxy,
      enumerable: false,
      writable: true,
      configurable: true,
    });
    // File's `extends` link still points at the real NativeBlob (unchanged), so
    // `new File(...) instanceof Blob` holds natively — no re-parenting needed.
  }

  const nativeCreate = URL.createObjectURL.bind(URL);
  const nativeRevoke =
    typeof URL.revokeObjectURL === "function" ? URL.revokeObjectURL.bind(URL) : null;
  const wrappedCreate = function createObjectURL(obj) {
    const url = nativeCreate(obj);
    const parts = blobParts.get(obj);
    if (parts) blobUrlSources.set(url, decode(parts));
    return url;
  };
  Object.defineProperty(wrappedCreate, INSTALLED, { value: true });
  URL.createObjectURL = wrappedCreate;
  if (nativeRevoke) {
    URL.revokeObjectURL = function revokeObjectURL(url) {
      blobUrlSources.delete(url);
      return nativeRevoke(url);
    };
  }
}

module.exports = { blobUrlSources, installBlobUrlSupport };
