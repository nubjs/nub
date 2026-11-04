# Node.js `worker_threads.Worker` — design history vs. the web `Worker`

Node's worker API diverges from the web `Worker` on seven axes, and two documented causes cover all of them: `EventTarget` did not exist in Node in 2018, and everything else follows a stated Node-first principle.

**Question.** Node's `worker_threads.Worker` deliberately diverges from the WHATWG web `Worker` on many axes (EventEmitter vs EventTarget, raw message values vs `MessageEvent`, bare `Error` vs `ErrorEvent`, `Promise<exitCode>` terminate, `online`/`exit` events, `workerData`, `eval: true`). For each: was it litigated and justified, or an under-discussed default? Are these principled Node idioms Nub should respect, or accidents of history?

**Bottom line.** The deviations cluster into exactly two documented causes: (1) **one genuine accident of timing** — universal `EventEmitter` usage, because Node core had no `EventTarget` until ~2 years after `worker_threads` shipped; partially walked back for `MessagePort` but deliberately kept for the `Worker` handle. (2) **A consistent, stated principle for everything else** — *"match Node.js's abilities and requirements,"* inspired-by but not conformant-to the web. The error/raw-value surfaces are downstream consequences of (1); `terminate()`, `online`/`exit`, `workerData`, and `eval` are deliberate Node-idiom choices (mostly `child_process`/`cluster`-shaped), several of them *more* capable than the web API. Where the team later wanted web parity it **added** it additively (`data:` URLs, `messageerror`, `MessagePort extends EventTarget`) rather than reshaping the Node-native surface. **For Nub: every one of these is a justified, intentionally-held Node idiom today — support them additively on a global `Worker` with confidence. None is an embarrassing accident that Nub should "fix" by diverging from Node.**

---

## The decisive chronology — EventTarget did not exist at design time

This timeline is the single load-bearing fact for the event-model axis.

| Milestone | Date | Version |
|---|---|---|
| `worker_threads.Worker` ships (behind `--experimental-worker`) | 2018-06-20 | v10.5.0 |
| `EventTarget` exists *internally* — not user-constructable, not a global | 2020-06-30 | v14.5.0 |
| `EventTarget`/`Event`/`MessageEvent`/`MessagePort`/`MessageChannel` become **global + user-constructable**; `MessagePort` gets a real `MessageEvent` path | 2020-10-20 | v15.0.0 |
| `ErrorEvent` exposed as a Node global | 2025-08-18 (PR merge) | ~v24/25 (version not pinned) |

Sources: [v10.5.0 release](https://github.com/nodejs/node/releases/tag/v10.5.0); experimental `EventTarget` = [#33556](https://github.com/nodejs/node/pull/33556) in v14.5.0 ([CHANGELOG_V14](https://github.com/nodejs/node/blob/main/doc/changelogs/CHANGELOG_V14.md)), whose own docs state *"Neither the `EventTarget` nor `Event` classes are available for end user code to create"* ([v14.5.0 events docs](https://nodejs.org/download/release/v14.5.0/docs/api/events.html)); globals = [#35496](https://github.com/nodejs/node/pull/35496) in v15.0.0 ([CHANGELOG_V15](https://github.com/nodejs/node/blob/main/doc/changelogs/CHANGELOG_V15.md)), confirmed by doc-fix [#37059](https://github.com/nodejs/node/pull/37059); `ErrorEvent` global = [#58920](https://github.com/nodejs/node/pull/58920).

The gap between `worker_threads` and a usable global `EventTarget` is **~2 years 4 months**. Even the earliest *internal* EventTarget is ~2 years out. The authors in mid-2018 had no Node-native EventTarget to build on, and Node's own v10.5.0 docs said so explicitly: *"With the exception of `MessagePort`s being `EventEmitter`s rather than `EventTarget`s, this implementation matches browser `MessagePort`s."* ([v10.5.0 worker_threads docs](https://nodejs.org/download/release/v10.5.0/docs/api/worker_threads.html)).

---

## Provenance — who built it, where it was litigated

The tracking issue, the staging repos the design passed through, the landing PR and the flag removal, and the one pull request where web alignment was argued and rejected.

- **Tracking issue:** [#13143](https://github.com/nodejs/node/issues/13143) "Tracking issue: Worker support" (addaleax, 2017-05-21). A formal Enhancement Proposal in `nodejs/node-eps` was never written; design work happened in a staging repo (`nodejs/worker`) and in the `ayojs/ayo` experimental fork (PRs #98/#106/#110/#113/#114/#115/#117, Sep–Oct 2017) before upstreaming.
- **Landing PR:** [#20876](https://github.com/nodejs/node/pull/20876) "worker: initial implementation" (addaleax, opened 2018-05-22). Landed via `git node land` (so GitHub shows it `closed`, not `merged`), shipped in **v10.5.0** behind `--experimental-worker`. Flag removed in [#25361](https://github.com/nodejs/node/pull/25361) (Jan 2019), rationale: *"the Worker API has been essentially stable since its initial introduction, and no noticeable doubt about possibly not keeping the feature around has been voiced."*
- **The web-compat debate venue:** [#21414](https://github.com/nodejs/node/pull/21414) "worker: remove `workerData` option" (devsnek, 2018-06-19), opened explicitly *"to start out my crawl of aligning node's workers with the web"* — **closed unmerged**. This thread is the clearest design-time litigation of Node-idiom vs web-spec.

### Was web-compat a live goal? No — it was an explicit non-goal.

The maintainers are on record, repeatedly: the web Worker API was inspiration, and two-way compatibility was never the bar.

- **addaleax (implementor)**, [#43583](https://github.com/nodejs/node/issues/43583#issuecomment-1167661985): *"The goal was to implement an API that matches Node.js's abilities and requirements. You can probably support all or almost all of the Web API if you try hard enough, and the Node.js API is certainly inspired by the [Web Worker API]…"* — the canonical goal statement.
- **addaleax**, [#21414](https://github.com/nodejs/node/pull/21414#issuecomment-400811971): *"Node's idea of Workers is pretty different from Web Workers – having a full Node.js API available, rather than being sandboxed… The important question for me was whether we can implement Web Workers on top of the current API, in userland."* And: *"Node.js is already providing significant non-standard features for Workers… That makes asking for two-way compatibility pointless."*
- **Rich Trott**, [#43583](https://github.com/nodejs/node/issues/43583#issuecomment-1167402481): *"she modeled the API on Web Workers but it was not possible to support the entire API, so it diverged in places."*
- **jasnell**, [#43583](https://github.com/nodejs/node/issues/43583#issuecomment-1167781088): worker_threads are *"close enough"*; alignment PRs *"welcome"* but never the bar.
- **bnoordhuis**, [#13143](https://github.com/nodejs/node/issues/13143) (2017): *"I have nothing against the WebWorkers model per se, but: 1. it doesn't need to be implemented in node.js core, and 2. there will inevitably be scope creep when it is."*

The borrowing from the web was the **structured-clone transport** (postMessage wire format), per the [design FAQ gist](https://gist.github.com/benjamingr/3d5e86e2fb8ae4abe2ab98ffe4758665): *"we don't use JSON, but rather do the same thing that `postMessage()` does in browsers."* Node copied the wire format and kept its own event surface. That split is the whole story.

---

## Per-axis verdict table

Seven axes — event model, message delivery, error surface, `terminate()`, `online`/`exit`, `workerData`, and `eval` — each with whether it was litigated, the source for the rationale, and the verdict.

| Axis | Node behavior | Litigated? | Rationale (source) | Verdict |
|---|---|---|---|---|
| **Event model** | `Worker`/`MessagePort extends EventEmitter` (`.on`) | Yes — documented accident, partly corrected | EventTarget didn't exist in Node until 2020. addaleax: *"`MessagePort` instances are supposed to be `EventTarget`s… we just didn't have `EventTarget` when we introduced it"* ([#35835](https://github.com/nodejs/node/issues/35835#issuecomment-717592993)) | **Accident → now principled.** Original universal-EE was an accident of timing; current split (MessagePort=EventTarget, Worker handle=EventEmitter) is deliberately held |
| **Message delivery** | raw value to `.on('message')` listener | Indirect (falls out of event model) | EventEmitter listeners take positional args; only the web-style `port.onmessage` wraps in `MessageEvent.data` | **Principled idiom.** Bare value is the natural EE contract; a wrapper would be ceremony |
| **Error surface** | bare `Error` on `'error'` | No dedicated debate | EE consequence; `worker.on('error')` reserved strictly for uncaught exceptions ([#35835](https://github.com/nodejs/node/issues/35835#issuecomment-717786998)). `ErrorEvent` is an EventTarget/DOM construct Node had no reason to reconstruct | **Idiom-by-default.** Surfacing a real `Error` (with stack) is the Node shape; mild accident flavor (never explicitly weighed) but not regretted |
| **`terminate()`** | `Promise<exitCode>` (was a callback) | Yes — at a collaborator summit | Discussed at Berlin summit; found to be unintentionally synchronous; switched to Promise for async teardown ([#28021](https://github.com/nodejs/node/pull/28021), [summit#141](https://github.com/nodejs/summit/issues/141)). Web returns `undefined` (fire-and-forget) | **Principled idiom, deliberately *more* capable** |
| **`online`/`exit`** | both fired | Not separately; present from commit 1 | Mirrors `child_process`/`cluster` `'exit'`+exitCode; `'online'` is a Node-specific readiness signal the opaque web Worker has no moment for | **Principled idiom.** Node models a worker as a process-like supervised resource |
| **`workerData`** | constructor opt + child global | Present from design; survived removal attempt | [#21414](https://github.com/nodejs/node/pull/21414) tried to remove it for web alignment; closed unmerged on the "non-standard value, full compat pointless" argument. Structured-clones initial data, sparing the postMessage handshake | **Principled idiom — deliberate ergonomic superset** |
| **`eval: true`** | inline source string | Indirect; web parity added alongside | Node-native (`node -e`-style) affordance; `data:`-URL parity later **added** without removing `eval` ([#34584](https://github.com/nodejs/node/pull/34584), v14.9.0) | **Principled idiom + additive web parity** |

### Supporting evidence in Node's own source/docs

Code comments, changelog entries, and doc paragraphs in Node itself that pin each verdict above: the original `.onmessage` shim, the `terminate()` change, the `MessagePort` migration, and the `port.start()` note.

- Original `.onmessage` shim carried an explicit code comment: *"This is for compatibility with the Web's MessagePort API. It makes sense to provide it as an `EventEmitter` in Node.js, but if somebody overrides `onmessage`, we'll switch over to the Web API model."* ([worker.js @ landing commit, L60](https://github.com/nodejs/node/blob/b7c7c0c4961fd4e382b7cadcfb7c8360b8904a4a/lib/internal/worker.js#L60)).
- `terminate()` changelog: added v10.5.0 (callback, *"useless… as the Worker was actually terminated synchronously"*), returns a Promise since v12.5.0 ([#28021](https://github.com/nodejs/node/pull/28021)) — local `doc/api/worker_threads.md` §`worker.terminate()`.
- `MessagePort` migrated EventEmitter → EventTarget in v14.7.0 ([#34057](https://github.com/nodejs/node/pull/34057)) via a hybrid `NodeEventTarget` that keeps `.on('message')` working — local doc, Class `MessagePort` changes block. This caused a real regression ([#35835](https://github.com/nodejs/node/issues/35835): `parentPort.emit('error')` stopped working) — the clearest retrospective.
- The `Worker` handle still `extends EventEmitter` (local `doc/api/worker_threads.md` Class `Worker`); never migrated, because it has no web counterpart to be compatible *with* (the web exposes the worker only through the global `postMessage`/`onmessage`, not a handle object).
- `port.start()` doc: *"This method exists for parity with the Web `MessagePort` API. In Node.js, it is only useful for ignoring messages when no event listener is present. Node.js also diverges in its handling of `.onmessage`…"* — local doc, §`port.start()`.
- Incremental web-alignment steps that prove EventTarget was a known, deliberately-deferred future even in early 2019: [#26082](https://github.com/nodejs/node/pull/26082) (*"would make such extensions non-breaking changes if we desire them at some point"*), [#26487](https://github.com/nodejs/node/pull/26487).

---

## How divisive was the API?

Mildly, and along one fault line only: **how hard to chase web-Worker parity.**

The core team converged early (addaleax, bnoordhuis, jasnell, Trott all on record) that worker_threads should serve Node's needs first — full Node API surface, not a sandbox — and treat the web Worker as inspiration, not a conformance target. The one concerted push to align (devsnek's [#21414](https://github.com/nodejs/node/pull/21414)) was rejected with reasoned argument, not ignored. There was no factional fight; the API shipped stable enough to unflag in ~6 months. The only *retrospective regret* on record is narrow and specific: the universal `EventEmitter` choice, which addaleax explicitly attributes to EventTarget not existing yet ([#35835](https://github.com/nodejs/node/issues/35835#issuecomment-717592993)), and which Node has been incrementally correcting on the web-facing surfaces (`MessagePort`) while deliberately leaving the Node-native handle alone. bnoordhuis's *"long tail of lesser issues where node is subtly incompatible"* ([#43583](https://github.com/nodejs/node/issues/43583#issuecomment-1171618887)) is the honest summary: the divergences are known, mostly intentional, and parity is welcome-but-not-prioritized.

---

## Bottom line for Nub

Nub augments the user's Node and exposes a browser-style global `Worker` backed by Node worker threads. The read:

- **Respect the Node idioms additively — all of them.** `terminate(): Promise<exitCode>`, `online`/`exit`, `workerData`, `eval: true`, raw-value `.on('message')`, and bare-`Error` `.on('error')` are intentional, maintainer-defended Node conventions (mostly `child_process`-shaped). They are NOT accidents to "correct"; Nub should match Node byte-for-byte on them. A user writing `worker.on('exit', …)` / `await worker.terminate()` / `workerData` must get identical behavior under Nub.
- **The single real accident — universal EventEmitter — is one Node itself is already healing, additively, the right way.** `MessagePort` is now an `EventTarget` that *also* keeps `.on('message')`; the `Worker` handle stays EventEmitter on purpose. So the model to follow is exactly Nub's additivity rule: provide both surfaces, break neither. If Nub exposes a web-platform `Worker` global, it should layer the EventTarget/`MessageEvent`/`ErrorEvent` web shape *on top of* (not in place of) Node's EventEmitter behavior — which is precisely what current Node does on `MessagePort`.
- **Web-compat is a legitimate additive target, never a reason to diverge from Node.** The Node maintainers' own stance ("PRs that move closer to standard alignment welcome… incremental changes") is the same posture Nub holds: add the web shape where it composes cleanly, keep the Node idiom intact underneath. There is no axis here where Nub should prefer the web spec *over* Node behavior on the augmented runtime.

---

## The `port.start()` "wart" — verified precisely (2026-06-30)

Node auto-starts a `MessagePort` on every listener idiom, so the real divergence is Node-versus-browser rather than one between Node's own two APIs. Verified empirically on six Node versions.

A prior claim held that the `port.start()` docs "APOLOGIZE" for a "dual-contract WART" — specifically an **auto-start asymmetry** where `.on('message')` (EventEmitter) auto-starts the port but `addEventListener('message')` (EventTarget) does NOT, requiring `port.start()` or `onmessage`. Verified from primary sources and empirically; the claim is **partly right, partly wrong**.

**What the doc actually says (verbatim, local `doc/api/worker_threads.md` §`port.start()`, lines 1431-1439):**

> Starts receiving messages on this `MessagePort`. When using this port as an event emitter, this is called automatically once `'message'` listeners are attached.
>
> This method exists for parity with the Web `MessagePort` API. In Node.js, it is only useful for ignoring messages when no event listener is present. Node.js also diverges in its handling of `.onmessage`. Setting it automatically calls `.start()`, but unsetting it lets messages queue up until a new handler is set or the port is discarded.

- **"Apologize" is overstated.** The doc uses the neutral word **"diverges"** and explains the method is mostly vestigial in Node ("only useful for ignoring messages when no event listener is present"). It documents a deviation factually; there is no apologetic tone. Don't repeat "apologize."
- **The specific auto-start asymmetry claim is FALSE in Node.** Empirically tested across **v14.18.3, v16.13.2, v18.19.0, v20.19.0, v22.15.0, v26.2.0** (`MessageChannel`, attach a listener with each idiom, `postMessage`, observe delivery WITHOUT calling `start()`): **all four idioms — `.on('message')`, `addEventListener('message')`, `addEventListener('message')`+`start()`, and `onmessage=` — receive the message with no `start()` call, on every version.** Node's `addEventListener` auto-starts too. Root cause in source: `MessagePort` extends `NodeEventTarget`; `event_target.js` `addEventListener` calls `this[kNewListener]` (lines 677, 706), and `worker/io.js setupPortReferencing` overrides `kNewListener` to call `MessagePortPrototype.start(port)` on the first message listener (io.js:234-238). `NodeEventTarget.on()` is just `addEventListener` with a flag (event_target.js:996-999), so both paths hit the same auto-start.
- **The REAL divergence (the actual "wart") is Node-vs-browser, not within Node's two APIs.** In the **browser**, `addEventListener('message')` does NOT auto-start a `MessagePort` — you must call `port.start()` (or assign `onmessage`); messages silently queue/drop until then. Node deliberately diverges by auto-starting on **any** listener attach (`.on`, `addEventListener`, or `onmessage`). So Node is strictly *more forgiving* than the web — it removes the browser's silent-drop footgun. The second documented divergence: setting `.onmessage` calls `start()`, but **unsetting** it lets messages queue again (the web has no equivalent un-start).

**Timeline correction.** "EventTarget arriving in Node ~v15, 2.3 years after worker_threads" is roughly right but imprecise for *this* surface. `worker_threads` added **v10.5.0 (2018-06-20)**; `MessagePort` switched `EventEmitter → EventTarget` in **v14.7.0 (2020-07-29)** via [#34057](https://github.com/nodejs/node/pull/34057) — a ~2.1-year gap, not v15. (The global `EventTarget` class was exposed in v15.0.0; that's a different milestone than the MessagePort migration that produced this behavior.)

**Implication for Nub (concrete).**
- **This is NOT a Node wart to "fix" — Node already made the safe choice.** Auto-starting on any listener idiom is the better DX; Nub should preserve it, not "correct" it.
- **It is a MessagePort concern, not a Worker-handle concern.** The `start()`/auto-start mechanics live on `MessagePort` (from `MessageChannel` or transferred ports). The `Worker` handle (`extends EventEmitter`, no web counterpart) has no `start()`, so this does not bear on the global-`Worker` constructor. It matters only for `MessagePort`/`MessageChannel`, which Nub leaves as Node's own globals.
- **Backing the global `Worker` with `node:worker_threads` is what makes the auto-start behavior correct for free.** A hand-rolled, spec-pure `MessagePort` would face one decision — auto-start versus browser-style `start()`-required — and Node's forgiving auto-start is the side to take, so the browser's silent-drop footgun is not reintroduced. That is the same additivity stance the rest of this doc lands on: layer the web shape on top of Node's behavior, break neither.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-06-30 — Added "`port.start()` wart — verified precisely" section, correcting a prior claim. The claimed within-Node auto-start asymmetry (`.on` starts, `addEventListener` doesn't) is FALSE — empirically, all listener idioms auto-start on v14.18.3→v26.2.0; the real divergence is Node-vs-browser (Node auto-starts where the browser requires `start()`). The doc "diverges" (no apology). MessagePort→EventTarget was v14.7.0 (#34057), ~2.1y after worker_threads v10.5.0. Concern is MessagePort-only, not the Worker handle.
- 2026-06-30 — Initial write-up.
- 2026-08-28 — Updated the Nub sections to the shipped `Worker` global, and trimmed the changelog wording.
