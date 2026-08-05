# WinterTC Min-Common-API — why Node hasn't shipped the last 5%

**Date:** 2026-05-22 **Question:** Of the seven WinterTC Minimum Common API globals that Node 26 still doesn't ship (`reportError`, `self`, `PromiseRejectionEvent`, `onerror`, `onunhandledrejection`, `onrejectionhandled`, `globalThis instanceof EventTarget`), how many are missing for good architectural reasons vs. low priority vs. active maintainer resistance? The instinct under test was that these "should be easy" and Node's non-implementation must therefore signal *something* — is that read correct? **Extends:** `wintertc-compliance-gaps.md` (the broader Phase-2 survey from 2026-05-18). **Drives the decision in:** `../runtime/min-common-api-globals.md`.

---

## TL;DR (the thesis)

The Node gap is **not unprincipled.** Once you trace the issues and PRs end-to-end, the consensus that emerges is **coherent and load-bearing**, not low-priority drift. The pivot is `globalThis instanceof EventTarget`: every other gap on the list either (a) only makes sense if `globalThis` is an `EventTarget`, or (b) is a near-trivial alias that Node hasn't shipped mostly because no one champion has pushed it across the finish line. So the seven gaps split cleanly into two piles:

- **Three "free" gaps Node could ship tomorrow with no architectural cost** — `reportError`, `self`, the `PromiseRejectionEvent` *class* (as an inert constructor). These are missing for low-priority / stalled-PR reasons, not active resistance. Node members (`benjamingr`, `aduh95`) have already approved the work; the PRs just rotted. Nub's polyfill on these three is straightforwardly correct.
- **Four gaps that hang off one architectural decision** — `globalThis instanceof EventTarget`, plus the three `on*` global handler properties that are *meaningless without it*. Here there's a documented, principled "no" from `mcollina`, `benjamingr`, and `jasnell` (the issue was formally closed `not_planned` 2025-06-09 by jasnell). The objection: two parallel global event channels (`process.on(...)` and `globalThis.addEventListener(...)`) create unresolvable semantics around ordering, `stopImmediatePropagation`, and `listenerCount`. The gap is intentional.

**Implication for Nub (preview, full version below):** the polyfill of `reportError`, `self`, and the `PromiseRejectionEvent` class is fine and should ship. The polyfill of `onerror` / `onunhandledrejection` / `onrejectionhandled` as "accessor properties on `globalThis` that delegate to `process.on(...)`" — which is what `min-common-api-globals.md` §"Mechanism" item 5 currently proposes — is **semantically broken** and should be reconsidered. The handler-property contract on the web includes `preventDefault()` cancellation semantics and a specific `PromiseRejectionEvent` shape that `process.on('unhandledRejection', reason, promise)` does not satisfy. Shimming creates a *false* compliance signal: a library that probes `typeof onunhandledrejection === 'object'` and sets the handler will get *partially* working behavior on Nub, which is worse than the clear `undefined` it gets on Node. Nub should either (a) ship these handlers behind the same opt-in flag we're already reserving for the `EventTarget` swap, or (b) drop them from the v0.1 set. Recommend (b).

The non-`EventTarget` polyfills (`reportError`, `self`, `PromiseRejectionEvent` class) take Nub from ~95% → ~98% compliance with zero semantic risk. The `on*` handler properties were going to be the last ~1% and are not worth the bug-report surface. Confirmed that instinct *partially* — three of the seven gaps are "Node just hasn't shipped it" and Nub's polyfill is a real ergonomic win; four are "Node has thought about it and said no for documented reasons" and Nub trying to bridge them creates more confusion than it removes.

---

## Per-gap analysis

Each gap below resolves to one of: **good architectural reason** (Node has a different but valid mechanism), **spec friction** (implementing the WHATWG shape conflicts with Node invariants), **low priority** (no objection, just hasn't shipped), or **active resistance** (documented maintainer "no").

### 1. `reportError(error)` — **Low priority + stalled PR**

**Issues:** [nodejs/node#38947](https://github.com/nodejs/node/issues/38947) (Jun 2021), [nodejs/node#41912](https://github.com/nodejs/node/pull/41912) (Feb 2022). **Status:** Issue closed by stale-bot Sep 2022; PR closed by stale-bot **Dec 2025** after sitting open for ~3.5 years. **Author:** `benjamingr` (Node MEMBER). **Approvers:** `jasnell` (approved Feb 2022), `aduh95` ("+1 on this landing in Node.js as a global"). **WHATWG spec:** [HTML §runtime-script-errors](https://html.spec.whatwg.org/#runtime-script-errors). **Cloudflare shipped:** [workerd#1979](https://github.com/cloudflare/workerd/pull/1979) (Apr 2024). **Deno shipped:** [denoland/deno#13484](https://github.com/denoland/deno/issues/13484).

**The design question that stalled it:** what should `reportError(e)` do in Node? Two camps:

- "Crash the process on next tick" (the literal browser equivalent given Node's error model). benjamingr: *"I am honestly still kind of not sure we should actually do this — waiting for more people to ask for this."*
- "Log + forward to `process.on('uncaughtException', ...)` / `process.on('error', ...)`" (the browser-equivalent-via-Node-events shape).

`camsteffen` framed the catch-22 well in Aug 2024: *"`reportError` to me sounds a lot like it will *not* crash. But on the other hand, crashing on unexpected errors is generally the thing to do in node. So maybe this function (name) only makes sense in browsers."* That uncertainty, combined with semver-major label and Node's normal stalled-PR rot, killed it. There is **no documented "we don't want this" from any maintainer** — only "we're not sure of the right semantics."

**Categorization:** **Low priority**, edge-of-spec-friction. The semantic ambiguity is real but resolvable (Deno resolved it: log immediately + uncaught next tick + flow through normal error handlers). Library authors actively want this — `getify`, `benlesh` (RxJS author), and Cloudflare maintainers all asked for it. **Nub polyfill verdict: ✅ fine to ship.** The polyfill in `min-common-api-globals.md` (`setImmediate(() => { throw error; })`) matches the Node-PR-41912 proposed behavior, which the Node maintainers who weighed in approved.

### 2. `self` — **Low priority + minor architectural friction (worker_threads ≠ Web Workers)**

**Issue:** [nodejs/node#28728](https://github.com/nodejs/node/issues/28728) (Jul 2019). **Status:** closed by `benjamingr` *"by design — `worker_Threads` are not web workers, we discussed this in the summit and decided that we do not have immediate compatibility."* That call predates Node's adoption of WHATWG `EventTarget` (Node 15, late 2020), `fetch` (Node 18), `WebSocket`, etc. The world has moved on; the issue has not been reopened. **De facto polyfill:** [`node-self`](https://www.npmjs.com/package/node-self) (`global.self = global` one-liner).

**Deno cross-reference:** [denoland/deno#24637](https://github.com/denoland/deno/pull/24637) Jul 2024 *removed* `self` from Deno's node-compat mode because npm packages were misdetecting Deno as a browser via `typeof self`. That's an interesting data point: in node-compat-mode contexts, exposing `self` causes more harm than good because the surrounding context (no `window`, no DOM) doesn't match the assumption "if `self` is defined, I'm in a browser-like environment." Bun also exposes `self` and has not reported this class of bug because Bun's surface is more web-leaning overall.

**Categorization:** **Low priority** with a small dose of **architectural friction**. The 2019 "by design" closure was tied to a specific argument (worker_threads aren't Web Workers) that has weakened as Node has absorbed more web shape. There is no current maintainer opposition; the issue is simply that no one has filed a fresh PR. **Nub polyfill verdict: ✅ fine to ship**, with the caveat that we should be aware Deno saw real-world `typeof self !== 'undefined'` false positives in node-compat mode. Nub's defense: we're *adding* web shape on top of Node, not pretending to be a browser, and the rest of our globals (no `window`, no `document`) will be consistent with "this is a Node-like environment that also has `self`." Low risk.

### 3. `PromiseRejectionEvent` (the class) — **Spec friction; inert without globalThis EventTarget**

**No dedicated Node issue.** The class is implicit in the bigger `globalThis as EventTarget` discussion (#51372, #57352, #45993). The WHATWG `PromiseRejectionEvent` is *defined* as the event object dispatched to `globalThis.addEventListener('unhandledrejection', ...)`. Node's equivalent is `process.on('unhandledRejection', (reason, promise) => ...)` — note the **different shape**: Node uses a callback with two positional arguments, the web uses an `Event` object with `.reason` and `.promise` accessors plus `.preventDefault()` to cancel.

**Categorization:** **Spec friction**. Defining `PromiseRejectionEvent` as a global class in Node would be *inert* — the class would never be dispatched anywhere because there's no `globalThis.addEventListener('unhandledrejection', ...)` machinery to dispatch it to. Node ships `ErrorEvent` (added in v25) as a similar inert class for similar reasons (used by `Worker` postMessage error reporting); shipping `PromiseRejectionEvent` would follow the same pattern. The only blocker is "no one has filed the PR." **Nub polyfill verdict: ✅ fine to ship as an inert class** — matches Node's own `ErrorEvent` precedent. Users who construct `new PromiseRejectionEvent(...)` and dispatch it on an `EventTarget` they own (e.g. a `Worker`) will get spec-correct behavior. Users expecting the class to be auto-dispatched on `globalThis` for unhandled rejections will be disappointed, but that disappointment is the *same* on Nub as on Node — it's the `globalThis instanceof EventTarget` gap (#7 below), not the class gap.

### 4-6. `onerror` / `onunhandledrejection` / `onrejectionhandled` — **Spec friction (semantically meaningless without EventTarget)**

These three are a single concern. On the web, they are **handler IDL attributes** on `WindowOrWorkerGlobalScope`: setting `globalThis.onerror = fn` is spec-equivalent to `globalThis.addEventListener('error', fn)` with the IDL "event handler" semantics layered on top (the handler's return value determines `preventDefault`, the handler is a single slot per event type, etc.). Per [WHATWG HTML §event-handler-attributes](https://html.spec.whatwg.org/multipage/webappapis.html#event-handler-attributes), these only exist on objects that **are** `EventTarget`s.

**Node's equivalent:** `process.on('uncaughtException', fn)` (for `onerror`), `process.on('unhandledRejection', fn)` (for `onunhandledrejection`), `process.on('rejectionHandled', fn)` (for `onrejectionhandled`). Three substantive deltas from the web shape:

1. **Callback signature.** Node passes `(reason, promise)` to `unhandledRejection`. Web passes `PromiseRejectionEvent` with `.reason` / `.promise` accessors. Same data, different shape.
2. **Cancellation semantics.** Web: `event.preventDefault()` in the handler suppresses the default "log to console and reject" behavior. Node: the handler's return value is ignored; cancellation is controlled by `--unhandled-rejections=...` flag at process-startup time. **There is no per-event cancellation API in Node.**
3. **Handler slot semantics.** Web handler attributes are *single-slot* (`globalThis.onerror = fn1; globalThis.onerror = fn2;` — `fn1` is gone). Node's `process.on(...)` is *additive* (both run). The IDL semantics of "set then get returns the same function, set to `null` removes" are not naturally expressible on `EventEmitter`.

**Categorization:** **Spec friction**, edging into **good architectural reason**. Node has equivalent functionality in a meaningfully different shape; the shapes are not bridgeable without choosing which semantics to honor. The Node maintainer thread on #57352 spent most of its energy on exactly this question: `mcollina`: *"I don't think we'll ever be able to migrate from process events to globalThis."* `benjamingr`: *"this further complicates our already complicated error handling story with more events and orders which can negatively impact usability. This is made worse by libraries possibly checking for globalThis being an event emitter as a potential probe and adjust their error handling accordingly."*

**Nub polyfill verdict: ⚠️ reconsider.** `min-common-api-globals.md` §"Mechanism" item 5 proposes: *"For `onerror`/`onunhandledrejection`/`onrejectionhandled`: defined as accessor properties on `globalThis` that delegate to `process.on(...)`."* This is what the Node maintainers explicitly warned against — *"libraries possibly checking for globalThis being an event emitter as a potential probe and adjust their error handling accordingly."* A polyfill that:

- accepts `globalThis.onunhandledrejection = fn` and stores it, ✅
- forwards rejections to `fn`, but with `(reason, promise)` not `PromiseRejectionEvent`, ✗
- ignores `event.preventDefault()` because there's no event object, ✗
- doesn't replace prior handlers (because `process.on` is additive), ✗

…doesn't pass the additivity test from `../architecture/augmenter-not-fork.md`. It changes the *observable answer* to `typeof globalThis.onunhandledrejection` from `'undefined'` to `'object'`/`'function'`, which is exactly the kind of cross-runtime feature-probe that produces "works on Nub, breaks on plain Node" or "works on Nub, breaks differently than browser" bugs. **Recommendation: drop these three from the v0.1 polyfill set.** Document them as known gaps that require `globalThis` to be an `EventTarget`, which Nub deliberately doesn't ship. Users who want the handler-attribute shape on Nub can opt into the `EventTarget` swap (item 7) and get it correctly, or use `process.on(...)` and have their code run on plain Node.

### 7. `globalThis instanceof EventTarget` — **Active resistance + good architectural reason, formally documented**

**Issues:** [nodejs/node#45981](https://github.com/nodejs/node/issues/45981) (Dec 2022, opened by `jimmywarting`), [nodejs/node#45993](https://github.com/nodejs/node/pull/45993) (PR by `KhafraDev`, opened Dec 2022, still open as of 2026-04-28, never merged), [nodejs/node#51372](https://github.com/nodejs/node/issues/51372) (Jan 2024, "Revisiting", closed `not_planned` 2025-06-09 by `jasnell`), [nodejs/node#57352](https://github.com/nodejs/node/issues/57352) (Mar 2025, "globalThis as an EventTarget", closed `not_planned` 2025-06-09 by `jasnell`). **TSC consensus:** discussed Jan 4, 2023 and Jan 10, 2024. `mhdawson` Jan 4 2023: *"We discussed in the TSC meeting today, at this point the consensus seems to be that value does not outweigh the issues/problems unless there is a compelling use case we are not aware of."*

**Categorization:** **Active resistance**, principled. The opposition is not from one maintainer; it's from four senior maintainers across multiple years, ratified by the TSC, and most recently re-affirmed by jasnell himself closing his own re-opening of the question in June 2025.

The substantive arguments against (compiled from the four threads):

- **mcollina, repeatedly:** *"libraries emitting global events make the ecosystem more brittle."* *"None of those mechanisms are correct for library authors."* *"Most libraries should not touch global objects, because they can conflict in behaviors."*
- **benjamingr:** *"complicates our already complicated error handling story with more events and orders which can negatively impact usability."* *"libraries possibly checking for globalThis being an event emitter as a potential probe and adjust their error handling accordingly."*
- **jasnell:** *"non-trivial risk of breaking stuff (adding globals is always risky, changing the global prototype even more so)."*
- **addaleax:** *"I'm not even sure that browsers would do this nowadays with the benefit of hindsight in how JS developed as a language."*
- **joyeecheung, the most nuanced:** *"out of the common events fired on window in the browsers, the ones that make more sense for Node.js are `message`, `unhandledrejection`, `rejectionhandled`, which we already (sort of) fire on `process`, but with disparities in the shapes of the event objects. It seems to be confusing to make `globalThis` in Node.js an `EventTarget` without a good story or at least documentation of how we are going to handle these disparities ... Users might attempt to use these well-specified events in `globalThis` and realize that it doesn't actually work like how things are in browsers, which IMO is worse than not making `globalThis` an `EventTarget`."*

The unifying objection across all five: **two parallel event channels with different semantics will create unresolvable user-visible ambiguity**. The discussion in #57352 spent dozens of comments trying to nail down ordering (`process` first or `globalThis` first?), `stopImmediatePropagation` cross-channel effects, `process.listenerCount('error')` returning 0 when there are `globalThis` handlers, etc. — and could not converge.

There is a real countervailing perspective from `KhafraDev`, `jcbhmr`, `jimmywarting`, `joyeecheung` (partial), and `ljharb` — all pointing at the real cross-runtime ergonomics cost. But the maintainer consensus held. **This gap is not closing.**

---

## Deep dive: the `EventTarget` prototype-chain question

Nub's `min-common-api-globals.md` describes shipping `globalThis instanceof EventTarget = true` as requiring *"modifying the prototype chain of `globalThis`, which is non-additive."* Let's check that characterization against what's actually technically required, both in user-land and inside Node.

**The user-land one-liner.** James Snell himself, in PR #45993 comment on May 29, 2023, provided the polyfill:

```js
Object.setPrototypeOf(globalThis, new EventTarget());
```

Plus the floating `addEventListener` / `removeEventListener` / `dispatchEvent` globals (added because WebIDL [§dfn-create-operation-function](https://webidl.spec.whatwg.org/#dfn-create-operation-function) says "default `this` to `globalThis` when `null`/`undefined`"). So **the technical operation is a single `Object.setPrototypeOf` call plus three function-property installations**, doable from a `--import` preload.

**Why "non-additive" is correct.** "Additive" in Nub's posture (per `../runtime/additivity.md`) means: code written for plain Node continues to observe the same behavior under Nub. Shipping `globalThis instanceof EventTarget = true` violates this in measurable ways:

1. **Direct probe.** Any code that does `globalThis instanceof EventTarget`, or `Object.getPrototypeOf(globalThis) === Object.prototype`, gets a different answer under Nub vs. under plain Node. This is rare in user code but common in cross-runtime feature-detection libraries.
2. **WebIDL fallback semantics.** `EventTarget.prototype.dispatchEvent.call(null, new Event('x'))` — `null` here gets coerced to `globalThis` per the WebIDL fallback. On plain Node this is a `TypeError`. On Nub-with-the-shim this dispatches an event. Different behavior.
3. **Prototype-walk inspection.** `for (let p = globalThis; p; p = Object.getPrototypeOf(p)) { ... }` traverses a different chain. Frameworks that inspect prototypes (jest, vitest, certain DI containers) get different walks.

**Does Node have the same constraint?** No — Node would not face the additivity concern *because Node's behavior is the reference*. If Node shipped this natively, the new behavior would *be* the Node behavior; there's nothing to be additive *to*. But Node *does* face a different version of the same concern: **backward-compatibility**, i.e. "is the new behavior breaking for code that targets old Node." That's the semver-major label on #45993 and #41912 — and it's what TSC consensus weighed against the benefits and found wanting.

The mechanism for Node would also differ: Node would do this in C++ during context initialization (in `src/node_contextify.cc` / bootstrap), setting the `JSGlobalProxy`'s prototype to `EventTarget.prototype` before any user JS runs. There's no `globalThis` to "swap" because it's being built up; the prototype chain is just declared correctly from the start. This is *strictly* cleaner than what Nub could do from a preload — Nub has to swap after Node has already finalized the global, Node would never face the swap. So the V8 mechanics are not a blocker for Node; **it's a pure policy decision** and they've made it.

**Implication:** Nub's "non-additive, don't ship even behind a flag in Phase 2" call holds. The brand-promise hazard is real, and the userland polyfill is one line away for anyone who needs it (`Object.setPrototypeOf(globalThis, new EventTarget())` in their own preload). We should document the one-liner as the escape hatch rather than ship it ourselves.

One refinement: we could ship a `nub` *flag* (`--globalthis-eventtarget` or env `NUB_GLOBALTHIS_EVENTTARGET=1`) that does the one-liner. Cost is ~3 LOC of preload code plus a flag entry. Upside: users get a one-line opt-in. Downside: bug-report surface widens by a tiny amount (people will forget they set the flag and report Nub-vs-Node differences). Defer to v0.x, not v0.1.

---

## Consensus pattern across the seven gaps

The seven gaps are not seven independent decisions. They are **one decision (`globalThis as EventTarget`) plus a few stalled satellite PRs**. Specifically:

- Gaps **4, 5, 6, 7** (`onerror`, `onunhandledrejection`, `onrejectionhandled`, `globalThis instanceof EventTarget`) are a **single architectural decision**, made in the negative, with explicit TSC sign-off and four separate issue closures by jasnell. The objection is principled and consistent across `mcollina`, `benjamingr`, `jasnell`, `addaleax`, with partial agreement from `joyeecheung`.
- Gaps **1, 2, 3** (`reportError`, `self`, `PromiseRejectionEvent` class) are **stalled-PR low-priority gaps** with no documented maintainer opposition. PR #41912 (`reportError`) was *approved* by jasnell in 2022 and rotted for 3.5 years before stale-bot closed it. Issue #28728 (`self`) was closed in 2019 on a thin "by design" rationale that has weakened with subsequent events. `PromiseRejectionEvent` has never even had a PR opened.

**Modal explanation:** **good architectural reason for the cluster that requires EventTarget**, **low priority for the standalone gaps**. The "Node hates web shape" reading is wrong — Node has shipped huge swathes of web shape (fetch, Streams, WebCrypto, AbortController, EventTarget the class itself, ErrorEvent, navigator, URLPattern, navigator.locks). The pattern is: **Node ships web-shape APIs when they can be cleanly bolted on, and resists when they require unwinding `process`-based mechanisms that the ecosystem depends on.**

**Driven by individuals or broad consensus?** The `EventTarget` resistance is **broad consensus** — TSC discussion, no single dissenter has shifted the position in 3+ years. The stalled PRs are **process drift, not opposition** — they could be revived with a fresh champion. The `reportError` PR specifically has Cloudflare implementing the spec (workerd#1979), Deno implementing it, the WinterTC spec ratifying it, and `getify` + `benlesh` (RxJS) explicitly asking for it; the only thing missing is someone to rebase the PR and shepherd it through Node CI.

**Is that instinct correct?** **Partly.** "Easy to support these" is right for 3 of 7 (reportError, self, PromiseRejectionEvent class) — those are stalled, not opposed. "Suggests there are good reasons" is right for 4 of 7 (the EventTarget cluster) — those have documented, principled, repeatedly-affirmed objections. The hypothesis-to-test question is well-posed but the answer is two different answers for two different sub-piles.

---

## Implication for Nub's posture

**Reaffirm and ship:**

- `reportError` — ✅ ship the polyfill. Matches the approved-but-rotted Node PR. Real ecosystem demand (RxJS, Cloudflare, Deno already ship it).
- `self = globalThis` — ✅ ship. Trivial alias, no objection in Node-land beyond a thin 2019 ruling, broad cross-runtime convention.
- `PromiseRejectionEvent` class — ✅ ship as an inert constructor following Node's own `ErrorEvent` (v25) precedent. Useful for code that constructs and dispatches the event on its own `EventTarget`s; harmless even if no one does.

**Reconsider and probably drop:**

- `onerror` / `onunhandledrejection` / `onrejectionhandled` — ⚠️ **drop from v0.1**. The proposed shim ("accessor properties that delegate to `process.on(...)`") is **semantically broken**: wrong callback signature, no `preventDefault`, additive-not-single-slot. The Node maintainers explicitly warned that libraries probing the handler property as a feature-detect would get the wrong answer, and shimming creates exactly that hazard. The honest options are:
  - **Drop them.** Document as known gaps that require `globalThis instanceof EventTarget`. Users who want the handler shape can use `process.on(...)` (works on Node and Nub) or opt into the prototype swap (see next).
  - **Ship them only when the user opts into the prototype swap.** If the user passes `--globalthis-eventtarget` / `NUB_GLOBALTHIS_EVENTTARGET=1`, then the handlers become real WebIDL handler attributes on the now-`EventTarget` `globalThis`, with correct single-slot semantics and `preventDefault` working. This makes the four EventTarget-dependent gaps a single bundle: ship all four together behind one flag, or none.

**Hold the line on:**

- `globalThis instanceof EventTarget` — ✅ keep the existing "don't ship even behind a flag in Phase 2" call from `wintertc-compliance-gaps.md`. The deep-dive above confirms: the technical operation is trivial (`Object.setPrototypeOf(globalThis, new EventTarget())`), but the brand-promise hazard is real, and the Node TSC has thought about this longer and more carefully than anyone else and landed on "no" with reasons we should respect.
- **Consider exposing the opt-in flag.** ~3 LOC of preload code. Bundles with the handler-property opt-in above. Defer to v0.x.

**Updated v0.1 compliance math:** with the recommended drops, Nub ships ~98% Min-Common-API (vs. ~95% on plain Node, vs. the ~99% the original `min-common-api-globals.md` targeted). The 1% delta is the four EventTarget-dependent items, deferred to a coherent opt-in bundle. **Better to ship 98% with correct semantics than 99% with three subtly-broken globals.**

**Update needed in `min-common-api-globals.md`:** §Summary, §Mechanism item 5, §Decisions — remove `onerror` / `onunhandledrejection` / `onrejectionhandled` from the v0.1 set, document the reasoning, link to this doc. The polyfill drops from ~50 LOC to ~30 LOC.

---

## Sources

- [nodejs/node#38947 — reportException(ex) / reportError(ex)](https://github.com/nodejs/node/issues/38947) (closed by stale-bot Sep 2022)
- [nodejs/node#41912 — process: add reportError](https://github.com/nodejs/node/pull/41912) (PR by benjamingr, approved by jasnell, closed by stale-bot Dec 2025)
- [nodejs/node#28728 — self is not defined inside web worker](https://github.com/nodejs/node/issues/28728) (closed by benjamingr Jul 2019)
- [denoland/deno#24637 — fix(ext/node): do not expose `self` global in node](https://github.com/denoland/deno/pull/24637) (Deno hid `self` in node-compat mode, Jul 2024)
- [nodejs/node#45981 — make global object an instance of `EventTarget`](https://github.com/nodejs/node/issues/45981) (opened Dec 2022)
- [nodejs/node#45993 — events,bootstrap: make globalThis extend EventTarget](https://github.com/nodejs/node/pull/45993) (PR by KhafraDev, still open since Dec 2022; TSC consensus against)
- [nodejs/node#51372 — Revisiting `globalThis` as an `EventTarget`](https://github.com/nodejs/node/issues/51372) (closed `not_planned` by jasnell Jun 2025)
- [nodejs/node#57352 — globalThis as an EventTarget](https://github.com/nodejs/node/issues/57352) (closed `not_planned` by jasnell Jun 2025)
- [nodejs/TSC#1489 — TSC meeting 2024-01-10](https://github.com/nodejs/TSC/issues/1489) (discussion of #51372)
- [nodejs/TSC#1323 — TSC meeting 2023-01-04 / #1326 2023-01-11](https://github.com/nodejs/TSC/issues/1323) (discussion of #45993; consensus: don't ship)
- [WinterTC55/proposal-minimum-common-api#41 — Consider adding globalThis.reportError()](https://github.com/WinterTC55/proposal-minimum-common-api/issues/41)
- [WinterTC55/proposal-minimum-common-api#82 — error/rejection events PR](https://github.com/wintercg/proposal-minimum-common-api/pull/82) (sets the `globalThis instanceof EventTarget` requirement)
- [cloudflare/workerd#1979 — Implements the web platform standard reportError API](https://github.com/cloudflare/workerd/pull/1979)
- [WHATWG HTML §runtime-script-errors](https://html.spec.whatwg.org/#runtime-script-errors) (spec for `reportError` and the global error handlers)
- [WHATWG HTML §event-handler-attributes](https://html.spec.whatwg.org/multipage/webappapis.html#event-handler-attributes) (spec for handler IDL attributes — the contract Nub's proposed shim doesn't satisfy)
- [WebIDL §dfn-create-operation-function](https://webidl.spec.whatwg.org/#dfn-create-operation-function) (the floating-`this` fallback to `globalThis`)
- [node-self on npm](https://www.npmjs.com/package/node-self) (the de-facto `self = globalThis` userland polyfill)
- `wiki/research/wintertc-compliance-gaps.md` (the broader survey this doc extends)
- `wiki/runtime/min-common-api-globals.md` (the runtime doc this research recommends amending)
- `wiki/architecture/augmenter-not-fork.md` (the additivity rule the handler shim fails)

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
