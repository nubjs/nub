// Browser-shape Worker global polyfill for Node.js.
// Wraps node:worker_threads.Worker with EventTarget inheritance,
// real MessageEvent/ErrorEvent, and URL-only constructor (Deno shape).

// node: builtins are fetched via `process.getBuiltinModule` rather than static
// `import`. This file is loaded via `require(esm)` from the preload, and Node's
// `require(esm)` instantiates an ES module by walking its STATIC IMPORT graph
// through whatever ESM loader chain is registered — including the USER's
// `--loader`/`register()` hooks. A static `import { Worker } from
// "node:worker_threads"` therefore routes the builtin through the user chain; a
// user load hook that returns SOURCE for node:worker_threads makes V8 see no
// `Worker` export, so `new NodeWorker(...)` references an undefined binding and
// the child crashes (observed against test-esm-loader-chaining). `process
// .getBuiltinModule` fetches the real builtin synchronously off the loader
// graph, bypassing the user chain entirely — same fix transform-core.mjs uses.
// `process.getBuiltinModule` doesn't exist below Node 22.3 / 20.16 / 18.20.4 (it
// threw `TypeError: ... is not a function` there). Fall back to a createRequire
// bootstrapped from a single static `node:module` import. The actual builtins below
// are still fetched through `__getBuiltin` — getBuiltinModule on new Node, CJS
// `__require` on old — and BOTH bypass the user ESM loader chain (the whole reason
// this file avoids a static `import` of node:worker_threads). Only the bootstrap
// touches node:module, which user loader hooks don't intercept (unlike the mockable
// node:worker_threads the chaining test targets), so the bypass guarantee holds.
import { createRequire as __bootstrapCreateRequire } from "node:module";
const __getBuiltin =
  typeof process.getBuiltinModule === "function"
    ? (id) => process.getBuiltinModule(id)
    : ((__r) => (id) => __r(id))(__bootstrapCreateRequire(import.meta.url));
const { Worker: NodeWorker, parentPort, isMainThread } = __getBuiltin("node:worker_threads");
const { fileURLToPath } = __getBuiltin("node:url");

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

if (typeof globalThis.Worker === "undefined") {
  class Worker extends EventTarget {
    #worker;

    constructor(url, options = {}) {
      super();

      let workerPath;
      if (url instanceof URL) {
        workerPath = fileURLToPath(url);
      } else if (typeof url === "string") {
        if (url.startsWith("file://")) {
          workerPath = fileURLToPath(url);
        } else {
          workerPath = url;
        }
      } else {
        throw new TypeError("Worker constructor: url must be a string or URL");
      }

      // `type: "module" | "classic"` is accepted for web compatibility but not
      // enforced: Node decides module-vs-CJS for the worker entry by file
      // extension + nearest package.json "type" (the same rule nub applies to
      // the main entry), and there is no classic/importScripts mode. Passing
      // `type` through to NodeWorker is harmless — it ignores unknown options.
      // See wiki/research/worker-polyfill.md.
      this.#worker = new NodeWorker(workerPath, {
        ...options,
        eval: false,
        execArgv: process.execArgv,
      });

      this.#worker.on("message", (data) => {
        this.dispatchEvent(new MessageEvent("message", { data }));
      });

      this.#worker.on("messageerror", (err) => {
        this.dispatchEvent(new MessageEvent("messageerror", { data: err }));
      });

      this.#worker.on("error", (err) => {
        const ErrorEventCtor = getErrorEventCtor();
        this.dispatchEvent(new ErrorEventCtor("error", { error: err, message: err.message }));
      });

      this.#worker.on("exit", (code) => {
        this.dispatchEvent(new Event("exit"));
      });
    }

    postMessage(data, transfer) {
      this.#worker.postMessage(data, transfer);
    }

    terminate() {
      return this.#worker.terminate();
    }

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

  // `message`/`messageerror` are DELEGATED straight onto the native `parentPort`
  // (a real Node MessagePort) so Node's own C++ event-loop ref-counting governs
  // worker lifetime: a worker that never listens leaves parentPort with no
  // listeners → Node unrefs it → the worker exits naturally (matching
  // `node:worker_threads` and Bun); a worker listening via `self.onmessage` /
  // `addEventListener("message", …)` refs it → stays alive. Node reflects
  // `{once}`/`{signal}`/last-listener removal in the loop ref-count in C++,
  // which no userland counter can observe. (Earlier this block eagerly held a
  // `parentPort.on("message")` forwarder, which kept EVERY worker's event loop
  // alive → pure `parentPort` workers that should exit hung forever. See
  // wiki/research/worker-polyfill.md §4.) All OTHER event types go to a private
  // EventTarget (additive; no globalThis prototype re-parenting — `event.target`
  // is that private target, the documented minor divergence).
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
  // user listener → its parentPort wrapper (per delegated event), so
  // removeEventListener detaches the exact wrapper Node registered.
  const wrappers = { message: new Map(), messageerror: new Map() };

  function addDelegated(evt, listener, opts) {
    const cb =
      typeof listener === "function"
        ? listener
        : listener && typeof listener.handleEvent === "function"
          ? (e) => listener.handleEvent(e)
          : null;
    if (!cb) return;
    const map = wrappers[evt];
    if (map.has(listener)) return; // EventTarget dedups identical (type, listener)
    const o = opts && typeof opts === "object" ? opts : {};
    if (o.signal && o.signal.aborted) return;
    const fire = (data) => cb.call(scope, new MessageEvent(evt, { data }));
    let wrapper;
    if (o.once) {
      wrapper = (data) => {
        map.delete(listener);
        fire(data);
      };
      parentPort.once(evt, wrapper);
    } else {
      wrapper = fire;
      parentPort.on(evt, wrapper);
    }
    map.set(listener, wrapper);
    if (o.signal) {
      o.signal.addEventListener("abort", () => removeDelegated(evt, listener), {
        once: true,
      });
    }
  }
  function removeDelegated(evt, listener) {
    const wrapper = wrappers[evt].get(listener);
    if (wrapper) {
      parentPort.off(evt, wrapper);
      wrappers[evt].delete(listener);
    }
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
