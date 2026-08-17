---
**Scope:** Survey Node.js's `--experimental-*` flag surface across Node 22 LTS, 24 LTS, and 25/26 current, and identify which flags Nub should consider auto-injecting in default mode that are not already covered by an existing per-feature plan. Per Nub's mechanism rule (augmenter, not fork), the only knob in scope is "Nub prepends a flag to the spawned `node` invocation" — no source patches, no library substitutions.

**Status:** v1, 2026-05-18. A point-in-time survey of the flag surface as it stood across Node 22, 24 and 25/26. The candidate list is at the bottom under "Recommended new plan docs."

**Already settled elsewhere, not re-litigated here:** the unflag decisions for Temporal, URLPattern, WebSocket, connect-sockets, HTMLRewriter, the WinterTC runtime key, the minimum-common-API globals, the Cloudflare APIs, and SQLite. The mechanism this research feeds is Nub's flag-injection table; the additivity policy gates everything; the augmented-mode floor is Node 22.15.
---

# Experimental flag unflagging survey

Which of Node's `--experimental-*` flags Nub should prepend to the spawned `node` invocation in default mode, and which stay for the user to pass.

## TL;DR

Four flags earn injection — `vm-modules`, `eventsource`, `import-meta-resolve` and `require-module`. The rest are already default-on, removed from Node, or break additivity.

### Unflag in default mode (recommended shortlist)

These are the flags Nub should add to its flag-injection table beyond what's already there, ordered roughly by confidence.

1. **`--experimental-require-module`** — On Node 22.0–22.11 only. Stable / unflagged since 22.12. Zero risk, high value: CJS importing ESM is one of the biggest sources of ecosystem pain. Warrants its own plan doc.
2. **`--experimental-vm-modules`** — On all supported versions. No behavior change for code that doesn't use `vm.SourceTextModule`; strict feature-add. Big quality-of-life win for Jest, Vitest ESM, and module-aware tooling generally. Warrants its own plan doc.
3. **`--experimental-eventsource`** — On Node versions where it's still flagged (22.0–22.x prior to default-on; needs verification in implementation). Strict additive global. Warrants its own plan doc.
4. **`--experimental-import-meta-resolve`** — On Node 22.x. Partial stabilization in 20.6; the full sync version stabilized later. Code that uses it is already opt-in, so the risk surface is limited to feature-detection edge cases. Warrants its own plan doc.
5. **`--experimental-detect-module`** — Already on by default in Node 22.7+. **No action needed for the supported range** (22.15 floor), but worth a one-paragraph note in the flag-injection table so future agents don't re-investigate. No plan doc needed.

### Leave flagged (user opts in)

These are real features but unflagging-by-default would either violate additivity, change global state in feature-detect-visible ways, or carry too much risk.

- **`--experimental-strip-types`** / **`--experimental-transform-types`** — Nub has its own transpiler, and the flag-injection plan already covers this. `--experimental-transform-types` was **removed in Node 26** per the [v26.0.0 release notes](https://nodejs.org/en/blog/release/v26.0.0). Decision stands: do not inject.
- **`--experimental-permission`** / **`--permission`** — Pseudo-stable in 24+ (renamed from experimental-permission to just `--permission`). This is an opt-in security model that **explicitly breaks** code by design. Cannot inject by default. Possibly worth exposing through a Nub CLI flag like `nub --permission` that passes through.
- **`--experimental-shadow-realm`** — Adds `ShadowRealm` global; feature-detect-visible; TC39 still Stage 3. Niche audience. Leave to user.
- **`--experimental-sea-config`** — Single Executable Applications. Build-time only, not a "runtime feature." Out of scope for default injection.
- **`--experimental-test-coverage`**, **`--experimental-test-snapshots`**, **`--experimental-test-module-mocks`** — These only fire when the user runs `node --test`. Default-injecting them is harmless for non-test runs but signals support of the node:test runner in a way Nub has not committed to. Document but don't inject.
- **`--experimental-wasm-modules`** — Adds ability to `import './foo.wasm'`. Already on by default in newer Node (22.x+ unflagged for the import-attribute syntax). Verify per-version; probably no action in supported range.
- **`--experimental-async-context-frame`** — A faster AsyncLocalStorage implementation. The public API (`AsyncLocalStorage`) is Stability 2; the flag selects an implementation. Default in 23+. Worth a follow-up read but probably already-on in our supported range.
- **`--experimental-network-imports`** — **Removed** in Node 22.6+. No action.
- **`--experimental-quic`** — Node 25+, Stability 1.1. New surface; adds globals (potentially). Leave flagged; revisit when status changes.
- **`--experimental-addon-modules`** — Node 23.6+, Stability 1.0. Lets `.node` addons be imported via ES modules. Niche, no rush.
- **`--experimental-print-required-tla`** — Diagnostic flag, emits warnings, not a feature. Don't inject.
- **`--experimental-default-config-file`** / **`--experimental-config-file`** — Node config file feature. Stability 1.0. Adds `node.config.json` resolution behavior; if Nub unflagged this by default, user code that didn't expect a `node.config.json` to be honored would see surprising behavior. Skip.
- **`--experimental-ffi`** (Node 26+) — `node:ffi` module. Requires FFI-enabled build; not present on most users' Node. Skip.
- **`--experimental-stream-iter`** (Node 25.9+) — `node:stream/iter`. Strictly additive (new module). Possible future candidate, but too new to make a default call on as of 2026-05-18.

## Methodology

Six questions per flag:

1. **What it does.** One sentence.
2. **Stability index.** Where Node ranks it on the [stability ladder](https://nodejs.org/api/documentation.html#stability-index) (0 deprecated, 1.0 early development, 1.1 active development, 1.2 release candidate, 2 stable, 3 legacy). Most experimental features are still 1.x; the interesting filter is 1.1 vs 1.2.
3. **Breakage risk if turned on by default** — the additivity check. Two failure modes: a **direct semantic change** to code that doesn't use the feature (almost always disqualifying), or an **indirect change** via globals appearing where they used to be `undefined`.
4. **Compatibility risk via feature detection.** If user code does `if (typeof globalThis.X === 'undefined') { /* polyfill */ }` and Nub makes `X` defined, the polyfill branch goes cold. Usually fine, since the polyfill is functionally equivalent, but sometimes not: older polyfills have wider surface, the native implementation may have a different prototype chain, and iframe-like `instanceof` checks can fail. Tracked separately because it is the most common reason a "strictly additive" flag has hidden teeth.
5. **Real-world signal.** Is the feature widely used in production despite the flag (Jest's reliance on `--experimental-vm-modules` is the canonical example)? Is the Node team signaling imminent unflagging?
6. **Recommendation for Nub.**

The constraint specific to Nub: even if Node's own posture is "we'll unflag this in N.x soon," that does not change whether **today's user on Node 22.15** benefits from Nub injecting it. The point of flag injection is to give users the experience of a newer Node on whatever Node they have installed.

### Caveats and uncertainty

Sources drawn on:
- [`https://nodejs.org/api/cli.html`](https://nodejs.org/api/cli.html) via WebFetch on 2026-05-18.
- [`https://nodejs.org/en/blog/release/v26.0.0`](https://nodejs.org/en/blog/release/v26.0.0) for the Node 26 changeover.
- Search results for individual flag histories ([`require-esm`](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/), [strip-types](https://github.com/nodejs/typescript/issues/24), [permission model](https://github.com/nodejs/node/pull/56201), [network-imports removal](https://github.com/orgs/nodejs/discussions/54948)).

Uncertainty about a specific Node version (e.g. "did eventsource get unflagged in 22.x or 23.x?") is flagged in the per-flag section. Populating the injection table requires verifying each against the per-version Node release notes; this survey does not carry that level of version-specific certainty.

V8-level flags (`--harmony-*`, `--turboshaft`, etc.) were **not** investigated in depth. A short section at the end collects what was noticed in passing.

## Per-flag analysis

Each flag scored on the six questions above, ending in an inject or don't-inject recommendation.

### `--experimental-require-module`

Stable since Node 22.12, so injection only reaches 22.0–22.11 — already below Nub's augmented-mode floor.

- **What it does.** Allows `require()` to load ES modules synchronously, provided the ESM has no top-level await.
- **Stability.** Stable as of Node 22.12 LTS / 23.0; the **lack** of this flag (`--no-require-module`) is now the user-facing escape hatch. Per [Joyee Cheung's writeup, Dec 2025](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/), the feature is unflagged across all supported LTS lines and marked Stability 2.
- **Breakage risk if on by default.** Approximately zero for code that doesn't `require()` an ESM. A `require()` of an ESM with TLA throws regardless of the flag; the flag only gates whether the call is attempted. No change to existing CJS-of-CJS or ESM-of-anything semantics.
- **Compatibility risk.** Minimal. Some libraries do `try { require(esmPkg) } catch { /* fall back */ }`; with the flag on the try succeeds where it used to fail, which is what the user wants — the fallback path is usually slower or feature-limited.
- **Real-world signal.** One of the most-requested features in Node history, and on the experimental-feature re-evaluation thread ([nodejs/next-10#285](https://github.com/nodejs/next-10/issues/285)). Unflagging it on Node 22.0–22.11 gives those users a forward-looking experience without upgrading.
- **Recommendation.** **Inject on Node 22.0–22.11**; no injection needed on 22.12+, where it is already default. Nub's hard floor for augmented mode is Node 22.15.0, so **22.0–22.11 is already compat-mode-only territory** and the injection is a no-op for augmented mode. The entry still earns its place: it is the documentation, and it is already there if the floor ever lowers. Warrants a one-pager plan doc capturing the version-range logic.

### `--experimental-vm-modules`

Experimental since Node 9.6 with no stabilization track, invisible to code that never constructs a module object, and required by Jest's ESM mode. Inject everywhere.

- **What it does.** Exposes the `vm.SourceTextModule` and `vm.SyntheticModule` constructors in `node:vm`. Without the flag, those classes throw on construction.
- **Stability.** Stability 1 — Experimental, [open since v9.6.0](https://nodejs.org/api/cli.html). No clear signal of imminent stabilization; the [nodejs/next-10 re-evaluation issue](https://github.com/nodejs/next-10/issues/285) flagged it as one of the long-running experiments, and as of 2026 no PR found is on the stabilize track.
- **Breakage risk if on by default.** Zero. The flag is invisible to code that never constructs a `vm.SourceTextModule`; no existing semantics change.
- **Compatibility risk.** Some test runners (Jest in particular, per [jestjs/jest#14156](https://github.com/jestjs/jest/issues/14156)) do `try { new vm.SourceTextModule(...) } catch { /* fallback */ }` for environment detection, so making the constructor available means the fallback branch is never taken — almost always what the user wants. The deeper risk is **subtle Node regressions in `vm.SourceTextModule` itself** (e.g. the [Node 15.9 regression](https://github.com/nodejs/node/issues/37426) that broke Jest ESM); those were specific to historical Node versions and are not active on the supported 22.15+ range.
- **Real-world signal.** Heavy. Jest's ESM mode requires it, as do ts-jest, Vitest in some modes, and much other tooling. JetBrains has an [open feature request](https://youtrack.jetbrains.com/issue/WEB-52967/) asking WebStorm to inject it automatically — essentially the same value prop.
- **Recommendation.** **Inject on all supported versions** — high value, low risk. Warrants a plan doc, which should note that Jest ESM users run `node --experimental-vm-modules node_modules/jest/bin/jest.js` today, where `nub ./node_modules/.bin/jest` would work directly. That is a tangible value prop for marketing copy.

### `--experimental-eventsource`

A spec-shaped global whose flip to default-on is unconfirmed; inject where it is still flagged, carrying the usual polyfill-detection caveat.

- **What it does.** Adds `EventSource` to the global scope, matching the [WHATWG Server-Sent Events spec](https://html.spec.whatwg.org/multipage/server-sent-events.html).
- **Stability.** Stability 1 — Experimental, as of the CLI doc snapshot. Search results suggest it was enabled by default in some 22.x point release and kept its `--experimental-*` name. **The exact version where it flipped to default-on is unconfirmed**; populating the flag-injection table requires verifying it. If it is still flagged on Node 22.15 (the floor), inject it; if already default, no-op.

- **Breakage risk if on by default.** Adds a new global, so the risk is the usual feature-detection one:

  ```js
  // User code that runs on browsers, Node-with-polyfill, etc.
  if (typeof EventSource === 'undefined') {
    globalThis.EventSource = require('eventsource').EventSource;
  }
  ```

  With Nub injecting the flag, `EventSource` is defined and Node's native takes over. The [`eventsource` npm package](https://github.com/EventSource/eventsource) and the native implementation both follow the spec, so they should be substitutable. **Risk: low but non-zero**, the same as every WinterTC-style global polyfill addition and the same shape as the WebSocket, URLPattern and Temporal additions already planned. If Nub's policy is "we add web globals where Node has them behind a flag," EventSource is in scope of it.
- **Real-world signal.** Moderate. Server-Sent Events is a real production pattern (LLM streaming, live updates). The native `EventSource` matters specifically for the **client** side — code that consumes an SSE stream from another service. Less common in pure-server Node, but increasingly common with AI tooling that proxies SSE.
- **Recommendation.** **Inject on versions where it's flagged.** Verify Node 22.15 status during implementation; if flagged, inject, if already default, no-op. Warrants a plan doc in the same family as the WebSocket and minimum-common-API-globals plans — a Cloudflare/Workers-shape web global Nub makes ambient.

### `--experimental-import-meta-resolve`

Partially stabilized in Node 20.6: the parent-URL form is still gated, and enabling it changes nothing for code that never calls it.

- **What it does.** Enables `import.meta.resolve(specifier, parent)` — resolving a module specifier the same way the loader would, synchronously. **Partial stabilization** happened in Node 20.6: the no-parent-argument form is stable, the two-argument form is still flagged.
- **Stability.** Mixed. The sync, no-parent form is stable; the parent-URL form is still 1.0/1.1.
- **Breakage risk if on by default.** Zero for code that doesn't call `import.meta.resolve`. For code that does, the flag enables the second-argument form, which throws without it. Same shape as `require-module`: flag-on enables more, no existing semantics change.
- **Compatibility risk.** Similar to `require-module`: try/catch detection could see new success where it used to see failure. Usually desired.
- **Real-world signal.** Used by some bundlers and test runners that need synchronous resolution, since the async version requires `module.register()` plumbing. Less load-bearing than `vm-modules` or `require-module`, but real.
- **Recommendation.** **Inject on supported versions where it's still flagged.** Whether 22.15 has it default or still gated is unconfirmed; implementation should verify. Warrants the smallest plan doc of the four, since the feature is narrow.

### `--experimental-detect-module`

Default-on since Node 22.7, so there is nothing to inject anywhere in the supported range.

- **What it does.** When a file has no `package.json` `"type"` field, Node tries CommonJS first and retries as ESM if CJS parsing fails.
- **Stability.** Enabled by default in Node 22.7+ per the [v22.7.0 release notes](https://nodejs.org/en/blog/release/v22.7.0) trail. Still nominally Stability 1, but on by default.
- **Breakage risk.** N/A — already on by default in the supported range.
- **Recommendation.** **No action.** The flag-injection plan already notes this; a one-line "verified: still default-on through Node 26" could be added when implementation begins, but there is nothing to inject.

### `--experimental-strip-types` / `--experimental-transform-types`

Nub's load hook claims `.ts` files before Node's stripper sees them, and Node removed `transform-types` in 26. Neither flag is injected.

- **What it does.** Strips TypeScript types and runs the resulting JS; with `transform-types`, also handles enums/namespaces/decorators via SWC.
- **Stability.** `strip-types` is **default-on as of Node 23.6** per the [v23.6.0 release notes](https://nodejs.org/en/blog/release/v23.6.0), warnings removed in 24.3, **stable in 24.12** (Stability 2). `transform-types` was **removed in Node 26** per the [v26.0.0 release notes](https://nodejs.org/en/blog/release/v26.0.0) (PR #61803); Node's official direction is strip-types only, with a real transpiler for non-erasable syntax.
- **Recommendation.** **Do not inject** — the existing plan covers this in detail. Nub owns `.ts` files via its registerHooks load hook, so Node's strip-types never sees them. Nothing new from this research: the decision holds, and Node's removal of `transform-types` in 26 matches the Nub direction of shipping a real transpiler that handles enums. No plan doc change.

### `--experimental-permission` / `--permission`

The one flag whose purpose is to break code, so it can never be injected. A pass-through would be a separate feature decision.

- **What it does.** Restricts file system, network, child_process, worker_threads, native addons, WASI, and inspector access at runtime. It moved to Stability 1.1 in 2023 per [PR #50068](https://github.com/nodejs/node/pull/50068), and matured through the stabilization [PR #56201](https://github.com/nodejs/node/pull/56201). As of 2026 the flag is [renamed to `--permission`](https://openjsf.org/blog/nodejs-24-released) in Node 24.
- **Stability.** 1.2 → 2 in the 24+ timeframe.
- **Breakage risk if on by default.** **Catastrophic.** The permission model exists to break code that does unsafe things. Enabling it unasked would make a large fraction of npm packages fail to load — native addons require `--allow-addons`, FS access requires `--allow-fs-*`, and so on. This is the **antithesis** of additivity.
- **Compatibility risk.** N/A — this is intentionally breaking.
- **Real-world signal.** Used by security-conscious users; mostly avoided by general developers because of how invasive it is.
- **Recommendation.** **Do not inject by default. Ever.** Possibly worth a Nub CLI pass-through: `nub --permission script.ts` forwards `--permission` to Node, with Nub-specific allowlist expansion to handle Nub's own preloads. That is a feature question rather than a flag-injection question, and out of scope here — it would warrant its own plan doc only if it becomes a priority.

### `--experimental-shadow-realm`

Stage 3 at TC39 with near-zero production use and a known memory leak in Node's implementation. Left to the user.

- **What it does.** Enables the [ShadowRealm](https://github.com/tc39/proposal-shadowrealm) global, a TC39 Stage 3 proposal for sandboxed JS evaluation.
- **Stability.** Stability 1 — Experimental. The [Node implementation](https://github.com/nodejs/node/commit/e86a638305) shipped in 2022 but has known [memory leak issues](https://github.com/nodejs/node/issues/47353). TC39 status is Stage 3: no more spec changes expected, but no movement to Stage 4.
- **Breakage risk if on by default.** Low at the language level, since user code rarely depends on `ShadowRealm` being undefined. High at the implementation level — the memory leak is a real bug.
- **Compatibility risk.** Some libraries may feature-detect `ShadowRealm` and switch implementation paths. Not a big surface today.
- **Real-world signal.** Almost zero. Cloudflare uses something similar internally, but the public-API audience is still small.
- **Recommendation.** **Leave flagged.** Niche feature with known implementation issues; users who want it can pass the flag themselves. No plan doc needed.

### `--experimental-sea-config`

Build tooling rather than runtime behavior, so flag injection does not apply to it at all.

- **What it does.** Configures [Single Executable Applications](https://nodejs.org/api/single-executable-applications.html) — bundling a Node app as a single binary.
- **Stability.** Stability 1 — Experimental.
- **Breakage risk if on by default.** N/A: `node --experimental-sea-config sea.json` is a build-time invocation that generates a blob and does not change runtime behavior of unrelated code.
- **Recommendation.** **Out of scope.** SEA is build tooling, not runtime behavior. A future `nub build --executable` subcommand could use it internally; it is not a flag-injection concern.

### `--experimental-test-coverage` / `--experimental-test-snapshots` / `--experimental-test-module-mocks`

All three are inert outside `node --test`, and Nub ships no test subcommand to inject them from.

- **What they do.** Extend Node's built-in test runner (`node:test`) with coverage reporting, snapshot testing, and module mocking respectively.
- **Stability.** All Stability 1. The base `node:test` runner graduated to Stability 2 in Node 20.
- **Breakage risk if on by default.** Effectively zero — these flags only do anything under `node --test`, and are inert otherwise.
- **Compatibility risk.** Code wrapping test APIs in `try { ... }` could see different behavior, a degenerate case.
- **Real-world signal.** node:test usage is growing but still overshadowed by Jest/Vitest. Coverage is the most-used of the three.
- **Recommendation.** **Don't inject by default.** Nub ships no subcommand that shells out to `node:test`, so there is no dispatch point for injecting them. On the generic `nub script.ts` path they are noise and carry a slight startup cost — test-coverage in particular enables V8 coverage, which is not free.

### `--experimental-wasm-modules`

A strict feature-add whose default status on Node 22.15 is unconfirmed; verification decides whether anything gets injected.

- **What it does.** Allows `import wasmModule from './foo.wasm'` ES module syntax for WebAssembly modules.
- **Stability.** Stability 1, with ongoing work. The import-attributes-syntax variant may have been unflagged, but the current 22.15 status is **unconfirmed**; implementation should verify.
- **Breakage risk if on by default.** Zero unless user code imports a `.wasm` file. Strict feature-add.
- **Compatibility risk.** Low. The WASM-module spec is still evolving, so code using an early form could break if TC39 changes it — Node's problem to manage, not Nub's.
- **Real-world signal.** Moderate and growing. WASM in npm packages (esbuild-wasm, wasm crypto libs) is increasingly common.
- **Recommendation.** **Probably inject on Node 22.15 if still flagged** — "probably" because the current default-status needs verifying and the import-attributes syntax has been in flux. Plan doc only if verification shows it is still flagged on the supported range; otherwise a note in the flag-injection table suffices.

### `--experimental-async-context-frame`

Selects a faster `AsyncLocalStorage` implementation behind an unchanged public API, and is already the default on Node 23+.

- **What it does.** Switches `AsyncLocalStorage`'s backing implementation from the older async_hooks-based mechanism to a faster V8 AsyncContextFrame integration.
- **Stability.** The public API (`AsyncLocalStorage`) is Stability 2; the flag selects the implementation. The frame implementation is the default on Node 23+, with `--no-experimental-async-context-frame` to disable.
- **Breakage risk if on by default.** Should be zero — same public API contract. In practice Electron and some users explicitly opt out, because their context flow differs from what AsyncContextFrame expects.
- **Compatibility risk.** Edge cases around continuation semantics in unusual control flow (generators, native code calling JS callbacks across realms).
- **Recommendation.** **No injection needed in supported range** — already default-on for Node 23+. Document that Nub leaves it at the default and does not disable it. No plan doc.

### `--experimental-network-imports`

**Status:** Removed in Node 22.6+ per [nodejs#54948](https://github.com/orgs/nodejs/discussions/54948). No action needed. Listed only so future agents don't re-investigate.

### `--experimental-quic`

Node 25+ and Stability 1.1 — too new a surface to commit to, and revisited when Node 26 goes LTS.

- **What it does.** Enables `node:quic` for QUIC protocol support.
- **Stability.** Stability 1.1, Node 25+.
- **Breakage risk if on by default.** Low for code that doesn't import `node:quic`, though socket-creating code patterns may hold surface area nobody has thought through.
- **Recommendation.** **Leave flagged for now.** Node 25 isn't LTS and the surface is too new to commit to. Revisit when Node 26 LTS ships in October 2026 and QUIC's status is clearer.

### `--experimental-addon-modules`

A strict feature-add for importing `.node` files; worth injecting where it is still flagged, at low priority.

- **What it does.** Allows `.node` files to be loaded via `import` rather than only via `require()`.
- **Stability.** Stability 1.0, Node 23.6+.
- **Breakage risk if on by default.** Strict feature-add: without the flag `import './foo.node'` fails, with it it works. No existing code path changes.
- **Compatibility risk.** Low. Some loaders and resolvers have hand-rolled `.node` handling, which becomes redundant.
- **Real-world signal.** Niche but growing as more native addons ship ESM-first wrappers.
- **Recommendation.** **Worth injecting on supported versions where flagged, but low priority.** Could fold into the same plan doc as `vm-modules` and `import-meta-resolve`, or stand alone as a tiny doc.

### `--experimental-print-required-tla`

A diagnostic that prints on failure rather than a runtime feature, and it works against the `--no-warnings` Nub already injects.

- **What it does.** Prints the location of top-level await in ES modules that fail to be required via `require()`.
- **Stability.** Stability 1. Diagnostic flag.
- **Recommendation.** **Don't inject.** A debugging aid that prints info on failure, not a runtime feature — and it would conflict with the `--no-warnings` Nub injects.

### `--experimental-default-config-file` / `--experimental-config-file`

Honoring `node.config.json` would let a stale file silently change behavior, which breaks the no-surprises rule for default mode.

- **What they do.** Load configuration from a JSON file (`node.config.json`) that maps to CLI flags.
- **Stability.** Stability 1.0, Node 23.10+.
- **Breakage risk if on by default.** **Real.** With the flag on, Node looks for `node.config.json` in cwd and honors options like `"nodeOptions"`, so a stale config file left over from experimenting would silently apply — a behavior change to code that didn't ask for it.
- **Recommendation.** **Skip.** Violates the no-surprises rule for default mode; users who want config-file support can opt in.

### `--experimental-ffi` (Node 26+)

Node 26.1+ only, and gated on an FFI-enabled build most distributions do not ship.

- **What it does.** Enables `node:ffi` for foreign-function-interface calls, similar to Deno's FFI.
- **Stability.** Stability 1, Node 26.1+. Requires an FFI-enabled build, not present on most distributions yet.
- **Recommendation.** **Skip.** Too new, build-flag-gated, niche. Revisit in 2027.

### `--experimental-stream-iter` (Node 25.9+)

A new submodule and strictly additive, but too recent for a default-on call as of this survey.

- **What it does.** Enables `node:stream/iter`, a new submodule with iterator-based stream utilities.
- **Stability.** Stability 1, very new.
- **Recommendation.** **Skip.** Too new for a default-on call. Reconsider once it has been in two LTS lines.

### `--experimental-global-navigator` / `--no-experimental-global-navigator`

Default-on since Node 22, so nothing is injected; the record belongs in the minimum-common-API globals plan.

- **What it does.** Exposes `globalThis.navigator` (with `hardwareConcurrency`, `userAgent`, `language`, `platform`). Default-on as of Node 22+.
- **Recommendation.** **No action — already default.** Worth noting in the minimum-common-API-globals plan, since `navigator` is part of WinterTC's minimum common API, but Node already provides it.

### `--experimental-webstorage` / `--no-experimental-webstorage`

Two globals with disk-persistent semantics — the survey's strongest candidate for a recorded policy decision rather than an injection.

- **What it does.** Exposes `localStorage` and `sessionStorage` globals backed by file-system storage. Default-on as of Node 25.
- **Stability.** Stability 1.2 (release candidate).
- **Breakage risk if on by default *on older Node*.** Real — it adds two globals. Code that feature-detects `typeof localStorage === 'undefined'` and concludes "we're in Node, no localStorage" loses a signal that was valid for a long time once Nub makes it ambient on Node 22.x.
- **Compatibility risk.** The bigger issue is **state semantics**: `localStorage` persists to disk. Injecting this flag on Node 22.15 means any code that accidentally writes to `localStorage` — browser-targeted code running on the server, say — starts persisting to disk. An unusual failure mode, but real.
- **Real-world signal.** Mixed. Cloudflare Workers doesn't ship localStorage; Deno does, Bun does, and Node 25 ships it.
- **Recommendation.** **Defer** — not a clean inject-and-forget call, and the strongest "leave flagged but record the decision" candidate in this survey. It wants a dedicated webstorage plan doc covering the storage-path question (where does the file live, and is it per project?) and answering whether we want the feature at all before unflagging it. **That doc may decide NOT to inject**; this is a policy question, not a mechanical one.

## Cross-cutting concerns

Five issues that cut across every injected flag: feature detection, warning suppression, `process.features`, flag precedence, and version-table maintenance.

### Feature detection via `typeof globalThis.X === 'undefined'`

Several of the flag-on candidates add globals (`EventSource`, `ShadowRealm`, `localStorage`, `navigator`, etc.). Every one of those carries the feature-detection trap:

```js
if (typeof URLPattern !== 'undefined') {
  // use native
} else {
  // load polyfill, which may have wider/narrower surface
}
```

Polyfill libraries are a tricky surface:
- Some polyfills are a strict superset of the native (e.g. wider Unicode support), so making the native appear loses the superset. Edge case but real.
- Some polyfills check `instanceof` against their own constructor; if user code calls a Nub-native API that returns a native instance, that check fails.

Nub already accepted this trade-off for `URLPattern`, `Temporal` and `WebSocket`. New globals added via flag injection — mainly EventSource — carry the same risk profile, to be documented in each plan doc.

### Interaction with `--no-warnings`

Nub injects `--no-warnings`, which already suppresses the "ExperimentalWarning: X is an experimental feature" emitted the first time an experimental feature is used. Two effects:

1. **Good**: the user doesn't see noisy warnings about features Nub intentionally enabled.
2. **Caveat**: the user also doesn't see warnings about features **they** enabled. A Nub user who passes `--experimental-quic` themselves has that warning swallowed too, which may surprise them.

The flag-injection plan's Open Questions section already flags this. More flag injection makes the implicit warning-suppression more impactful.

### The `process.features` surface

Node exposes some of these via `process.features`, so code introspecting `process.features.{quic,sqlite,...}` sees different values depending on what Nub injects.

For example, `process.features.typescript` indicates strip-types status. Same risk as `globalThis.X` feature detection in a different shape, with the same mitigation: document per plan doc.

### Flag conflicts and ordering

Node generally takes last-wins for conflicting CLI flags. Nub's injection prepends, so user-passed flags win. The relevant edge cases:

- `NODE_OPTIONS` is parsed before argv. If the user has `NODE_OPTIONS=--no-experimental-vm-modules` and Nub injects `--experimental-vm-modules`, Nub's wins by being later in the effective command line. That is probably **wrong** — the user explicitly opted out and Nub overrode them.
- Needs explicit handling: read `NODE_OPTIONS`, detect any `--no-experimental-*` opt-outs, and either skip the corresponding inject or document the precedence clearly.

An implementation-level concern, and a **new open question** for the flag-injection plan.

### Version-table maintenance

The flag table needs per-version entries, and maintenance cost grows with the number of flags injected. The pattern:

```
22.15-22.x: --experimental-vm-modules --experimental-eventsource [--experimental-sqlite]
23.x:       --experimental-vm-modules
24.x:       --experimental-vm-modules  (eventsource and sqlite both unflagged)
25.x:       --experimental-vm-modules
26.x:       --experimental-vm-modules  (until Node stabilizes vm-modules)
```

The `vm-modules` flag stays injected across the full range until Node officially stabilizes it; `eventsource` and `sqlite` drop off once Node makes them default. Implementation should treat the table as a **set per version**, not a flat list — easier to reason about.

## V8-level flags (cursory)

Out of focus per the brief, but noted:

- **`--harmony-*`** — Gate proposals not yet at TC39 Stage 4. None currently look like a sensible default-on call. Skip.
- **`--turboshaft`** / Turbofan tuning — V8 compiler-pipeline flags. In theory they could tune for cold-start vs steady-state, but the cold-start research ([[research/cold-start]]) showed the ROI is marginal and the breakage surface (rare V8 bugs) is real. Skip.
- **`--max-old-space-size`** — Memory ceiling, already in the flag-injection consideration set as a tuned-defaults candidate. Not an experimental flag; deferred to a separate Nub heap-defaults plan doc if prioritized.
- **`--expose-gc`** — Adds `globalThis.gc`. Tempting for some benchmarking code, but it adds a global; skip by default.

No V8 flag belongs in the experimental-flags-unflagging analysis.

## Recommended new plan docs

Per-feature plan docs to create as follow-ups:

1. **`--experimental-vm-modules` unflag** — Inject on all supported versions. High priority, the biggest value-prop unlock. Same pattern as the SQLite unflag: a tiny doc adding one row to the flag table.

2. **`--experimental-eventsource` unflag** — Inject on versions where it's still flagged. Implementation must verify Node 22.15 status before deciding the range.

3. **`--experimental-import-meta-resolve` unflag** — Inject on versions where the parent-URL form is still flagged.

4. **`--experimental-require-module` unflag** *(low priority)* — Inject on Node 22.0–22.11. Mostly already handled by Nub's version floor; primarily a record-of-decision so future agents don't re-investigate.

5. **`--experimental-addon-modules` unflag** *(optional, can fold into the vm-modules doc)* — Inject on Node 23.6+. Low-volume feature; consider folding into a shared ESM-loader-related unflag doc.

6. **Webstorage policy** — a decision record rather than a feature doc. Documents the choice **not** to inject `--experimental-webstorage`, or to inject it with caveats, and why. The interesting question is the on-disk storage location and whether persisting accidental writes is acceptable. Probably "no inject", but worth a real write-up.

The flag table should also be updated to:

- Add a row noting `--experimental-detect-module` is already default-on through Node 26 (verification pending).
- Note the `NODE_OPTIONS` precedence question as an Open Question; the post-survey table makes it more pressing.
- Reference this research doc by name in its "Related" section.

## Open questions

Six unresolved items: per-version flag states, `NODE_OPTIONS` opt-out precedence, webstorage policy, permission pass-through, test-runner injection, and phase placement.

1. **Exact Node 22.15 default state for several flags.** Whether `--experimental-eventsource`, `--experimental-wasm-modules` and a couple of others are flagged or default-on at exactly Node 22.15 is unresolved. The per-minor-version [release notes](https://nodejs.org/en/about/previous-releases) settle it; the flag-table work should do that verification systematically, not opportunistically.

2. **`NODE_OPTIONS` opt-out handling.** If the user sets `NODE_OPTIONS=--no-experimental-vm-modules`, should Nub's injection respect it? Probably yes — a user-explicit opt-out beats a Nub-injected opt-in — but it is a real design decision and belongs in the flag-injection plan's open questions.

3. **Webstorage policy.** Should Nub inject `--experimental-webstorage` on Node 22.15? It adds two globals (`localStorage`, `sessionStorage`) with persistent disk-backed semantics. The Cloudflare/Workers audience expects them to exist but be ephemeral; the disk persistence is the surprise. **Needs a real decision doc.**

4. **`--experimental-permission` pass-through.** Not a default-on question, but a real one: should `nub --permission` exist as a first-class flag? If so, Nub has to handle its own preloads under the permission model — preloads require `--allow-fs-read`, and the transpile cache requires `--allow-addons`. Material follow-up, not v0.

5. **Test-runner flag injection.** A Nub-orchestrated `node:test` run would plausibly want `--experimental-test-coverage`, `--experimental-test-snapshots` and `--experimental-test-module-mocks` injected. Nub ships no test subcommand, so this is untested.

6. **Flag injection vs. Phase 1 vs. Phase 2.** The four high-confidence unflags (`vm-modules`, `eventsource`, `import-meta-resolve`, `require-module`) are mechanical, the same shape as the existing flag-table work, and should land in Phase 1 rather than being deferred to Phase 2 with the polyfill-injection work. The webstorage policy and addon-modules unflags could be Phase 2 calls.

## Sources

Node's CLI and stability-index documentation, the release notes that moved each flag, and the issue threads behind the permission-model, require(esm) and vm-modules findings.

- [`nodejs.org/api/cli.html`](https://nodejs.org/api/cli.html) — canonical flag list, accessed 2026-05-18.
- [`nodejs.org/api/documentation.html`](https://nodejs.org/api/documentation.html) — stability index definitions.
- [`nodejs.org/en/blog/release/v26.0.0`](https://nodejs.org/en/blog/release/v26.0.0) — Node 26 stabilizations and removals (Temporal stable; `--experimental-transform-types` removed; `module.register()` runtime-deprecated).
- [`nodejs.org/en/blog/release/v23.6.0`](https://nodejs.org/en/blog/release/v23.6.0) — strip-types default-on.
- [Joyee Cheung, "require(esm) in Node.js: from experiment to stability"](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/) — require-module stable across all LTS lines.
- [`nodejs/node` PR #50068](https://github.com/nodejs/node/pull/50068) — permission model to active development.
- [`nodejs/node` PR #56201](https://github.com/nodejs/node/pull/56201) — permission model stabilization PR.
- [`nodejs/next-10` #285](https://github.com/nodejs/next-10/issues/285) — re-evaluating long-running experiments (network-imports removed, others under review).
- [`nodejs` Discussions #54948](https://github.com/orgs/nodejs/discussions/54948) — confirmation that `--experimental-network-imports` was removed.
- [`jestjs/jest` #14156](https://github.com/jestjs/jest/issues/14156) — `--experimental-vm-modules` + native addons edge case.
- [JetBrains WEB-52967](https://youtrack.jetbrains.com/issue/WEB-52967/) — request for WebStorm to auto-inject `--experimental-vm-modules` (same value-prop as Nub's proposal).

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
