---
**Scope:** Survey Node.js's `--experimental-*` flag surface as it
stands across Node 22 LTS, 24 LTS, and 25/26 current, and identify
which flags Nub should consider auto-injecting in default mode that
are not already covered by an existing per-feature plan. Per Nub's mechanism
rule (augmenter, not fork), the only knob in scope here is "Nub prepends a flag to the spawned
`node` invocation" — no source patches, no library substitutions.

**Status:** v1, 2026-05-18. A point-in-time survey of the flag surface as it
stood across Node 22, 24 and 25/26.
The candidate list lives at the bottom under "Recommended new
plan docs."

**Related docs:**
- `../runtime/auto-flag-injection.md`
  — the mechanism this research feeds.
- `../runtime/additivity.md` — the
  additivity policy that gates everything.
- `../runtime/target-version.md`
  — Node 22.15 floor for augmented mode.
- `../runtime/sqlite-unflag.md` —
  reference example of a small unflag plan doc.
- Already-covered unflag candidates (do not re-litigate):
  `temporal.md`,
  `url-pattern.md`,
  `websocket.md`,
  `connect-sockets.md`,
  `html-rewriter.md`,
  `wintertc-runtime-key.md`,
  `min-common-api-globals.md`,
  `cloudflare-apis.md`,
  `sqlite-unflag.md`.
---

# Experimental flag unflagging survey

## TL;DR

### Unflag in default mode (recommended shortlist)

These are the flags Nub should add to its `auto-flag-injection.md` table in addition to what's already there. Order is roughly by confidence (high → lower).

1. **`--experimental-require-module`** — On Node 22.0–22.11 only. Stable / unflagged since 22.12. Zero risk, high value (CJS importing ESM is one of the single biggest sources of ecosystem pain). Suggests new plan doc: `runtime/require-esm-unflag.md`.
2. **`--experimental-vm-modules`** — On all supported versions. No behavior change for code that doesn't use `vm.SourceTextModule`; strict feature-add. Big quality-of-life win for Jest, Vitest ESM, and anyone writing module-aware tooling. Suggests new plan doc: `runtime/vm-modules-unflag.md`.
3. **`--experimental-eventsource`** — On Node versions where it's still flagged (22.0–22.x prior to default-on; needs verification in implementation). Strict additive global. Suggests new plan doc: `runtime/eventsource-unflag.md`.
4. **`--experimental-import-meta-resolve`** — On Node 22.x. Partial stabilization in 20.6; full sync version stabilized later. Code that uses it is already opt-in; risk surface is limited to feature-detection edge cases. Suggests new plan doc: `runtime/import-meta-resolve-unflag.md`.
5. **`--experimental-detect-module`** — Already on by default in Node 22.7+. **No action needed for the supported range** (22.15 floor), but worth a one-paragraph note in `auto-flag-injection.md` so future agents don't re-investigate. Documented here only for completeness — no plan doc needed.

### Leave flagged (user opts in)

These are real features but unflagging-by-default would either violate additivity, change global state in feature-detect-visible ways, or carry too much risk.

- **`--experimental-strip-types`** / **`--experimental-transform-types`** — We have our own transpiler. Already covered in `auto-flag-injection.md` ("What about `--experimental-strip-types`?"). `--experimental-transform-types` was **removed in Node 26** per the [v26.0.0 release notes](https://nodejs.org/en/blog/release/v26.0.0). Decision stands: do not inject.
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

For each flag I tried to answer six questions:

1. **What it does.** One sentence.
2. **Stability index.** Where Node ranks it on the [stability ladder](https://nodejs.org/api/documentation.html#stability-index) (0 deprecated, 1.0 early development, 1.1 active development, 1.2 release candidate, 2 stable, 3 legacy). Most experimental features are still 1.x; the interesting filter is 1.1 vs 1.2.
3. **Breakage risk if turned on by default.** This is the additivity check. Two failure modes:
   - **Direct semantic change** to code that doesn't use the feature (almost always disqualifying).
   - **Indirect change** via globals appearing where they used to be `undefined` (compatibility risk, see below).
4. **Compatibility risk via feature detection.** If user code does `if (typeof globalThis.X === 'undefined') { /* polyfill */ }`, and Nub makes `X` defined, the polyfill branch goes cold. This is usually OK (the polyfill is functionally equivalent) but *sometimes* not (older polyfills have wider surface; native implementation may have a different prototype chain; iframe-like instanceof checks can fail). Logged as a separate column because it's the most common reason a "strictly additive" flag has hidden teeth.
5. **Real-world signal.** Is the feature widely used in production despite the flag (Jest's reliance on `--experimental-vm-modules` is the canonical example)? Is the Node team signaling imminent unflagging?
6. **Recommendation for Nub.**

The methodological constraint specific to Nub: even if Node's own posture is "we'll unflag this in N.x soon," that doesn't change whether **today's user on Node 22.15** would benefit from Nub injecting it. The whole point of `auto-flag-injection.md` is to give users the experience of a newer Node on whatever Node they have installed.

### Caveats and uncertainty

I drew on:
- [`https://nodejs.org/api/cli.html`](https://nodejs.org/api/cli.html) via WebFetch on 2026-05-18.
- [`https://nodejs.org/en/blog/release/v26.0.0`](https://nodejs.org/en/blog/release/v26.0.0) for the Node 26 changeover.
- Search results for individual flag histories ([`require-esm`](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/), [strip-types](https://github.com/nodejs/typescript/issues/24), [permission model](https://github.com/nodejs/node/pull/56201), [network-imports removal](https://github.com/orgs/nodejs/discussions/54948)).

Where I'm uncertain about a specific Node version (e.g., "did eventsource get unflagged in 22.x or 23.x?"), I've flagged that in the per-flag section. The implementation work for `auto-flag-injection.md` will need to verify against the actual Node release notes per version when populating the table; I'm not going to pretend I have that level of version-specific certainty here.

I have **not** investigated V8-level flags (`--harmony-*`, `--turboshaft`, etc.) in depth. The brief asked me to note them but stay out of the V8 rabbit hole. A short section at the end collects what I noticed in passing.

## Per-flag analysis

### `--experimental-require-module`

**What it does.** Allows `require()` to load ES modules synchronously, provided the ESM has no top-level await.

**Stability.** Stable as of Node 22.12 LTS / 23.0; the **lack** of this flag (i.e., `--no-require-module`) is now the user-facing escape hatch. Per [Joyee Cheung's writeup, Dec 2025](https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/), the feature is unflagged across all supported LTS lines and marked Stability 2.

**Breakage risk if on by default.** Approximately zero for code that doesn't use `require()` of ESM. For code that *does* call `require()` on an ESM with TLA, the call throws — but that's true regardless of the flag; flagging just gates whether the call is attempted at all. No change to existing CJS-of-CJS or ESM-of-anything semantics.

**Compatibility risk.** Some libraries do `try { require(esmPkg) } catch { /* fall back */ }`. With the flag on, the try succeeds where it used to fail, which is technically a behavior change. In practice this is what the *user wants* — the fallback path is usually slower or feature-limited. Risk: minimal.

**Real-world signal.** This is one of the single most-requested features in Node history. Was on the experimental-feature re-evaluation thread ([nodejs/next-10#285](https://github.com/nodejs/next-10/issues/285)). Unflagging it on the Node 22.0–22.11 range gives those users a forward-looking experience without upgrading.

**Recommendation.** **Inject on Node 22.0–22.11.** No injection needed on 22.12+ (already default). Document in `auto-flag-injection.md`; warrants a small plan doc `runtime/require-esm-unflag.md` to capture the version-range logic.

Note that Nub's hard floor for augmented mode is Node 22.15.0 per `target-version.md` — which means **the 22.0–22.11 range is already in compat-mode-only territory**. So this injection ends up being a no-op for augmented mode. But: (a) the table entry is the documentation, and (b) if the floor ever lowers (unlikely but possible), the entry will already be there.

The honest answer is: **this flag is mostly already-handled by Nub's version floor**. The plan doc can be a tiny one-pager.

### `--experimental-vm-modules`

**What it does.** Exposes the `vm.SourceTextModule` and `vm.SyntheticModule` constructors in the `node:vm` module. Without the flag, those classes throw on construction.

**Stability.** Stability 1 — Experimental. [Open since v9.6.0](https://nodejs.org/api/cli.html). No clear signal of imminent stabilization; the [nodejs/next-10 re-evaluation issue](https://github.com/nodejs/next-10/issues/285) flagged it as one of the long-running experiments. As of 2026, no PR I found is on the stabilize track.

**Breakage risk if on by default.** Zero. Without using `vm.SourceTextModule`, the flag is invisible. With the flag, the constructor exists; without it, the constructor throws. No existing code's semantics change unless it specifically tries to construct one.

**Compatibility risk.** The one edge: some test runners (Jest in particular, per [jestjs/jest#14156](https://github.com/jestjs/jest/issues/14156)) do `try { new vm.SourceTextModule(...) } catch { /* fallback to something else */ }` for environment detection. Nub making the constructor available means the fallback branch is never taken. This is almost always what the user wants.

The deeper risk is **subtle Node regressions in `vm.SourceTextModule` itself** (e.g., the [Node 15.9 regression](https://github.com/nodejs/node/issues/37426) that broke Jest ESM). These were specific to historical Node versions; on the supported 22.15+ range they're not active issues.

**Real-world signal.** Heavy. Jest's ESM mode requires it; ts-jest, Vitest in some modes, lots of tooling. JetBrains has an [open feature request](https://youtrack.jetbrains.com/issue/WEB-52967/) asking WebStorm to inject it automatically — which is essentially the same value prop Nub would be providing.

**Recommendation.** **Inject on all supported versions.** This is high-value, low-risk. Warrants a plan doc: `runtime/vm-modules-unflag.md`.

The plan doc should explicitly note that Jest's ESM-mode users have to run `node --experimental-vm-modules node_modules/jest/bin/jest.js` today; with Nub in front, `nub ./node_modules/.bin/jest` would Just Work. That's a very tangible value prop to land in marketing copy.

### `--experimental-eventsource`

**What it does.** Adds `EventSource` to the global scope, matching the [WHATWG Server-Sent Events spec](https://html.spec.whatwg.org/multipage/server-sent-events.html).

**Stability.** Stability 1 — Experimental, as of the CLI doc snapshot. Search results suggest it was enabled by default in some 22.x point release and remained `--experimental-*`-named. **I am not certain of the exact version where it flipped to default-on**; the implementation work for the flag-injection table will need to verify. If it's still flagged on Node 22.15 (the floor), inject it; if already default, no-op.

**Breakage risk if on by default.** Adds a new global. The compatibility risk is the usual feature-detection one:

```js
// User code that runs on browsers, Node-with-polyfill, etc.
if (typeof EventSource === 'undefined') {
  globalThis.EventSource = require('eventsource').EventSource;
}
```

With Nub injecting the flag, `EventSource` is defined and Node's native takes over. The [`eventsource` npm package](https://github.com/EventSource/eventsource) follows the spec; the native implementation also follows the spec; they should be substitutable. **Risk: low but non-zero**, same as every WinterTC-style global polyfill addition.

**Compatibility risk.** See above. Same shape as the WebSocket, URLPattern, Temporal additions we already plan (`min-common-api-globals.md`). If Nub's policy is "we add web globals where Node has them behind a flag," EventSource is in scope of that policy.

**Real-world signal.** Moderate. Server-Sent Events is a real production pattern (LLM streaming, live updates). The native `EventSource` matters specifically for the **client** side — code that consumes an SSE stream from another service. Less common in pure-server Node but increasingly common with AI tooling that proxies SSE.

**Recommendation.** **Inject on versions where it's flagged.** Verify Node 22.15 status during implementation; if flagged, inject; if already default, no-op. Warrants a plan doc: `runtime/eventsource-unflag.md`. This doc should be in the same family as `websocket.md` and `min-common-api-globals.md` — a Cloudflare/Workers-shape web global Nub makes ambient.

### `--experimental-import-meta-resolve`

**What it does.** Enables `import.meta.resolve(specifier, parent)` — resolving a module specifier the same way the loader would, synchronously. **Partial stabilization** happened in Node 20.6: the no-parent-argument form is now stable; the two-argument form is still flagged.

**Stability.** Mixed. The sync, no-parent form is stable. The parent-URL form is still 1.0/1.1.

**Breakage risk if on by default.** Zero for code that doesn't call `import.meta.resolve`. For code that does, the flag enables the second-argument form; without the flag, calling with two args throws. Same shape as `require-module`: flag-on enables more, no existing semantics change.

**Compatibility risk.** Similar to `require-module`: try/catch detection could see new success where it used to see failure. Usually desired.

**Real-world signal.** Used by some bundlers and test runners that need synchronous resolution (the async version requires `module.register()` plumbing). Less load-bearing than `vm-modules` or `require-module`, but real.

**Recommendation.** **Inject on supported versions where it's still flagged.** I'm uncertain whether 22.15 includes it as default or still gates it; implementation should verify. Plan doc: `runtime/import-meta-resolve-unflag.md`. Probably the smallest plan doc of the four, since the feature is narrow.

### `--experimental-detect-module`

**What it does.** When a file has no `package.json` `"type"` field, Node tries CommonJS first, and if parsing fails as CJS, retries as ESM.

**Stability.** Enabled by default in Node 22.7+ per the [v22.7.0 release notes](https://nodejs.org/en/blog/release/v22.7.0) trail. Still nominally Stability 1 but on-by-default.

**Breakage risk.** N/A — already on by default in supported range.

**Recommendation.** **No action.** The current `auto-flag-injection.md` already notes this. Could add a one-line "verified: still default-on through Node 26" to that doc when implementation begins, but nothing to inject.

### `--experimental-strip-types` / `--experimental-transform-types`

**What it does.** Strips TypeScript types and runs the resulting JS; with `transform-types`, also handles enums/namespaces/decorators via SWC.

**Stability.**
- `strip-types`: **default-on as of Node 23.6** per the [v23.6.0 release notes](https://nodejs.org/en/blog/release/v23.6.0). Warnings removed in 24.3. **Stable in 24.12** (Stability 2).
- `transform-types`: **removed in Node 26** per the [v26.0.0 release notes](https://nodejs.org/en/blog/release/v26.0.0) (PR #61803). Node's official direction is "strip types only; if you need non-erasable syntax, use a real transpiler."

**Recommendation.** **Do not inject.** Already covered in detail in the existing plan doc. Nub owns `.ts` files via its registerHooks load hook; Node's strip-types never sees them. Updates for this research: nothing new — the existing decision holds, and Node's removal of `transform-types` in 26 *vindicates* the Nub direction (we ship a real transpiler that handles enums etc.). No plan doc change.

### `--experimental-permission` / `--permission`

**What it does.** Restricts file system, network, child_process, worker_threads, native addons, WASI, and inspector access at runtime. Per [PR #50068](https://github.com/nodejs/node/pull/50068) moved to Stability 1.1 in 2023; per [PR #56201](https://github.com/nodejs/node/pull/56201) being merged to stabilize, the model has matured. As of 2026 the flag is [renamed to `--permission`](https://openjsf.org/blog/nodejs-24-released) in Node 24.

**Stability.** 1.2 → 2 in the 24+ timeframe.

**Breakage risk if on by default.** **Catastrophic.** The whole point of the permission model is to break code that does unsafe things. Enabling it without explicit user request would make a huge fraction of npm packages fail to load (native addons require `--allow-addons`; FS access requires `--allow-fs-*`; etc.). This is the **antithesis** of additivity.

**Compatibility risk.** N/A — this is intentionally breaking.

**Real-world signal.** Used by security-conscious users; mostly not used by general developers because of how invasive it is.

**Recommendation.** **Do not inject by default. Ever.** Possibly worth a Nub CLI pass-through: `nub --permission script.ts` forwards `--permission` to Node, with Nub-specific allowlist expansion to handle Nub's own preloads. That's a real feature question, not a flag-injection question — would warrant its own plan doc only if/when it becomes a priority.

Out of scope for this research; flagged for future thinking.

### `--experimental-shadow-realm`

**What it does.** Enables the [ShadowRealm](https://github.com/tc39/proposal-shadowrealm) global, a TC39 Stage 3 proposal for sandboxed JS evaluation.

**Stability.** Stability 1 — Experimental. The [Node implementation](https://github.com/nodejs/node/commit/e86a638305) shipped in 2022 but has known [memory leak issues](https://github.com/nodejs/node/issues/47353). TC39 status is Stage 3 (no more spec changes expected, but no movement to Stage 4).

**Breakage risk if on by default.** Low at the language level (ShadowRealm is a new global that user code doesn't typically depend on being undefined). High at the implementation level — the memory leak is a real bug.

**Compatibility risk.** Some libraries may feature-detect `ShadowRealm` and switch implementation paths. Not a big surface today.

**Real-world signal.** Almost zero. Cloudflare uses something similar internally, but the public-API audience for ShadowRealm is still small.

**Recommendation.** **Leave flagged.** Niche feature with known implementation issues. Users who want it can pass the flag themselves. No plan doc needed.

### `--experimental-sea-config`

**What it does.** Configures [Single Executable Applications](https://nodejs.org/api/single-executable-applications.html) — bundling a Node app as a single binary.

**Stability.** Stability 1 — Experimental.

**Breakage risk if on by default.** N/A in the sense that this is a build-time flag (`node --experimental-sea-config sea.json` generates a blob); it doesn't change runtime behavior of unrelated code.

**Recommendation.** **Out of scope.** SEA is build-tooling, not runtime behavior. If Nub ever adds a `nub build --executable` subcommand, that command can use this internally; not a flag-injection concern.

### `--experimental-test-coverage` / `--experimental-test-snapshots` / `--experimental-test-module-mocks`

**What they do.** Extend Node's built-in test runner (`node:test`) with coverage reporting, snapshot testing, and module mocking respectively.

**Stability.** All Stability 1. The base `node:test` runner itself graduated to Stability 2 in Node 20.

**Breakage risk if on by default.** Effectively zero — these flags only do anything when the user runs `node --test`. For non-test runs, they're inert.

**Compatibility risk.** Code that does `try { ... }` around test APIs could see different behavior. But that's a degenerate case.

**Real-world signal.** node:test usage is growing but still overshadowed by Jest/Vitest. Coverage is the most-used of the three.

**Recommendation.** **Don't inject by default.** Nub ships no subcommand that shells out to `node:test`, so there is no dispatch point where injecting these flags under-the-hood would apply. For the generic `nub script.ts` path, leave them off — they have a slight startup cost (test-coverage in particular enables V8 coverage which is non-free) and they're noise.

### `--experimental-wasm-modules`

**What it does.** Allows `import wasmModule from './foo.wasm'` ES module syntax for WebAssembly modules.

**Stability.** Stability 1. There's been ongoing work; the import-attributes-syntax variant may have been unflagged but I am **not certain** of the current 22.15 status. Implementation should verify.

**Breakage risk if on by default.** Zero unless user code imports a `.wasm` file. Strict feature-add.

**Compatibility risk.** Low. The main risk is that the WASM-module spec is still evolving; if Node ships an early version and TC39 changes the spec, code using the early form could break. That's Node's problem to manage, not Nub's.

**Real-world signal.** Moderate-and-growing. WASM in npm packages (esbuild-wasm, wasm crypto libs, etc.) is increasingly common.

**Recommendation.** **Probably inject on Node 22.15 if still flagged.** I'm flagging this with "probably" rather than "definitely" because (a) verification of current default-status is needed, (b) the import-attributes syntax has been in flux. Plan doc only if verification shows it's still flagged on the supported range; otherwise the existing `auto-flag-injection.md` note suffices.

### `--experimental-async-context-frame`

**What it does.** Switches `AsyncLocalStorage`'s backing implementation from the older async_hooks-based mechanism to a faster V8 AsyncContextFrame integration.

**Stability.** The public API (`AsyncLocalStorage`) is Stability
2. The flag selects the implementation. The frame implementation
is the default on Node 23+; the flag exists in the form `--no-experimental-async-context-frame` to disable.

**Breakage risk if on by default.** Should be zero — same public API contract. In practice, Electron and some users explicitly opt out via `--no-experimental-async-context-frame` because their context flow differs from what AsyncContextFrame expects.

**Compatibility risk.** Edge cases around continuation semantics in unusual control-flow patterns (generators, native code calling JS callbacks across realms).

**Recommendation.** **No injection needed in supported range.** Already default-on for Node 23+. Document that we leave it default and don't disable it. No plan doc.

### `--experimental-network-imports`

**Status:** Removed in Node 22.6+ per [nodejs#54948](https://github.com/orgs/nodejs/discussions/54948). No action needed. Listed only so future agents don't re-investigate.

### `--experimental-quic`

**What it does.** Enables `node:quic` for QUIC protocol support.

**Stability.** Stability 1.1, Node 25+.

**Breakage risk if on by default.** Low for code that doesn't import `node:quic`. Possibly some surface-area for socket-creating code patterns we haven't thought through.

**Recommendation.** **Leave flagged for now.** Node 25 isn't LTS; the surface is too new to commit to. Revisit when Node 26 LTS ships in October 2026 and QUIC's status is clearer.

### `--experimental-addon-modules`

**What it does.** Allows `.node` files to be loaded via `import` rather than only via `require()`.

**Stability.** Stability 1.0, Node 23.6+.

**Breakage risk if on by default.** Strict feature-add — without the flag, `import './foo.node'` fails; with it, it works. No existing code path changes.

**Compatibility risk.** Low. Some loaders/resolvers have hand-rolled .node handling; that path becomes redundant.

**Real-world signal.** Niche but growing as more native addons ship ESM-first wrappers.

**Recommendation.** **Worth injecting on supported versions where flagged, but low priority.** Could roll into the same plan doc as `vm-modules` and `import-meta-resolve` if we want fewer docs; otherwise a tiny standalone: `runtime/addon-modules-unflag.md`.

### `--experimental-print-required-tla`

**What it does.** Prints the location of top-level await in ES modules that fail to be required via `require()`.

**Stability.** Stability 1. Diagnostic flag.

**Recommendation.** **Don't inject.** This is a debugging aid that prints info on failure. Not a runtime feature. If anything, this flag would conflict with `--no-warnings` (which Nub does inject).

### `--experimental-default-config-file` / `--experimental-config-file`

**What they do.** Load configuration from a JSON file (`node.config.json`) that maps to CLI flags.

**Stability.** Stability 1.0, Node 23.10+.

**Breakage risk if on by default.** **Real.** With this flag on, Node looks for `node.config.json` in cwd and honors options like `"nodeOptions"`. If a user has a stale config file from experimenting, it would silently apply. That's a behavior change to code that didn't ask for it.

**Compatibility risk.** Real (see above).

**Recommendation.** **Skip.** Violates the "no surprises" rule for default mode. Users who want config-file support can opt in.

### `--experimental-ffi` (Node 26+)

**What it does.** Enables `node:ffi` for foreign-function-interface calls (similar to Deno's FFI).

**Stability.** Stability 1, Node 26.1+. Requires FFI-enabled build (not present on most distributions yet).

**Recommendation.** **Skip.** Too new, build-flag-gated, niche. Revisit in 2027.

### `--experimental-stream-iter` (Node 25.9+)

**What it does.** Enables `node:stream/iter`, a new submodule with iterator-based stream utilities.

**Stability.** Stability 1, very new.

**Recommendation.** **Skip.** Too new for a default-on call. Reconsider when it's been in two LTS lines.

### `--experimental-global-navigator` / `--no-experimental-global-navigator`

**What it does.** Exposes `globalThis.navigator` (with `hardwareConcurrency`, `userAgent`, `language`, `platform`). Default-on as of Node 22+.

**Recommendation.** **No action — already default.** Worth noting in `min-common-api-globals.md` since `navigator` is part of WinterTC's minimum common API, but Node already provides it.

### `--experimental-webstorage` / `--no-experimental-webstorage`

**What it does.** Exposes `localStorage` and `sessionStorage` globals backed by file-system storage. Default-on as of Node 25.

**Stability.** Stability 1.2 (release candidate).

**Breakage risk if on by default *on older Node*.** Real — adds two globals. Compatibility risk: code that feature-detects `typeof localStorage === 'undefined'` and assumes "we're in Node, no localStorage." For a long time that was a valid signal; with Nub making it ambient on Node 22.x, the signal goes away.

**Compatibility risk.** The bigger issue is **state semantics**: `localStorage` persists to disk. If Nub injects this flag on Node 22.15, suddenly any code that accidentally writes to `localStorage` (e.g., browser-targeted code running on the server) starts persisting to disk. That's an unusual failure mode but real.

**Real-world signal.** Mixed. Cloudflare Workers doesn't ship localStorage. Deno does. Bun does. Node 25 ships it.

**Recommendation.** **Defer.** Not a clean "inject and forget" call. Probably want a plan doc dedicated to webstorage that discusses the storage-path question (where does the file live? per project?), and answers "do we want this?" before unflagging it. Plan doc: `runtime/webstorage-policy.md` — **but the plan doc may decide NOT to inject.** It's a policy question, not a mechanical one.

This is the most interesting "leave flagged but write a plan doc to record the decision" candidate.

## Cross-cutting concerns

### Feature detection via `typeof globalThis.X === 'undefined'`

Several of the flag-on candidates add globals (`EventSource`, `ShadowRealm`, `localStorage`, `navigator`, etc.). Every one of those carries the feature-detection trap:

```js
if (typeof URLPattern !== 'undefined') {
  // use native
} else {
  // load polyfill, which may have wider/narrower surface
}
```

Polyfill libraries are a known-tricky surface:
- Some polyfills are strict-superset of the native (e.g., wider Unicode support); Nub making the native appear means losing the superset. Edge case but real.
- Some polyfills check `instanceof` against their own constructor; if user code calls a Nub-native API that returns a native instance, the polyfill's `instanceof` check fails.

We accepted this trade-off for `URLPattern`/`Temporal`/`WebSocket` already (see `min-common-api-globals.md`). For new globals added via flag injection (mainly EventSource), the same risk profile applies. Document in each plan doc.

### Interaction with `--no-warnings`

Nub injects `--no-warnings`. Many experimental flags emit "ExperimentalWarning: X is an experimental feature" the first time the feature is used. By injecting `--no-warnings`, Nub already suppresses these. Two effects:

1. **Good**: user doesn't see noisy warnings about features Nub intentionally enabled.
2. **Caveat**: user also doesn't see warnings about features **they** enabled. If a Nub user passes `--experimental-quic` themselves, Nub's `--no-warnings` swallows the warning, which may surprise them.

The existing `auto-flag-injection.md` Open Questions section already flags this. No new concern from this research — just noting that more flag-injection makes the implicit warning-suppression more impactful.

### `process.features` surface

Node exposes some of these via `process.features` (e.g., `process.features.typescript` indicates strip-types status). Code that introspects `process.features.{quic,sqlite,...}` would see different values depending on what Nub injects.

This is exactly the same risk as `globalThis.X` feature detection, in a different shape. Same mitigation: document per plan doc.

### Flag conflicts and ordering

Node generally takes last-wins for conflicting CLI flags. Nub's injection prepends, so user-passed flags win. The relevant edge cases:

- `NODE_OPTIONS` is parsed before argv. If the user has `NODE_OPTIONS=--no-experimental-vm-modules` and Nub injects `--experimental-vm-modules`, Nub's wins (later in the effective command line). This is probably **wrong** behavior — the user explicitly opted out, Nub overrode them.
- Need explicit handling: read `NODE_OPTIONS`, detect any `--no-experimental-*` opt-outs, and either (a) skip the corresponding inject, or (b) document the precedence clearly.

This is an implementation-level concern but worth surfacing in `auto-flag-injection.md`'s Open Questions. **New open question** for that doc.

### Version-table maintenance

The flag table needs per-version entries. Maintenance cost grows with the number of flags injected. The pattern:

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

- **`--harmony-*`** — These gate proposals not yet at TC39 Stage 4. None currently look like a sensible default-on call. Skip.
- **`--turboshaft`** / Turbofan tuning — V8 compiler-pipeline flags. Could in theory tune for cold-start vs steady-state, but the cold-start research ([cold-start.md](cold-start.md)) showed the ROI is marginal and the breakage surface (rare V8 bugs) is real. Skip.
- **`--max-old-space-size`** — Memory ceiling. Already in `auto-flag-injection.md`'s consideration set as a "tuned defaults" candidate. Not really an experimental flag; deferred to a separate "Nub heap defaults" plan doc if/when it's prioritized.
- **`--expose-gc`** — Adds `globalThis.gc`. Tempting for some benchmarking code but adds a global; skip by default.

No V8 flags rise to the level of "should be in the experimental-flags-unflagging analysis."

## Recommended new plan docs

Based on this survey, the following per-feature plan docs should be created as follow-ups:

1. **`runtime/vm-modules-unflag.md`** — Inject `--experimental-vm-modules` on all supported versions. High priority — biggest value-prop unlock. Pattern: same as `sqlite-unflag.md`; tiny doc, adds one row to the flag table.

2. **`runtime/eventsource-unflag.md`** — Inject `--experimental-eventsource` on versions where it's still flagged. Implementation must verify Node 22.15 status before deciding the range.

3. **`runtime/import-meta-resolve-unflag.md`** — Inject `--experimental-import-meta-resolve` on versions where the parent-URL form is still flagged.

4. **`runtime/require-esm-unflag.md`** *(low priority)* — Inject `--experimental-require-module` on Node 22.0–22.11. Mostly already-handled by Nub's version floor; this doc is primarily a record-of-decision so future agents don't re-investigate.

5. **`runtime/addon-modules-unflag.md`** *(optional, can fold into vm-modules doc)* — Inject `--experimental-addon-modules` on Node 23.6+. Low-volume feature; consider folding into a shared "ESM-loader-related unflag" doc.

6. **`runtime/webstorage-policy.md`** — Decision-record doc, not a feature doc. Documents the choice **not** to inject `--experimental-webstorage` (or to inject it with caveats) and why. The interesting question is the on-disk storage location and whether persisting accidental writes is acceptable. Probably "no inject" but worth a real write-up.

The flag table in `../runtime/auto-flag-injection.md` should also be updated to:

- Add a row noting `--experimental-detect-module` is already default-on through Node 26 (verification pending).
- Note the `NODE_OPTIONS` precedence question as an Open Question (already noted, but the post-this-survey table makes it more pressing).
- Reference this research doc by name in its "Related" section.

## Open questions

1. **Exact Node 22.15 default state for several flags.** I was unable to nail down whether `--experimental-eventsource`, `--experimental-wasm-modules`, and a couple others are flagged or default-on at exactly Node 22.15. The [release notes](https://nodejs.org/en/about/previous-releases) for each minor version would resolve this; the implementation work for the flag table should do this verification systematically, not opportunistically.

2. **`NODE_OPTIONS` opt-out handling.** If the user sets `NODE_OPTIONS=--no-experimental-vm-modules`, should Nub's injection respect that? My instinct is yes — user-explicit opt-out beats Nub-injected opt-in. But it's a real design decision, not obvious, and worth documenting in `auto-flag-injection.md` open questions.

3. **Webstorage policy.** Should Nub inject `--experimental-webstorage` on Node 22.15? Adds two globals (`localStorage`, `sessionStorage`) with persistent disk-backed semantics. The Cloudflare/Workers audience expects them to exist but ephemeral; the disk-persistence is the surprise. **Needs a real decision doc**; see `webstorage-policy.md` recommendation above.

4. **`--experimental-permission` pass-through.** Not a default-on question, but a real "should `nub --permission` exist as a first-class flag?" question. If yes, Nub has to handle its own preloads correctly under the permission model (preloads require `--allow-fs-read`, `--allow-addons` for the transpile cache, etc.). Material follow-up; not v0.

5. **Test-runner flag injection.** A Nub-orchestrated `node:test` run would plausibly want `--experimental-test-coverage`, `--experimental-test-snapshots` and `--experimental-test-module-mocks` injected. Nub ships no test subcommand, so this is untested.

6. **Flag injection vs. Phase 1 vs. Phase 2.** The four high-confidence unflags above (`vm-modules`, `eventsource`, `import-meta-resolve`, `require-module`) are all *mechanical* — same shape as the existing flag-table work. They should land in Phase 1 alongside the rest of `auto-flag-injection.md`, not deferred to Phase 2 with the polyfill-injection work. `webstorage-policy.md` and `addon-modules-unflag.md` could be Phase 2 calls.

## Sources

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

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
