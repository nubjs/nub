# `globalThis` as `EventTarget` — stress-testing the rejection rationale against Bun's lived experience

Bun ships `globalThis instanceof EventTarget`, so this doc tests whether Node's rejection of it can still stand. Every failure mode the Node maintainers named has a matching Bun bug, which strengthens the prior no rather than reversing it.

**Date:** 2026-05-24 **Question:** The prior research at [[research/wintertc-node-gap-rationale]] concluded Node's rejection of `globalThis instanceof EventTarget` and the `on*` handler properties was a "principled architectural no" backed by unresolvable dual-channel semantics. The counter-argument: **Bun ships them. If Bun ships them without catastrophe, the "unresolvable" framing has to be wrong or at least overstated.** Verified against maintainer comments, Bun source, and Bun/Deno/Workers bug history. **Extends/stress-tests:** [[research/wintertc-node-gap-rationale]] (write-once, not amended).

---

## 1. TL;DR

Bun has shipped the EventTarget global and paid for it in a two-year stream of cross-channel bugs. Deno's forwarder design avoids most of them, but only because Deno owns `process`. Nub does not, so the prior rejection stands.

- **Bun has not shipped this without catastrophe — it has shipped it *with* catastrophe, gradually and in pieces small enough that nobody calls it that.** Every concrete failure mode the Node maintainers cited has a corresponding open or recently-fixed Bun issue: process-hang on `globalThis.onmessage = fn` ([Bun #24256](https://github.com/oven-sh/bun/issues/24256), fixed May 2026 in [PR #30586](https://github.com/oven-sh/bun/pull/30586) by adding an `isWorker` guard); `event.preventDefault()` on `globalThis.addEventListener('error', ...)` not actually preventing default ([Bun #29043](https://github.com/oven-sh/bun/issues/29043), open); re-entrant `uncaughtException` crashing the runtime ([Bun #28648](https://github.com/oven-sh/bun/issues/28648), fix May 2026); `process.on('uncaughtException')` not firing for years ([Bun #5219](https://github.com/oven-sh/bun/issues/5219) → [#429](https://github.com/oven-sh/bun/issues/429)); `process.nextTick` uncaught exceptions firing handlers multiple times and exiting with code 0 ([Bun PR #27229](https://github.com/oven-sh/bun/pull/27229), 2026). The "unresolvable semantics" framing is rhetorically too strong — Bun and Deno have both resolved them with specific design choices — but the engineering cost of resolving them is multi-year and ongoing, and the bug shape matches Node maintainer predictions one-for-one. *Strengthened* the prior research's conclusion.
- **Deno made a different design choice than Bun and it has been the better one.** Deno's `globalThis` is the canonical event bus; Deno's `process.on('unhandledRejection')` polyfill is implemented *as a listener on `globalThis.addEventListener('unhandledrejection')` that calls `event.preventDefault()` and re-emits on `process`* — see [`denoland/deno/ext/node/polyfills/process.ts`](https://github.com/denoland/deno/blob/main/ext/node/polyfills/process.ts). The design choice is "globalThis is canonical, process is the forwarder." This works in Deno because Deno owns `process` (it's a polyfill they wrote). **Nub cannot make this choice** — Node owns `process`, and the entire `process.on('uncaughtException')` ecosystem will exist regardless of Nub, on top of code Nub does not control.
- **The right characterization of the Node maintainer position is "the dual-channel design has resolvable semantics, but the resolution has cost we don't want to pay for limited benefit."** Not "unresolvable." mcollina's "I don't think we'll ever be able to migrate from process events to globalThis" is a *migration-strategy* objection, not an architectural-impossibility objection. The TSC consensus (mhdawson, Jan 2023: *"value does not outweigh the issues/problems"*) is a cost-benefit call, not a logical-impossibility call.
- **For Nub specifically, the technical operation is cheap (one `Object.setPrototypeOf` + three handler accessors, ~10 LOC), but the wiring is the expensive part.** Three options: (A) don't ship; (B) prototype swap with no `process` wiring (handlers exist but never fire); (C) prototype swap + wire `process.on('uncaughtException')` → `globalThis.dispatchEvent(new ErrorEvent(...))`. Option B is worse than (A) — it converts `typeof globalThis.onerror === 'undefined'` (correct on Node) into a setter that swallows handlers and never fires them. Option C imports Bun's bug class — including the specific cross-channel `preventDefault` failure ([#29043](https://github.com/oven-sh/bun/issues/29043)) that has been open since the design landed.
- **Recommendation: (A) Stand by the prior decision, with a tightened rationale.** The new evidence concretizes and strengthens the prior recommendation rather than reversing it. Hold the line on shipping `globalThis instanceof EventTarget` and the three `on*` handler attributes. Document the Bun bug history as the concrete evidence, in place of "unresolvable semantics." Keep the future `--globalthis-eventtarget` opt-in flag deferred to v0.x as the userland escape hatch; ship the one-liner `Object.setPrototypeOf(globalThis, new EventTarget())` as documented userland preload guidance for anyone who really wants it.

---

## 2. The Node maintainer objections, concretely

The **specific failure scenarios** each maintainer cited, with comment-URL citations.

### 2.1 Cross-channel ordering and `preventDefault` ambiguity (jasnell, benjamingr)

**jasnell, [nodejs/node#57352 comment 2025-03-07](https://github.com/nodejs/node/issues/57352)** posed the concrete questions; the thread never converged on answers:

> One case where it gets a bit tricky is with `process.on('error', ...)`. The `'error'` events are the only ones we have that propagate up... if I have an EventEmitter `foo` and it does not have an `'error'` event handler, that gets bumped to `process.on('error', ....)` if it exists, and if it does not, then it is forwarded to `process.on('uncaughtException', ...)` or otherwise thrown as an uncaught exception. Where would `globalThis.addEventListener('error', ...)` fall within that flow? Or would it at all? If someone only [has] a `globalThis.addEventListener('error', ...)` would that be enough to stop the propagation to an unhandled error or would the globalThis error handler not matter in the typical flow of an EventEmitter error?

> What would y'all expect the behavior to be if someone registers *both* process event and global event handlers? Which order should they be invoked in: (a) `process` first then `globalThis`, (b) `globalThis` first then `process`, (c) only [one] or the other? If option (b) and the user code requests to stop event propagation using, for instance, `preventDefault()` or `stopImmediatePropagation()` should that prevent the process event handler from being fired?

> If I call `globalThis.dispatchEvent(new ErrorEvent('error', ...))` would you expect both `globalThis.addEventListener('error', ...)` *and* `process.on('error', ...)` to be called? Likewise, if I called `process.emit('error', ...)` would you expect both the `globalThis` and `process` error handlers to be called?

The thread split: ljharb said globalThis should be "the parent" (process fires first, then globalThis); ShogunPanda proposed an explicit opt-in `process.useGlobalThisForEvents()` switch; ljharb then argued the switch is unworkable because packages and application code must be free to use either side. **No convergence.**

**benjamingr, [nodejs/node#57352 comment 2025-03-07](https://github.com/nodejs/node/issues/57352#issuecomment-2706598290):**

> Does `e.stopImmediatePropagation` on `error` prevent `"uncaughtException"` from firing? What order are events fired at? Does removing all listeners from `uncaughtException` impact `error`? How do we reconcile the different design decisions in `EventTarget` and `EventEmitter` without complicating the error handling story significantly for end users?

### 2.2 Listener-count / feature-probe hazards (jasnell, benjamingr)

**jasnell, [nodejs/node#57352](https://github.com/nodejs/node/issues/57352)**:

> When a developer ends up using a `globalThis` listener for, say, `'error'` events but not a `process` listener, the `process.listenerCount('error')` would return `0`, possibly giving the false impression that there are no error event handlers when there actually might be.

Real Node code uses `process.listenerCount('uncaughtException')` widely — graceful-shutdown libraries, test frameworks like vitest/jest, the Sentry SDK — to decide whether the runtime will crash on the next throw. A design that lets `globalThis.addEventListener('error', ...)` count as an error handler invisible to `process.listenerCount` silently changes behavior in those libraries.

**benjamingr, [nodejs/node#57352](https://github.com/nodejs/node/issues/57352#issuecomment-2702951170):**

> this further complicates our already complicated error handling story with more events and orders which can negatively impact usability. This is made worse by libraries possibly checking for `globalThis` being an event emitter as a potential probe and adjust their error handling accordingly (since the error model in browsers is different than our own).

The probe hazard: `if (globalThis instanceof EventTarget) { /* use web error model */ } else { /* use process.on */ }` is exactly the cross-runtime detection libraries write today. Shipping the swap with broken or partial wiring makes the probe lie.

### 2.3 The `process` migration objection (mcollina, repeatedly)

**mcollina, [nodejs/node#51372 comment 2024-01-04](https://github.com/nodejs/node/issues/51372#issuecomment-1876608028):**

> I'm -1 as I think it would add more confusion. None of those mechanisms are correct for library authors. Most libraries should not touch global objects, because they can conflict in behaviors.

**mcollina, [nodejs/node#57352 comment 2025-03-06](https://github.com/nodejs/node/issues/57352#issuecomment-2702434430):**

> I don't think we'll ever be able to migrate from process events to globalThis. We need to figure out a strategy where those can coexist.

The "coexist" framing is **not "this is unresolvable"** but "this requires a migration strategy we don't have" — an important distinction the prior research overstated.

**mcollina, [nodejs/node#45993 comment 2023-01-01](https://github.com/nodejs/node/pull/45993#issuecomment-1368380538):**

> libraries emitting global events make the ecosystem more brittle.

### 2.4 The "browsers wouldn't do this today" priors objection (addaleax)

**addaleax, [nodejs/node#45993 comment 2022-12-31](https://github.com/nodejs/node/pull/45993#issuecomment-1368253966):**

> Do we feel like this is something we *want* in Node.js? I understand that this is something that other platforms do, but those platforms aren't Node.js and didn't have things like the `process` object (which is effectively where we emit all our "global" events) from the start. ... I'm not even sure that browsers would do this nowadays with the benefit of hindsight in how JS developed as a language.

The weakest objection — a priors argument — but it shapes the TSC consensus: the bar is not "is it a web API," it is "is it a web API we'd choose to ship today, given what we now know."

### 2.5 The most nuanced objection (joyeecheung)

**joyeecheung, [nodejs/node#45993 comment 2023-01-02](https://github.com/nodejs/node/pull/45993#issuecomment-1369081420):**

> out of the common events fired on `window` in the browsers, the ones that make more sense for Node.js are `message`, `unhandledrejection` (note the cases here), `rejectionhandled`, which we already (sort of) fire on `process`, but with disparities in the shapes of the event objects. It seems to be confusing to make `globalThis` in Node.js an `EventTarget` without a good story or at least documentation of how we are going to handle these disparities or even whether we are going to support these Web events in our `globalThis` at all. ... Users might attempt to use these well-specified events in `globalThis` and realize that it doesn't actually work like how things are in browsers, which IMO is worse than not making `globalThis` an `EventTarget`.

**joyeecheung, [nodejs/node#51372 comment 2024-01-10](https://github.com/nodejs/node/issues/51372#issuecomment-1885803060):**

> In general, exposing things that are available to Window but not to Worker in the spec can be problematic, because Node.js mostly implement worker scope things but not the window scope things.

The most predictive of the maintainer objections: implement `globalThis` as a worker-scope-like object and you get worker-scope-only semantics in a main-thread context. As §3.2 shows, that is exactly the bug class Bun has been fighting for two years.

### 2.6 The TSC consensus

**mhdawson, [nodejs/node#45993 comment 2023-01-04](https://github.com/nodejs/node/pull/45993#issuecomment-1371261697):**

> We discussed in the TSC meeting today, at this point the consensus seems to be that value does not outweigh the issues/problems unless there is a compelling use case we are not aware of.

A **cost-benefit call**, not a logical-impossibility call. jasnell himself, who opened both #51372 (Jan 2024) and #57352 (Mar 2025) re-raising the question, closed them both `not_planned` on the same day (2025-06-09) — a "the discussion has gone in circles enough times" decision, not a "this can't be done" decision.

### 2.7 jasnell's own polyfill

**jasnell, [nodejs/node#45993 comment 2023-05-29](https://github.com/nodejs/node/pull/45993#issuecomment-1568010833):**

> If someone really wanted to opt in to that they could fairly easily do...
>
> ```js
> Object.setPrototypeOf(globalThis, new EventTarget());
> ```

The maintainer who closed the issue four times offered the one-line userland polyfill himself. That puts the rejection in the "Node-runtime policy decision, not a technical impossibility" bucket.

---

## 3. What Bun actually does

Bun's `globalThis` inherits from `WorkerGlobalScope` on every context, and `process` is a second, parallel channel. The mechanism is correct WebIDL; the coordination between the two channels is where the five bug classes below come from.

### 3.1 Mechanism (verified from source)

Bun's `globalThis` is `instanceof EventTarget` because the `Zig::GlobalObject` is a `WorkerGlobalScope`-backed object on every context — main thread, workers, and ShadowRealms alike.

From [`src/jsc/bindings/BunWorkerGlobalScope.cpp`](https://github.com/oven-sh/bun/blob/main/src/jsc/bindings/BunWorkerGlobalScope.cpp) (as cited in [Bun PR #30586](https://github.com/oven-sh/bun/pull/30586)):

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

The shape is correct WebIDL: single-slot, set-returns-same. **Bun did the implementation right** — the bugs are not in the mechanism but in its interaction with `process`.

### 3.2 Observed cross-channel bugs

Each Node maintainer concern has a corresponding open or recently-fixed Bun bug:

**Bug 1 — Worker-scope semantics leaking to main thread (matches joyeecheung's prediction).** [Bun #24256](https://github.com/oven-sh/bun/issues/24256) (Oct 2025, confirmed bug): setting `globalThis.onmessage = () => {}` on the main thread caused Bun to hang forever instead of exiting. Cause: `WorkerGlobalScope::onDidChangeListenerImpl` refs the event loop on every `message` listener, but on the main thread there is no parent, so the ref is never balanced. Fixed in [PR #30586](https://github.com/oven-sh/bun/pull/30586) (May 2026) by adding an `isWorker` guard. The `lzma` npm package ([Bun #24484](https://github.com/oven-sh/bun/issues/24484)) hit this because it sets `onmessage` as part of its fake-worker shim, and `require("lzma")` permanently hung Bun processes. **Two-year gap between the design landing and the fix.**

**Bug 2 — Cross-channel `preventDefault` ambiguity (matches jasnell's prediction exactly).** [Bun #29043](https://github.com/oven-sh/bun/issues/29043) (open as of search date): in a worker, `globalThis.addEventListener('error', e => { e.preventDefault(); })` does *not* suppress propagation to the parent thread — Bun still emits the error to the main thread's `worker.on('error', ...)`. That is jasnell's first question in §2.1, and Bun's answer in code is no.

**Bug 3 — Re-entrant uncaughtException crashing the runtime (matches benjamingr's "complicates the error story" prediction).** [Bun #28648](https://github.com/oven-sh/bun/issues/28648) (May 2026): if a worker thread's `process.on('uncaughtException')` handler itself throws, Bun panics ("Uncaught exception while handling uncaught exception"). The fix in [PR #28650](https://github.com/oven-sh/bun/pull/28650) had to dispatch the secondary error to the parent via `onUnhandledRejection` to recover. Node handles this correctly (the secondary error gets the standard uncaught-exception treatment); Bun's dual-channel architecture made the re-entrant case crash-by-default.

**Bug 4 — `process.on('uncaughtException')` not firing at all, for years.** [Bun #5219](https://github.com/oven-sh/bun/issues/5219) (Sep 2023), duped to [Bun #429](https://github.com/oven-sh/bun/issues/429) (the original open issue): for years, `process.on('uncaughtException')` simply did not fire on Bun. From the dup: *"`process.on` technically works but it does not monitor the events as expected, it just extends from `EventEmitter`."* That is: the EventEmitter wiring existed, so listeners could be registered, but the runtime never delivered events to them. A six-month-plus open bug at minimum. Sentry's [issue #5091](https://github.com/oven-sh/bun/issues/5091) tracking this specifically lists the missing process events that prevented Sentry from working on Bun until they got fixed.

**Bug 5 — `process.nextTick` uncaught exceptions firing handlers multiple times / exit code 0.** [Bun PR #27229](https://github.com/oven-sh/bun/pull/27229) (2026):

> Exceptions thrown inside `process.nextTick` callbacks (including EventEmitter `"error"` handlers scheduled via `emitErrorNT`/`emitErrorCloseNT`) were caught by `processTicksAndRejections` but not properly treated as fatal uncaught exceptions. The process continued running after the throw, causing the error handler to fire multiple times and the process to exit with code 0.

This is precisely the dual-channel ordering issue from §2.1: two places could legitimately treat a throw as fatal — the JS-level `process.nextTick` machinery and the runtime-level uncaught-exception handler — and they were not coordinated.

### 3.3 Bun's documentation status

[Bun's `process` reference](https://bun.com/reference/node/process) self-documents: *"`process.binding` (internal Node.js bindings some packages rely on) is partially implemented... See the 'process' entry in the globals section for specific implementation status and missing APIs."*

The page lists `uncaughtException`, `uncaughtExceptionMonitor`, `unhandledRejection`, and `rejectionHandled` as available events, but the surrounding caveats indicate ongoing partial implementation work.

### 3.4 What this tells us about Bun's design call

Bun **did not** make Deno's design call.

Bun ships `globalThis` as an EventTarget natively (because Bun is JSC-based and inheriting `WorkerGlobalScope` is the natural shape), and ships `process.on` as a separate EventEmitter-based channel the runtime notifies separately. The two channels are *parallel* on Bun, not *forwarded* — which is why all five bug classes above exist: two error-reporting code paths that must stay coordinated, and have not.

---

## 4. What Deno actually does

Deno picked one canonical channel: `globalThis` dispatches, and the `process` polyfill forwards to it. That removes the ambiguity Bun still carries, but only because Deno owns `process` — so the design does not transfer to Nub.

### 4.1 Mechanism

Deno's `globalThis` is `instanceof EventTarget` natively — Deno was built web-shape-first. Its `process` polyfill is implemented *as a forwarder over `globalThis`*. From [`denoland/deno/ext/node/polyfills/process.ts`](https://github.com/denoland/deno/blob/main/ext/node/polyfills/process.ts):

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

The design choice: **`globalThis` is canonical, `process` is the forwarder.** The listener calls `event.preventDefault()` to suppress Deno's default crash, and `process.listenerCount` is the gate — no `process` listeners means wrap in `ERR_UNHANDLED_REJECTION` and treat as uncaught; listeners present means re-emit on the EventEmitter.

This is a **resolvable** design — it just requires the runtime to *own* `process` (it's a polyfill they wrote) and to *pick* one channel as canonical.

### 4.2 Engineering cost Deno paid

The "globalThis canonical, process forwarder" choice has not been free:

- **[denoland/deno#19307](https://github.com/denoland/deno/pull/19307) (2023):** Deno had to engineer a "managed globals" semi-proxy to segregate Node-mode and Deno-mode globals. The two modes see different sets of globals via runtime mode detection. *"This commit makes the `globalThis` of the entire runtime a semi-proxy. This proxy returns a different set of globals depending on the caller's mode."*
- **[denoland/deno#24637](https://github.com/denoland/deno/pull/24637) (Jul 2024):** Deno *removed* `self` from node-compat mode because npm packages were misdetecting Deno as a browser via `typeof self`. `self` is a much cheaper polyfill than the EventTarget swap and Deno still had to walk it back.
- **[denoland/deno#32535](https://github.com/denoland/deno/pull/32535) (Mar 2026):** Deno had to wrap non-Error unhandled rejections in `ERR_UNHANDLED_REJECTION` to match Node's behavior — *"Deno was passing the raw value directly, which caused crashes when exception handlers accessed `.message` or `.name`."* As of March 2026, 14 of 20 promise compat tests still failing.

Deno's approach works better than Bun's — the forwarder eliminates one of the two parallel channels by making it a wrapper around the other — but getting there took three-plus years of compat-mode engineering and requires Deno to own and re-implement `process` from scratch.

### 4.3 Deno's resolution generalizes — but not to Nub

If Nub *replaced* Node's `process`, the Deno design would generalize: pick `globalThis` as canonical, route everything through it, wrap the `process` shape as a forwarder.

But Nub augments unmodified Node, so it would have to either run two parallel channels (Bun's choice → Bun's bugs) or intercept `process.on` calls and route them to `globalThis`, which violates additivity — `process.on` would change behavior under Nub.

---

## 5. What Cloudflare Workers does

CF Workers has no `process` in the strict sense (they ship a partial polyfill but not the full `process.on(...)` event surface), so the dual-channel problem from §2.1 doesn't exist there.

There are still nontrivial unhandled-rejection bookkeeping bugs:

- **[cloudflare/workerd#6020](https://github.com/cloudflare/workerd/issues/6020) (2025):** `unhandledrejection` was misfiring on Workers — promises that *were* handled (via `.then(...).catch(...)` or `assert.rejects(async () => ...)`) were being reported as unhandled because workerd fired the event before V8's promise microtask chain had a chance to settle. **Fix in [PR #6049](https://github.com/cloudflare/workerd/pull/6049):** delay the unhandledrejection report until after V8's microtasks-completed callback fires, behind a feature flag.

Informative for Nub in one specific way: even a *single-channel* runtime with no `process` ships nontrivial unhandled-rejection bugs, so "just ship the EventTarget swap and you're done" oversimplifies the implementation cost.

CF Workers does ship the WHATWG handler attributes ([`globalThis.addEventListener('error', ...)`](https://developers.cloudflare.com/workers/runtime-apis/handlers/), `globalThis.addEventListener('unhandledrejection', ...)`) on its own `ServiceWorkerGlobalScope`-style global. That is what `globalThis instanceof EventTarget` looks like *in a runtime that never had `process`*. Nub is not that runtime — Nub has `process` because Node has `process`.

---

## 6. Honest reassessment of the prior research

The prior research at [[research/wintertc-node-gap-rationale]] made three claims under stress:

### 6.1 "Unresolvable semantics" — overstated, but the underlying call still holds

The prior research said the dual-channel design produces "unresolvable semantics." That's too strong.

**Bun resolved them** by picking parallel channels and accepting coordination bugs; **Deno resolved them** by picking globalThis as canonical and writing process as a forwarder. The Node maintainer position, read carefully, is *"the resolutions we've seen require engineering investment and migration strategy that the value doesn't justify"* — a cost-benefit position, not an impossibility position.

**Recommended tightening for the prior framing:** replace "unresolvable semantics" with "the dual-channel design has resolutions, but each resolution has multi-year engineering cost (Bun is still grinding through it; Deno paid the cost by owning process) and Nub cannot afford either path because Nub does not own process."

### 6.2 "Bug shape Node maintainers predicted has materialized in Bun" — strengthens the prior conclusion

The prior research framed the Node TSC's objection as principled-and-load-bearing without specific empirical evidence beyond the maintainer comments. The five Bun bug classes in §3.2 map to specific maintainer predictions and **strengthen** that framing:

- joyeecheung predicted worker-scope-on-main-thread bugs (§2.5). Bun #24256 (`globalThis.onmessage` hangs the process) is exactly that bug.
- jasnell predicted cross-channel `preventDefault` ambiguity (§2.1). Bun #29043 (preventDefault on `error` doesn't suppress propagation) is exactly that bug.
- benjamingr predicted error-story complication (§2.2). Bun #28648 (re-entrant uncaughtException crashes the runtime) and Bun PR #27229 (handlers firing multiple times, exit code 0) are exactly that complication.
- jasnell predicted `listenerCount` ambiguity (§2.2). Not yet a filed Bun bug, because users haven't started writing code that relies on cross-channel listener counts; but it is structurally present in Bun's design.

### 6.3 "Additivity violation" framing — was the right framing, for the right reason

The prior research argued that shipping `globalThis instanceof EventTarget` is non-additive because it changes:
1. Direct probe answers (`globalThis instanceof EventTarget`, `Object.getPrototypeOf(globalThis)`)
2. WebIDL fallback semantics (`EventTarget.prototype.dispatchEvent.call(null, ev)`)
3. Prototype-walk inspection

All three are correct, but the new evidence surfaces a more important concern: **the wiring is what's non-additive, not just the prototype chain.** If Nub ships the prototype swap *without* wiring `process.on('uncaughtException')` to `globalThis.dispatchEvent(new ErrorEvent('error', ...))`, then `globalThis.addEventListener('error', fn)` registers a handler that never fires — a worse failure mode than the prior `undefined`, because it is a setter that silently swallows. Adding the wiring makes Nub's `process` behavior differ from plain Node's, which additivity forbids. **Either way, additivity fails.**

### 6.4 The reflect-metadata parallel — does it apply here?

In the [[research/emit-decorator-metadata]] discussion, an "additivity-violating" framing was wrong because the proposed feature was correctly additive. Does that parallel apply here?

**No, and the reason is informative.** The `emitDecoratorMetadata` transform is purely additive at the *transpilation* layer — it adds `Reflect.defineMetadata(...)` calls to classes Nub transpiles and changes nothing for classes it does not.

The EventTarget swap is different: no opt-in surface makes it conditional on user request, so the instant the prototype chain is swapped, all three probes above return a different answer for every library in the process. **The probe surface is global and unopt-outable.**

The `--globalthis-eventtarget` flag from the prior research's "future opt-in" position is the right escape: behind a flag, the swap is opt-in, the additivity violation only applies to users who explicitly asked, and the default behavior matches plain Node.

---

## 7. Implementation cost in Nub

Three shapes were costed: the prototype swap alone, the swap plus `process` wiring, and the swap behind a flag. The code is 15 to 30 lines of preload; the expense is the bug class the wiring imports.

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

**Cost:** ~15 LOC of preload. **Behavior:** `globalThis instanceof EventTarget === true`; `globalThis.addEventListener('error', fn)` registers a handler that never fires; `globalThis.onerror = fn` likewise.

**This is a worse failure mode than not shipping** — feature-detection libraries that probe `typeof globalThis.onerror === 'object'` (it's `null` on the web when unset, which is `typeof 'object'`) will get a "yes" answer and proceed to set a handler that will never be called, then file bugs against Nub. **Do not ship Option B.**

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

- **The `preventDefault()` problem is unfixable from a preload.** Node's `--unhandled-rejections` flag controls whether unhandled rejections crash the process; it's a process-startup decision. Inside a `process.on('unhandledRejection', ...)` callback there is no way to retroactively tell Node "don't crash" — the crash decision was made when the flag was parsed. So even if a user calls `event.preventDefault()` in their `globalThis` handler, Node's default behavior (crash on unhandled rejection, if `--unhandled-rejections=strict`) still fires. **This replicates Bun's #29043 in Nub.**
- **Ordering becomes Nub's problem.** If a user has both `process.on('unhandledRejection', ...)` and `globalThis.addEventListener('unhandledrejection', ...)`, which fires first? Whatever Nub picks is observable and will get filed as a bug by users who expected the other order.
- **`process.listenerCount` becomes ambiguous.** As jasnell predicted: `process.listenerCount('unhandledRejection') === 0` when the user has only `globalThis` handlers.
- **The `(reason, promise)` → `PromiseRejectionEvent` adapter is lossy in the other direction too.** A user who registers `globalThis.addEventListener('unhandledrejection', e => process.exit(1))` and *also* has a `process.on('unhandledRejection', (reason, promise) => ...)` listener has a race: the `globalThis` handler exits the process, the `process` handler may or may not run depending on dispatch ordering. Bun has had this class of bug (PR #27229).

**Cost in bug-report surface:** the same five Bun bug classes from §3.2 land on Nub, plus Bun's two-year grind to work through them. **Do not ship Option C.**

### 7.3 Behind a flag — Option C-prime (future v0.x `--globalthis-eventtarget`)

The new evidence does not change the prior research's assessment — the opt-in flag is the right escape hatch for the rare user who needs the EventTarget shape, with the cost documented in the flag's help text.

**Keep the flag deferred to v0.x**, and when a concrete user requests it, evaluate the bug-class import-cost against that user's specific need.

### 7.4 The userland one-liner (always available, never blocked)

Per jasnell's own polyfill, anyone who wants `globalThis instanceof EventTarget` can put this in their own `--import` preload, today, on plain Node or Nub:

```js
Object.setPrototypeOf(globalThis, new EventTarget());
```

Nub does not have to ship anything for this to be available; documenting it as the escape hatch is the right call.

---

## 8. Recommendation

**(A) Stand by the current decision.** Don't ship `globalThis instanceof EventTarget`, don't ship the three `on*` handler attributes, in v0.1 or v0.x default.

**One-line rationale:** The Node maintainer concerns from 2022–2025 have materialized as specific open and recently-fixed bugs in Bun, including the exact `preventDefault`, `listenerCount`, re-entrancy, and worker-scope-context-bleed failure modes the maintainers cited; Bun has paid two years of engineering cost grinding through them; Deno avoided that cost only by owning `process`, which Nub cannot do.

**What changes versus the prior research:**

- **Tighten the rationale, not the conclusion.** Replace "unresolvable semantics" with "the dual-channel design has resolutions, but each resolution has multi-year engineering cost that Nub cannot afford because Nub does not own `process`." Cite the specific Bun bugs (§3.2) and the specific Deno engineering cost (§4.2).
- **Keep the future `--globalthis-eventtarget` opt-in flag deferred to v0.x.** Don't promise it for v0.1; don't promise it ever, but don't rule it out. Evaluate when a concrete user request lands.
- **Document the userland one-liner explicitly** as the escape hatch for users who want the swap. `Object.setPrototypeOf(globalThis, new EventTarget())` works on plain Node and Nub alike; no Nub-specific surface needed.
- **Hold the line on `onerror` / `onunhandledrejection` / `onrejectionhandled`** exactly as the prior research recommended. Drop them from v0.1.

**Why not (B) reverse:** the failure modes are concrete and predictable rather than speculative, and Nub can neither afford Bun's grind nor use Deno's resolution.

**Why not (C) opt-in flag in v0.1:** adding a flag for a feature with no concrete user request is bug-report surface for no return. Defer until someone asks; the userland one-liner is sufficient for everyone who currently asks.

---

## 9. Open questions

Four questions left open. The inert event class is already cleared for v0.1; the rest wait on a concrete user request or on Bun landing a resolution.

- **Is there a way to safely ship just the inert `PromiseRejectionEvent` class without the dispatch wiring?** The prior research said yes (matches Node's own `ErrorEvent` v25 precedent). This stress-test confirms: an inert constructor that users dispatch on their own `EventTarget`s is fine. Already in the v0.1 set.
- **Should an issue be filed against [Bun #29043](https://github.com/oven-sh/bun/issues/29043) to track resolution?** Not Nub's job, but watching that bug is informative — if Bun lands a resolution, the design lessons may unblock the case for a future Nub shipment.
- **When (if ever) the `--globalthis-eventtarget` flag lands, should it auto-wire `process.on('uncaughtException')` → `globalThis.dispatchEvent`?** Open. The flag could come in flavors: `--globalthis-eventtarget=prototype-only` (Option B-style, no wiring, fires only on explicit `globalThis.dispatchEvent`) vs. `--globalthis-eventtarget=full` (Option C-style, with wiring and the bug class). The prototype-only flavor is honest and useful for polyfill-author audiences; the full flavor is what Bun does. Decide at flag-design time.
- **What does the WinterTC compliance number look like under this recommendation?** Same as the prior research: ~98%, with the four `EventTarget`-dependent items held out. Confirmed.

---

## 10. Sources

Maintainer threads and TSC minutes behind the Node rejection, source and issue trackers for Bun, Deno and Workers, the specs governing handler attributes, and the prior doc this one stress-tests.

### Node maintainer threads

Four issues and PRs spanning Dec 2022 to Jun 2025, plus the two TSC meetings that set the consensus. jasnell opened the last two and closed both as `not_planned`.

- [nodejs/node#45981 — make global object an instance of `EventTarget`](https://github.com/nodejs/node/issues/45981) (opened Dec 26, 2022 by jimmywarting; closed May 31, 2023)
- [nodejs/node#45993 — events,bootstrap: make globalThis extend EventTarget](https://github.com/nodejs/node/pull/45993) (PR by KhafraDev, opened Dec 28, 2022; still open as of 2026-04-28; TSC consensus against)
- [nodejs/node#51372 — Revisiting `globalThis` as an `EventTarget`](https://github.com/nodejs/node/issues/51372) (opened Jan 4, 2024 by jasnell; closed `not_planned` Jun 9, 2025 by jasnell)
- [nodejs/node#57352 — globalThis as an EventTarget](https://github.com/nodejs/node/issues/57352) (opened Mar 6, 2025 by jasnell; closed `not_planned` Jun 9, 2025 by jasnell)
- [nodejs/TSC#1323 — TSC meeting 2023-01-04](https://github.com/nodejs/TSC/issues/1323) (initial TSC consensus against)
- [nodejs/TSC#1489 — TSC meeting 2024-01-10](https://github.com/nodejs/TSC/issues/1489) (re-affirmed)

### Bun source and issues

The five bug classes with their fixes, plus the two source files that show how `globalThis` and the `on*` handler attributes are wired.

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

The forwarder polyfill itself, plus the three PRs showing what the globalThis-canonical choice cost Deno in compat-mode engineering.

- [denoland/deno: `ext/node/polyfills/process.ts`](https://github.com/denoland/deno/blob/main/ext/node/polyfills/process.ts) (canonical forwarder design — globalThis canonical, process forwards)
- [denoland/deno#19307 — properly segregate node globals](https://github.com/denoland/deno/pull/19307) (managed-globals semi-proxy)
- [denoland/deno#24637 — do not expose `self` global in node](https://github.com/denoland/deno/pull/24637) (removed `self` from node-compat mode after npm packages misdetected Deno as browser)
- [denoland/deno#32535 — wrap non-Error unhandled rejections in ERR_UNHANDLED_REJECTION](https://github.com/denoland/deno/pull/32535) (Mar 2026; promise compat work)

### Cloudflare Workers source and issues

The unhandled-rejection misfire and its fix, plus the global-scope source for a runtime that never had `process` at all.

- [cloudflare/workerd#6020 — unhandledRejection misfires a lot](https://github.com/cloudflare/workerd/issues/6020) (2025)
- [cloudflare/workerd#6049 — fix unhandledRejection misfires](https://github.com/cloudflare/workerd/commit/bd3b76acd9dd4666bacccb24ebf0f7d73d25ec91) (microtasks-completed callback)
- [cloudflare/workerd: `src/workerd/api/global-scope.c++`](https://github.com/cloudflare/workerd/blob/main/src/workerd/api/global-scope.c%2B%2B) (verified mechanism — `ServiceWorkerGlobalScope` is the EventTarget)
- [cloudflare/workerd: `src/node/internal/events.ts`](https://github.com/cloudflare/workerd/blob/main/src/node/internal/events.ts) (Node-compat events polyfill for Workers)

### Specs and references

The IDL contracts for handler attributes and floating methods, the web cancellation semantics, Node's own error model, and the WinterTC requirement that drives the gap.

- [WHATWG HTML §event-handler-attributes](https://html.spec.whatwg.org/multipage/webappapis.html#event-handler-attributes) (the IDL contract for `on*` handler attributes)
- [WebIDL §dfn-create-operation-function](https://webidl.spec.whatwg.org/#dfn-create-operation-function) (floating-method fallback to `globalThis`)
- [MDN: Window: unhandledrejection event](https://developer.mozilla.org/en-US/docs/Web/API/Window/unhandledrejection_event) (`preventDefault()` cancellation semantics on the web)
- [Node.js process docs: `'uncaughtException'`](https://nodejs.org/api/process.html) (Node's equivalent error model; no per-event cancellation API)
- [WinterTC55/proposal-minimum-common-api#82](https://github.com/wintercg/proposal-minimum-common-api/pull/82) (the WinterTC requirement that drives this gap)

### Nub cross-references

The prior research this doc stress-tests. It is write-once and was not amended.

- [[research/wintertc-node-gap-rationale]] — prior research being stress-tested (write-once; not amended)

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
