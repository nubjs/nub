# Iterator Helpers — Engine Support & Nub Relevance

## TC39 Status

**Stage 4, ES2025.** The [Sync Iterator Helpers proposal](https://github.com/tc39/proposal-iterator-helpers) advanced to Stage 4 in October 2024 and is included in the ES2025 specification. Source: [tc39/proposals finished-proposals list](https://github.com/tc39/proposals/blob/main/finished-proposals.md).

**Async Iterator Helpers is a SEPARATE proposal** and is still at Stage 2 as of mid-2026. It has not shipped in any engine. Source: [tc39/proposal-async-iterator-helpers](https://github.com/tc39/proposal-async-iterator-helpers). This doc covers sync helpers only.

---

## Surface

The spec defines the following on `Iterator.prototype`:

`map` · `filter` · `take` · `drop` · `flatMap` · `reduce` · `toArray` · `forEach` · `some` · `every` · `find`

Plus a static `Iterator.from(O)` method and the `Iterator` constructor itself. All helpers are lazy (return `%IteratorHelperPrototype%`-based objects) except `reduce`, `toArray`, `forEach`, `some`, `every`, and `find` (eager/consuming).

---

## Engine Landing (default-on in all cases)

| Engine | First shipped | Default? | Notes |
|---|---|---|---|
| **V8** | v12.2 | Yes, default | Chrome 122 (Feb 2024). V8 12.2 is the introduction point. |
| **SpiderMonkey** | Firefox 131 | Yes, default | September 2024. |
| **JavaScriptCore** | Safari 18.4 | Yes, default | March 2025 ("Baseline 2025 — Newly available"). |

MDN marks Iterator Helpers as "Baseline 2025" (available across all major engines since March 2025). Global browser usage: ~83.84% per caniuse.

---

## V8 → Node.js Version Map

| Node.js | V8 | Iterator Helpers? |
|---|---|---|
| 18.19.x | 10.2.154.x | **No** |
| 20.x | 11.3.244.x | **No** |
| 21.x | 11.8.172.x | **No** |
| **22.0.0+** | **12.4.254.x** | **Yes — on by default** |
| 22.15.0 | 12.4.254.21 | **Yes — on by default** |

Node.js 21.x never shipped V8 12.x; it ended at V8 11.8 (EOL April 2024). Node 22.0.0 (April 2024) jumped to V8 12.4, which is already past the V8 12.2 introduction point. No intermediate Node version bridged the gap. There was no `--harmony-iterator-helpers` flag period in Node.js — by the time any Node shipped V8 12.2+, Iterator Helpers were already on by default.

Verified sources: `deps/v8/include/v8-version.h` in Node 22.15.0 source (`V8_MAJOR_VERSION 12`, `V8_MINOR_VERSION 4`, `V8_BUILD_NUMBER 254`, `V8_PATCH_LEVEL 21`); Node 22.0.0 release notes ("update V8 to 12.4.254.14"); Node 21/20/18 changelogs.

---

## Nub Relevance

Nub inherits the user's installed Node's V8 verbatim — it augments via extension surfaces only, it does not ship or patch V8. This means:

### (a) Which Node versions get Iterator Helpers for free?

**Node 22.0.0 and above.** Any nub user on Node ≥ 22 gets `Iterator.prototype.{map,filter,…}` natively with zero configuration. No polyfill, no flag, no nub action.

### (b) Is there a V8 flag nub needs to unfurl?

**No.** Iterator Helpers shipped on by default in every Node.js version that has V8 12.2+. The feature never required `--harmony-iterator-helpers` in any released Node.js build. There is nothing for nub's feature matrix (`crates/nub-core/src/node/flags.rs` / `feature_matrix.rs`) to unflag.

### (c) The floor and fast-tier boundary

- **Node 18.19 (nub's support floor) → V8 10.2 → NO Iterator Helpers.** Users on Node 18 or 20 or 21 do not have the `Iterator` global or any prototype helpers natively. If nub were to provide a polyfill path, it would need to cover these versions. Today nub does not polyfill web/runtime APIs, so this is a gap for those users — expected, not a nub bug.
- **Node 22.15 (nub's fast-tier classifier) → V8 12.4.254.21 → YES, on by default.** The fast-tier threshold already coincides with full native Iterator Helpers support.

### Summary for nub

No action required in nub itself. Iterator Helpers are purely a V8 runtime feature available on-by-default in every Node.js version nub could plausibly see in the fast tier (Node ≥ 22). The support boundary is:

- **< Node 22** → not available (no polyfill from nub; user-land polyfill possible e.g. `core-js`).
- **≥ Node 22.0.0** → natively available, no flag or nub involvement.

---

## Changelog

- 2026-06-30 — Initial write-up.
