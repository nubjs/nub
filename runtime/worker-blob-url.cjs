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
// first `new Worker` to protect cold start. The Worker class calls THIS module's
// `blobUrlSource()`. Touches only URL / Blob / Buffer — all already-realized
// core globals — so requiring it adds nothing to the main-thread bootstrap set.
//
// WHAT IS RETAINED, AND WHY IT IS THE MINIMUM. This wrap is installed on EVERY
// nub run, and the overwhelming majority of object URLs never back a Worker at
// all (a download link, an image preview, a CSV export). Node cannot read a
// Blob's bytes synchronously, so supporting `new Worker(blobUrl)` at all means
// capturing SOMETHING when the Blob is built. Three properties keep that honest:
//
//   * it is a COMPACT COPY, not the caller's parts. Holding the parts pinned
//     whatever they referenced — `new Blob([big.subarray(0, 32)])` kept the
//     whole 256 KB backing buffer alive for 32 bytes of content, measured at
//     504 MB retained for 2000 such Blobs where plain Node held 2 MB.
//   * it lives OFF the V8 heap, as a Buffer. The previous decoded-string form
//     put a second full copy of every blob on the JS heap — measured at +43 MB
//     against plain Node's +1 MB for 20k object URLs — where it counts against
//     `--max-old-space-size` and can OOM a process Node would not. Decoding to
//     text is deferred to `blobUrlSource()`, which only `new Worker` reaches.
//   * it is CAPPED. A blob: worker entry is a script; past the cap nothing is
//     kept, and `new Worker` reports the URL as unknown rather than silently
//     retaining tens of megabytes per object URL to serve a case that does not
//     occur in practice.

// blob: URL → the Blob's UTF-8 source bytes (a Buffer, off the V8 heap).
// Read via blobUrlSource(); cleared by revokeObjectURL.
const blobUrlSources = new Map();
// Per-Blob captured source bytes, so createObjectURL can assemble the entry
// synchronously and a nested Blob part can be resolved.
const blobParts = new WeakMap();

// Generous ceiling on what is worth capturing: far above any real inline worker
// script, far below the media/data blobs this must not duplicate.
const MAX_CAPTURE_BYTES = 4 * 1024 * 1024;

// Encode a Blob's construction parts into ONE compact Buffer, or null when the
// content exceeds MAX_CAPTURE_BYTES (or a nested Blob was itself uncaptured).
// `Buffer.concat` allocates a fresh exactly-sized buffer, which is also what
// detaches the result from any larger backing store the caller passed a view of.
function encodeParts(parts) {
  const chunks = [];
  let total = 0;
  for (const p of parts ?? []) {
    let chunk;
    if (typeof p === "string") chunk = Buffer.from(p, "utf8");
    else if (typeof Buffer !== "undefined" && Buffer.isBuffer(p)) chunk = p;
    else if (ArrayBuffer.isView(p)) chunk = Buffer.from(p.buffer, p.byteOffset, p.byteLength);
    else if (p instanceof ArrayBuffer) chunk = Buffer.from(p);
    else if (typeof p === "object" && p && typeof p.size === "number") {
      chunk = blobParts.get(p); // a nested Blob made via our wrapper
      if (chunk === undefined) return null; // nested blob was over the cap
    } else continue;
    total += chunk.length;
    if (total > MAX_CAPTURE_BYTES) return null;
    chunks.push(chunk);
  }
  return Buffer.concat(chunks, total);
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
        if (args[0] != null) {
          // Capture a compact copy NOW — the only moment the bytes are readable
          // synchronously. `encodeParts` returns null past the cap, and a Blob
          // with no entry simply is not usable as a worker entry. Never let our
          // accounting fail a Blob construction that vanilla Node would allow.
          try {
            const bytes = encodeParts(args[0]);
            if (bytes !== null) blobParts.set(inst, bytes);
          } catch {
            /* exotic parts: no capture, so no blob: worker from this Blob */
          }
        }
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
    // Point the URL at the compact bytes captured at construction — never at the
    // Blob itself, which would keep our own WeakMap entry (and so the caller's
    // parts) reachable for as long as the URL lives.
    const bytes = blobParts.get(obj);
    if (bytes !== undefined) blobUrlSources.set(url, bytes);
    return url;
  };
  Object.defineProperty(wrappedCreate, INSTALLED, { value: true });
  URL.createObjectURL = wrappedCreate;
  if (nativeRevoke) {
    URL.revokeObjectURL = function revokeObjectURL(url) {
      blobUrlSources.delete(url);
      // Forward the caller's real arguments rather than a fixed arity of one, so a
      // zero-arg call still reaches Node's own ERR_MISSING_ARGS check.
      return nativeRevoke(...arguments);
    };
  }
}

// The source text behind a `blob:` worker entry, decoded ON DEMAND — the only
// caller is worker-polyfill.mjs's Worker constructor, so an object URL that
// never becomes a Worker is never decoded at all. `undefined` when the URL was
// not minted through our wrap, was revoked, or held content past the capture
// cap; the constructor turns that into its "not a known object URL" TypeError.
function blobUrlSource(url) {
  const bytes = blobUrlSources.get(url);
  return bytes === undefined ? undefined : bytes.toString("utf8");
}

module.exports = { blobUrlSources, blobUrlSource, installBlobUrlSupport };
