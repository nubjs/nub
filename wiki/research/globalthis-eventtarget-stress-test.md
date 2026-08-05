# `globalThis` as `EventTarget` — stress-testing the rejection rationale against Bun's lived experience

**Date:** 2026-05-24 **Question:** The prior research at [`wintertc-node-gap-rationale.md`](wintertc-node-gap-rationale.md) concluded Node's rejection of `globalThis instanceof EventTarget` and the `on*` handler properties was a "principled architectural no" backed by unresolvable dual-channel semantics. The counter-argument: **Bun ships them. If Bun ships them without catastrophe, the "unresolvable" framing has to be wrong or at least overstated.** Verify the claim against actual maintainer comments, actual Bun source, and actual Bun/Deno/Workers bug history. **Drives the decision in:** `../runtime/min-common-api-globals.md`. **Extends/stress-tests:** [`wintertc-node-gap-rationale.md`](wintertc-node-gap-rationale.md) (write-once, not amended).

---

## 1. TL;DR

- **Bun has not shipped this without catastrophe — it has shipped this *with* catastrophe, just gradually and in pieces small enough that nobody calls it that.** Every concrete failure-mode the Node maintainers cited has a corresponding open or recently-fixed Bun issue: process-hang on `globalThis.onmessage = fn` ([Bun #24256](https://github.com/oven-sh/bun/issues/24256), fixed May 2026 in [PR #30586](https://github.com/oven-sh/bun/pull/30586) by adding an `isWorker` guard); `event.preventDefault()` on `globalThis.addEventListener('error', ...)` not actually preventing default ([Bun #29043](https://github.com/oven-sh/bun/issues/29043), open); re-entrant `uncaughtException` crashing the runtime ([Bun #28648](https://github.com/oven-sh/bun/issues/28648), fix May 2026); `process.on('uncaughtException')` not firing for years ([Bun #5219](https://github.com/oven-sh/bun/issues/5219) → [#429](https://github.com/oven-sh/bun/issues/429)); `process.nextTick` uncaught exceptions firing handlers multiple times and exiting with code 0 ([Bun PR #27229](https://github.com/oven-sh/bun/pull/27229), 2026). The "unresolvable semantics" framing is rhetorically too strong — Bun and Deno have both resolved them with specific design choices — but the engineering cost of resolving them is multi-year and ongoing, and the bug shape matches Node maintainer predictions one-for-one. *Strengthened* the prior research's conclusion.
- **Deno made a different design choice than Bun and it has been the better one.** Deno's `globalThis` is the canonical event bus; Deno's `process.on('unhandledRejection')` polyfill is implemented *as a listener on `globalThis.addEventListener('unhandledrejection')` that calls `event.preventDefault()` and re-emits on `process`* — see [`denoland/deno/ext/node/polyfills/process.ts`](https://github.com/denoland/deno/blob/main/ext/node/polyfills/process.ts). The design choice is "globalThis is canonical, process is the forwarder." This works in Deno because Deno owns `process` (it's a polyfill they wrote). **Nub cannot make this choice** — Node owns `process` and the entire `process.on('uncaughtException')` ecosystem will exist regardless of Nub, on top of code Nub does not control.
- **The right characterization of the Node maintainer position is "the dual-channel design has resolvable semantics, but the resolution has cost we don't want to pay for limited benefit."** Not "unresolvable." The prior research's framing should be tightened on this point. mcollina's "I don't think we'll ever be able to migrate from process events to globalThis" is a *migration-strategy* objection, not an architectural-impossibility objection. The TSC consensus (mhdawson, Jan 2023: *"value does not outweigh the issues/problems"*) is a cost-benefit call, not a logical-impossibility call.
- **For Nub specifically, the technical operation is cheap (one `Object.setPrototypeOf` + three handler accessors, ~10 LOC), but the wiring is the expensive part.** Three options: (A) don't ship; (B) prototype swap with no `process` wiring (handlers exist but never fire); (C) prototype swap + wire `process.on('uncaughtException')` → `globalThis.dispatchEvent(new ErrorEvent(...))`. Option B is worse than (A) — it converts `typeof globalThis.onerror === 'undefined'` (correct on Node) into a setter that swallows handlers and never fires them. Option C imports Bun's bug class — including the specific cross-channel `preventDefault` failure ([#29043](https://github.com/oven-sh/bun/issues/29043)) that has been open since the design landed.
- **Recommendation: (A) Stand by the prior decision, with a tightened rationale.** The new evidence does not reverse the prior recommendation; it concretizes and strengthens it. Hold the line on shipping `globalThis instanceof EventTarget` and the three `on*` handler attributes. Document the Bun bug-history as the concrete evidence (not "unresolvable semantics" hand-waving). Keep the future `--globalthis-eventtarget` opt-in flag deferred to v0.x as the userland escape hatch; ship the one-liner `Object.setPrototypeOf(globalThis, new EventTarget())` as documented userland preload guidance for anyone who really wants it.

---

## 2. The Node maintainer objections, concretely

This section pulls the **specific failure scenarios** each maintainer cited, with comment-URL citations. The objections fall into four buckets: (a) cross-channel ordering / `preventDefault` ambiguity, (b) listener-count and feature-probe hazards, (c) `process` migration strategy concerns, (d) the "what would browsers do today" priors objection.

### 2.1 Cross-channel ordering and `preventDefault` ambiguity (jasnell, benjamingr)

**jasnell, [nodejs/node#57352 comment 2025-03-07](https://github.com/nodejs/node/issues/57352)** posed the concrete questions explicitly, and the thread could not converge on answers:

> One case where it gets a bit tricky is with `process.on('error', ...)`. The `'error'` events are the only ones we have that propagate up... if I have an EventEmitter `foo` and it does not have an `'error'` event handler, that gets bumped to `process.on('error', ....)` if it exists, and if it does not, then it is forwarded to `process.on('uncaughtException', ...)` or otherwise thrown as an uncaught exception. Where would `globalThis.addEventListener('error', ...)` fall within that flow? Or would it at all? If someone only [has] a `globalThis.addEventListener('error', ...)` would that be enough to stop the propagation to an unhandled error or would the globalThis error handler not matter in the typical flow of an EventEmitter error?

> What would y'all expect the behavior to be if someone registers *both* process event and global event handlers? Which order should they be invoked in: (a) `process` first then `globalThis`, (b) `globalThis` first then `process`, (c) only [one] or the other? If option (b) and the user code requests to stop event propagation using, for instance, `preventDefault()` or `stopImmediatePropagation()` should that prevent the process event handler from being fired?

> If I call `globalThis.dispatchEvent(new ErrorEvent('error', ...))` would you expect both `globalThis.addEventListener('error', ...)` *and* `process.on('error', ...)` to be called? Likewise, if I called `process.emit('error', ...)` would you expect both the `globalThis` and `process` error handlers to be called?

The thread split: ljharb said globalThis should be "the parent" (process fires first, then globalThis); ShogunPanda proposed an explicit opt-in `process.useGlobalThisForEvents()` switch; ljharb then argued the switch is unworkable because packages and application code must be free to use either side. **No convergence.**

**benjamingr, [nodejs/node#57352 comment 2025-03-07](https://github.com/nodejs/node/issues/57352#issuecomment-2706598290):**

> Does `e.stopImmediatePropagation` on `error` prevent `"uncaughtException"` from firing? What order are events fired at? Does removing all listeners from `uncaughtException` impact `error`? How do we reconcile the different design decisions in `EventTarget` and `EventEmitter` without complicating the error handling story significantly for end users?

### 2.2 Listener-count / feature-probe hazards (jasnell, benjamingr)

**jasnell, [nodejs/node#57352](https://github.com/nodejs/node/issues/57352)**:

> When a developer ends up using a `globalThis` listener for, say, `'error'` events but not a `process` listener, the `process.listenerCount('error')` would return `0`, possibly giving the false impression that there are no error event handlers when there actually might be.

This is concrete: `process.listenerCount('uncaughtException')` is widely used in real Node code (graceful shutdown libraries, test frameworks like vitest/jest, the Sentry SDK) to decide whether the runtime will crash on the next throw. Any design that lets `globalThis.addEventListener('error', ...)` count as an error handler without making it visible to `process.listenerCount` produces silent behavior changes in those libraries.

**benjamingr, [nodejs/node#57352](https://github.com/nodejs/node/issues/57352#issuecomment-2702951170):**

> this further complicates our already complicated error handling story with more events and orders which can negatively impact usability. This is made worse by libraries possibly checking for `globalThis` being an event emitter as a potential probe and adjust their error handling accordingly (since the error model in browsers is different than our own).

The probe hazard: `if (globalThis instanceof EventTarget) { /* use web error model */ } else { /* use process.on */ }` is exactly the cross-runtime detection libraries write today. Shipping the swap with broken or partial wiring makes the probe lie.

### 2.3 The `process` migration objection (mcollina, repeatedly)

**mcollina, [nodejs/node#51372 comment 2024-01-04](https://github.com/nodejs/node/issues/51372#issuecomment-1876608028):**

> I'm -1 as I think it would add more confusion. None of those mechanisms are correct for library authors. Most libraries should not touch global objects, because they can conflict in behaviors.

**mcollina, [nodejs/node#57352 comment 2025-03-06](https://github.com/nodejs/node/issues/57352#issuecomment-2702434430):**

> I don't think we'll ever be able to migrate from process events to globalThis. We need to figure out a strategy where those can coexist.

The "coexist" framing — **not "this is unresolvable"** but "this requires a migration strategy we don't have." Important distinction; the prior research overstated this as "unresolvable."

**mcollina, [nodejs/node#45993 comment 2023-01-01](https://github.com/nodejs/node/pull/45993#issuecomment-1368380538):**

> libraries emitting global events make the ecosystem more brittle.

### 2.4 The "browsers wouldn't do this today" priors objection (addaleax)

**addaleax, [nodejs/node#45993 comment 2022-12-31](https://github.com/nodejs/node/pull/45993#issuecomment-1368253966):**

> Do we feel like this is something we *want* in Node.js? I understand that this is something that other platforms do, but those platforms aren't Node.js and didn't have things like the `process` object (which is effectively where we emit all our "global" events) from the start. ... I'm not even sure that browsers would do this nowadays with the benefit of hindsight in how JS developed as a language.

This is the weakest of the objections (it's a vibes argument) but it's worth recording because it shapes the TSC consensus: the bar for "should we ship this web API on Node" is *not* "is it a web API," it's "is it a web API we'd choose to ship today, given what we now know."

### 2.5 The most nuanced objection (joyeecheung)

**joyeecheung, [nodejs/node#45993 comment 2023-01-02](https://github.com/nodejs/node/pull/45993#issuecomment-1369081420):**

> out of the common events fired on `window` in the browsers, the ones that make more sense for Node.js are `message`, `unhandledrejection` (note the cases here), `rejectionhandled`, which we already (sort of) fire on `process`, but with disparities in the shapes of the event objects. It seems to be confusing to make `globalThis` in Node.js an `EventTarget` without a good story or at least documentation of how we are going to handle these disparities or even whether we are going to support these Web events in our `globalThis` at all. ... Users might attempt to use these well-specified events in `globalThis` and realize that it doesn't actually work like how things are in browsers, which IMO is worse than not making `globalThis` an `EventTarget`.

**joyeecheung, [nodejs/node#51372 comment 2024-01-10](https://github.com/nodejs/node/issues/51372#issuecomment-1885803060):**

> In general, exposing things that are available to Window but not to Worker in the spec can be problematic, because Node.js mostly implement worker scope things but not the window scope things.

This is the most predictive of the maintainer objections. It says: if you implement `globalThis` as a worker-scope-like object, you'll get worker-scope-only semantics in a main-thread context. As §3.1 shows, this is exactly the bug class Bun has been fighting for two years.

### 2.6 The TSC consensus

**mhdawson, [nodejs/node#45993 comment 2023-01-04](https://github.com/nodejs/node/pull/45993#issuecomment-1371261697):**

> We discussed in the TSC meeting today, at this point the consensus seems to be that value does not outweigh the issues/problems unless there is a compelling use case we are not aware of.

This is a **cost-benefit call**, not a logical-impossibility call. jasnell himself, who opened both #51372 (Jan 2024) and #57352 (Mar 2025) re-raising the question, closed them both `not_planned` on the same day (2025-06-09) — which is a "the discussion has gone in circles enough times" decision, not a "this can't be done" decision.

### 2.7 jasnell's own polyfill (the kicker)

**jasnell, [nodejs/node#45993 comment 2023-05-29](https://github.com/nodejs/node/pull/45993#issuecomment-1568010833):**

> If someone really wanted to opt in to that they could fairly easily do...
>
> ```js
> Object.setPrototypeOf(globalThis, new EventTarget());
> ```

The maintainer who closed the issue four times offered the one-line userland polyfill himself. That puts the rejection clearly in the "Node-runtime policy decision, not a technical impossibility" bucket.

---

## 3. What Bun actually does

### 3.1 Mechanism (verified from source)

Bun's `globalThis` is `instanceof EventTarget` because the `Zig::GlobalObject` is a `WorkerGlobalScope`-backed object on every context — main thread, workers, and ShadowRealms alike. From [`src/jsc/bindings/BunWorkerGlobalScope.cpp`](https://github.com/oven-sh/bun/blob/main/src/jsc/bindings/BunWorkerGlobalScope.cpp) (as cited in [Bun PR #30586](https://github.com/oven-sh/bun/pull/30586)):

> `WorkerGlobalScope` (in `BunWorkerGlobalScope.cpp`) backs the `globalThis` event target on **every** `Zig::GlobalObject` — main thread, workers, and ShadowRealms alike. Its `onDidChangeListenerImpl` hook calls `scriptExecutionContext()->refEventLoop()` whenever a `message` listener is added, so that a worker stays alive to receive messages from its parent.

The `on*` handler attributes are wired as JSC custom getter/setters that delegate to the WebIDL handler-attribute pattern. From [`src/bun.js/bindings/ZigGlobalObject.cpp`](https://github.com/oven-sh/bun/blob/7abe6c38/src/bun.js/bindings/ZigGlobalObject.cpp):

```cpp
JSC_DEFINE_CUSTOM_GETTER(globalOnError,
    (JSC::JSGlobalObject * lexicalGlobalObject, JSC::EncodedJSValue thisValue,
        JSC::PropertyName))
{
    Zig::GlobalObject* thisObject = JSC::jsCast<Zig::GlobalObject*>(JSValue::decode(thisValue));
    return JSValue::encode(eventHandlerAttribute(thisObject->eventTarget(), eventNames().errorEvent, thisObject->world()));
}
```

This is the correct WebIDL handler-attribute shape: single-slot, set-returns-same, etc. **Bun did the implementation right.** The bugs aren't in the basic mechanism; they're in the interaction between this mechanism and `process`.

### 3.2 Observed cross-channel bugs

Each Node maintainer concern has a corresponding open or recently-fixed Bun bug:

**Bug 1 — Worker-scope semantics leaking to main thread (matches joyeecheung's prediction).** [Bun #24256](https://github.com/oven-sh/bun/issues/24256) (Oct 2025, confirmed bug): setting `globalThis.onmessage = () => {}` on the main thread caused Bun to hang forever instead of exiting. Cause: `WorkerGlobalScope::onDidChangeListenerImpl` refs the event loop on every `message` listener (because in a worker, the worker stays alive to receive messages from its parent), but on the main thread there's no parent, so the ref is never balanced. Fixed in [PR #30586](https://github.com/oven-sh/bun/pull/30586) (May 2026) by adding an `isWorker` guard. The `lzma` npm package ([Bun #24484](https://github.com/oven-sh/bun/issues/24484)) hit this because it sets `onmessage` as part of its fake-worker shim, and `require("lzma")` permanently hung Bun processes. **Two-year gap between the design landing and the fix.**

**Bug 2 — Cross-channel `preventDefault` ambiguity (matches jasnell's prediction exactly).** [Bun #29043](https://github.com/oven-sh/bun/issues/29043) (open as of search date): in a worker, `globalThis.addEventListener('error', e => { e.preventDefault(); })` does *not* suppress propagation of the error to the parent thread. The handler runs, `preventDefault()` is called, and Bun still emits the error to the main thread's `worker.on('error', ...)`. This is exactly jasnell's question 1 in §2.1: "if the user code requests to stop event propagation using `preventDefault()` ... should that prevent the process event handler from being fired?" Bun's answer in code: no, it shouldn't, even though the user is asking it to. Bug is open.

**Bug 3 — Re-entrant uncaughtException crashing the runtime (matches benjamingr's "complicates the error story" prediction).** [Bun #28648](https://github.com/oven-sh/bun/issues/28648) (May 2026): if a worker thread's `process.on('uncaughtException')` handler itself throws, Bun panics ("Uncaught exception while handling uncaught exception"). The fix in [PR #28650](https://github.com/oven-sh/bun/pull/28650) had to dispatch the secondary error to the parent via `onUnhandledRejection` to recover. Node handles this correctly (the secondary error gets the standard uncaught-exception treatment); Bun's dual-channel architecture made the re-entrant case crash-by-default.

**Bug 4 — `process.on('uncaughtException')` not firing at all, for years.** [Bun #5219](https://github.com/oven-sh/bun/issues/5219) (Sep 2023), duped to [Bun #429](https://github.com/oven-sh/bun/issues/429) (the original open issue): for years, `process.on('uncaughtException')` simply did not fire on Bun. Bun's Electroid in the dup: *"`process.on` technically works but it does not monitor the events as expected, it just extends from `EventEmitter`."* That is: the EventEmitter wiring existed (so listeners could be registered) but the runtime never delivered events to them. This is a six-month-plus open bug at minimum. Sentry's [issue #5091](https://github.com/oven-sh/bun/issues/5091) tracking this specifically lists the missing process events that prevented Sentry from working on Bun until they got fixed.

**Bug 5 — `process.nextTick` uncaught exceptions firing handlers multiple times / exit code 0.** [Bun PR #27229](https://github.com/oven-sh/bun/pull/27229) (2026):

> Exceptions thrown inside `process.nextTick` callbacks (including EventEmitter `"error"` handlers scheduled via `emitErrorNT`/`emitErrorCloseNT`) were caught by `processTicksAndRejections` but not properly treated as fatal uncaught exceptions. The process continued running after the throw, causing the error handler to fire multiple times and the process to exit with code 0.

This is precisely the dual-channel ordering issue from §2.1: the dual-channel design has two places that could legitimately treat a throw as fatal (the JS-level `process.nextTick` machinery, the runtime-level uncaught-exception handler), they were not coordinated, and the result was handlers running 2+ times and the process exiting cleanly when it should have exited with code 1.

### 3.3 Bun's documentation status

[Bun's `process` reference](https://bun.com/reference/node/process) self-documents: *"`process.binding` (internal Node.js bindings some packages rely on) is partially implemented... See the 'process' entry in the globals section for specific implementation status and missing APIs."* The page lists `uncaughtException`, `uncaughtExceptionMonitor`, `unhandledRejection`, and `rejectionHandled` as available events, but the surrounding caveats indicate ongoing partial implementation work.

### 3.4 What this tells us about Bun's design call

Bun **did not** make Deno's design call. Bun made a different call: ship `globalThis` as an EventTarget natively (because Bun is JSC-based and inheriting `WorkerGlobalScope` is the natural shape), and ship `process.on` as a separate EventEmitter-based channel that the runtime separately notifies. The two channels are *parallel* on Bun, not *forwarded*. That's why all five bug classes above exist: the runtime has two error-reporting code paths, they need to stay coordinated, and they have not been.

Bun is shipping. The bug shape Node maintainers predicted has materialized. The two facts are not contradictory; they describe a runtime that took on the engineering cost of dual-channel coordination and has spent two years grinding through the bugs that result.

---

## 4. What Deno actually does

### 4.1 Mechanism

Deno's `globalThis` is `instanceof EventTarget` natively — Deno was built web-shape-first. The interesting question is: **how does Deno bridge `globalThis` events to its `process` polyfill?**

Answer: `process` is implemented *as a forwarder over `globalThis`*. From [`denoland/deno/ext/node/polyfills/process.ts`](https://github.com/denoland/deno/blob/main/ext/node/polyfills/process.ts):

```js
internals.nodeProcessUnhandledRejectionCallback = (event) => {
  if (process.listenerCount("unhandledRejection") === 0) {
    // If an unhandled rejection occurs and there are no unhandledRejection
    // listeners.
    event.preventDefault();
    // ... wrap reason in ERR_UNHANDLED_REJECTION ...
    uncaught(reason, "unhandledRejection");
    return;
  }
  event.preventDefault();
  process.emit("unhandledRejection", event.reason, event.promise);
};
```

The design choice: **`globalThis` is canonical, `process` is the forwarder.** The `globalThis.addEventListener('unhandledrejection', ...)` listener calls `event.preventDefault()` to suppress Deno's default crash, then re-emits on `process`. `process.listenerCount` is honored as the gate: if no `process` listeners exist, it does the wrap-in-ERR_UNHANDLED_REJECTION + uncaught dance; if listeners exist, the event gets re-emitted on the EventEmitter.

This is a **resolvable** design — it just requires the runtime to *own* `process` (it's a polyfill they wrote) and to *pick* one channel as canonical. Deno picked `globalThis`.

### 4.2 Engineering cost Deno paid

The "globalThis canonical, process forwarder" choice has not been free for Deno:

- **[denoland/deno#19307](https://github.com/denoland/deno/pull/19307) (2023):** Deno had to engineer a "managed globals" semi-proxy to segregate Node-mode and Deno-mode globals. The two modes see different sets of globals via runtime mode detection. *"This commit makes the `globalThis` of the entire runtime a semi-proxy. This proxy returns a different set of globals depending on the caller's mode."* Significant engineering.
- **[denoland/deno#24637](https://github.com/denoland/deno/pull/24637) (Jul 2024):** Deno *removed* `self` from node-compat mode because npm packages were misdetecting Deno as a browser via `typeof self`. Mentioned in the prior research; recall that `self` is a much cheaper polyfill than the EventTarget swap and Deno still had to walk it back.
- **[denoland/deno#32535](https://github.com/denoland/deno/pull/32535) (Mar 2026):** Deno had to wrap non-Error unhandled rejections in `ERR_UNHANDLED_REJECTION` to match Node's behavior — *"Deno was passing the raw value directly, which caused crashes when exception handlers accessed `.message` or `.name`."* As of March 2026, 14 of 20 promise compat tests still failing.

Deno's approach works better than Bun's — the forwarder design eliminates the "two parallel channels" problem by *eliminating one of the channels and making it a wrapper around the other* — but the cost of getting there has been three-plus years of compat-mode engineering work, and it requires Deno to own and re-implement `process` from scratch. **Nub cannot do this** because Node owns `process` and the `process.on('uncaughtException')` ecosystem lives on top of code Nub does not control.

### 4.3 Deno's resolution generalizes — but not to Nub

If Nub were a runtime that *replaced* Node's `process`, the Deno design would generalize: pick `globalThis` as canonical, route everything through it, wrap the `process` shape as a forwarder. But Nub explicitly does not own `process` — Nub is an augmenter on top of unmodified Node, and Node's `process` is what it is. The Deno design is unavailable to Nub the same way Bun's is: Nub would have to either run two parallel channels (Bun's choice → Bun's bugs) or intercept `process.on` calls and route them to `globalThis` (which violates additivity — `process.on` would change behavior under Nub).

---

## 5. What Cloudflare Workers does

CF Workers has no `process` (in the strict sense — they ship a partial polyfill but not the full `process.on(...)` event surface), so the dual-channel problem from §2.1 doesn't exist there. But there are still nontrivial unhandled-rejection bookkeeping bugs:

- **[cloudflare/workerd#6020](https://github.com/cloudflare/workerd/issues/6020) (2025):** `unhandledrejection` was misfiring on Workers — promises that *were* handled (via `.then(...).catch(...)` or `assert.rejects(async () => ...)`) were being reported as unhandled because workerd was firing the event before V8's promise microtask chain had a chance to settle. **Fix in [PR #6049](https://github.com/cloudflare/workerd/pull/6049):** delay the unhandledrejection report until after V8's microtasks-completed callback fires, behind a feature flag.

This is informative for Nub in one specific way: even a *single-channel* runtime (no `process`) ships nontrivial unhandled-rejection bugs. The "just ship the EventTarget swap and you're done" framing oversimplifies — there are real implementation surface costs even when there's no dual-channel coordination to worry about.

CF Workers does ship the WHATWG handler attributes ([`globalThis.addEventListener('error', ...)`](https://developers.cloudflare.com/workers/runtime-apis/handlers/), `globalThis.addEventListener('unhandledrejection', ...)`). It also has its own `ServiceWorkerGlobalScope`-style global, which is the natural shape for a Workers runtime; Workers don't need to support Node's `process` mental model at all.

For Nub the relevant takeaway is: **the CF Workers shape is what `globalThis instanceof EventTarget` looks like *in a runtime that never had `process`*.** Nub is not that runtime. Nub has `process` because Node has `process`.

---

## 6. Honest reassessment of the prior research

The prior research at [`wintertc-node-gap-rationale.md`](wintertc-node-gap-rationale.md) made three claims under stress:

### 6.1 "Unresolvable semantics" — overstated, but the underlying call still holds

The prior research said the dual-channel design produces "unresolvable semantics." That's too strong. **Bun resolved them** by picking parallel channels and accepting coordination bugs. **Deno resolved them** by picking globalThis as canonical and writing process as a forwarder. Both resolutions exist. The Node maintainer position, read carefully, is **not** "unresolvable" — it's *"the resolutions we've seen require engineering investment and migration strategy that the value doesn't justify."* That's a cost-benefit position, not an impossibility position.

**Recommended tightening for the prior framing:** replace "unresolvable semantics" language with "the dual-channel design has resolutions, but each resolution has multi-year engineering cost (Bun is still grinding through it; Deno paid the cost by owning process) and Nub cannot afford either path because Nub does not own process." The conclusion (don't ship) is unchanged; the rationale gets concrete with bug citations.

### 6.2 "Bug shape Node maintainers predicted has materialized in Bun" — strengthens the prior conclusion

The prior research framed the Node TSC's objection as principled-and-load-bearing without specific empirical evidence beyond the maintainer comments. The new evidence from §3.2 (five specific Bun bug classes mapped to specific maintainer predictions) **strengthens** rather than weakens that framing. Specifically:

- joyeecheung predicted worker-scope-on-main-thread bugs (§2.5). Bun #24256 (`globalThis.onmessage` hangs the process) is exactly that bug.
- jasnell predicted cross-channel `preventDefault` ambiguity (§2.1). Bun #29043 (preventDefault on `error` doesn't suppress propagation) is exactly that bug.
- benjamingr predicted error-story complication (§2.2). Bun #28648 (re-entrant uncaughtException crashes the runtime) and Bun PR #27229 (handlers firing multiple times, exit code 0) are exactly that complication.
- jasnell predicted `listenerCount` ambiguity (§2.2). This one is not yet a filed Bun bug because users haven't started writing code that relies on cross-channel listener counts; but it's structurally present in Bun's design.

This is a tight prediction-to-realization match. The Node maintainers were not hand-waving; they were describing the actual failure modes that have materialized in the runtime that shipped the design.

### 6.3 "Additivity violation" framing — was the right framing, for the right reason

The prior research argued that shipping `globalThis instanceof EventTarget` is non-additive because it changes:
1. Direct probe answers (`globalThis instanceof EventTarget`, `Object.getPrototypeOf(globalThis)`)
2. WebIDL fallback semantics (`EventTarget.prototype.dispatchEvent.call(null, ev)`)
3. Prototype-walk inspection

All three are correct, but the *more important* additivity concern surfaces from the new evidence: **the wiring is what's non-additive, not just the prototype chain.** If Nub ships the prototype swap *without* wiring `process.on('uncaughtException')` to `globalThis.dispatchEvent(new ErrorEvent('error', ...))`, then `globalThis.addEventListener('error', fn)` registers a handler that never fires. That's a worse failure mode than the prior `undefined` — it's a setter that silently swallows. Adding the wiring makes Nub's `process` behavior different from plain Node's `process` behavior, which is exactly the kind of change additivity forbids. **Either way, additivity fails.** The prior framing was right; the new evidence makes the failure mode more concrete.

### 6.4 The reflect-metadata parallel — does it apply here?

A useful comparison is the [`emit-decorator-metadata.md`](emit-decorator-metadata.md) discussion, where an "additivity-violating" framing was wrong because the proposed feature was correctly additive (it added new behavior without changing existing behavior). Does that parallel apply here?

**No, and the reason is informative.** `emitDecoratorMetadata` is purely additive at the *transpilation* layer — it adds calls to `Reflect.defineMetadata(...)` on classes that Nub transpiles, and doesn't change anything for classes Nub doesn't transpile. Code that doesn't ask for the feature is unchanged.

`globalThis instanceof EventTarget` is different: there's no opt-in surface that makes the swap conditional on user request. The instant the prototype chain is swapped, *every* probe in the runtime gets a different answer. Any library that does `if (globalThis instanceof EventTarget) { /* web mode */ } else { /* node mode */ }` flips branches. Any library that does `for (let p = globalThis; p; p = Object.getPrototypeOf(p)) { ... }` walks a different chain. Any library that does `EventTarget.prototype.dispatchEvent.call(null, ev)` gets a different result. **The probe surface is global and unopt-outable.** That's what makes it different from the reflect-metadata case.

The `--globalthis-eventtarget` flag from the prior research's "future opt-in" position is the right escape: behind a flag, the swap is opt-in, the additivity violation only applies to users who explicitly asked, and the default behavior matches plain Node. The prior research had this; we just want to keep it deferred to v0.x and not promise it in v0.1.

---

## 7. Implementation cost in Nub

If Nub *were* to ship this (which the recommendation says we shouldn't), what would the implementation look like?

### 7.1 The minimum delta — Option B (prototype swap only, no `process` wiring)

```js
// In Nub's --import preload:
Object.setPrototypeOf(globalThis, new EventTarget());

// WebIDL "floating method" fallback globals:
globalThis.addEventListener = EventTarget.prototype.addEventListener.bind(globalThis);
globalThis.removeEventListener = EventTarget.prototype.removeEventListener.bind(globalThis);
globalThis.dispatchEvent = EventTarget.prototype.dispatchEvent.bind(globalThis);

// On* handler attributes via accessor descriptors, with single-slot semantics:
function defineHandler(name, event) {
  let current = null;
  Object.defineProperty(globalThis, name, {
    configurable: true,
    enumerable: true,
    get() { return current; },
    set(fn) {
      if (current) globalThis.removeEventListener(event, current);
      current = (typeof fn === 'function' || (typeof fn === 'object' && fn !== null)) ? fn : null;
      if (current) globalThis.addEventListener(event, current);
    },
  });
}
defineHandler('onerror', 'error');
defineHandler('onunhandledrejection', 'unhandledrejection');
defineHandler('onrejectionhandled', 'rejectionhandled');
```

**Cost:** ~15 LOC of preload. **Behavior:** `globalThis instanceof EventTarget === true`; `globalThis.addEventListener('error', fn)` registers a handler that never fires; `globalThis.onerror = fn` likewise. **This is a worse failure mode than not shipping** — feature-detection libraries that probe `typeof globalThis.onerror === 'object'` (it's `null` on the web when unset, which is `typeof 'object'`) will get a "yes" answer and proceed to set the handler that will never be called. Users will report bugs against Nub. **Do not ship Option B.**

### 7.2 The full version — Option C (prototype swap + wire `process` to dispatch on `globalThis`)

In addition to Option B's ~15 LOC, wire `process` events to dispatch on `globalThis`:

```js
process.on('uncaughtException', (err, origin) => {
  globalThis.dispatchEvent(new ErrorEvent('error', { error: err, message: err?.message }));
});
process.on('unhandledRejection', (reason, promise) => {
  const event = new PromiseRejectionEvent('unhandledrejection', { reason, promise, cancelable: true });
  globalThis.dispatchEvent(event);
  // If event.preventDefault() was called, suppress Node's default reporting
  // ... but how? Node's --unhandled-rejections flag is set at process startup;
  //     it doesn't give us a per-event cancellation API.
});
process.on('rejectionHandled', (promise) => {
  globalThis.dispatchEvent(new PromiseRejectionEvent('rejectionhandled', { promise }));
});
```

**Cost:** ~30 LOC including stubs, but the wiring brings the bug class:

- **The `preventDefault()` problem is unfixable from a preload.** Node's `--unhandled-rejections` flag controls whether unhandled rejections crash the process; it's a process-startup decision. Inside a `process.on('unhandledRejection', ...)` callback, we have no way to retroactively tell Node "don't crash"; the crash decision was made when the flag was parsed. So even if a user calls `event.preventDefault()` in their `globalThis` handler, Node's default behavior (crash on unhandled rejection, if `--unhandled-rejections=strict`) still fires. **This replicates Bun's #29043 in Nub.**
- **Ordering question becomes our problem.** If a user has both `process.on('unhandledRejection', ...)` and `globalThis.addEventListener('unhandledrejection', ...)`, which fires first? Whatever choice Nub makes is observable and will get filed as a bug by users who expected the other order.
- **`process.listenerCount` becomes ambiguous.** As jasnell predicted: `process.listenerCount('unhandledRejection') === 0` when the user has only `globalThis` handlers.
- **The `(reason, promise)` → `PromiseRejectionEvent` adapter is lossy in the other direction too.** A user who registers `globalThis.addEventListener('unhandledrejection', e => process.exit(1))` and *also* has a `process.on('unhandledRejection', (reason, promise) => ...)` listener has a race: the `globalThis` handler exits the process, the `process` handler may or may not run depending on dispatch ordering. Bun has had this class of bug (PR #27229).

**Cost in bug-report surface:** the same five Bun bug classes from §3.2 land on Nub. Cost in engineering time to grind through them: see Bun's two-year struggle. **Do not ship Option C.**

### 7.3 Behind a flag — Option C-prime (future v0.x `--globalthis-eventtarget`)

The prior research proposed deferring this as an opt-in flag. The new evidence does not change that assessment — the opt-in flag is the right escape hatch for the rare user who needs the EventTarget shape, and the cost is documented in the flag's help text. **Keep the flag deferred to v0.x.** When a concrete user requests it, evaluate the bug-class import-cost against the user's specific need.

### 7.4 The userland one-liner (always available, never blocked)

Per jasnell's own polyfill, anyone who really wants `globalThis instanceof EventTarget` can put this in their own `--import` preload, today, on plain Node or Nub:

```js
Object.setPrototypeOf(globalThis, new EventTarget());
```

We don't have to ship anything for this to be available. Documenting it as the escape hatch is the right call.

---

## 8. Recommendation

**(A) Stand by the current decision.** Don't ship `globalThis instanceof EventTarget`, don't ship the three `on*` handler attributes, in v0.1 or v0.x default.

**One-line rationale:** The Node maintainer concerns from 2022–2025 have materialized as specific open and recently-fixed bugs in Bun, including the exact `preventDefault`, `listenerCount`, re-entrancy, and worker-scope-context-bleed failure modes the maintainers cited; Bun has paid two years of engineering cost grinding through them; Deno avoided that cost only by owning `process` (which Nub cannot do, because Node owns `process`). The prior research's call to hold the line is *strengthened* by the new evidence, not weakened.

**What changes in the recommendation vs. the prior research:**

- **Tighten the rationale, not the conclusion.** Replace "unresolvable semantics" language with "the dual-channel design has resolutions, but each resolution has multi-year engineering cost that Nub cannot afford because Nub does not own `process`." Cite the specific Bun bugs (§3.2) and the specific Deno engineering cost (§4.2). This is more concrete and more honest than the abstract framing.
- **Keep the future `--globalthis-eventtarget` opt-in flag deferred to v0.x.** Don't promise it for v0.1; don't promise it ever, but don't rule it out. Evaluate when a concrete user request lands.
- **Document the userland one-liner explicitly** in `min-common-api-globals.md` as the escape hatch for users who really want the swap. The one-line `Object.setPrototypeOf(globalThis, new EventTarget())` works on plain Node and Nub alike; no Nub-specific surface needed.
- **Hold the line on `onerror` / `onunhandledrejection` / `onrejectionhandled` exactly as the prior research recommended.** Drop them from v0.1. The new evidence makes the case stronger, not weaker.

**Why not (B) reverse:** because Bun has been grinding through the bugs for two years and Nub cannot afford to inherit them, *and* because Nub cannot use Deno's resolution (it requires owning `process`), *and* because the failure modes are concrete and predictable rather than speculative.

**Why not (C) opt-in flag in v0.1:** because adding a flag for a feature with no concrete user request is bug-report-surface for no return. Defer until someone asks. The userland one-liner is sufficient for everyone who currently asks.

---

## 9. Open questions

- **Is there a way to safely ship just the inert `PromiseRejectionEvent` class without the dispatch wiring?** The prior research said yes (matches Node's own `ErrorEvent` v25 precedent). This stress-test confirms: yes, an inert constructor that users dispatch on their own `EventTarget`s is fine. Already in the v0.1 set.
- **Should we file an issue against [Bun #29043](https://github.com/oven-sh/bun/issues/29043) to track resolution?** Not Nub's job, but watching that bug is informative — if Bun lands a resolution, the design lessons may unblock the case for a future Nub shipment.
- **When (if ever) the `--globalthis-eventtarget` flag lands, should it auto-wire `process.on('uncaughtException')` → `globalThis.dispatchEvent`?** Open. The flag could be flavors: `--globalthis-eventtarget=prototype-only` (Option B-style, no wiring, fires only on explicit `globalThis.dispatchEvent`) vs. `--globalthis-eventtarget=full` (Option C-style, with wiring and the bug class). The prototype-only flavor is honest and useful for polyfill-author audiences; the full flavor is what Bun does. Decide at flag-design time.
- **What does the WinterTC compliance number look like under this recommendation?** Same as the prior research: ~98%, with the four `EventTarget`-dependent items held out. Confirmed.

---

## 10. Sources

### Node maintainer threads

- [nodejs/node#45981 — make global object an instance of `EventTarget`](https://github.com/nodejs/node/issues/45981) (opened Dec 26, 2022 by jimmywarting; closed May 31, 2023)
- [nodejs/node#45993 — events,bootstrap: make globalThis extend EventTarget](https://github.com/nodejs/node/pull/45993) (PR by KhafraDev, opened Dec 28, 2022; still open as of 2026-04-28; TSC consensus against)
- [nodejs/node#51372 — Revisiting `globalThis` as an `EventTarget`](https://github.com/nodejs/node/issues/51372) (opened Jan 4, 2024 by jasnell; closed `not_planned` Jun 9, 2025 by jasnell)
- [nodejs/node#57352 — globalThis as an EventTarget](https://github.com/nodejs/node/issues/57352) (opened Mar 6, 2025 by jasnell; closed `not_planned` Jun 9, 2025 by jasnell)
- [nodejs/TSC#1323 — TSC meeting 2023-01-04](https://github.com/nodejs/TSC/issues/1323) (initial TSC consensus against)
- [nodejs/TSC#1489 — TSC meeting 2024-01-10](https://github.com/nodejs/TSC/issues/1489) (re-affirmed)

### Bun source and issues

- [oven-sh/bun#24256 — Bun process doesn't exit when `globalThis.onmessage` is set](https://github.com/oven-sh/bun/issues/24256) (Oct 31, 2025; open as of doc date)
- [oven-sh/bun#24484 — Inclusion of lzma library causes Bun to never stop execution](https://github.com/oven-sh/bun/issues/24484) (downstream of #24256)
- [oven-sh/bun#30586 — Don't keep process alive for globalThis.onmessage on main thread](https://github.com/oven-sh/bun/pull/30586) (May 12, 2026; fixes #24256 / #24484 via `isWorker` guard in `BunWorkerGlobalScope.cpp`)
- [oven-sh/bun#29043 — Worker error event ignores `preventDefault()` and still terminates](https://github.com/oven-sh/bun/issues/29043) (open)
- [oven-sh/bun#28648 — Bun crashes when worker throws inside `uncaughtException` handler](https://github.com/oven-sh/bun/issues/28648) (May 2026; fixed in #28650)
- [oven-sh/bun#27229 — fix: exit process on unhandled exceptions in process.nextTick callbacks](https://github.com/oven-sh/bun/pull/27229) (2026; documents the multi-fire / exit-code-0 bug class)
- [oven-sh/bun#5219 — process 'uncaughtException' event is not triggered](https://github.com/oven-sh/bun/issues/5219) (Sep 2023; duped to #429)
- [oven-sh/bun#429 — Support `process.on("unhandledRejection")` and `process.on("uncaughtException")`](https://github.com/oven-sh/bun/issues/429) (longstanding; comment record of multi-year struggle)
- [oven-sh/bun#5091 — Get Sentry-like error reporting working](https://github.com/oven-sh/bun/issues/5091) (Sentry SDK blocker)
- [oven-sh/bun#19547 — test-eventtarget.js](https://github.com/oven-sh/bun/pull/19547) (test additions; documents Bun's EventTarget conformance test history)
- [Bun source: `src/jsc/bindings/BunWorkerGlobalScope.cpp`](https://github.com/oven-sh/bun/blob/main/src/jsc/bindings/BunWorkerGlobalScope.cpp) (verified mechanism — `WorkerGlobalScope` backs `globalThis` event target on all contexts)
- [Bun source: `src/bun.js/bindings/ZigGlobalObject.cpp`](https://github.com/oven-sh/bun/blob/7abe6c38/src/bun.js/bindings/ZigGlobalObject.cpp) (`globalOnError` custom getter; `promiseRejectionTracker`)
- [Bun docs: `process` reference](https://bun.com/reference/node/process) (self-documents partial implementation)

### Deno source and issues

- [denoland/deno: `ext/node/polyfills/process.ts`](https://github.com/denoland/deno/blob/main/ext/node/polyfills/process.ts) (canonical forwarder design — globalThis canonical, process forwards)
- [denoland/deno#19307 — properly segregate node globals](https://github.com/denoland/deno/pull/19307) (managed-globals semi-proxy)
- [denoland/deno#24637 — do not expose `self` global in node](https://github.com/denoland/deno/pull/24637) (removed `self` from node-compat mode after npm packages misdetected Deno as browser)
- [denoland/deno#32535 — wrap non-Error unhandled rejections in ERR_UNHANDLED_REJECTION](https://github.com/denoland/deno/pull/32535) (Mar 2026; promise compat work)

### Cloudflare Workers source and issues

- [cloudflare/workerd#6020 — unhandledRejection misfires a lot](https://github.com/cloudflare/workerd/issues/6020) (2025)
- [cloudflare/workerd#6049 — fix unhandledRejection misfires](https://github.com/cloudflare/workerd/commit/bd3b76acd9dd4666bacccb24ebf0f7d73d25ec91) (microtasks-completed callback)
- [cloudflare/workerd: `src/workerd/api/global-scope.c++`](https://github.com/cloudflare/workerd/blob/main/src/workerd/api/global-scope.c%2B%2B) (verified mechanism — `ServiceWorkerGlobalScope` is the EventTarget)
- [cloudflare/workerd: `src/node/internal/events.ts`](https://github.com/cloudflare/workerd/blob/main/src/node/internal/events.ts) (Node-compat events polyfill for Workers)

### Specs and references

- [WHATWG HTML §event-handler-attributes](https://html.spec.whatwg.org/multipage/webappapis.html#event-handler-attributes) (the IDL contract for `on*` handler attributes)
- [WebIDL §dfn-create-operation-function](https://webidl.spec.whatwg.org/#dfn-create-operation-function) (floating-method fallback to `globalThis`)
- [MDN: Window: unhandledrejection event](https://developer.mozilla.org/en-US/docs/Web/API/Window/unhandledrejection_event) (`preventDefault()` cancellation semantics on the web)
- [Node.js process docs: `'uncaughtException'`](https://nodejs.org/api/process.html) (Node's equivalent error model; no per-event cancellation API)
- [WinterTC55/proposal-minimum-common-api#82](https://github.com/wintercg/proposal-minimum-common-api/pull/82) (the WinterTC requirement that drives this gap)

### Nub internal cross-references

- [`wintertc-node-gap-rationale.md`](wintertc-node-gap-rationale.md) — prior research being stress-tested (write-once; not amended)
- `../runtime/min-common-api-globals.md` — runtime doc the recommendation feeds back into
- `../philosophy.md#additivity` — additivity policy
- `../architecture.md#augmenter-not-fork` — the "would a user on plain Node + the corresponding `module.register()` / `--import` / npm-addon get the same result?" mechanism rule

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
