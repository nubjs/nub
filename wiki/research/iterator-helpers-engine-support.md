# Iterator Helpers — engine support and Nub relevance

## TC39 status

**Stage 4, ES2025.** The [Sync Iterator Helpers proposal](https://github.com/tc39/proposal-iterator-helpers) advanced to Stage 4 in October 2024 and is included in the ES2025 specification. Source: [tc39/proposals finished-proposals list](https://github.com/tc39/proposals/blob/main/finished-proposals.md).

**Async Iterator Helpers is a SEPARATE proposal**, still at Stage 2 as of mid-2026 and shipped in no engine. Source: [tc39/proposal-async-iterator-helpers](https://github.com/tc39/proposal-async-iterator-helpers). This doc covers sync helpers only.

---

## Surface

The spec defines the following on `Iterator.prototype`:

`map` · `filter` · `take` · `drop` · `flatMap` · `reduce` · `toArray` · `forEach` · `some` · `every` · `find`

Plus a static `Iterator.from(O)` method and the `Iterator` constructor. All helpers are lazy (returning `%IteratorHelperPrototype%`-based objects) except `reduce`, `toArray`, `forEach`, `some`, `every`, and `find`, which are eager/consuming.

---

## Engine landing (default-on in all cases)

| Engine | First shipped | Default? | Notes |
|---|---|---|---|
| **V8** | v12.2 | Yes, default | Chrome 122 (Feb 2024). V8 12.2 is the introduction point. |
| **SpiderMonkey** | Firefox 131 | Yes, default | September 2024. |
| **JavaScriptCore** | Safari 18.4 | Yes, default | March 2025 ("Baseline 2025 — Newly available"). |

MDN marks Iterator Helpers as "Baseline 2025" (available across all major engines since March 2025). Global browser usage: ~83.84% per caniuse.

---

## V8 → Node.js version map

| Node.js | V8 | Iterator Helpers? |
|---|---|---|
| 18.19.x | 10.2.154.x | **No** |
| 20.x | 11.3.244.x | **No** |
| 21.x | 11.8.172.x | **No** |
| **22.0.0+** | **12.4.254.x** | **Yes — on by default** |
| 22.15.0 | 12.4.254.21 | **Yes — on by default** |

Node.js 21.x never shipped V8 12.x; it ended at V8 11.8 (EOL April 2024). Node 22.0.0 (April 2024) jumped to V8 12.4, already past the V8 12.2 introduction point, so no intermediate Node version bridged the gap. There was never a `--harmony-iterator-helpers` flag period in Node.js — by the time any Node shipped V8 12.2+, Iterator Helpers were on by default.

Verified sources: `deps/v8/include/v8-version.h` in Node 22.15.0 source (`V8_MAJOR_VERSION 12`, `V8_MINOR_VERSION 4`, `V8_BUILD_NUMBER 254`, `V8_PATCH_LEVEL 21`); Node 22.0.0 release notes ("update V8 to 12.4.254.14"); Node 21/20/18 changelogs.

---

## Nub relevance

Nub inherits the user's installed Node's V8 verbatim — it augments via extension surfaces only, and does not ship or patch V8. Consequences:

### (a) Which Node versions get Iterator Helpers for free?

**Node 22.0.0 and above.** Any Nub user on Node ≥ 22 gets `Iterator.prototype.{map,filter,…}` natively with zero configuration: no polyfill, no flag, no Nub action.

### (b) Is there a V8 flag Nub needs to unfurl?

**No.** Iterator Helpers shipped on by default in every Node.js version that has V8 12.2+, and never required `--harmony-iterator-helpers` in any released Node.js build. There is nothing for Nub's feature matrix (`crates/nub-core/src/node/flags.rs` / `feature_matrix.rs`) to unflag.

### (c) The floor and fast-tier boundary

- **Node 18.19 (Nub's support floor) → V8 10.2 → NO Iterator Helpers.** Users on Node 18, 20, or 21 have neither the `Iterator` global nor any prototype helpers natively. A Nub polyfill path would have to cover these versions; Nub does not polyfill web/runtime APIs today, so this is an expected gap rather than a defect. A user-land polyfill such as `core-js` covers it.
- **Node 22.15 (Nub's fast-tier classifier) → V8 12.4.254.21 → YES, on by default.** The fast-tier threshold already coincides with full native Iterator Helpers support.

No action is required in Nub itself: Iterator Helpers are purely a V8 runtime feature, available on-by-default in every Node.js version Nub could plausibly see in the fast tier.

---

## Changelog

- 2026-06-30 — Initial write-up.
