# Node `worker_threads` — real-world developer pain points

Survey to feed Nub's `Worker` API design. Nub ships both a web-style global `Worker` and the forwarded `node:worker_threads.Worker`; this catalogs what developers complain about so the surface can embrace or fix the right things.

Signal is weighted by recurrence across sources + GitHub comment volume + library proliferation — reaction piles on single tracker issues are LOW for this topic, because the demand shows up as workaround libraries and tutorial caveats.

Findings intrinsic to the V8-isolate / structured-clone model are tagged **[fundamental-constraint]**: they behave the same way in browser Web Workers, and only the diagnostics and ergonomics around them can change, never the semantics. Everything untagged is a Node design or DX choice rather than a property of the isolate model.

## Top 5 highest-signal complaints

Ranked by recurrence: the missing stdlib pool, the closure-versus-separate-file wall, TS and ESM workers that will not run, per-worker memory and spawn cost, and errors that do not survive the thread boundary.

1. **No built-in worker pool — the whole ecosystem works around it.** Node's own docs say "use a pool of Workers… otherwise the overhead of creating Workers would likely exceed their benefit," yet there is no stdlib pool that recycles threads. Every CPU-bound workload pulls a dependency. Signal: [`piscina`](https://github.com/piscinajs/piscina) ≈5.2k★ (de-facto standard), [`tinypool`](https://github.com/tinylibs/tinypool) (Vitest depends on it → transitively millions of installs), [`poolifier`](https://github.com/poolifier/poolifier), plus `workerpool`, `node-worker-threads-pool`, `wise-workers`, `qoper8-wt`. A dozen competing pool libs IS the gripe.

2. **Can't pass a function/closure — you must point at a separate file.** The single most-hit wall and the highest-heat complaint. `workerData`/`postMessage` reject any payload containing a function (`DataCloneError: () => {} could not be cloned`); you can't spawn a closure the way you do a goroutine / Rust thread / Python target — *"you're not spawning a concurrent task with a closure, but launching a separate program."* The serialization limit (no shared closures across V8 isolates) is fundamental; the **file-pointing + protocol boilerplate** around it is the DX gripe that spawned the entire pool-library ecosystem. [Inngest blog](https://www.inngest.com/blog/node-worker-threads), [HN #47428117](https://news.ycombinator.com/item?id=47428117). **[fundamental-constraint]** at the core; the file-pointing and protocol boilerplate stacked on top of it is not.

3. **ESM / TypeScript not directly runnable in a worker.** A `.ts` worker file won't run; developers bolt on `ts-node/esm`, `tsx`, `vite-node`, or `execArgv: ['--import', …]`. Worsened by [node#53195](https://github.com/nodejs/node/issues/53195) — a 22.2 regression (#52706) that stopped workers from registering custom ESM loader hooks. Many how-to writeups exist *because* it's painful. Nub transpiles a worker entry like any other file, so a `.ts` worker runs with no loader wiring.

4. **~10 MB per worker + tens-of-ms startup → spawning is uneconomical; can't scale like goroutines.** Each worker is a full V8 isolate with its own heap and event loop. Production users (Inngest, Zalando) state it plainly; under memory pressure Node "simply starts killing worker threads." val.town benchmarked it: Node baseline 651 req/s, **worker_threads dropped it to 426 req/s** (spawn + coordination negated the benefit) vs Deno 2,290 / Bun 2,208 / Go 5,227; a pre-spawned pool recovered it to ~2,209. [#34823](https://github.com/nodejs/node/issues/34823) ("reduce per-worker memory," closed without a real fix). The isolate memory floor is **[fundamental-constraint]**; the startup-time magnitude tracks bootstrap cost, and no working startup-snapshot path has landed in mainline ([#37069](https://github.com/nodejs/node/issues/37069), [#43122](https://github.com/nodejs/node/issues/43122), [#56077](https://github.com/nodejs/node/issues/56077) all show snapshots crashing with workers).

5. **Errors don't survive the thread boundary intact — and failures are surfaced poorly.** The worker `'error'` event delivers a structured-clone-reconstructed error: **prototype, custom fields, non-enumerable props, and the original stack are dropped** — a `class FooError extends Error` arrives as a bare `Error` with the stack pointing into worker internals. On top of that: throwing a primitive gets silently rewrapped as `ERR_UNHANDLED_ERROR` ([#35506](https://github.com/nodejs/node/issues/35506), still **open**, `confirmed-bug`); an uncaught error of certain classes (e.g. `require` of a missing pkg) bypasses isolation and **hard-crashes the whole process** with a V8 fatal ([#43331](https://github.com/nodejs/node/issues/43331)); and an unhandled promise rejection in a worker does **not** propagate to the parent's `worker.on('error')`, so the main thread silently never learns the task failed ([#33834](https://github.com/nodejs/node/issues/33834)). Developers universally hand-roll error normalization into plain objects and re-hydrate on the other side. The clone-loss is **[fundamental-constraint]**; the silent rewraps, the dropped stacks, the broken isolation guarantee, and the vanishing async rejections are not.

## Full ranked findings by category

Six categories — startup, errors, messaging, lifecycle, API shape, tooling — with every item the isolate model settles tagged **[fundamental-constraint]**.

### Startup cost / overhead / pooling

Four items, all downstream of each worker re-bootstrapping a full Node runtime: no stdlib pool, the per-worker memory and spawn cost, the short-task math, and startup snapshots that crash with workers.

- **No stdlib pool** (top-5 #1). **[nub-could-improve]**
- **~10 MB/worker, 10–40 ms spawn** (top-5 #4) — each worker re-bootstraps the full Node runtime. [#34823](https://github.com/nodejs/node/issues/34823). **[fundamental-constraint]** / **[nub-could-improve]** on bootstrap time.
- **Spawn dominates for short tasks** — for a 5 ms JSON parse the spawn cost dominates; "use a pool or skip workers." This caveat is in every tutorial — itself the signal it bites constantly. **[fundamental-constraint]** (per-task math) / **[nub-could-improve]** (spawn magnitude).
- **Startup snapshots don't cleanly help workers** — the obvious lever to cut bootstrap is fragile with workers; multiple crash issues ([#37069](https://github.com/nodejs/node/issues/37069), [#43122](https://github.com/nodejs/node/issues/43122), [#56077](https://github.com/nodejs/node/issues/56077)). **[nub-could-improve]**

### Error handling

Seven items. The clone loss is spec; the silent rewraps, the process-wide FATAL crashes, and the async rejections that never reach the parent are not.

- **Custom error type / stack / props lost across the boundary** (top-5 #5). [#26692](https://github.com/nodejs/node/issues/26692), upstream root cause [whatwg/html#5665](https://github.com/whatwg/html/issues/5665) (structured cloning normalizes `Error.name`). `error.stack` is a hidden getter only on builtin Errors → returns `undefined` on a cloned error, the mechanism behind "lost stack trace." **[fundamental-constraint]** loss / **[nub-could-improve]** surfacing.
- **Uncaught exception can hard-crash the whole process (FATAL)** — contradicts the isolation guarantee. [#43331](https://github.com/nodejs/node/issues/43331); OOM variant [#47224](https://github.com/nodejs/node/issues/47224). **[nub-could-improve]**
- **`uncaughtException` / `unhandledRejection` in a worker behaves unintuitively** — async rejections don't reach the parent's `worker.on('error')`; you can't rely on the main process's handler. [#33834](https://github.com/nodejs/node/issues/33834). **[nub-could-improve]**
- **Throwing a primitive → silently rewrapped `ERR_UNHANDLED_ERROR`** — regression since v14.7.0, still open, `confirmed-bug`, highest comment count of the bug reports. [#35506](https://github.com/nodejs/node/issues/35506). **[nub-could-improve]**
- **Error serialization can itself fail → you lose the error entirely** (`ERR_WORKER_UNSERIALIZABLE_ERROR` if the `stack` getter throws). [#26145](https://github.com/nodejs/node/pull/26145) (21 comments — most-discussed in this set). **[nub-could-improve]**
- **`messageerror` deserialization failures give no useful stack** — you just get "the error," not what inside the object failed to clone. Documented in the worker_threads docs. **[nub-could-improve]**
- **Un-cloneable value in the `Worker` constructor doesn't fail fast — the process keeps living** instead of throwing (vs `v8.serialize()` which throws normally). [#22736](https://github.com/nodejs/node/issues/22736) (3 👍, highest reaction count in the error set). **[nub-could-improve]**

### Messaging / structured-clone / MessagePort

Eight items, from the function and prototype losses the clone spec mandates, through Node-specific `MessagePort` divergence, to the Buffer-pool transfer footgun.

- **Can't send functions** (top-5 #2). **[fundamental-constraint]** (spec; same in browsers) / **[nub-could-improve]** on the error message + a documented escape hatch.
- **Class instances arrive as plain objects — prototype/methods silently lost.** Worse than functions because it's *silent*: no error, the object explodes later when a now-missing method is called. Demonstrated in Node's own docs. [nodejs/help#1558](https://github.com/nodejs/help/issues/1558). **[fundamental-constraint]** (spec) / **[nub-could-improve]** at the margins (warn surface).
- **Serialization overhead on large payloads — data lives in both heaps at once**; the serialize/deserialize cost can exceed the work saved. The dominant *performance* messaging complaint. **[fundamental-constraint]** mostly.
- **`transferList` friction** — forget to list an `ArrayBuffer` and it's silently deep-copied (no error, just slow); after transfer the buffer is detached in the sender (surprise `byteLength===0`); the **Buffer-pool footgun** — `Buffer.from()`/`alloc()` buffers come from a shared pool, can't be transferred (always clone), and transferring naively sends the *whole pool* (memory bloat + leak concern). [Advanced Web Machinery](https://advancedweb.hu/how-to-transfer-binary-data-efficiently-across-worker-threads-in-nodejs/). **[nub-could-improve]** — warn on large-ArrayBuffer-cloned-not-transferred.
- **`MessagePort.start()` / auto-start divergence from spec** — Node auto-`start()`s on attaching a `'message'` listener, but `addEventListener('message')` vs `port.onmessage=` behave differently from browsers, and *removing* `onmessage` calls `stopMessagePort()` so the port resumes buffering; a narrow window after `close()` throws `Cannot send data on closed MessagePort`. Node-specific divergence. [#26463](https://github.com/nodejs/node/issues/26463), [#42296](https://github.com/nodejs/node/issues/42296). **[nub-could-improve]**
- **SharedArrayBuffer/Atomics is the only zero-copy escape — but holds only numbers**, can't be in `transferList`, and needs hand-rolled serialize-into-typed-array + `Atomics`. **[fundamental-constraint]** / **[nub-could-improve]** via an ergonomic object-over-SAB layer.
- **No first-class sync structured-clone** — historically `v8.serialize/deserialize` (crosses the JS/C++ boundary twice) or a `MessageChannel` dance; largely resolved by the global `structuredClone()`, but the worker-message path still pays serialize/deserialize. [#34355](https://github.com/nodejs/node/issues/34355). Mostly **[Node-design-gripe]**, partly addressed.
- **Unsupported-type-at-runtime surprises** — `URL`, sockets, timers, event objects throw `DataCloneError` only at runtime; no static signal a payload is un-sendable. Node added `markAsUncloneable()`/`markAsUntransferable()` to *opt out*, nothing to make common cases just work. [threads.js#233](https://github.com/andywer/threads.js/issues/233). **[fundamental-constraint]** per-type / **[nub-could-improve]** earlier diagnostics.

### Lifecycle

Three items: termination cannot preempt running sync code, `ref`/`unref` behavior depends on listener order, and the lifecycle events draw neither complaint nor praise.

- **`terminate()` can't interrupt running sync code / hung workers** — async, returns a Promise, but a worker in a sync loop or holding an open native/async handle won't stop promptly. [#34567](https://github.com/nodejs/node/issues/34567), [undici#2026](https://github.com/nodejs/undici/issues/2026) (`fetch` in a worker → "stuck and process hang"), [help#3332](https://github.com/nodejs/help/issues/3332) (doesn't kill the worker's child processes). Largely **[fundamental-constraint]** (V8 can't cleanly preempt arbitrary JS).
- **`ref()`/`unref()` confusion** — whether `worker.unref()` lets the process exit depends on where `worker.on('message')` appears relative to the `unref()` call (attaching a listener silently re-`ref()`s the port). Order-dependent, surprising. [#53036](https://github.com/nodejs/node/issues/53036). **[nub-could-improve]**
- **`'online'`/`'exit'` of marginal use** — little complaint *and* little praise; `'online'` rarely used; `'exit'`'s exitCode is the hook people actually rely on. Low signal — flagging absence of enthusiasm, not a loud gripe.

### EventEmitter-vs-web shape (the API-shape debate)

One item with a churn history: Node's `MessagePort` is not an `EventTarget`, and a message arrives unwrapped rather than as a `MessageEvent`.

- **EE-vs-web shape & `MessageEvent.data` mismatch** — Node's `MessagePort` does **not** inherit `EventTarget` (only `.on('message')`/`.onmessage`, no `addEventListener`), and the raw value arrives **unwrapped** instead of as a `MessageEvent` with `.data`. History of churn: [#35835](https://github.com/nodejs/node/issues/35835) (`parentPort` flipped EventTarget→EventEmitter between Node 12 and 14 — a breaking change), [DefinitelyTyped#52340](https://github.com/DefinitelyTyped/DefinitelyTyped/issues/52340) (types don't expose `EventTarget`, forcing `@ts-ignore`). **[nub-could-improve]** — expose a web-standard `Worker` global + `MessageEvent` wrapping like Deno/Bun (which Nub already does).

### ESM / TS / eval / workerData / tooling

Four items where the friction is tooling rather than isolate semantics: TS and ESM workers, protocol boilerplate, set-once `workerData`, and bundler entry detection.

- **ESM/TS workers don't just-run** (top-5 #3). **[nub-could-improve]**
- **Boilerplate / verbosity** — separate file + protocol + wiring + crash handling + clean shutdown for even simple jobs. *"Someone is going to make the socket.io of workers someday, because this is verbose!"* ([threads.js HN](https://news.ycombinator.com/item?id=27252706)). The whole pool-lib category exists to absorb it. **[nub-could-improve]**
- **`workerData` limitations** — also structured-cloned (throws on functions) and effectively set-once at construction; loggers/callbacks/config-with-functions must stay on the main thread. **[fundamental-constraint]**
- **Bundler can't detect the worker entry** — webpack only statically detects `new Worker(new URL('./w.js', import.meta.url))`; any indirection (a variable, template literal) breaks detection, making library-shipped workers "a nightmare." Nub's own resolver could sidestep it. **[nub-could-improve]**

## Developer sentiment toward the EventEmitter shape

The vocal, signal-bearing sentiment leans clearly toward "wish it were web-standard / portable" — almost nobody defends the EventEmitter shape as a virtue.

The strongest evidence is not loud complaint threads (the shape mismatch is a steady B-tier irritant, not a flame war) — it is the **ecosystem of shim libraries whose entire reason to exist is web-Worker compatibility**: [developit/web-worker](https://github.com/developit/web-worker) (1.2k★, "Consistent Web Workers in browser and Node," sells DOM-style `Event.data`/`Event.type` and `worker.onmessage=`), [jimmywarting/whatwg-worker](https://github.com/jimmywarting/whatwg-worker), [andywer/threads.js](https://github.com/andywer/threads.js), [bthreads](https://github.com/chjj/bthreads). People keep building the web-standard surface Node declined to ship. The "`.on('message')` is idiomatic/familiar" framing exists only in tutorial/doc prose — never as a developer actively praising it over the web shape. Deno and Bun both ship the **web-standard `Worker` global**, making Node the odd one out.


## Changelog

Every revision to this document, with the date and what changed.

- 2026-06-30 — Initial write-up.
- 2026-08-28 — Removed the product-direction section; the survey stands on its own.
