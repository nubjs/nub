// Browser-shape Worker global polyfill for Node.js.
// Wraps node:worker_threads.Worker with EventTarget inheritance,
// real MessageEvent/ErrorEvent, and URL-only constructor (Deno shape).

// node: builtins are fetched via `process.getBuiltinModule` rather than static
// `import`. This file is loaded via `require(esm)` from the preload (polyfills.cjs),
// and Node's `require(esm)` instantiates an ES module by walking its STATIC IMPORT
// graph through whatever ESM loader chain is registered — including the USER's
// `--loader`/`register()` hooks. A static `import { Worker } from
// "node:worker_threads"` therefore routes the builtin through the user chain; a
// user load hook that returns SOURCE for node:worker_threads makes V8 see no
// `Worker` export, so `new NodeWorker(...)` references an undefined binding and
// the child crashes (observed against test-esm-loader-chaining). `process
// .getBuiltinModule` fetches the real builtin synchronously off the loader graph,
// bypassing the user chain entirely — same fix transform-core.mjs uses.
//
// The bootstrap MUST avoid a static `node:module` import too: it has the IDENTICAL
// leak. The chaining corpus registers a user load hook (loader-load-foo-or-42.mjs)
// that rewrites the SOURCE of `node:module` so its compiled namespace no longer
// exports `createRequire` — so a static `import { createRequire } from "node:module"`
// here threw `does not provide an export named 'createRequire'` and crashed every
// run with that loader (the earlier comment claimed user hooks "don't intercept
// node:module" — FALSE; this is the bug). So we use `process.getBuiltinModule` when
// present (fast tier + modern compat: no static import, nothing for the user chain
// to observe), and on the narrow FLOOR where it's absent (Node < 22.3/20.16/18.20.4,
// loaded only via the compat-tier entries OFF any user chain) the createRequire
// THREADED IN through `setBootstrapCreateRequire` below.
//
// BRAND BOUNDARY — the floor's `createRequire` is threaded through MODULE SCOPE, never
// parked on `globalThis` (a `globalThis.__nub*` sentinel is the same brand leak as a
// NUB_* env var — enumerable in user code AND worker realms — so it is forbidden). On
// the floor this module is loaded ONLY via the compat-tier main-thread preload
// (preload.mjs), which imports floor-builtin first, then — AFTER importing this module
// — calls `setBootstrapCreateRequire(createRequire)` and `installWorkerPolyfill()`. So
// the install work is deferred (this module does NOT auto-run on the floor): its body
// fetches builtins, and on the floor those aren't reachable until the setter runs.
// On the fast tier (getBuiltinModule present) the install runs eagerly at module eval
// — see the auto-install at the bottom — so the existing side-effect-`require` call
// sites (preload.cjs, polyfills.cjs) are unchanged.
let _bootstrapCreateRequire = null;
export function setBootstrapCreateRequire(fn) {
  _bootstrapCreateRequire = fn;
}
function __getBuiltin(id) {
  if (typeof process.getBuiltinModule === "function") return process.getBuiltinModule(id);
  return _bootstrapCreateRequire(import.meta.url)(id);
}

// `ErrorEvent` only became a global in Node 26. On the 22/24 floor it is
// undefined, so `new ErrorEvent(...)` below would throw a ReferenceError inside
// the worker's "error" handler — crashing the PARENT thread on every worker
// that throws. Resolve the constructor lazily and memoize on first use: use the
// native global when present, otherwise a minimal Event subclass carrying the
// standard ErrorEvent fields (message/error/filename/lineno/colno).
// See wiki/research/worker-polyfill.md.
//
// LAZY (not resolved at module load) on purpose: reading `globalThis.ErrorEvent`
// at top level trips Node's lazy `ErrorEvent` getter, which eagerly realizes
// ~100+ builtins (http2/tls/crypto/zlib/perf_hooks/webstreams) on EVERY startup
// — a cold-start regression (process.moduleLoadList ~230 vs node's ~110) that
// contradicts nub's fast-runner premise for the common "run a plain file, never
// touch Workers" case. The constructors are only ever needed inside the
// post-construction error/message handlers, so deferring resolution there costs
// nothing for non-Worker programs and nothing measurable for Worker ones.
let _ErrorEventCtor;
function getErrorEventCtor() {
  return (_ErrorEventCtor ??=
    typeof globalThis.ErrorEvent === "function"
      ? globalThis.ErrorEvent
      : class ErrorEvent extends Event {
          constructor(type, init = {}) {
            super(type, init);
            this.message = init.message ?? "";
            this.error = init.error ?? null;
            this.filename = init.filename ?? "";
            this.lineno = init.lineno ?? 0;
            this.colno = init.colno ?? 0;
          }
        });
}

// Define the browser-shape `Worker` global (main thread) + the worker-side scope
// (self/postMessage/message wiring). Acquires its node: builtins on entry — on the
// floor that needs the threaded createRequire, so this runs only after the compat
// entry has called setBootstrapCreateRequire (or, on the fast tier, eagerly via the
// auto-install at the bottom).
export function installWorkerPolyfill() {
  const { Worker: NodeWorker, parentPort, isMainThread } = __getBuiltin("node:worker_threads");
  const { fileURLToPath } = __getBuiltin("node:url");
  // blob: worker source registry, shared with the eager main-thread preload that
  // wraps URL.createObjectURL (worker-blob-url.cjs). Loaded via createRequire so
  // both this lazily-loaded ESM module and the eager CJS preload reference the SAME
  // module instance (Node dedupes by resolved path) — i.e. the SAME registry.
  const { blobUrlSource, installBlobUrlSupport } = (
    typeof process.getBuiltinModule === "function"
      ? __getBuiltin("node:module").createRequire(import.meta.url)
      : _bootstrapCreateRequire(import.meta.url)
  )("./worker-blob-url.cjs");

  // Resolve a worker-error stack frame to {filename,lineno,colno} so the
  // ErrorEvent carries real source location, per WHATWG §10.2.6 (the spec
  // requires these fields populated from where the error was raised). Node's
  // `error` event delivers the thrown Error; we read its first stack frame.
  // Browser-scrubbing of cross-origin frames does not apply here (all worker
  // sources are same-origin local), so we surface the raw location.
  //
  // The frame is anchored at `at ` and consumes the optional `Func (` wrapper so
  // group 1 is JUST the path (a `file://` URL, an absolute POSIX path, or a
  // Windows `C:\…` path — the leading `C:` is NOT mistaken for the location
  // colon because the line:col are the LAST two `:`-segments). Without the anchor
  // + wrapper-consumption the path captured the `at Func (` prefix verbatim.
  const STACK_FRAME = /^at\s+(?:.+?\s+\()?(.+?):(\d+):(\d+)\)?$/;
  function locationFromError(err) {
    let filename = "";
    let lineno = 0;
    let colno = 0;
    const stack = err && typeof err.stack === "string" ? err.stack : "";
    for (const line of stack.split("\n")) {
      const t = line.trim();
      if (!t.startsWith("at ")) continue;
      const m = STACK_FRAME.exec(t);
      if (m) {
        filename = m[1].startsWith("file://") ? fileURLToPath(m[1]) : m[1];
        lineno = Number(m[2]) || 0;
        colno = Number(m[3]) || 0;
        break;
      }
    }
    return { filename, lineno, colno };
  }

  if (typeof globalThis.Worker === "undefined") {
  class Worker extends EventTarget {
    #worker;
    #name;
    // Live registry of 'error' listeners, so an UNHANDLED worker 'error' can be
    // made fatal to match the oracle (node:worker_threads is an EventEmitter;
    // EventTarget exposes no listener count). Keyed listener → set of capture
    // flags, mirroring EventTarget's `(type, callback, capture)` listener identity
    // — `(fn, capture)` and `(fn, bubble)` are TWO distinct registrations, so the
    // registry must distinguish them or a capture-only remove could empty it while
    // a live bubble listener remains (a false fatality). The onerror setter routes
    // through addEventListener below, so it is tracked here automatically.
    #errorListeners = new Map();

    constructor(url, options = {}) {
      super();

      // ADDITIVE node-mirroring (NOT a spec mechanism): `new Worker(code,
      // { eval: true })` runs the first arg as raw source, mirroring
      // node:worker_threads. Gated STRICTLY on `options.eval === true` — never
      // content-sniffed — so the spec URL path below is untouched for every web
      // caller. execArgv (carrying nub's preload) is forwarded for eval workers
      // too, so an eval worker inherits nub's augmentation like any other.
      const isEval = options.eval === true;

      // The WHATWG Worker constructor accepts a script URL. Per §10.2.6.3 the
      // standard inline mechanisms are `blob:` and `data:` URLs (there is no
      // inline-source-string form in the spec). We map each to a Node spawn:
      //   - file path / file: URL  → spawn the file (transpiled by nub's preload)
      //   - data: URL              → Node runs it directly (worker_threads v14.9)
      //   - blob: URL              → snapshot the Blob's source, spawn it as a
      //                              data: URL (Node can't open blob:)
      let spawnTarget;

      if (isEval) {
        if (typeof url !== "string") {
          throw new TypeError(
            "Worker constructor: with { eval: true }, the source must be a string"
          );
        }
        spawnTarget = url;
      } else {

        const asUrlString =
          url instanceof URL ? url.href : typeof url === "string" ? url : null;
        if (asUrlString === null) {
          throw new TypeError("Worker constructor: url must be a string or URL");
        }

        if (asUrlString.startsWith("blob:")) {
          // A `blob:` worker (WHATWG inline mechanism). Node cannot open a blob:
          // URL as a worker entry, and the Blob's bytes are only readable
          // ASYNCHRONOUSLY (Blob.text/arrayBuffer) while this constructor is sync.
          // We close that gap by capturing the Blob SYNCHRONOUSLY at
          // `URL.createObjectURL(blob)` time (see installBlobUrlSupport) into a
          // module-scope registry keyed by URL, then spawn the source as a `data:`
          // URL — a real module load (a proper worker entry that receives nub's
          // preload, so `self`/`postMessage` are present) on every supported tier.
          // The UTF-8 decode happens HERE rather than at createObjectURL time, so
          // the object URLs that never become a Worker — nearly all of them — pay
          // nothing for this feature.
          const source = blobUrlSource(asUrlString);
          if (source === undefined) {
            throw new TypeError(
              `Worker constructor: blob URL '${asUrlString}' is not a known object URL`
            );
          }
          spawnTarget = new URL(
            "data:text/javascript;base64," + Buffer.from(source, "utf8").toString("base64")
          );
        } else if (asUrlString.startsWith("data:")) {
          spawnTarget = new URL(asUrlString);
        } else if (asUrlString.startsWith("file://")) {
          spawnTarget = fileURLToPath(asUrlString);
        } else {
          spawnTarget = asUrlString;
        }

      }

      this.#name = typeof options.name === "string" ? options.name : "";

      // `type: "module" | "classic"` selects the worker's module system per the
      // spec. The worker-side scope exposes `importScripts` only for classic
      // workers (WHATWG WorkerGlobalScope — classic-only) and throws for module
      // workers. nub signals the choice to the worker via the internal
      // NUB_WORKER_TYPE env (internal plumbing var — exempt from the brand
      // boundary). Node still decides the entry's actual module/CJS PARSING by
      // file extension + package.json "type"; this env governs only which
      // importScripts surface the polyfill installs.
      const workerType =
        options.type === "classic" ? "classic" : "module";

      // Node rejects flags in Worker execArgv that imply V8 `--harmony-*` staging
      // flags (ERR_WORKER_INVALID_EXEC_ARGV): `--harmony-*` themselves, and
      // `--experimental-shadow-realm` (implies `--harmony-shadow-realm`). Strip
      // those from whatever execArgv we forward.
      const stripHarmony = (argv) =>
        argv.filter(
          f => !f.startsWith("--harmony") && f !== "--experimental-shadow-realm"
        );
      // execArgv: forward nub's preload-carrying parent execArgv by DEFAULT (so a
      // worker inherits nub's transpile augmentation), but if the user supplied
      // their own execArgv, MERGE rather than clobber — parent flags first, user
      // flags appended so the user's win on conflict.
      const execArgv = stripHarmony(
        Array.isArray(options.execArgv)
          ? [...process.execArgv, ...options.execArgv]
          : process.execArgv
      );

      const nodeOptions = {
        ...options,
        eval: isEval,
        execArgv,
      };
      // Thread the worker type AND name to the worker via internal env vars
      // (NUB_WORKER_TYPE / NUB_WORKER_NAME — internal plumbing, exempt from the
      // brand boundary). NUB_WORKER_NAME is REQUIRED for self.name across the
      // whole compat tier: worker_threads.threadName (the only native worker-side
      // reader of the {name} option) lands in v24.6.0 / v22.20.0, so it is absent
      // below that and the env is the sole portable carrier. We avoid disturbing
      // the user's env semantics: `worker_threads.SHARE_ENV` is a Symbol
      // (live-shared parent env) — spreading it would destroy the share — so in
      // that case we leave env untouched (self.name then falls back to native
      // threadName/"" and importScripts defaults to the classic form).
      const userEnv = options.env;
      if (typeof userEnv === "symbol") {
        // SHARE_ENV: leave nodeOptions.env as the user gave it; can't inject.
      } else {
        nodeOptions.env = {
          ...(userEnv ?? process.env),
          NUB_WORKER_TYPE: workerType,
          NUB_WORKER_NAME: this.#name,
        };
      }
      if (this.#name) nodeOptions.name = this.#name;

      this.#worker = new NodeWorker(spawnTarget, nodeOptions);

      this.#worker.on("message", (data) => {
        this.dispatchEvent(new MessageEvent("message", { data }));
      });

      // WHATWG: messageerror fires when an inbound message fails deserialization;
      // it is a plain MessageEvent with `data: null` (NOT carrying the error).
      this.#worker.on("messageerror", () => {
        this.dispatchEvent(new MessageEvent("messageerror", { data: null }));
      });

      this.#worker.on("error", (err) => {
        const ErrorEventCtor = getErrorEventCtor();
        const { filename, lineno, colno } = locationFromError(err);
        this.dispatchEvent(
          new ErrorEventCtor("error", {
            error: err,
            message: err.message,
            filename,
            lineno,
            colno,
          })
        );
        // ORACLE CONFORMANCE: node:worker_threads re-throws an 'error' that has
        // NO listener (the EventEmitter unhandled-'error' convention) → it surfaces
        // as an uncaughtException on the main thread, exit code 1, stack printed.
        // nub's wrapper owns the node-side 'error' listener, so that throw never
        // fires natively; we reproduce its fatality when the browser-shape Worker
        // has no 'error'/onerror listener. Without this an EventTarget silently
        // drops the unlistened event and a failed worker load exits 0 (the bug).
        // process.nextTick (not a sync throw inside this emit callback) mirrors
        // Node's surfacing path exactly: a fresh tick → uncaughtException, so a
        // user process.on('uncaughtException') still intercepts it as on Node.
        //
        // DUAL-CHANNEL FATALITY INVARIANT: an 'error' is "handled" if the user
        // registered a listener on EITHER channel — the web one (addEventListener
        // /onerror, tracked in #errorListeners) OR the node one (.on('error'),
        // which delegates straight onto this underlying worker). The wrapper owns
        // exactly ONE persistent node-side 'error' listener (this very one), so a
        // node-channel listenerCount > 1 means the user added their own; counting
        // only the web channel would wrongly throw a fatal at a user handling via
        // .on('error'), or swallow one they aren't handling. Both channels MUST be
        // counted here.
        if (
          this.#errorListeners.size === 0 &&
          this.#worker.listenerCount("error") <= 1
        ) {
          process.nextTick(() => {
            throw err;
          });
        }
      });

      // ADDITIVE node lifecycle events, dispatched as plain EventTarget `Event`s
      // on the parent handle. `exit` carries the worker's exit code as a property
      // (the raw-arg `.on('exit', code)` form rides the node channel via the
      // delegation methods below). NavigatorOnLine's web `online` targets the
      // worker GLOBAL (`self`), not this parent handle — disjoint objects, no
      // collision. The raw-arg `.on('online'|'exit')` node form is available via
      // delegation; these are the web-shape Event mirrors.
      this.#worker.on("online", () => {
        this.dispatchEvent(new Event("online"));
      });
      this.#worker.on("exit", (code) => {
        const ev = new Event("exit");
        ev.code = code;
        this.dispatchEvent(ev);
      });
    }

    get name() {
      return this.#name;
    }

    // Track 'error' listeners (for the unhandled-'error' fatality above) without
    // changing EventTarget semantics — super does the real registration; we only
    // record/forget the (listener, capture) identity. The registry mirrors
    // EventTarget's identity-based dedup INCLUDING the capture flag, so a
    // capture-only remove can never empty it while a live bubble listener remains.
    // The only un-mirrored case is {once}/{signal} auto-removal (EventTarget drops
    // those without routing through this override) — a stale entry there errs
    // toward DELIVERY (swallow), never toward a false fatality.
    addEventListener(type, listener, options) {
      super.addEventListener(type, listener, options);
      if (
        type === "error" &&
        (typeof listener === "function" ||
          (listener != null && typeof listener.handleEvent === "function"))
      ) {
        const capture =
          options === true ||
          (typeof options === "object" && options !== null && !!options.capture);
        let caps = this.#errorListeners.get(listener);
        if (!caps) this.#errorListeners.set(listener, (caps = new Set()));
        caps.add(capture);
      }
    }

    removeEventListener(type, listener, options) {
      super.removeEventListener(type, listener, options);
      if (type === "error") {
        const capture =
          options === true ||
          (typeof options === "object" && options !== null && !!options.capture);
        const caps = this.#errorListeners.get(listener);
        if (caps) {
          caps.delete(capture);
          if (caps.size === 0) this.#errorListeners.delete(listener);
        }
      }
    }

    postMessage(data, transfer) {
      this.#worker.postMessage(data, transfer);
    }

    // WHATWG terminate() returns void; node:worker_threads returns a
    // `Promise<exitCode>`. We RETURN that promise — void→value widening, additive
    // and unobservable to spec code that ignores the return.
    terminate() {
      return this.#worker.terminate();
    }

    // ADDITIVE EventEmitter surface, delegated to the underlying real
    // node:worker_threads.Worker (Node's ACTUAL EventEmitter, not a reimpl). The
    // node channel delivers what node delivers — `.on('message')` the RAW value,
    // `.on('error')` a BARE Error, `.on('exit', code)`/`.on('online')` the raw
    // args — disjoint from the web channel (addEventListener/onmessage/onerror →
    // MessageEvent/ErrorEvent), which stays primary and unchanged. Chainable
    // methods return THIS wrapper (not the inner worker) so chaining stays on the
    // web-shape object; .emit returns the EventEmitter boolean. The unhandled-
    // 'error' fatality (constructor) counts node-channel listeners via
    // listenerCount, so a `.on('error')` here correctly suppresses the fatal.
    on(type, listener) { this.#worker.on(type, listener); return this; }
    once(type, listener) { this.#worker.once(type, listener); return this; }
    off(type, listener) { this.#worker.off(type, listener); return this; }
    addListener(type, listener) { this.#worker.addListener(type, listener); return this; }
    removeListener(type, listener) { this.#worker.removeListener(type, listener); return this; }
    emit(type, ...args) { return this.#worker.emit(type, ...args); }

    #onmessageHandler = null;
    get onmessage() { return this.#onmessageHandler; }
    set onmessage(fn) {
      if (this.#onmessageHandler) this.removeEventListener("message", this.#onmessageHandler);
      this.#onmessageHandler = fn;
      if (fn) this.addEventListener("message", fn);
    }

    #onerrorHandler = null;
    get onerror() { return this.#onerrorHandler; }
    set onerror(fn) {
      if (this.#onerrorHandler) this.removeEventListener("error", this.#onerrorHandler);
      this.#onerrorHandler = fn;
      if (fn) this.addEventListener("error", fn);
    }

    #onmessageerrorHandler = null;
    get onmessageerror() { return this.#onmessageerrorHandler; }
    set onmessageerror(fn) {
      if (this.#onmessageerrorHandler) this.removeEventListener("messageerror", this.#onmessageerrorHandler);
      this.#onmessageerrorHandler = fn;
      if (fn) this.addEventListener("messageerror", fn);
    }
  }

  // NON-ENUMERABLE: invisible to `Object.keys(globalThis)` / for-in is the
  // additive contract — vanilla-Node code that enumerates the global object must
  // not observe nub's injected `Worker`. Node defines its own globals the same
  // way. Writable+configurable so user code can still override it.
  Object.defineProperty(globalThis, "Worker", {
    value: Worker,
    enumerable: false,
    writable: true,
    configurable: true,
  });

  // Enable blob: workers: wrap URL.createObjectURL so the source is captured
  // synchronously for the constructor's blob: branch. Transparent for all other
  // uses; installs once. Only on the main thread (where blob: URLs are minted).
  if (isMainThread) installBlobUrlSupport();
}

// Worker-side bootstrap: emulate the DedicatedWorkerGlobalScope on top of
// node:worker_threads — `self`, `postMessage`, `close`, AND inbound message
// events. Node's worker global is not an EventTarget and exposes none of these
// (verified), so the polyfill provides the whole surface. Without the inbound
// wiring, `self.onmessage` / `self.addEventListener("message", …)` never fire
// and a parent→worker round-trip hangs — see wiki/research/worker-polyfill.md.
if (!isMainThread && parentPort) {
  const scope = globalThis;
  // All of nub's worker-scope global injections below (self, addEventListener,
  // removeEventListener, dispatchEvent, postMessage, close) are defined
  // NON-ENUMERABLE. Node's worker global is not an EventTarget and exposes none
  // of these, so a worker doing `Object.keys(globalThis)` / for-in must not see
  // nub's additions — invisibility-to-enumeration is the additive contract.
  // writable+configurable mirrors Node's own global descriptors. (`onmessage`/
  // `onmessageerror` below already use Object.defineProperty, whose enumerable
  // defaults to false.)
  const defineGlobal = (name, value) =>
    Object.defineProperty(scope, name, {
      value,
      enumerable: false,
      writable: true,
      configurable: true,
    });
  defineGlobal("self", scope);

  // `self.name` — the worker's name from the constructor's {name} option (WHATWG
  // DedicatedWorkerGlobalScope.name). Node only exposes a worker-side reader for
  // the {name} option as `worker_threads.threadName` from v24.6.0 / v22.20.0 — it
  // is ABSENT across nub's whole compat tier (18.19–22.19), so it cannot be the
  // floor mechanism. We THREAD the name in ourselves via the internal
  // NUB_WORKER_NAME env (internal plumbing var — exempt from the brand boundary),
  // set by the main-side constructor.
  //
  // RESOLUTION ORDER — env FIRST (not native threadName): NUB_WORKER_NAME carries
  // the user's EXACT intent including the empty string, whereas native
  // `threadName` defaults to the literal sentinel "WorkerThread" for an UNNAMED
  // worker (it's the thread DISPLAY name, not the WHATWG worker name) — surfacing
  // that as self.name would be a spec divergence (an unnamed worker's name must be
  // ""). So: the injected env wins when present; native threadName is the fallback
  // ONLY on the SHARE_ENV path (where the ctor couldn't inject env), with the
  // "WorkerThread" sentinel filtered to "".
  {
    const wt = __getBuiltin("node:worker_threads");
    let name;
    if (typeof process.env.NUB_WORKER_NAME === "string") {
      name = process.env.NUB_WORKER_NAME;
    } else {
      const tn = wt && typeof wt.threadName === "string" ? wt.threadName : "";
      name = tn === "WorkerThread" ? "" : tn;
    }
    Object.defineProperty(scope, "name", {
      value: name,
      enumerable: false,
      writable: true,
      configurable: true,
    });
  }

  // `importScripts(...urls)` — WHATWG WorkerGlobalScope, CLASSIC workers only.
  // Synchronously fetches + evaluates each script in the global scope, in order.
  // A module worker MUST throw on importScripts (use `import` instead). nub learns
  // the worker's type from NUB_WORKER_TYPE (set by the main-side constructor).
  // Remote URLs are not supported (no sync network in Node); local file:/relative
  // paths and data: URLs are read synchronously.
  // Default to the classic (working) importScripts when the type is unset — this
  // is the SHARE_ENV edge where the constructor couldn't inject NUB_WORKER_TYPE.
  if (process.env.NUB_WORKER_TYPE !== "module") {
    const fs = __getBuiltin("node:fs");
    const { fileURLToPath: f2p, pathToFileURL } = __getBuiltin("node:url");
    defineGlobal("importScripts", (...urls) => {
      for (const u of urls) {
        const s = String(u);
        let code;
        if (s.startsWith("data:")) {
          const comma = s.indexOf(",");
          const meta = s.slice(5, comma);
          const body = s.slice(comma + 1);
          code = meta.includes("base64")
            ? Buffer.from(body, "base64").toString("utf8")
            : decodeURIComponent(body);
        } else if (/^https?:/.test(s)) {
          throw new TypeError(
            "importScripts: remote URLs are not supported (no synchronous network)"
          );
        } else {
          const path = s.startsWith("file://") ? f2p(s) : s;
          code = fs.readFileSync(path, "utf8");
        }
        // Indirect eval → runs in global scope, matching importScripts semantics.
        (0, eval)(code);
      }
    });
  } else {
    // Module workers: importScripts must throw (spec). Provide the throwing form
    // so the surface exists and the error is the spec-correct one.
    defineGlobal("importScripts", () => {
      throw new TypeError("importScripts is not available in module workers");
    });
  }

  // `message`/`messageerror` are DELEGATED onto the native `parentPort` (a real
  // Node MessagePort) so Node's own C++ event-loop ref-counting governs worker
  // lifetime: a worker that never listens leaves parentPort with no listeners →
  // Node unrefs it → the worker exits naturally (matching `node:worker_threads`
  // and Bun); a worker listening via `self.onmessage` / `addEventListener` refs
  // it → stays alive. (Earlier this block eagerly held a `parentPort.on("message")`
  // forwarder that kept EVERY worker's loop alive → pure `parentPort` workers that
  // should exit hung forever. See wiki/research/worker-polyfill.md §4.)
  //
  // DISPATCH THROUGH A REAL EventTarget (`inbound`), not a hand-invoked callback:
  // an inbound event MUST have `event.target === self` and `event.currentTarget
  // === self` per WHATWG DedicatedWorkerGlobalScope (browsers + Deno agree) — the
  // old path `cb.call(scope, new MessageEvent(...))` never went through dispatch,
  // so both stayed null. We can't make globalThis a genuine EventTarget without
  // prototype re-parenting (a plain object has no [[EventTargetData]] slot, so a
  // native dispatch keyed on it is impossible), so we dispatch on a private
  // EventTarget and SHADOW `target`/`currentTarget` = self as own data properties
  // on the event (they override Event.prototype's accessors; native dispatch still
  // drives ordering / stopImmediatePropagation over the ONE shared event object).
  //
  // REF-COUNTING is preserved by mediating every add/remove and implementing
  // `{once}`/`{signal}` OURSELVES (not via native EventTarget options), so every
  // removal — including once-fire and signal-abort — routes through our count and
  // attaches/detaches the SINGLE parentPort forwarder per type exactly on the
  // 0↔1 boundary. All OTHER (custom) event types still go to a separate private
  // EventTarget (`other`); a user-dispatched event's `target` is that private
  // target — the documented minor divergence, unchanged here.
  const other = new EventTarget();
  const otherAdd =
    typeof scope.addEventListener === "function"
      ? scope.addEventListener.bind(scope)
      : other.addEventListener.bind(other);
  const otherRemove =
    typeof scope.removeEventListener === "function"
      ? scope.removeEventListener.bind(scope)
      : other.removeEventListener.bind(other);
  const otherDispatch =
    typeof scope.dispatchEvent === "function"
      ? scope.dispatchEvent.bind(scope)
      : other.dispatchEvent.bind(other);

  const DELEGATED = new Set(["message", "messageerror"]);
  const inbound = new EventTarget();
  // Per delegated type: the single parentPort forwarder + a live listener count,
  // so parentPort holds a listener iff ≥1 user listener exists (worker lifetime).
  const portState = {
    message: { count: 0, forwarder: null },
    messageerror: { count: 0, forwarder: null },
  };
  // user listener → the wrapper actually registered on `inbound`, so remove
  // detaches the exact one and dedup of identical (type, listener) holds.
  const registered = { message: new Map(), messageerror: new Map() };

  function forward(evt, data) {
    const ev = new MessageEvent(evt, { data });
    // Shadow the inherited accessors so worker code observes the spec-required
    // DedicatedWorkerGlobalScope as target/currentTarget instead of `inbound`.
    Object.defineProperty(ev, "target", { value: scope, configurable: true });
    Object.defineProperty(ev, "currentTarget", { value: scope, configurable: true });
    inbound.dispatchEvent(ev);
  }
  function attachPort(evt) {
    const st = portState[evt];
    if (st.count++ === 0) {
      st.forwarder = (data) => forward(evt, data);
      parentPort.on(evt, st.forwarder);
    }
  }
  function detachPort(evt) {
    const st = portState[evt];
    if (--st.count === 0 && st.forwarder) {
      parentPort.off(evt, st.forwarder);
      st.forwarder = null;
    }
  }

  function addDelegated(evt, listener, opts) {
    // Normalize to the function to invoke, binding a handleEvent object so its
    // `this` is the object (spec) while a bare function gets `this` = self below.
    const cb =
      typeof listener === "function"
        ? listener
        : listener && typeof listener.handleEvent === "function"
          ? listener.handleEvent.bind(listener)
          : null;
    if (!cb) return;
    const map = registered[evt];
    if (map.has(listener)) return; // EventTarget dedups identical (type, listener)
    const o = opts && typeof opts === "object" ? opts : {};
    if (o.signal && o.signal.aborted) return;
    // `once`/`signal` are OURS (not native options) so removal stays observable to
    // the ref-count. The once wrapper removes itself BEFORE invoking — safe mid
    // dispatch (removing an EventTarget listener while dispatching is spec-legal).
    const wrapper = o.once
      ? (ev) => {
          removeDelegated(evt, listener);
          cb.call(scope, ev);
        }
      : (ev) => cb.call(scope, ev);
    map.set(listener, wrapper);
    inbound.addEventListener(evt, wrapper);
    attachPort(evt);
    if (o.signal) {
      o.signal.addEventListener("abort", () => removeDelegated(evt, listener), {
        once: true,
      });
    }
  }
  function removeDelegated(evt, listener) {
    const wrapper = registered[evt].get(listener);
    if (!wrapper) return;
    registered[evt].delete(listener);
    inbound.removeEventListener(evt, wrapper);
    detachPort(evt);
  }

  defineGlobal("addEventListener", (type, listener, opts) =>
    DELEGATED.has(type)
      ? addDelegated(type, listener, opts)
      : otherAdd(type, listener, opts));
  defineGlobal("removeEventListener", (type, listener, opts) =>
    DELEGATED.has(type)
      ? removeDelegated(type, listener)
      : otherRemove(type, listener, opts));
  defineGlobal("dispatchEvent", (ev) => otherDispatch(ev));

  // `onmessage` / `onmessageerror` register via the delegating add/remove above,
  // mirroring the web API and the main-side Worker. Assigning `null` removes the
  // last listener → parentPort unrefs → the worker can exit (Bun parity).
  for (const evt of ["message", "messageerror"]) {
    let handler = null;
    Object.defineProperty(scope, "on" + evt, {
      configurable: true,
      get() {
        return handler;
      },
      set(fn) {
        if (handler) scope.removeEventListener(evt, handler);
        handler = typeof fn === "function" ? fn : null;
        if (handler) scope.addEventListener(evt, handler);
      },
    });
  }

  // Outbound + lifecycle.
  if (typeof scope.postMessage !== "function") {
    defineGlobal("postMessage", (data, transfer) => parentPort.postMessage(data, transfer));
  }
  if (typeof scope.close !== "function") {
    defineGlobal("close", () => process.exit(0));
  }
  }
}

// Fast tier (and modern compat): getBuiltinModule is present, so the install needs no
// threaded createRequire — run it eagerly at module eval, preserving the side-effect-
// on-`require` contract the fast-tier call sites (preload.cjs, polyfills.cjs) rely on.
// On the FLOOR (getBuiltinModule absent) this is skipped; the compat main-thread
// preload calls setBootstrapCreateRequire(...) + installWorkerPolyfill() explicitly.
if (typeof process.getBuiltinModule === "function") installWorkerPolyfill();
