# Prototype-pollution hardening — prior art, breakage scope, and design implications

**Status:** research complete (2026-07-22).
**Question:** can Nub offer an opt-in (maybe default) "hardened mode" that freezes JS intrinsics to make prototype pollution structurally impossible — and if so, at what ecosystem-breakage cost?

Origin: Matteo Collina, ["No, We Can't Harden Node.js Against Prototype Pollution"](https://adventures.nodeland.dev/archive/no-we-cant-harden-nodejs-against-prototype/).

---

## TL;DR

An opt-in freeze of `Object.prototype` closes both named pollution vectors, and the prior art puts its breakage cost near zero — provided the override mistake is repaired.

1. **The article's futility argument is real but narrowly scoped, and does not refute Nub's approach.** Collina argues *read-side* hardening (Node's "primordials" — rewriting core to use captured intrinsics) can never make apps safe, because the app's own options objects still inherit a polluted `Object.prototype`, and JS dispatches through prototype slots via **syntax** (destructuring, spread, `for…of`, `await`-thenable) that no core rewrite controls. An **opt-in, sink-side, whole-graph freeze is a different bet**: if `Object.prototype` is immutable *before* user code runs, the pollution *write itself* fails, and the unbounded gadget set stops mattering.

2. **No quantitative npm-wide breakage study exists — anywhere.** Not from Node's frozen-intrinsics/primordials champions, not from the SES/MetaMask team (who run full lockdown in production), not in the academic literature (Silent Spring, GHunter, Dasty, ProbeTheProto, Bullseye all measure the *attack* surface, never the *breakage* surface). This is a consistent finding across all four research prongs.

3. **The best real-world breakage corpus (SES full-graph `lockdown()`, run across the whole npm tree in production for years) is ~30 named packages, cumulative, over multiple years** — and its own maintainers characterize it as "most ordinary JavaScript can run without issues." The tail is narrow, mechanical, and `defineProperty`-fixable.

4. **A curated `Object.prototype`-ONLY freeze breaks far less than the full-graph freeze the corpus describes.** Most documented SES offenders break on *other* intrinsics (`Array`, `Error`, `Function`, `Map`), on the compartment sandbox, or on determinism tamings — none of which an `Object.prototype`-only freeze touches. The dominant polyfill population (core-js, es-shims, babel-polyfill) writes to `Array.prototype`/`String.prototype`, never `Object.prototype`, since an enumerable `Object.prototype` property breaks every `for…in`. Nub's own empirical spike confirmed it: `Object.prototype`-only passes express + core-js + Zod while blocking both named pollution vectors.

5. **The "override mistake" is the one class that DOES hit an `Object.prototype`-only freeze, and it is mandatory to repair.** TC39 measured it triggering on ~10% of strict-mode / ~20% of sloppy-mode codebases. A naive `Object.freeze(Object.prototype)` breaks the ordinary `obj.toString = …` / `obj.constructor = …` shadowing idiom that transpiled CJS emits. Both Node's `--frozen-intrinsics` and SES defeat it identically — by converting frozen prototype data-properties into accessor pairs whose setter re-defines the property on the instance. Nub must do the same; it is not optional even for the narrow freeze.

6. **Every prior deployment converged on the same compat playbook, and it maps cleanly onto Nub:** graduated/opt-in rollout (never a global flip); taming-aware freeze (never naive); diagnostic-first attribution with an escape hatch (Deno's record-first-access + crash-hint + `--unsafe-proto` *is* Nub's warn mode); and "load polyfills before the freeze" as a hard invariant. The one cautionary tale (Salesforce Locker) argues *against* freeze-everything and *for* a narrow, targeted curation.

---

## 1. The mechanism landscape

Ordered least → most aggressive. Nub's tiers map onto this.

- **`--disable-proto=delete|throw`** (Node, **stable**). Removes/traps the `Object.prototype.__proto__` accessor. Closes the `__proto__`-via-merge/JSON vector. Does **not** close `constructor.prototype` pollution or `Object.setPrototypeOf`. Deno ships `delete` **by default** ([denoland/deno#4341](https://github.com/denoland/deno/pull/4341)), the ecosystem-scale proof it's near-zero-breakage. Object-literal `{__proto__:}` syntax is a separate grammar production, unaffected — only `obj.__proto__` property access changes.

- **Freeze `Object.prototype` only (with override repair)** — Nub's proposed default. Both named pollution vectors (`{}.__proto__` and `{}.constructor.prototype`) terminate at `Object.prototype`, so freezing that one object closes both. Empirically (Nub spike, Node 20 + 26): passes express, core-js, Zod; blocks both vectors. Leaves an `Array.prototype` pollution gap (see §5).

- **Freeze the whole intrinsic graph + override taming** = SES `lockdown()` / Node `--frozen-intrinsics`. Closes the `Array.prototype` gap too, but breaks the polyfill population (core-js writes ESNext-proposal methods to `Array.prototype` unconditionally). Nub's "strict" tier.

- **Full OCap lockdown** (deterministic `Date`/`Math`, censored globals) — **out of scope**; user-hostile, unrelated to pollution defense. Determinism is a separate potential Nub feature, deliberately parked.

**Implementation choice:** stock SES cannot express the `Object.prototype`-only curation — it freezes the whole primordial graph and breaks core-js on `WeakMap.prototype.get` even at maximum permissiveness. So Nub **hand-rolls a lean freeze preload** (no `ses` dependency), reusing SES's *technique* (accessor conversion for the override mistake) but not its scope. Node's `--frozen-intrinsics` is a reference/possible-max-tier only: it's been Stability-1 Experimental for 7 years, is all-or-nothing, freezes `Error` (breaking `Error.prepareStackTrace` mutators like depd), and does not guarantee `globalThis.Array` reference identity.

---

## 2. The override mistake — the central compat landmine

A naive `Object.freeze(Object.prototype)` breaks legitimate code because ES5 made an inherited non-writable data property **un-overridable by assignment**.

After the freeze, `obj.toString = fn` (a *shadowing assignment* on a descendant) throws in strict mode / silently no-ops in sloppy mode, instead of creating an own property. Mark Miller (SES co-author, TC39): *"my single biggest failure and disappointment as a member of TC39"* ([esdiscuss](https://esdiscuss.org/topic/object-freeze-object-prototype-vs-reality)).

- **Scope:** TC39 measured it triggering on **~10% of strict-mode / ~20% of sloppy-mode** codebases ([tc39/proposal-symbol-proto](https://github.com/tc39/proposal-symbol-proto)). bmeck: initial tests showed ~15% of websites hit the usage counters ([PR #25685](https://github.com/nodejs/node/pull/25685)). This is the hard floor on *naive*-freeze breakage, and the reason the repair is mandatory.
- **Why it can't be fixed in the language:** TC39 backed out of a language-wide fix after hitting an accidental web dependency — `regenerator-runtime` <0.13.8 on live sites (theathletic.com) does `Gp.constructor = GeneratorFunctionPrototype`, depending on the mistake ([ecma262#1320](https://github.com/tc39/ecma262/issues/1320), [proposal-iterator-helpers#286](https://github.com/tc39/proposal-iterator-helpers/issues/286)).
- **The fix everyone uses:** convert frozen prototype data-props to get/set accessor pairs whose setter re-defines the property on the instance (SES `overrideTaming`; Node [PR #28254](https://github.com/nodejs/node/pull/28254), same technique). SES reports the enablement list "rapidly converged — we rarely come across any more such cases"; the `'severe'` setting exists specifically for rollup/webpack-generated `exports`-by-assignment code.
- **Applies to `Object.prototype`-only too:** `obj.hasOwnProperty = …` / `obj.toString = …` on a plain object are exactly `Object.prototype`-inherited-property assignments. So **Nub needs the accessor repair even for the narrow freeze** — confirmed by both the SES corpus and Nub's spike.

**Active TC39 work (both Stage 1, neither shipped):** [`proposal-stabilize`](https://github.com/tc39/proposal-stabilize) (Miller et al.) adds integrity traits (`overridable`, `fixed`, `non-trapping`, `stable`) — the language-level version of exactly what Nub wants, letting you freeze primordials *without* the override mistake. [`proposal-symbol-proto`](https://github.com/tc39/proposal-symbol-proto) (Google) deletes `__proto__`/`constructor` *string* access under an opt-in mode; notes only ~0.89% of websites dynamically access `__proto__`/`constructor`.

---

## 3. Ecosystem breakage — the empirical picture

Two bodies of evidence bear on breakage: the SES lockdown corpus, which is the only real-world record at npm scale, and the academic prevalence work, which measures the attack surface and never the breakage surface.

### 3.1 The SES / LavaMoat / MetaMask corpus (the richest real-world data)

SES `lockdown()` freezes the entire intrinsic graph and is run **in production across whole npm dependency trees** by MetaMask (browser extension, years) and Agoric (smart contracts).

Their own characterization ([ses README](https://github.com/endojs/endo/blob/master/packages/ses/README.md)): *"Most ordinary JavaScript can run without issues in a realm locked down by SES,"* and failures *"almost always take the form of assignments that fail because of the override mistake."*

The curated "what breaks" catalog lives in the [Endo wiki Compatibility notes](https://github.com/endojs/endo/wiki) and [endojs/endo#576](https://github.com/endojs/endo/issues/576) (opened by Mark Miller). The **entire multi-year cumulative catalog is ~30 packages** — for full-graph lockdown across all of npm. It falls into four mechanistic classes, and **only one of them is triggered by an `Object.prototype`-only freeze:**

| Class | Examples | Hits `Object.prototype`-only freeze? |
|---|---|---|
| **Override mistake** (assignment to inherited proto prop) | `regenerator-runtime`→`Object.prototype.constructor`; `@formatjs`→`exports.hasOwnProperty`; rollup/webpack `exports`-by-assignment; `tape` | **Only when the prop is inherited from `Object.prototype`** (`constructor`, `hasOwnProperty`, `toString`, `valueOf`). `immer`→`Map.set`, `web3`→`Function.call`, `luxon`→Date-ish do **not**. |
| **Intrinsic mutation on OTHER intrinsics** | `@wry/context`→`Array`; `define-properties`→`Object` *constructor*; core-js→`Array`/`String`; `error-polyfill`→`Error` | **No** — different intrinsics. |
| **`Error.prepareStackTrace` mutation** | `depd`(→express/morgan), `better-assert`, `node-lmdb` | **No** — `Error` ≠ `Object.prototype`. |
| **Compartment-only** (missing globals, node builtins/`process.env`/`Buffer` at import, `Math.random` determinism) | `jsesc`, `babel`, `temp` | **No** — Nub freezes one realm with normal globals; it is not a compartment sandbox. |

SES's full-lockdown breakage is therefore a strict, loose upper bound on Nub's curated freeze. The residue that would actually hit Nub is the narrow set of `Object.prototype`-inherited-property assignments, dominated by transpiler-generated (old babel/tsc/regenerator) `exports.hasOwnProperty` / `constructor`-by-assignment output — exactly the class the accessor repair defeats.

**MetaMask** is the production existence-proof, with SES lockdown shipped across the whole dep tree for years. Its most detailed public breakage record is the React-Native rollout, where the blockers were **engine** problems (Hermes lacks the `with` statement SES needs), not packages; only single digits of named deps needed "vetted shims" ([MetaMask Security Monthly Aug 2023](https://metamask.io/news/security/metamask-security-monthly-august-2023)). No public quantitative retrospective exists.

**Reusable prior art:** SES's `overrideDebug` + `LOCKDOWN_OVERRIDE_TAMING` diagnostic (= Nub's warn/attribution mode); the "load polyfills before freeze" invariant (node-lmdb, reflect-metadata, core-js, regenerator all fixed this way); the "vetted shims" staging (repair → apply reviewed shims → freeze). No standalone "does this package survive lockdown" linter exists.

### 3.2 Empirical prevalence — measured vs. gap

The **attack** side is well-measured; the **breakage** side — native-prototype-extension prevalence, the population that breaks under a freeze — has **no published measurement**.

- Attack-side scale: Silent Spring (11 universal gadgets in Node core, 8 RCEs incl. npm CLI / Parse Server / Rocket.Chat — [USENIX '23](https://www.usenix.org/conference/usenixsecurity23/presentation/shcherbakov)); GHunter (56 Node + 67 Deno gadgets, test-suite-driven — [USENIX '24](https://www.usenix.org/conference/usenixsecurity24/presentation/cornelissen)); Dasty (gadgets in 1,269 of 1,856 analyzed packages, 49 PoCs — [The Web Conf '24](https://openreview.net/pdf?id=OO1T2D6cYA)); ProbeTheProto (2,917 web zero-days across 1M sites — [NDSS '22](https://www.ndss-symposium.org/ndss-paper/auto-draft-207/)). **None quantifies freeze breakage or coverage.**
- The structural fact that makes `Object.prototype`-only safe: near-all native-prototype extension in npm targets `Array.prototype`/`String.prototype`/`Promise`/collections, **not** `Object.prototype`, because an enumerable `Object.prototype` property breaks every `for…in`. **SmooshGate** — MooTools's enumerable `Array.prototype.flatten` forcing TC39 to rename the native method to `flat` ([Chrome dev blog](https://developer.chrome.com/blog/smooshgate)) — is the scale-proof that native-prototype extension is common enough to break an ecosystem, but it was `Array.prototype`, browser-side, enumerability-driven: a cautionary tale about *Array/String* freezing, not `Object.prototype`.

---

## 4. Prior-art deployments

Every lockdown that reached ecosystem scale either narrowed what it froze or gated the rollout by version, and Node itself treats prototype pollution as outside its threat model.

- **Node primordials** (`lib/internal/per_context/primordials.js`): captured intrinsics so core stays correct under userland mutation. Protects *core from breaking*, **not apps from being pollutable** — precisely the read-side approach Collina's article says can't work. It **stalled under a measured perf controversy** ([node#29766](https://github.com/nodejs/node/issues/29766)): V8 deopts from calling through captured references (`ArrayPrototypeMap(x,f)` vs `x.map(f)`) forced reverts in hot paths; the contributing guide now *forbids* primordials in `http`/`http2`/`tls`/`zlib` ([primordials.md](https://github.com/nodejs/node/blob/main/doc/contributing/primordials.md)). A 2023 TSC proposal to remove them wholesale ([TSC#1438](https://github.com/nodejs/TSC/issues/1438)) ended in a compromise: keep only in error paths + web-standards spots, stop adding elsewhere. *Perf caveat for Nub:* this is about call-indirection cost, **not** the cost of a one-time `Object.prototype` freeze — weak evidence about freeze cost, so Nub must measure its own.
- **Node's official posture:** prototype pollution is **not** a vulnerability — *"Node.js trusts the inputs provided to it by application code"* ([SECURITY.md](https://github.com/nodejs/node/blob/main/SECURITY.md), CWE-1321), and core prototype pollution is explicitly excluded from the bug bounty. Nub filling this gap is additive, not redundant with Node.
- **Salesforce Locker Service** — the largest "freeze intrinsics under a huge third-party ecosystem" deployment ever (SES-derived, ~5M developers). It froze intrinsics **and wrapped objects** in `Secure*` proxies; it broke d3/jQuery/Chart.js/FullCalendar, forbade sloppy mode + `eval`, and was rolled out by **API-version gating** (≥40.0 — graduated, legacy code bypasses). It was compat-painful enough that its successor **Lightning Web Security abandoned wrapping for lighter selective "distortions"** because "most third-party libraries work as expected without changes" under LWS ([LWS vs Locker](https://developer.salesforce.com/docs/platform/lwc/guide/security-lwsec-locker-comparison.html)). **Lesson: freeze-everything + wrapping is too compat-hostile; targeted/selective wins** — an argument for the narrow curated freeze over the strict tier as the default.
- **Deno** — deletes `Object.prototype.__proto__` **by default** (delete, not throw). A later attempt to make access *throw* broke Playwright/pnpm/Next and was reverted; Deno settled on silent-delete + an accessor that records first access + a crash-time hint to re-run with `--unsafe-proto` ([PR #35192](https://github.com/denoland/deno/pull/35192)). **This is Nub's warn-mode blueprint, proven at ecosystem scale.**
- **Cloudflare Workers** — does **not** freeze intrinsics (memory isolation only; frozen `Date.now()` for Spectre). Intra-isolate prototype pollution is unaddressed — runtime intrinsic freezing is novel territory among mainstream serverless runtimes.
- **Moddable XS** — native Hardened JS (`lockdown`/`Compartment` in-engine), but embedded/IoT — weak transfer.
- **Bun** — found vulnerable to prototype pollution; exposes Node-compatible `--frozen-intrinsics` but takes no hardening-by-default stance.

---

## 5. Nub's own empirical work (calibration + spike)

Raw Node flags were calibrated against a 10-library basket, then curated freezes were hand-rolled: freezing `Object.prototype` alone is the only configuration that blocked both vectors without breaking express, core-js or Zod.

- **Calibration** (Node 26.5.0, 10-lib basket, raw node flags): `--disable-proto` blocks only `__proto__`, not `constructor.prototype`; `--frozen-intrinsics` blocks both but breaks 2/10 — core-js (writes `Function.prototype.toString`) and express (via depd's `Error.prepareStackTrace`, a **silent** sloppy-mode no-op).
- **Spike** (Node 20 + 26, hand-rolled curated freezes vs SES): **freezing `Object.prototype` ONLY (with override repair) passes express + core-js + Zod AND blocks both named pollution vectors.** Freezing `Array.prototype` too is the single thing that breaks core-js — it writes ESNext-proposal methods like `filterOut` to `Array.prototype` unconditionally, because Node doesn't ship the proposals and its self-skip only covers stable natives. Stock SES can't express the curation (breaks core-js on `WeakMap.prototype.get`). The override repair does **not** help core-js, which writes via `Object.defineProperty` and so throws on a non-extensible object regardless of the repair; the only lever for core-js is leaving the prototype extensible.
- **The one real tradeoff:** `Object.prototype`-only leaves an `Array.prototype` pollution hole (`_.merge([], {"__proto__":{…}})` succeeds). The dominant, universal vector (`Object.prototype` — essentially all documented Silent Spring / GHunter gadget chains) is closed; `Array.prototype` gadgets (narrower, arrays-only) are not. Closing them is exactly what reintroduces the core-js breakage, so it belongs in the opt-in `strict` tier.

---

## 6. Design implications

The prior art validates the hardening plan and sharpens it:

1. **Tiers:** default = `Object.prototype`-only curated freeze (override-repaired); `proto` = `--disable-proto` equivalent (partial, zero-breakage); `strict` = also freeze `Array.prototype` (closes the gap, breaks core-js → auto-disable-to-default + warn); `warn` = non-blocking detect + attribute; `false` = off.
2. **Hand-roll the freeze** (no `ses` dep) — but reuse SES's accessor-conversion technique for the override mistake. **The repair is mandatory even for the `Object.prototype`-only tier.**
3. **Load Nub's own polyfills (and any pre-freeze allowlist) BEFORE the freeze** — a hard invariant, proven by every deployment. Nub's preload freeze-point gives this for free; the freeze is the terminal preload step.
4. **Diagnostic-first UX:** the runtime error-rewrite (attribute the frozen-write throw to the offending package plus the exact `nub.jsonc` fix) is the load-bearing mechanism, modeled on Deno's record-first-access + crash-hint + escape-hatch. Static `no-extend-native`-style pre-flight (already in oxc/oxlint) is a secondary early-warning. Note the sloppy-mode silent-no-op gap: a throwing accessor setter (SES-style) converts even sloppy writes to loud attributable throws — another reason to use accessor-based freezing over raw `Object.freeze`.
5. **Default posture:** freeze-by-default is viable for the `Object.prototype`-only tier (near-zero breakage, confirmed) but it *does* redefine Nub's "byte-for-byte" promise to "byte-for-byte, with secure hardening you can opt out of" — a product posture call. Graduated rollout (Salesforce's API-version model → Nub's per-project opt-in) is the field-proven path.
6. **Honesty boundary for copy:** the default blocks **`Object.prototype` pollution** (the universal gadget), not all prototype pollution (the `Array.prototype` gap). Never claim "all prototype pollution" on the default tier.

---

## 7. The measurement to run

No quantitative npm-wide freeze-breakage study exists anywhere. Nub can produce the first one, and it doubles as the "which deps are safe" compat list. Methodology:

- **Dynamic (gold standard):** run the **test suites** of the top-N most-depended npm packages twice — vanilla, and under Nub's `Object.prototype`-only freeze **with override taming applied** — and diff pass/fail, attributing each failure to the throwing frame. This reuses the exact test-suite-driven harness GHunter/Bullseye already built. Pitfalls to state honestly: test-suite coverage gaps (a non-exercised monkeypatch path won't show — LavaMoat's own caveat); devDep noise (classify by where the throw originates); measure *with* taming or the count is massively inflated.
- **Static (whole-registry denominator):** a `no-extend-native`-style AST scan (already in oxc/oxlint) over an npm mirror, bucketed by which prototype is written — the `Object.prototype`-vs-`Array.prototype` split is the exact number needed. Under-counts (aliasing, computed keys, minified code), so it is a lower bound.
- **Cheapest good-enough:** static denominator over the registry plus dynamic on the top ~100–300 packages.

---

## Open questions / gaps

Three quantities are unmeasured: freeze cost on Nub's V8, how often real pollution chains run through `Array.prototype`, and the npm-wide breakage rate.

- **Freeze performance on Nub's V8 is unmeasured.** The historical V8 frozen-prototype deopt was fixed in Chrome 62 (2017); the Node primordials perf controversy is about call-indirection, not freeze cost. No fresh benchmark of a one-time `Object.prototype` freeze exists — Nub must measure it on its own target.
- **How common are `Array.prototype` gadgets** (vs `Object.prototype`) in real pollution chains? Determines how much the default tier's `Array.prototype` gap matters. Unmeasured; most documented chains are `Object.prototype`.
- **The npm-wide breakage rate itself** — the §7 measurement is the way to close this.

## Changelog

Each entry dates a revision and records the evidence or premise correction it carried in.

- 2026-07-22 — Initial write-up. Synthesizes a four-prong prior-art sweep (Node core primordials/frozen-intrinsics
  history; SES/LavaMoat/MetaMask/Agoric compat corpus; Salesforce/Deno/Cloudflare/XS/TC39 platform lockdowns; academic
  + empirical prevalence + methodology) plus nub's own calibration + `Object.prototype`-only spike. Two premise
  corrections carried in from the Node-core prong: `--frozen-intrinsics` **freezes** `Error` (it is not excluded;
  console needed the separate [#27663](https://github.com/nodejs/node/pull/27663) unfreeze), and
  [#38211](https://github.com/nodejs/node/pull/38211) adds `globalThis` to *primordials* — it is not "the globalThis
  freeze"; the `globalThis.Array` reference-identity caveat is a docs statement in cli.md.
