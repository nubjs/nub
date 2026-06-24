// Cloudflare-Workers-shape `HTMLRewriter` global for Node.js, backed by a WASM
// build of lol-html (Cloudflare's streaming HTML rewriter). Code written against
// Cloudflare Workers' (or Bun's) HTMLRewriter ports here unchanged:
//
//   new HTMLRewriter()
//     .on("a[href]", { element(el) { el.setAttribute("rel", "noopener"); } })
//     .transform(response)
//
// ENGINE: the vendored html-rewriter-wasm build (runtime/html-rewriter-engine/),
// lol-html compiled to WebAssembly with Binaryen's Asyncify pass. One portable
// .wasm for every platform — no per-platform native prebuilds. Asyncify is what
// gives FULL ASYNC-HANDLER support: when a handler returns a Promise, the engine
// unwinds the WASM stack, awaits the Promise, then rewinds and continues the
// transform — so `engine.write()`/`engine.end()` are async and a handler may be
// `async`/return a Promise (Cloudflare/Bun parity). The engine owns parsing + the
// rewritable-unit methods; this file is the thin fluent wrapper + the WHATWG
// Response streaming bridge.

// node: builtins via process.getBuiltinModule (NOT static import) — the same loader-
// chain-leak avoidance the Worker polyfill documents: a static `import` would route
// the builtin through the user's --loader/register() hooks. On the floor where
// getBuiltinModule is absent, createRequire is threaded in via the setter below.
let _bootstrapCreateRequire = null;
export function setBootstrapCreateRequire(fn) {
  _bootstrapCreateRequire = fn;
}
function __getBuiltin(id) {
  if (typeof process.getBuiltinModule === "function") return process.getBuiltinModule(id);
  return _bootstrapCreateRequire(import.meta.url)(id);
}

// Resolve the WASM engine constructor lazily + memoized. The engine is a CommonJS
// module in nub's distribution (runtime/html-rewriter-engine/html_rewriter.js); it
// instantiates the .wasm synchronously off its own __dirname, so it never touches
// the ESM loader chain. Loaded only on the first transform — non-HTMLRewriter runs
// never pay the ~900KB .wasm instantiation.
let _engineCtor;
function getEngineCtor() {
  if (_engineCtor !== undefined) return _engineCtor;
  const { createRequire } = __getBuiltin("node:module");
  const { fileURLToPath } = __getBuiltin("node:url");
  const require = createRequire(import.meta.url);
  _engineCtor = null;
  for (const rel of [
    "./html-rewriter-engine/html_rewriter.js",
    "../runtime/html-rewriter-engine/html_rewriter.js",
  ]) {
    try {
      const mod = require(fileURLToPath(new URL(rel, import.meta.url)));
      if (mod && mod.HTMLRewriter) {
        _engineCtor = mod.HTMLRewriter;
        break;
      }
    } catch {
      // try the next candidate path
    }
  }
  return _engineCtor;
}

// Output sink for the throwaway engine used only to validate selectors at .on().
const NOOP_SINK = () => {};

function requireEngine() {
  const Engine = getEngineCtor();
  if (!Engine) {
    throw new Error(
      "HTMLRewriter: the WASM engine is unavailable (html-rewriter-engine not found).",
    );
  }
  return Engine;
}

function assertHandlers(handlers) {
  if (handlers == null || typeof handlers !== "object") {
    throw new TypeError("HTMLRewriter: handlers must be an object");
  }
  return handlers;
}

class HTMLRewriter {
  // Registrations are buffered until transform(): a fresh engine is built per
  // transform (the WASM engine is single-use — one end() per instance), so one
  // HTMLRewriter can transform multiple inputs (Cloudflare parity).
  #elementHandlers = [];
  #documentHandlers = [];

  on(selector, handlers) {
    if (typeof selector !== "string") {
      throw new TypeError("HTMLRewriter.on: selector must be a string");
    }
    assertHandlers(handlers);
    // Validate the selector eagerly so an invalid selector throws HERE, matching
    // Cloudflare's "throws at .on() registration" contract. The real engine is
    // built per-transform, but a throwaway engine parses the selector immediately
    // and surfaces the error now. free() it so the WASM instance isn't leaked.
    const Engine = getEngineCtor();
    if (Engine) {
      const probe = new Engine(NOOP_SINK);
      try {
        probe.on(selector, {});
      } finally {
        probe.free();
      }
    }
    this.#elementHandlers.push([selector, handlers]);
    return this;
  }

  onDocument(handlers) {
    assertHandlers(handlers);
    this.#documentHandlers.push(handlers);
    return this;
  }

  #buildEngine(sink) {
    const Engine = requireEngine();
    const engine = new Engine(sink);
    for (const [selector, h] of this.#elementHandlers) engine.on(selector, h);
    for (const h of this.#documentHandlers) engine.onDocument(h);
    return engine;
  }

  // Cloudflare-exact: transform a Response, returning a new streaming Response.
  transform(input) {
    if (!(input instanceof Response)) {
      throw new TypeError("HTMLRewriter.transform: input must be a Response");
    }

    const sourceBody = input.body;
    if (sourceBody == null) {
      // No body to rewrite — return an equivalent empty-body Response.
      return new Response(null, input);
    }

    const self = this;
    let engine = null;
    let reader = null;
    // Set when the consumer cancels the output stream. The source-read loop bails
    // and skips end()/close() so it can't write/enqueue into a torn-down stream.
    let cancelled = false;

    // Free the WASM engine, tolerating a held wasm-bindgen borrow. If the consumer
    // cancels WHILE an async handler is suspended (Asyncify), the suspended Rust
    // write frame still holds its RefCell borrow, so free() throws "recursive use
    // of an object". Swallowing that here is safe: the suspended write resumes when
    // its handler promise settles, its frame returns and drops the borrow, and
    // start()'s `finally{cleanup()}` then frees the engine on that resume. Without
    // the guard the throw escapes reader.cancel() (rejecting an ordinary abort) AND
    // leaks the engine.
    const safeFree = () => {
      if (!engine) return;
      try {
        engine.free();
        engine = null;
      } catch (_) {
        // Borrow still held (cancel-mid-suspend); freed on resume — see above.
      }
    };

    // Release the source reader + free the engine. Idempotent.
    const cleanup = () => {
      if (reader) {
        try {
          reader.releaseLock();
        } catch (_) {
          // already released (e.g. by reader.cancel in the cancel path)
        }
        reader = null;
      }
      safeFree();
    };

    const stream = new ReadableStream({
      async start(controller) {
        engine = self.#buildEngine((chunk) => {
          // The engine hands back a Uint8Array view into WASM memory; copy it,
          // since the buffer is reused across chunks. Skip once cancelled — the
          // controller is torn down.
          if (!cancelled && chunk && chunk.length) controller.enqueue(new Uint8Array(chunk));
        });
        reader = sourceBody.getReader();
        try {
          for (;;) {
            if (cancelled) break;
            const { done, value } = await reader.read();
            if (done || cancelled) break;
            // Async: the engine awaits any Promise a handler returns (Asyncify).
            await engine.write(value);
          }
          if (!cancelled) {
            await engine.end();
            controller.close();
          }
        } catch (err) {
          if (!cancelled) controller.error(err);
        } finally {
          cleanup();
        }
      },
      // Consumer aborted the output stream: stop reading the source body and let
      // start()'s `finally{cleanup()}` own ALL freeing. cancel() must NOT call
      // free() itself: if an async handler is in flight the write is suspended with
      // the Rust borrow held, so free() throws — but wasm-bindgen's
      // __destroy_into_raw() has ALREADY zeroed the wrapper's ptr before the
      // throwing wasm free, orphaning a still-alive Rust object (ptr=0, object
      // live). cleanup() then can only retry on a null pointer → the engine leaks
      // permanently and the shared WASM memory eventually OOBs. Deferring all
      // freeing to cleanup() — which runs only AFTER the suspended write resumes
      // and the borrow releases — frees exactly once, cleanly.
      cancel(reason) {
        cancelled = true;
        const r = reader;
        // r.cancel(reason) releases the reader lock; capture its promise first.
        const c = r ? r.cancel(reason) : undefined;
        reader = null;
        return c;
      },
    });

    // Rewriting changes byte length, so content-length must not carry over.
    const headers = new Headers(input.headers);
    headers.delete("content-length");
    return new Response(stream, {
      status: input.status,
      statusText: input.statusText,
      headers,
    });
  }
}

export function installHTMLRewriter() {
  // Forward-compat / additive: if a real HTMLRewriter is already present (a future
  // Node, or the user's own), do nothing.
  if (typeof globalThis.HTMLRewriter !== "undefined") return;
  // NON-ENUMERABLE: invisible to Object.keys(globalThis)/for-in is the additive
  // contract — vanilla-Node code enumerating the global object must not observe
  // nub's injected global. writable+configurable so user code can override it.
  Object.defineProperty(globalThis, "HTMLRewriter", {
    value: HTMLRewriter,
    enumerable: false,
    writable: true,
    configurable: true,
  });
}

// Fast tier (getBuiltinModule present): install eagerly at module eval, preserving
// the side-effect-on-require contract the lazy preload getter relies on. The engine
// (and its .wasm) is still resolved lazily on the first transform, so this costs
// ~nothing for the common "never touch HTMLRewriter" run. On the floor the compat
// preload calls setBootstrapCreateRequire(...) + installHTMLRewriter() explicitly.
if (typeof process.getBuiltinModule === "function") installHTMLRewriter();
