# Research: CommonJS handling — what stance Nub should take

**Status:** 2026-05-18. **Related:** [`research/tsx-architecture.md`](tsx-architecture.md).

## Contents

- [Why this question is live](#why-this-question-is-live)
- [State of play in Node](#state-of-play-in-node)
- [Quantified ecosystem breakage](#quantified-ecosystem-breakage)
- [How the neighbors handle it](#how-the-neighbors-handle-it)
- [Five candidate stances for Nub](#five-candidate-stances-for-nub)
- [Recommendation](#recommendation)
- [Sources](#sources)

## Why this question is live

Nub is a Rust orchestrator over the user's installed Node — the soft-fork direction was abandoned 2026-05-18 — so every Nub behavior is mechanically a wrapper around what Node already does, installed via `registerHooks`, `--import`, `--require`, env, or N-API. **Anything Nub does about module format must be expressible through the surface Node already exposes**, and anything that changes interpretation of an existing file risks breaking the trust contract ("code that runs on Node also runs on Nub").

CommonJS-vs-ESM is where the cheapest quality-of-life wins live for a TS-first runner, and also where a runtime that gets too clever does the most ecosystem damage.

## State of play in Node

### `package.json` `"type"` is still authoritative — by design

Node 24's resolution for a `.js` file is, in order:

1. Extension wins (`.mjs` → ESM, `.cjs` → CJS).
2. Otherwise, walk to nearest ancestor `package.json`.
3. If `"type": "module"` → ESM; `"type": "commonjs"` → CJS.
4. If the field is absent → run **syntax detection** (default-on since v22.7.0); pick ESM iff ESM-only syntax is found, else CJS.

The third branch — explicit `"type"` — is non-negotiable, even in the face of contradictory syntax. A `.js` script with `import` syntax under an ancestor `"type": "commonjs"` parses as CJS and crashes with `Cannot use import statement outside a module`. That is the behavior Nub's planned wedge over vanilla Node targets, for **files Nub owns the load of** (`.ts`, `.tsx`, `.jsx`, plus transformed `.js`).

### `--experimental-detect-module` (Node 22.7+, default on)

Specifically scoped: applies only to files with `.js` or no extension **when** the nearest controlling `package.json` either does not exist or lacks a `"type"` field. It does not override an explicit `"type"` and it does not touch `.cjs` / `.mjs` / `.ts`. Node parses the source, and if it sees any of the following, runs the file as ESM:

- `import` declarations (not dynamic `import()`)
- `export` declarations
- `import.meta` references
- Top-level `await`
- Lexical redeclarations of CJS wrapper bindings (`require`, `module`, `exports`, `__dirname`, `__filename`)

Stability marker is **1.2 (Release Candidate)** as of Node 24+. Maintainers explicitly removed the parallel `--experimental-default-type` flag in [PR #56092][pr-56092] (Nov 2024 Dublin collaborator summit), with the reasoning: *"Node.js will not be flipping the default module system anytime soon, if ever, now that the use case of 'run ESM syntax without needing to opt in' has been satisfied by syntax detection."* **Detect-module is the official answer; an ESM-by-default mode is not coming.**

### The ESM-by-default proposal — open, but stalled

[Issue #49432][issue-49432] ("New 'ESM by default' mode", filed by Geoffrey Booth, a Node maintainer rather than a community contributor) is still technically open but has effectively been retired. The follow-on issue [#49494][issue-49494] (how to handle typeless `package.json`s under an ESM-first mode) catalogued the breakage paths — auto-patching deps' `package.json`, special-casing `node_modules`, applying the flip only to the entry-point package scope — and reached no consensus. The corresponding `--experimental-default-type` flag was implemented, then removed.

[Issue #50043][issue-50043] proposed the detect-module heuristic as a narrower alternative; that one **did** ship (as `--experimental-detect-module`, now default-on). It is the only component of the ESM-first vision that survived contact with the TSC: **detection over flipping**.

### `require(esm)` — stable, ships before Nub

[PR #51977][pr-51977] (407 reactions, the highest-reaction closed PR in the repo) added synchronous `require()` of ESM modules without top-level await. Stabilized [end of 2025][joyee-blog] in v20.19.0 and v22.12.0, default behavior in v23+. Tracking issue [#52697][issue-52697] is closed.

Interaction with detect-module is clean: the format of the importing file and the format of the imported file are independent classifications. A CJS file can `require()` an ESM file detected by syntax; an ESM file can `import` a CJS file. The only remaining failure mode is `ERR_REQUIRE_ASYNC_MODULE` when the ESM target has top-level await — affecting ~0.02% of high-impact packages per Joyee Cheung's analysis.

**The dual-package hazard is largely gone in Node 24+**: users no longer need the old "compile to CJS for require, ship dual" workaround for libraries, which is the single biggest reason CJS-vs-ESM is less of a footgun in 2026 than it was in 2023.

## Quantified ecosystem breakage

The cleanest numbers come from [wooorm/npm-esm-vs-cjs][npm-esm-cjs], a continuously-updated crawl of high-impact npm packages (those most depended-upon, sourced from ecosyste.ms). **As of 2025-12-04, on 14,159 popular packages:**

| Bucket | Count | Share |
|---|---:|---:|
| CJS-only | 7,911 | 55.9% |
| Dual (ESM + CJS) | 2,947 | 20.8% |
| ESM-only | 1,779 | 12.6% |
| "Faux ESM" (claims ESM, ships CJS) | 1,522 | 10.7% |

The dataset is biased toward high-impact packages — small/abandoned packages skew even more CJS. Bun's blog cites 73.6% CJS share against a broader baseline; the wooorm dataset's 55.9% reflects faster migration in the heavily-used tail.

### What does that mean for Nub?

The relevant question is not what share of packages are CJS — Node 24 already runs them fine — but **how many real packages break if Nub treats `.js` files in typeless packages as ESM**. Estimating from the wooorm data:

- The 7,911 CJS-only packages are split between those that declare `"type": "commonjs"` and those that omit `"type"` entirely. Spot-checks of top-100 CJS packages (express, lodash, react, chalk@4) show the **vast majority omit `"type"`** — making them "typeless CJS." Express, the canonical example, has no `type` field; its `.js` files are CJS purely by Node's default.
- Conservatively, ~80% of the CJS-only bucket is typeless CJS: ~6,330 packages out of 14,159, or **~44.7% of high-impact packages**.
- If Nub unilaterally flipped to "typeless = ESM" for `.js` files inside `node_modules`, every one of those packages would break on the first `require` or top-level `module.exports` assignment in a file that lacks ESM-detectable syntax.

**That is the rage-quit number: ~45% of the dependency surface area.**

Applying a flip only outside `node_modules` drops the breakage dramatically — most application code there is either explicitly typed or new enough to be ESM — but it creates a confusing two-rule mental model (your own code follows one rule, deps another), and Node already covers that half via syntax detection.

**Outside the typeless-defaults question, the behavioral surface where Nub can be opinionated without ecosystem damage is small** — Node has already taken the load-bearing decision.

## How the neighbors handle it

### Bun

Bun is ESM-first in spirit but CJS-respecting in practice; per [Bun's 2024 blog post "CommonJS is not going away"][bun-cjs-blog], its explicit position is that CJS is a first-class format, not a legacy to be phased out. Detection heuristic for `.js`:

- `.cjs` / `.mjs` extension wins.
- `package.json` `"type"` consulted next.
- For typeless ambiguous files: scans source for CJS markers (`module.exports`, `exports.foo =`, `require()` calls, free references to `__dirname`/`__filename`). Also added in 1.2: a `"use strict"` directive at top of file is treated as a CJS hint (because ESM is always strict-mode, the directive is redundant in ESM and ubiquitous in compiled CJS).
- ESM markers (`import`/`export`/`import.meta`) classify as ESM.
- Falls back to ESM when truly ambiguous.

Bun ignores `"type"` when source syntax is unambiguous — the same rule Nub plans for owned files, and the same divergence from Node.

### Deno

Deno is the most ESM-pure of the field. Native code is ESM-only; `.js` files imported by URL or local path are always ESM. Deno 2.0+ added CJS support for npm packages specifically — Deno detects the package as CJS via `package.json` and runs it through a CJS loader internally, but **the loader is hidden from user code**. You don't write `require()` in your own `.ts` files; you `import` from `npm:express` and Deno does the right thing under the hood.

The `.cjs` extension also works for explicit CJS in user code as of Deno 2.1. Deno's static analysis of CJS modules was improved in 2.1 to not require `--allow-read` for many CJS packages.

The stance is the most opinionated possible: **your code is ESM, even when its deps aren't.** It works because Deno controls its own resolver and module graph, with npm interop a bolted-on compatibility layer whose complexity it absorbs. Nub is Node, with `node_modules` as a peer rather than a quarantined sub-graph.

### tsx

tsx uses `es-module-lexer` to detect ESM vs CJS for ambiguous files (see [`tsx-architecture.md`](tsx-architecture.md#what-the-esm-load-hook-does)). It records the result as `module-typescript` vs `commonjs-typescript` format strings to Node's loader. In practice tsx is ESM-biased — if a `.ts` file lacks both `import` and `require()`, tsx defaults to ESM, which can break old-style CJS TS scripts. An empirical test confirms this: tsx mis-handles a CJS-syntax `.ts` file when `package.json` says ESM, while Bun and Deno get it right.

### ts-node

ts-node defers entirely to `tsconfig.json` (`module`/`target` plus `module: NodeNext`/`Node16`) and `package.json` `"type"`. It does no source-syntax detection, making it the most "Node-faithful" of the TS runners and the one most prone to the "explicit `type: commonjs` crashes a syntactically-ESM file" trap.

### Summary table

| Runtime | Typeless `.js` default | Source-syntax override `"type"`? | Typeless `.ts` default |
|---|---|---|---|
| Node 22.7+ | Syntax detection (default ESM only if ESM markers) | No | CJS (no detection for `.ts`) |
| Bun 1.2+ | Syntax detection w/ richer heuristics | Yes | ESM unless syntax says CJS |
| Deno 2+ | ESM only | N/A (user code is always ESM) | ESM |
| tsx | ESM bias | Half — only when no explicit `"type"` | ESM (bias) |
| ts-node | Defers to tsconfig + `"type"` | No | Per tsconfig (CJS by default) |
| **Nub planned** | Node default for non-owned `.js`; syntax wins for owned files | **Yes** for owned files (`.ts`/`.tsx`/`.jsx`/transformed `.js`) | Syntax wins, ESM fallback |

The TS-file column is the interesting one: Bun, Deno, and Nub all treat unbuilt TS as ESM-by-default, which is the right call because modern TS is written ESM-shaped 95%+ of the time. ts-node's CJS-default is a relic of the old "transpile to CJS, run on Node 12" world.

## Five candidate stances for Nub

Each option assumes Nub's existing wedge — source syntax wins for files Nub's hook intercepts (`.ts`, `.tsx`, `.jsx`, plus any `.js` Nub transforms). The question here is about **untransformed `.js` files**, which is where the trust-contract risk lives.

### (a) Match Node's default — do nothing

Node 22.7+ already runs `--experimental-detect-module` by default for typeless `.js`. Nub does no additional work; users get Node's behavior verbatim.

- **Pros:** Zero trust-contract risk. Zero new code. Behaviorally identical to vanilla Node. Future-proof against Node's own changes.
- **Cons:** Zero goodwill from this axis. Nub's TS-runner story doesn't change. No new "Nub gets this right and Node doesn't" narrative on `.js` specifically.
- **Breakage:** 0.
- **Goodwill:** 0 net — the wedge lives on the TS side, which is separate.

### (b) Inject `--experimental-detect-module` always

Mechanically identical to (a) because the flag is already on by default in Node 22.7+. Would only matter for users on older Node, who should be told to upgrade.

- **Pros:** Same as (a).
- **Cons:** Same as (a), plus the flag is redundant and adds noise to the spawn pipeline.
- **Breakage:** 0.
- **Goodwill:** 0 — a non-action dressed up as an action.

### (c) ESM-by-default like Deno

Treat all typeless `.js` as ESM, regardless of file contents or location.

- **Pros:** Clean mental model. Strongest "modern runtime" signal.
- **Cons:** Breaks ~45% of the popular-package dependency surface area (the typeless CJS bucket). Direct violation of the trust contract. Even Deno doesn't do this for npm packages — Deno detects CJS in `node_modules` and runs the CJS loader internally.
- **Breakage:** Catastrophic, ~45% of high-impact packages.
- **Goodwill:** Negative. **Do not pick this.**

### (d) Auto-detect from file contents for all `.js`

Run source-syntax detection on every `.js` file Nub encounters, overriding `package.json` `"type"` when syntax is unambiguous — the same rule Nub already applies to owned TS files, extended to JS.

- **Pros:** Fixes the rare case of a `.js` script with `import` syntax under an ancestor `"type": "commonjs"` (the failure mode Nub fixes for `.ts`). Internally consistent — the owned-files rule applies to everything.
- **Cons:** **Two real risks.** It requires Nub to intercept and parse every `.js` load — a full extra parse pass for the vast majority of `node_modules` files that need no transformation. More importantly, it changes behavior on files that work fine on Node: a `.js` file inside a package with explicit `"type": "commonjs"` whose strings or comments the detector false-positives on (rare, but real), and any `.js` file using both `import` and `require()` (legal in CJS via dynamic-import-inside-CJS), which would become a Nub-side hard error.
- **Breakage:** Small but nonzero, ~0.5–2% of dependencies in tail cases. Hard to bound precisely without a corpus scan.
- **Goodwill:** Mild positive for the cases it fixes, mild negative for the surprises it introduces. Net wash.

### (e) Hybrid: detect-module + source-syntax-wins for owned files only

The status quo plan. Detect-module is already on for `.js` (Node does this for free). Nub's source-syntax-wins rule applies only to files Nub transforms (`.ts`/`.tsx`/`.jsx`/optionally `.js` files inside the project root the user has explicitly opted into transformation for via tsconfig/preprocessing). Inside `node_modules`, Nub touches nothing.

- **Pros:** Maximum trust-contract preservation. The owned-files wedge delivers the real wins (TS scripts ignore explicit-`"type"` traps). Zero behavioral change for `node_modules` contents. Cheap to implement — it is already the plan. Aligns with Node's own direction (detect-module is the TSC's blessed answer).
- **Cons:** The wedge is narrow, bounded to TS-runner ergonomics.
- **Breakage:** 0 outside owned files; the owned-file behavior diverges from Node only in cases Node would have crashed.
- **Goodwill:** Positive on the TS-runner axis (the empirical comparison shows a real wedge over tsx and ts-node). Neutral on the JS-runner axis.

## Recommendation

**Pick (e): hybrid, owned-files-only.** It is already the plan and this research confirms it.

The trust contract is the load-bearing constraint: *code that runs on vanilla Node also runs on Nub*, so any change to how a `.js` file inside `node_modules` is interpreted is a violation by definition. Options (c) and (d) violate it; (a) and (b) are non-actions; (e) confines the wedge to files Nub is already responsible for, where it fixes real user pain (the `.ts`-script-under-`type:commonjs` crash).

Node already runs all the JS, so there is no "we run more code than Node" argument to win there. Nub's argument is on TS — drop a `.ts` file anywhere and it just works — where Node's type-stripping support is deliberately scoped narrower than what users want.

### Explicit tradeoff

- **Given up:** the "Nub is ESM-first like Deno" headline. Nub will not be the runtime that drags the ecosystem to ESM, and some fraction of "modern runtime" mindshare goes to whoever pushes that harder.
- **Gained:** zero trust-contract breakage on the JS side, preserved CJS support for the ~45% of the dep surface that is typeless CJS, and a clean model: *Nub runs your TS the way you'd expect; Nub runs your JS the way Node does.*
- **To message:** "your CJS works, your ESM works, your TS works, and you don't need to think about `package.json` `"type"` when you write a quick `.ts` script anywhere." The detect-module default and `require(esm)` stability mean the Node-side experience is already much better than the discourse acknowledges; Nub's job is the last-mile TS ergonomics without new incompatibility surfaces.

### Things to do anyway

Two cheap quality-of-life moves are worth keeping on the plan regardless of the stance:

- **Suppress the `MODULE_TYPELESS_PACKAGE_JSON` perf warning** when Nub made the format decision. Node's warning is noise once Nub is in the load path.
- **Diagnostic improvements** on the `Cannot use import statement outside a module` error for `.js` files — point users at the controlling `package.json` and suggest adding `"type": "module"`, since this is the one CJS-vs-ESM error that still fires on Nub even after the owned-files wedge.

Neither requires modifying Node, and neither is a behavioral change beyond "the runtime is more helpful when things go wrong."

## Sources

- [Node Issue #49432 — Discussion: New "ESM by default" mode][issue-49432]
- [Node Issue #49494 — How to handle typeless package.json under ESM-first][issue-49494]
- [Node Issue #50043 — Proposal: detect-module heuristic][issue-50043]
- [Node PR #56092 — Remove `--experimental-default-type`][pr-56092]
- [Node Issue #53016 — Default detect-module to commonjs in require hooks][issue-53016]
- [Node PR #51977 — require(esm) original implementation][pr-51977]
- [Node Issue #52697 — Tracking issue: require(esm)][issue-52697]
- [Joyee Cheung — require(esm) from experiment to stability (2025-12-30)][joyee-blog]
- [wooorm/npm-esm-vs-cjs — share of ESM vs CJS on npm registry][npm-esm-cjs]
- [Bun blog — CommonJS is not going away][bun-cjs-blog]
- [Node docs — ECMAScript modules: syntax detection](https://nodejs.org/api/packages.html#syntax-detection)
- [Deno docs — Node and npm compatibility](https://docs.deno.com/runtime/fundamentals/node/)
- [`research/tsx-architecture.md`](tsx-architecture.md)

[issue-49432]: https://github.com/nodejs/node/issues/49432 [issue-49494]: https://github.com/nodejs/node/issues/49494 [issue-50043]: https://github.com/nodejs/node/issues/50043 [pr-56092]: https://github.com/nodejs/node/pull/56092 [issue-53016]: https://github.com/nodejs/node/issues/53016 [pr-51977]: https://github.com/nodejs/node/pull/51977 [issue-52697]: https://github.com/nodejs/node/issues/52697 [joyee-blog]: https://joyeecheung.github.io/blog/2025/12/30/require-esm-in-node-js-from-experiment-to-stability/ [npm-esm-cjs]: https://github.com/wooorm/npm-esm-vs-cjs [bun-cjs-blog]: https://bun.sh/blog/commonjs-is-not-going-away

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Links to internal planning documents were removed and reference-checkout paths rewritten; findings, tables and measured values are unchanged.
