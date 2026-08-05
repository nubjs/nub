# Node `worker_threads` — real-world developer pain points

Survey to feed nub's `Worker` API design. nub ships both a web-style global `Worker` and the
forwarded `node:worker_threads.Worker`; this catalogs what developers actually complain about so
the surface can embrace/fix the right things. Signal is weighted by recurrence across sources +
GitHub comment volume + library-proliferation (reaction piles on single tracker issues are LOW for
this topic — the demand shows up as workaround libraries and tutorial caveats, not 👍 farming).

Each finding is tagged **[nub-could-improve]** (a Node design/DX choice an augmenting runtime can do
better, within nub's additivity rule — augment via Node's extension surfaces, never patch the clone
algorithm or Node source) vs **[fundamental-constraint]** (intrinsic to the V8-isolate /
structured-clone model; same in browser Web Workers; can only improve diagnostics/ergonomics around
it, not the semantics).

## Top 5 highest-signal complaints

1. **No built-in worker pool — the whole ecosystem works around it.** Node's own docs say "use a
   pool of Workers… otherwise the overhead of creating Workers would likely exceed their benefit,"
   yet there is no stdlib pool that recycles threads. Every CPU-bound workload pulls a dependency.
   Signal: [`piscina`](https://github.com/piscinajs/piscina) ≈5.2k★ (de-facto standard),
   [`tinypool`](https://github.com/tinylibs/tinypool) (Vitest depends on it → transitively millions
   of installs), [`poolifier`](https://github.com/poolifier/poolifier), plus `workerpool`,
   `node-worker-threads-pool`, `wise-workers`, `qoper8-wt`. A dozen competing pool libs IS the
   gripe. **[nub-could-improve]** — ship a first-class recycled-worker pool primitive.

2. **Can't pass a function/closure — you must point at a separate file.** The single most-hit wall
   and the highest-heat complaint. `workerData`/`postMessage` reject any payload containing a
   function (`DataCloneError: () => {} could not be cloned`); you can't spawn a closure the way you
   do a goroutine / Rust thread / Python target — *"you're not spawning a concurrent task with a
   closure, but launching a separate program."* The serialization limit (no shared closures across
   V8 isolates) is fundamental; the **file-pointing + protocol boilerplate** around it is the DX
   gripe that spawned the entire pool-library ecosystem.
   [Inngest blog](https://www.inngest.com/blog/node-worker-threads),
   [HN #47428117](https://news.ycombinator.com/item?id=47428117). **[fundamental-constraint]** core /
   **[nub-could-improve]** ergonomics (inline-function/eval surface, less boilerplate).

3. **ESM / TypeScript not directly runnable in a worker.** A `.ts` worker file won't run; developers
   bolt on `ts-node/esm`, `tsx`, `vite-node`, or `execArgv: ['--import', …]`. Worsened by
   [node#53195](https://github.com/nodejs/node/issues/53195) — a 22.2 regression (#52706) that
   stopped workers from registering custom ESM loader hooks. Many how-to writeups exist *because*
   it's painful. **Squarely nub's wheelhouse** — its transpiler can make `.ts`/ESM workers
   just-run. **[nub-could-improve]**

4. **~10 MB per worker + tens-of-ms startup → spawning is uneconomical; can't scale like
   goroutines.** Each worker is a full V8 isolate with its own heap and event loop. Production
   users (Inngest, Zalando) state it plainly; under memory pressure Node "simply starts killing
   worker threads." val.town benchmarked it: Node baseline 651 req/s, **worker_threads dropped it to
   426 req/s** (spawn + coordination negated the benefit) vs Deno 2,290 / Bun 2,208 / Go 5,227; a
   pre-spawned pool recovered it to ~2,209. [#34823](https://github.com/nodejs/node/issues/34823)
   ("reduce per-worker memory," closed without a real fix). The isolate memory floor is
   **[fundamental-constraint]**; the startup-time magnitude and bootstrap cost are partly
   **[nub-could-improve]** (faster bootstrap / a working startup-snapshot path mainline hasn't
   landed — [#37069](https://github.com/nodejs/node/issues/37069),
   [#43122](https://github.com/nodejs/node/issues/43122),
   [#56077](https://github.com/nodejs/node/issues/56077) all show snapshots crashing with workers).

5. **Errors don't survive the thread boundary intact — and failures are surfaced poorly.** The
   worker `'error'` event delivers a structured-clone-reconstructed error: **prototype, custom
   fields, non-enumerable props, and the original stack are dropped** — a `class FooError extends
   Error` arrives as a bare `Error` with the stack pointing into worker internals. On top of that:
   throwing a primitive gets silently rewrapped as `ERR_UNHANDLED_ERROR`
   ([#35506](https://github.com/nodejs/node/issues/35506), still **open**, `confirmed-bug`); an
   uncaught error of certain classes (e.g. `require` of a missing pkg) bypasses isolation and
   **hard-crashes the whole process** with a V8 fatal
   ([#43331](https://github.com/nodejs/node/issues/43331)); and an unhandled promise rejection in a
   worker does **not** propagate to the parent's `worker.on('error')`, so the main thread silently
   never learns the task failed ([#33834](https://github.com/nodejs/node/issues/33834)). Developers
   universally hand-roll error normalization into plain objects and re-hydrate on the other side.
   The clone-loss is **[fundamental-constraint]**; the silent rewraps, dropped stacks, broken
   isolation guarantee, and vanishing async rejections are **[nub-could-improve]** — a worker
   bootstrap wrapper (via `--import`/preload) could ship a default error-serialization shim that
   preserves `name`/custom props/stack as a structured payload, surface async rejections on
   `worker.on('error')`, and stop the FATAL-on-throw cases.

## Full ranked findings by category

### Startup cost / overhead / pooling

- **No stdlib pool** (top-5 #1). **[nub-could-improve]**
- **~10 MB/worker, 10–40 ms spawn** (top-5 #4) — each worker re-bootstraps the full Node runtime.
  [#34823](https://github.com/nodejs/node/issues/34823). **[fundamental-constraint]** /
  **[nub-could-improve]** on bootstrap time.
- **Spawn dominates for short tasks** — for a 5 ms JSON parse the spawn cost dominates; "use a pool
  or skip workers." This caveat is in every tutorial — itself the signal it bites constantly.
  **[fundamental-constraint]** (per-task math) / **[nub-could-improve]** (spawn magnitude).
- **Startup snapshots don't cleanly help workers** — the obvious lever to cut bootstrap is fragile
  with workers; multiple crash issues ([#37069](https://github.com/nodejs/node/issues/37069),
  [#43122](https://github.com/nodejs/node/issues/43122),
  [#56077](https://github.com/nodejs/node/issues/56077)). **[nub-could-improve]**

### Error handling

- **Custom error type / stack / props lost across the boundary** (top-5 #5).
  [#26692](https://github.com/nodejs/node/issues/26692), upstream root cause
  [whatwg/html#5665](https://github.com/whatwg/html/issues/5665) (structured cloning normalizes
  `Error.name`). `error.stack` is a hidden getter only on builtin Errors → returns `undefined` on a
  cloned error, the mechanism behind "lost stack trace." **[fundamental-constraint]** loss /
  **[nub-could-improve]** surfacing.
- **Uncaught exception can hard-crash the whole process (FATAL)** — contradicts the isolation
  guarantee. [#43331](https://github.com/nodejs/node/issues/43331); OOM variant
  [#47224](https://github.com/nodejs/node/issues/47224). **[nub-could-improve]**
- **`uncaughtException` / `unhandledRejection` in a worker behaves unintuitively** — async
  rejections don't reach the parent's `worker.on('error')`; you can't rely on the main process's
  handler. [#33834](https://github.com/nodejs/node/issues/33834). **[nub-could-improve]**
- **Throwing a primitive → silently rewrapped `ERR_UNHANDLED_ERROR`** — regression since v14.7.0,
  still open, `confirmed-bug`, highest comment count of the bug reports.
  [#35506](https://github.com/nodejs/node/issues/35506). **[nub-could-improve]**
- **Error serialization can itself fail → you lose the error entirely** (`ERR_WORKER_UNSERIALIZABLE_ERROR`
  if the `stack` getter throws). [#26145](https://github.com/nodejs/node/pull/26145) (21 comments —
  most-discussed in this set). **[nub-could-improve]**
- **`messageerror` deserialization failures give no useful stack** — you just get "the error," not
  what inside the object failed to clone. Documented in the worker_threads docs. **[nub-could-improve]**
- **Un-cloneable value in the `Worker` constructor doesn't fail fast — the process keeps living**
  instead of throwing (vs `v8.serialize()` which throws normally).
  [#22736](https://github.com/nodejs/node/issues/22736) (3 👍, highest reaction count in the error
  set). **[nub-could-improve]**

### Messaging / structured-clone / MessagePort

- **Can't send functions** (top-5 #2). **[fundamental-constraint]** (spec; same in browsers) /
  **[nub-could-improve]** on the error message + a documented escape hatch.
- **Class instances arrive as plain objects — prototype/methods silently lost.** Worse than
  functions because it's *silent*: no error, the object explodes later when a now-missing method is
  called. Demonstrated in Node's own docs. [nodejs/help#1558](https://github.com/nodejs/help/issues/1558).
  **[fundamental-constraint]** (spec) / **[nub-could-improve]** at the margins (warn surface).
- **Serialization overhead on large payloads — data lives in both heaps at once**; the
  serialize/deserialize cost can exceed the work saved. The dominant *performance* messaging
  complaint. **[fundamental-constraint]** mostly.
- **`transferList` friction** — forget to list an `ArrayBuffer` and it's silently deep-copied (no
  error, just slow); after transfer the buffer is detached in the sender (surprise `byteLength===0`);
  the **Buffer-pool footgun** — `Buffer.from()`/`alloc()` buffers come from a shared pool, can't be
  transferred (always clone), and transferring naively sends the *whole pool* (memory bloat + leak
  concern). [Advanced Web Machinery](https://advancedweb.hu/how-to-transfer-binary-data-efficiently-across-worker-threads-in-nodejs/).
  **[nub-could-improve]** — warn on large-ArrayBuffer-cloned-not-transferred.
- **`MessagePort.start()` / auto-start divergence from spec** — Node auto-`start()`s on attaching a
  `'message'` listener, but `addEventListener('message')` vs `port.onmessage=` behave differently
  from browsers, and *removing* `onmessage` calls `stopMessagePort()` so the port resumes buffering;
  a narrow window after `close()` throws `Cannot send data on closed MessagePort`. Node-specific
  divergence. [#26463](https://github.com/nodejs/node/issues/26463),
  [#42296](https://github.com/nodejs/node/issues/42296). **[nub-could-improve]**
- **SharedArrayBuffer/Atomics is the only zero-copy escape — but holds only numbers**, can't be in
  `transferList`, and needs hand-rolled serialize-into-typed-array + `Atomics`. Powerful-but-painful.
  **[fundamental-constraint]** / **[nub-could-improve]** via an ergonomic object-over-SAB layer.
- **No first-class sync structured-clone** — historically `v8.serialize/deserialize` (crosses the
  JS/C++ boundary twice) or a `MessageChannel` dance; largely resolved by the global
  `structuredClone()`, but the worker-message path still pays serialize/deserialize.
  [#34355](https://github.com/nodejs/node/issues/34355). mostly **[Node-design-gripe]**, partly
  addressed.
- **Unsupported-type-at-runtime surprises** — `URL`, sockets, timers, event objects throw
  `DataCloneError` only at runtime; no static signal a payload is un-sendable. Node added
  `markAsUncloneable()`/`markAsUntransferable()` to *opt out*, nothing to make common cases just
  work. [threads.js#233](https://github.com/andywer/threads.js/issues/233).
  **[fundamental-constraint]** per-type / **[nub-could-improve]** earlier diagnostics.

### Lifecycle

- **`terminate()` can't interrupt running sync code / hung workers** — async, returns a Promise, but
  a worker in a sync loop or holding an open native/async handle won't stop promptly.
  [#34567](https://github.com/nodejs/node/issues/34567),
  [undici#2026](https://github.com/nodejs/undici/issues/2026) (`fetch` in a worker → "stuck and
  process hang"), [help#3332](https://github.com/nodejs/help/issues/3332) (doesn't kill the worker's
  child processes). Largely **[fundamental-constraint]** (V8 can't cleanly preempt arbitrary JS).
- **`ref()`/`unref()` confusion** — whether `worker.unref()` lets the process exit depends on where
  `worker.on('message')` appears relative to the `unref()` call (attaching a listener silently
  re-`ref()`s the port). Order-dependent, surprising.
  [#53036](https://github.com/nodejs/node/issues/53036). **[nub-could-improve]**
- **`'online'`/`'exit'` of marginal use** — little complaint *and* little praise; `'online'` rarely
  used; `'exit'`'s exitCode is the hook people actually rely on. Low signal — flagging absence of
  enthusiasm, not a loud gripe.

### EventEmitter-vs-web shape (the API-shape debate)

- **EE-vs-web shape & `MessageEvent.data` mismatch** — Node's `MessagePort` does **not** inherit
  `EventTarget` (only `.on('message')`/`.onmessage`, no `addEventListener`), and the raw value
  arrives **unwrapped** instead of as a `MessageEvent` with `.data`. History of churn:
  [#35835](https://github.com/nodejs/node/issues/35835) (`parentPort` flipped
  EventTarget→EventEmitter between Node 12 and 14 — a breaking change),
  [DefinitelyTyped#52340](https://github.com/DefinitelyTyped/DefinitelyTyped/issues/52340) (types
  don't expose `EventTarget`, forcing `@ts-ignore`). **[nub-could-improve]** — expose a web-standard
  `Worker` global + `MessageEvent` wrapping like Deno/Bun (which nub already does).

### ESM / TS / eval / workerData / tooling

- **ESM/TS workers don't just-run** (top-5 #3). **[nub-could-improve]**
- **Boilerplate / verbosity** — separate file + protocol + wiring + crash handling + clean shutdown
  for even simple jobs. *"Someone is going to make the socket.io of workers someday, because this is
  verbose!"* ([threads.js HN](https://news.ycombinator.com/item?id=27252706)). The whole pool-lib
  category exists to absorb it. **[nub-could-improve]**
- **`workerData` limitations** — also structured-cloned (throws on functions) and effectively
  set-once at construction; loggers/callbacks/config-with-functions must stay on the main thread.
  **[fundamental-constraint]**
- **Bundler can't detect the worker entry** — webpack only statically detects
  `new Worker(new URL('./w.js', import.meta.url))`; any indirection (a variable, template literal)
  breaks detection, making library-shipped workers "a nightmare." nub's own resolver could sidestep
  it. **[nub-could-improve]**

## Developer sentiment toward the EventEmitter shape

**The vocal, signal-bearing sentiment leans clearly toward "wish it were web-standard / portable" —
almost nobody actively defends the EventEmitter shape as a virtue.** The strongest evidence is not
loud complaint threads (the shape mismatch is a steady B-tier irritant, not a flame war) — it's the
**ecosystem of shim libraries whose entire reason to exist is web-Worker compatibility**:
[developit/web-worker](https://github.com/developit/web-worker) (1.2k★, "Consistent Web Workers in
browser and Node," sells DOM-style `Event.data`/`Event.type` and `worker.onmessage=`),
[jimmywarting/whatwg-worker](https://github.com/jimmywarting/whatwg-worker),
[andywer/threads.js](https://github.com/andywer/threads.js),
[bthreads](https://github.com/chjj/bthreads). People keep building the web-standard surface Node
declined to ship. The "`.on('message')` is idiomatic/familiar" framing exists only in tutorial/doc
prose — never as a developer actively praising it over the web shape. Deno and Bun both ship the
**web-standard `Worker` global**, making Node the odd one out.

**Critical caveat for nub's design priorities:** the API *shape* is NOT the top pain point. The
structured-clone "can't pass a function / must point to a separate file" ergonomics (#2) and ESM/TS
friction (#3) generate far more heat than `onmessage`-vs-`.on('message')`. So shipping a
web-standard `Worker` global is a correct, low-controversy portability win — but the A-tier
developer pain (and nub's biggest leverage as a TS-first augmenting runtime) is **TS/ESM workers
that just-run, inline-function/closure ergonomics over the eval surface, less boilerplate, and a
built-in pool** — not the event-API shape per se. Embrace the web shape *and* attack the ergonomics.

## Where the leverage is for nub

- **[nub-could-improve] / wheelhouse:** built-in recycled worker pool (#1); TS/ESM workers that
  just-run (#3); inline-function/closure ergonomics + less boilerplate over the eval surface (#2);
  web-standard `Worker` global + `MessageEvent.data` for portability (shape debate); a default
  error-serialization shim that preserves name/props/stack and surfaces async rejections on
  `worker.on('error')` (#5); turn the silent/late clone failures (prototype loss, forgotten
  transfer, runtime-only throws) into early, field-pointing diagnostics; saner `ref`/`unref`; worker
  resolution without bundler magic.
- **[fundamental-constraint] / don't fight the model:** no shared closures across isolates (#2
  core), per-isolate ~10 MB memory floor (#4), structured-clone semantics (function/prototype loss,
  `workerData`), `terminate()` preemption limits, SAB-is-bytes. nub's lever here is *diagnostics and
  ergonomic helper layers*, never changing clone/isolate semantics (additivity rule).

## Changelog

- 2026-06-30 — Initial write-up.
