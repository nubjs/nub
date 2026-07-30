---
**Status:** v1, 2026-05-18. Write-once research doc.
**Question:** Why did Node.js pick SWC (via `@swc/wasm-typescript`,
wrapped as `amaro`) rather than Oxc for `--experimental-strip-types`?
Does anything in that decision trail change Nub's commitment to
oxc-based transpilation?
**Headline answer:** No. Node's choice was driven by build/toolchain
constraints (no Rust in the Node build), an existing maintainer
relationship with SWC's author, and timing (Oxc transformer wasn't
production-ready in mid-2024). Oxc was effectively not on Node's
list of candidates — no public comparison was performed. For Nub,
which links Rust crates directly and ships a Rust binary, every
constraint that pushed Node to SWC-via-wasm points the other way.
**Builds on:** `../runtime/ts-transpilation.md`,
`../runtime/jsx-transpilation.md`,
`../runtime/non-erasable-syntax.md`,
[`node-strip-types-interaction.md`](node-strip-types-interaction.md).
---

# Why Node picked SWC (not Oxc) for `--strip-types`

## 1. TL;DR

- **Node didn't pick SWC over Oxc — Oxc wasn't really considered.** The PR that introduced strip-types (nodejs/node#53725, July 2024) was Marco Ippolito's individual choice. His stated rationale in the PR body and in linked discussion was "simplicity": `@swc/wasm-typescript` ships as wasm+JS glue, so Node's build doesn't have to grow a Rust toolchain. The only alternatives mentioned in the loaders WG discussion were SWC itself (chosen), `typescript-go` (rejected later, by design — no blank-spacing mode), `tsc` (rejected — slow, no semver), and ts-blank-space (which inspired SWC's blank-space codegen mode, didn't replace it).
- **Oxc was raised exactly once in public, by an external commenter, and was not engaged with by maintainers.** thernstig in nodejs/loaders#217 asked "would it not be better to use oxc.rs?" (Oct 2024). No response. By that point amaro was already shipped and the discussion had moved on.
- **The decision was timing-bound.** Oxc transformer hit alpha 2024-09-29 (after strip-types landed in 22.6 on 2024-08-06). Oxc's ts-blank-space-inspired stripping path that's now 4x faster than `swc_fast_ts_strip` only landed early 2025 (Evan You tweet 2025-02-14). When Marco was choosing in May-July 2024, the honest production options for fast Rust-backed TS stripping were SWC and esbuild (Go). Oxc wasn't competitive yet.
- **The wasm-vs-native build constraint is load-bearing for Node and irrelevant for Nub.** Node distributes a single C++ binary built by a notoriously conservative GYP-based build system that doesn't include Rust. Pulling Rust into the Node build would have been a multi-month infra effort with TSC-level approval costs. Pulling in a pre-compiled wasm blob through V8 cost ~nothing. Nub is a Rust binary that links Rust crates directly — none of this gating applies to us.
- **Implication for Nub: zero pressure to switch.** Node's choice doesn't validate SWC over Oxc on technical merits — it validates SWC-as-wasm over Rust-in-Node's-build. The Oxc choice for Nub stands. The one secondary observation worth flagging is amaro's emerging role as the de-facto "TS stripper" reference, which matters for source-map conventions and error-message shapes (see §5).

## 2. The decision trail (chronological)

### 2024-05 — Marco joins TSC; type stripping prototyping begins

Marco Ippolito (then transitioning from NearForm to HeroDevs) becomes a Node.js TSC member and starts prototyping native TS support. His public blog ("The Summer I Shipped Type Stripping," [satanacchio.hashnode.dev](https://satanacchio.hashnode.dev/the-summer-i-shipped-type-stripping)) describes the choice path: Ashley Claymore suggests the ts-blank-space approach (Bloomberg's "replace types with whitespace" trick for source-map-free stripping). Marco discovers `@swc/wasm-typescript` on GitHub — it has "basically zero downloads at the time."

### 2024-07-02 — loaders WG issue #208 opened, SWC named upfront

[nodejs/loaders#208](https://github.com/nodejs/loaders/issues/208) ("Support typescript with --experimental-strip-types") is Marco's tracking issue. The opening message names SWC as the implementation choice from the start:

> The main idea is to use type-stripping. This is achieved ideally through the use of an external dependency, in my opinion `@swc/wasm-typescript`.

Matteo Collina (mcollina) raises non-blocking concerns:

> 1. Is swc on board with following our LTS? This seems a _massive_ dependency we should not take on lightly. 2. TypeScript does not follow semver, and the language evolves relatively quickly. Is this approach stable long term?

Marco's reply addresses LTS and licensing only — no transpiler comparison was performed:

> SWC seems to be a mature project, it is already used by Deno for our same purpose, has Apache-2 license which is fine. I'm using @swc/wasm, the wasm version to avoid compiling rust.

The SWC maintainer Donny/강동윤 (`@kdy1`) joins the thread same day:

> I can configure a publish pipeline for a separate package/binary, or a Wasm publish pipeline if node.js will use SWC for this task.

Day later (2024-07-03) he creates `@swc/wasm-typescript` and ships documentation specifically for Node's use case. This is a load-bearing piece of project dynamics: the SWC team built bespoke infrastructure for Node. No equivalent outreach from the Oxc side is in public record from this period.

### 2024-07-04 — PR #53725 opens

[nodejs/node#53725](https://github.com/nodejs/node/pull/53725) ("module: add --experimental-strip-types") is opened by Marco. The PR body has a section explicitly titled **"Why I chose @swc/wasm-typescript"** — the only public rationale document:

> Because of *simplicity*. I have considered other tools but they require either rust or go to be added to the toolchain. `@swc/wasm-typescript` is a small package with a wasm and a js file to bind it. Swc is currently used by Deno for the same purpose, it's battle tested. In the future I see this being implemented in **native layer**. Massive shoutout to @kdy1 for releasing a swc version for us.

Note: "other tools" is unnamed. esbuild (Go) and the official `tsc` (TypeScript) are the obvious referents. Oxc isn't called out either as considered or as rejected. The de facto comparison set was {SWC, esbuild, tsc} — Oxc wasn't on the radar.

Comments on the PR over the next two weeks focus on:
- Source maps (Geoffrey Booth, Matteo) → resolved by adopting ts-blank-space's whitespace approach.
- `node_modules` handling (ljharb, ChALkeR) → resolved by reject-in-node_modules.
- Scope of features (TS team, Daniel Rosenwasser, Jake Bailey) → resolved by "type stripping only, no transforms" for the initial ship.

The TypeScript team (Daniel Rosenwasser, Jake Bailey, Ryan Cavanaugh, Aaron Frost) participates from issue #208 and the TSC meeting (see below) — but as scope advisors, not transpiler advisors. None of them weighs in on SWC vs alternatives.

### 2024-07-17 — TSC meeting

[nodejs/TSC meetings/2024-07-17.md](https://github.com/nodejs/TSC/blob/main/meetings/2024-07-17.md). Strip-types is discussed at length. The conversation is about **whether** to ship, **how** scoped, **what** to communicate to users — not transpiler choice. Transpiler is taken as settled:

> Marco: Currently blocked as swc having trouble with some architectures so still some time before it would land anyway. Feedback from TypeScript team is that there are some concerns but they can be solved. Created package to wrap swc which would go under the Node.js organization.

The "created package to wrap swc" is amaro. SWC is the only candidate discussed.

### 2024-07-24 — TSC meeting with TypeScript team

[nodejs/TSC meetings/2024-07-24.md](https://github.com/nodejs/TSC/blob/main/meetings/2024-07-24.md). The TS team is invited and presents concerns about:
- TS-files-in-`node_modules` (Rosenwasser: "don't think it's a good idea to publish runnable ts in modules")
- Monorepo support (Ryanca: "how to support monorepo without running ts files in node_modules")
- Type-checking-at-runtime (Bailey: "the answer to 'do we want type checking at runtime' is 'no'")

The transpiler choice is not raised in this meeting. SWC is treated as given.

### 2024-07-24 — PR merged

Marco's PR merges. The implementation pulls `@swc/wasm-typescript` in via amaro, which is a brand-new package under the nodejs org that wraps the wasm blob with a stable API surface. (Marco's blog explains the wrapping was specifically motivated by the TS team's concern about tight coupling between Node and SWC version evolution.)

### 2024-08-06 — strip-types ships in Node 22.6 (flagged)

[Node 22.6.0 release notes](https://nodejs.org/en/blog/release/v22.6.0). `--experimental-strip-types` available; default off.

### 2024-09-29 — Oxc transformer alpha

[oxc.rs/blog/2024-09-29-transformer-alpha](https://oxc.rs/blog/2024-09-29-transformer-alpha). Oxc transformer hits alpha — first time it's pitched as a usable transformer in public. Benchmarks vs SWC: 3-5x faster, 20% less memory, 2 MB vs 37 MB package size. **This is two months after Node's strip-types landed.** Oxc was not a viable production candidate when the Node decision was made.

### 2024-10-28 — Oxc raised, ignored

[nodejs/loaders#217 comment 2433489253](https://github.com/nodejs/loaders/issues/217#issuecomment-2433489253):

> thernstig: I am unsure if this comment is fitting here, but I see mentions of swc in the original post. Would it not be better to use https://oxc.rs/ and its parser as it is 3x faster than swc?

No reply. By this point amaro is in stable maintenance and the loaders WG discussion has moved on to scope and monorepo concerns. Nobody from the TSC engages.

### 2024-12-26 — strip-types unflagged in 23.6

[nodejs/node#56350](https://github.com/nodejs/node/pull/56350) unflags `--experimental-strip-types` for Node 23.6. The PR has zero discussion of transpiler choice. The choice is locked in.

### 2025-02-14 — Oxc ships ts-blank-space-inspired stripping

[Evan You tweet](https://x.com/youyuxi/status/1890701933767246117):

> Cool stuff: ts-blank-space inspired TypeScript type stripping implemented on OXC - strip TS types at the speed of parsing, 4x faster than swc_fast_ts_strip which is used in Node.js /cc @robpalmer2 @satanacchio

This is the moment when, if Node were re-deciding from scratch, the performance argument flips. **It does not flip retroactively.** No Node TSC discussion of switching has occurred.

### 2025-03-11 — typescript-go briefly considered, rejected

[nodejs/amaro#200](https://github.com/nodejs/amaro/issues/200). TypeScript announces the Go port of `tsc`. Question raised: should amaro switch to typescript-go? Marco's response:

> No blank spacing so its not possible until that is supported. I don't think its worth trying.

And later (2025-05-26):

> We discussed about this with the team and it's not possible by design, we should keep using SWC for the foreseeable future.

This is the only public statement of "we considered alternative X and stayed with SWC." Oxc is not mentioned in that thread either.

### 2025-09 — amaro 1.0 ships

[amaro 1.0 release](https://github.com/nodejs/amaro/releases). Stable API. Adds `transform-types` mode (handles enums, etc.) in addition to default `strip-types`. SWC remains the engine.

### 2026 — strip-types stable, flag renamed

[nodejs/node#60600](https://github.com/nodejs/node/pull/60600). `--experimental-strip-types` → `--strip-types`, stable in 24.12 LTS and 25.2. SWC is now baked-in as Node's reference TS stripper.

## 3. Technical comparison (what was weighed, what wasn't)

### What Node actually weighed

From the public record, Marco's decision criteria reduce to:

1. **Build-toolchain friction.** Node's build is C++ via GYP. Adding a Rust dep to that build is a project-level decision that touches CI infrastructure, reproducibility, supply-chain provenance, and a hundred other downstream concerns. Pulling in a pre-built wasm + JS glue is trivial — V8 already runs wasm, no new build step needed. **This was the dominant factor.** It's explicit in the PR body: "other tools require either rust or go to be added to the toolchain."

2. **Maintainer relationship.** `@kdy1` (강동윤, SWC's primary author) committed to building `@swc/wasm-typescript` as a Node-specific wasm distribution within days of being asked. He also committed to following Node's LTS cadence and to maintaining a separate publish pipeline. This is a real long-term cost for the SWC team and the offer materially de-risked the choice.

3. **Prior art.** Deno had been using SWC for a similar role for years. The PR body cites this explicitly. "Battle-tested" is the appeal.

4. **License.** SWC is Apache-2.0, which is compatible with Node's MIT. Oxc is MIT (would have been equivalent or marginally simpler). Not a differentiator.

5. **ts-blank-space approach feasibility.** The blank-space codegen technique (replace types with whitespace, no source-map generation needed) was suggested mid-PR by Ashley Claymore / Robin Palmer / Bloomberg's team. Marco asked SWC to add a `blank-space` mode (swc-project/swc#9144); `@kdy1` shipped it within days. Oxc didn't have an equivalent path at the time.

### What Node didn't weigh

Notably absent from any public discussion:

- **Performance comparison.** No benchmarks were referenced in the PR, the loaders WG issues, or the TSC meeting notes. The decision wasn't "SWC is faster" — it was "SWC is here and works."
- **Oxc.** The single instance of someone (an external commenter) raising Oxc is unanswered.
- **AST compatibility / ecosystem fit.** Both SWC and Oxc emit ESTree-compatible-ish ASTs; no discussion of whether Node's future loader hooks or addon ecosystem would benefit from alignment with one or the other.
- **Native binding option.** The "in the future I see this being implemented in **native layer**" line in the PR suggests Marco was open to a native (non-wasm) integration eventually, but no concrete plan exists in any tracking issue. The native binding question is still open per amaro#200 comment by JounQin ("Or why not use `napi-rs` as wrapper but `WASM` now?").

### Performance: what we know in 2026

| Engine | Operation | Speed (approximate, public benchmarks) |
|--------|-----------|----------------------------------------|
| `tsc` | TS strip (transpileModule) | baseline (very slow) |
| SWC wasm-typescript | TS strip | ~10x faster than tsc |
| SWC native (`@swc/core`) | TS strip | ~20x faster than tsc |
| Oxc transformer | TS strip (Feb 2025) | 4x faster than swc_fast_ts_strip |
| Oxc parser | parse-only | 3x faster than SWC parser |
| ts-blank-space (JS) | TS strip (whitespace-only) | "fastest emitter written in JavaScript" |

(Sources: pkgpulse.com benchmarks 2026, oxc.rs benchmarks, Evan You tweet, ts-blank-space README.)

Oxc is meaningfully faster than SWC for stripping today. That performance gap didn't exist in mid-2024. It does now. Node hasn't acted on it.

### Non-erasable syntax

Both SWC and Oxc transformers handle enums, namespaces, parameter properties, decorators, etc. The amaro `transform-types` mode exposes this via SWC. Oxc-transformer alpha had partial coverage in late 2024 and is more complete in 2026. For Nub's purposes (per `../runtime/non-erasable-syntax.md`), Oxc's coverage is sufficient.

Node chose to ship **only the erasable subset** by default — strip-types mode, not transform-types. That choice was made for **ecosystem stability reasons** (TS team concerns about fragmenting "valid TS"), not because of SWC API constraints. Either transpiler could have implemented either policy.

## 4. Project-level dynamics

### Maintainer relationships

The SWC ↔ Node relationship is real and load-bearing. `@kdy1` showed up on Marco's tracking issue within hours, agreed to build custom infrastructure (`@swc/wasm-typescript`), shipped requested features (blank-space mode in swc#9144) within days, and continues to maintain that pipeline. amaro's CI breaks regularly when SWC's output format shifts (amaro#329 "swc update workflow is broken"), but the relationship absorbs that.

Oxc's primary maintainer (Boshen) does not appear to have engaged with Node TSC in any public capacity. There's no equivalent "we'll build Oxc-wasm-typescript for Node" offer in any record I could find. This isn't a moral failing on Oxc's part — Oxc was a younger project in mid-2024, not yet at the alpha-transformer stage, and Node's choice was effectively over before Oxc could have plausibly competed.

### amaro's role

amaro is the actual integration surface, not raw SWC. amaro:
- Wraps `@swc/wasm-typescript` with a stable API.
- Versions independently from Node (so Node can ship amaro@1.x and amaro@1.y can be bumped in a patch release).
- Provides both `strip-types` (default) and `transform-types` modes.
- Is published under the nodejs GitHub org.
- Has a CODEOWNERS file with Marco Ippolito as primary maintainer.

This wrapper exists because the TypeScript team specifically pushed back on Node tightly coupling to SWC's release cadence. So amaro is a buffer — Node depends on amaro, amaro depends on SWC. In principle, amaro could swap engines under its API and Node wouldn't notice. In practice, this swap hasn't happened and isn't on amaro's roadmap.

### TSC commitment posture

The Node TSC's posture on TS-runtime support is "ship the minimum that solves the user complaint; defer transforms to the loader ecosystem." This is a deliberate scope-minimization stance. Picking SWC fits that posture — they get to point at amaro for the rest of the work and say "Node just ships the bare minimum stripping; do more via loaders."

A Node TSC reconsideration of transpiler choice would require:
- A concrete user-visible problem with SWC (none has materialized).
- A maintainer willing to do the swap PR (no one volunteered).
- TSC consensus that the swap is worth the churn (would require burning credibility on a non-user-facing change).

None of these conditions are present. The choice is institutionally sticky now, regardless of merit.

## 5. Implications for Nub

### Does Node's choice change Nub's commitment to Oxc?

**No.** The constraints that drove Node to SWC-via-wasm don't apply to Nub:

1. **No Rust-toolchain barrier.** Nub is a Rust binary. Linking `oxc_parser` and `oxc_transformer` crates directly is the path of least resistance, not a special accommodation.
2. **No "maintainer relationship" lock-in.** We don't need a wasm-publish pipeline. We don't need bespoke infrastructure from any transpiler team. Oxc is on crates.io; we depend on it like any other dep.
3. **No build-system gating.** No GYP, no Python build scripts, no TSC-level review for adding a dep. `cargo add oxc_parser` is the whole story.
4. **We're forward-looking on the ecosystem.** Rolldown ships on Oxc. Vite 8 ships on Rolldown. The ecosystem direction is Oxc-shaped. Aligning Nub's transpiler with the dominant future bundler stack means our diagnostics, source-map shape, and AST conventions land in the same family as the user's bundler.

### What about source-map and error-shape compatibility with amaro?

There's a secondary concern worth naming: amaro is becoming the de-facto "TypeScript runtime stripper" in the JS ecosystem. As more tooling assumes "this file was stripped by amaro," subtle differences between amaro's source-map shape and Oxc's could create user-visible inconsistencies.

Concretely:
- amaro uses **whitespace replacement** (the ts-blank-space approach) by default in `strip-types` mode. Source positions are preserved 1:1; no source map needed.
- amaro's `transform-types` mode emits source maps with SWC-shape mappings.
- Oxc's transformer emits source maps with Oxc-shape mappings, which are similar but not byte-identical.

Nub's TS pipeline (`../runtime/ts-transpilation.md`) already commits to source-map emission via Oxc. As long as our source maps are well-formed (V8 / Chrome DevTools / Node debugger all accept them), the byte-identical-ness with amaro doesn't matter for end-user experience.

The only place this could bite us is if a downstream tool reads the source map and **assumes amaro-shape mappings**. We've seen no evidence that any such tool exists. If one emerges, we can fix it at the source-map-emit stage.

### Could Oxc replace amaro inside Node someday?

Plausible but not imminent. The path would require:
- Someone (probably a Node TSC member with stake in Oxc, possibly Boshen himself or someone from the Vite team) to do the swap PR.
- A demonstration that the swap is risk-free (byte-compatible output for the strip-types subset, equivalent error messages, equivalent edge-case handling).
- TSC buy-in on rocking a load-bearing dep for performance reasons alone.

Realistic timeline: not before 2027. By which point Nub's choice will have been validated by usage regardless.

The reverse pressure — amaro getting *better* at stripping such that Nub should switch back to it — is conceivable but unlikely. Our integration is in-process Rust crates; amaro's integration is through V8's wasm boundary. We'd be giving up ~100x speed for ~0 benefit.

### One thing to keep on watch

The `nub node` compat mode passes everything through to Node, which includes Node's strip-types. If amaro starts handling cases our transpiler doesn't, or vice versa, the behavior under `nub node` will diverge from default-mode behavior. This is intentional — compat mode is supposed to be Node's behavior — but it's worth testing the divergence cases:

- Edge-case TS syntax that amaro accepts but Oxc doesn't (probably none; verify).
- Edge-case TS syntax that Oxc accepts but amaro doesn't (Oxc tends to be ahead on newer TS features; ours might accept syntax that fails under `--node`).

This is a test-surface item, not a strategic concern.

## 6. Open questions

These could not be answered from public sources:

- **Did Marco or any TSC member privately evaluate Oxc?** The PR language ("I have considered other tools") suggests some evaluation happened, but Oxc isn't named. Could have been a cursory "Rust toolchain, no good" rejection identical to esbuild's; could have been more substantive. No public artifact exists.
- **Did Vercel push for SWC?** Marco was at NearForm/HeroDevs, not Vercel; `@kdy1` was at Vercel at the time. The "SWC team built this for us" framing could be either a developer-to-developer collaboration or a Vercel-strategic move. Public record is ambiguous; ascribing motivation here would be speculation.
- **Will Node revisit?** No public indication. Marco's amaro#200 comment ("we should keep using SWC for the foreseeable future") is the most recent on-record statement. "Foreseeable future" is vague enough to leave room, but there's no scheduled revisit.
- **Could amaro switch engines under its own API?** Technically yes — amaro is a wrapper. If amaro maintainers wanted to swap to Oxc internally, Node consumers wouldn't notice (modulo source-map diffs). There's no public proposal for this.
- **How much of the SWC choice was relational vs technical?** The fact that `@kdy1` responded within hours and built bespoke infrastructure is hard to overstate. A counterfactual where the Oxc team had made the same offer at the same time would have produced a different conversation. The record doesn't let us test that counterfactual.

## 7. Bottom line for Nub

Continue with Oxc. The Node TSC decision doesn't validate SWC over Oxc on merits — it validates SWC's wasm-distribution shape and maintainer-relationship shape as fits for Node's specific build constraints. None of those constraints apply to Nub. The Oxc-first posture in `../runtime/ts-transpilation.md` stands without modification.

The one thing worth doing as a result of this research: add an "interop with amaro" smoke test to the prototype TS pipeline, verifying that a `.ts` file Nub transpiles via Oxc and a `.ts` file Node transpiles via amaro produce **functionally equivalent runtime behavior** for the erasable subset. We don't need byte-identical output; we need confidence that a user running the same file through `nub script.ts` and `nub node script.ts` gets the same answer.

## Sources

### Primary (Node decision trail)

- [nodejs/node#53725](https://github.com/nodejs/node/pull/53725) — "module: add --experimental-strip-types" (Marco Ippolito, 2024-07-04 → merged 2024-07-24). The PR body's "Why I chose @swc/wasm-typescript" section is the only first-person statement of rationale.
- [nodejs/loaders#208](https://github.com/nodejs/loaders/issues/208) — "Support typescript with --experimental-strip-types" tracking issue (Marco, 2024-07-02). Earliest naming of SWC.
- [nodejs/loaders#217](https://github.com/nodejs/loaders/issues/217) — "Roadmap for experimental TypeScript support" (Marco, 2024-07-26). Contains thernstig's unanswered Oxc question (2024-10-28).
- [nodejs/TSC meetings/2024-07-17.md](https://github.com/nodejs/TSC/blob/main/meetings/2024-07-17.md) — TSC meeting discussing strip-types landing.
- [nodejs/TSC meetings/2024-07-24.md](https://github.com/nodejs/TSC/blob/main/meetings/2024-07-24.md) — TSC meeting with TypeScript team. No transpiler discussion.
- [nodejs/node#56350](https://github.com/nodejs/node/pull/56350) — unflagging strip-types for 23.6 (2024-12-26). Zero transpiler discussion.
- [nodejs/node#60600](https://github.com/nodejs/node/pull/60600) — strip-types stable in 24.12 LTS / 25.2 (2026).
- [nodejs/amaro#200](https://github.com/nodejs/amaro/issues/200) — "Experiment with typescript-go instead of @swc/wasm-typescript" (2025-03-11). Marco's "keep using SWC for the foreseeable future" comment (2025-05-26).

### Secondary (Marco's first-person account)

- ["The Summer I Shipped Type Stripping"](https://satanacchio.hashnode.dev/the-summer-i-shipped-type-stripping) — Marco Ippolito's blog narrative of the decision.
- ["Run TypeScript Natively in Node.js"](https://gitnation.com/contents/run-typescript-natively-in-nodejs) — Marco's GitNation talk. Mentions SWC as "popular," "used by RSpack, Deno," "battle-tested." Doesn't compare alternatives.

### Tertiary (ecosystem positioning)

- [Evan You tweet, 2025-02-14](https://x.com/youyuxi/status/1890701933767246117) — Oxc TS stripping 4x faster than SWC's.
- [oxc.rs/blog/2024-09-29-transformer-alpha](https://oxc.rs/blog/2024-09-29-transformer-alpha) — Oxc transformer alpha announcement. Two months after Node's strip-types landed.
- [ts-blank-space](https://github.com/bloomberg/ts-blank-space) — Bloomberg's pure-JS whitespace-replacement type stripper. Source of the technique adopted by both SWC's blank-space mode and Oxc's TS-stripping path.
- [pkgpulse.com benchmark 2026](https://www.pkgpulse.com/blog/ts-blank-space-vs-node-strip-types-vs-swc-typescript-type-stripping-2026) — current performance comparison.
- [Socket.dev coverage of amaro 1.0](https://socket.dev/blog/node-js-moves-toward-stable-typescript-support-with-amaro-1-0) — confirmation of SWC continuing as the engine.
- [InfoQ coverage of amaro 1.0](https://www.infoq.com/news/2025/08/node-amaro-stable-ts-support/) — same.

### Non-sources (checked, found nothing)

- nodejs/TSC repo: only 3 meeting notes mention "strip-types" (2024-07-17, 2024-07-24, 2024-07-31). None mentions "oxc."
- nodejs/amaro issues: 30 issues searched, no mention of Oxc.
- nodejs/typescript issues: no mention of Oxc in issue search.
- oxc-project/oxc issues: no mention of Node's stripping choice or any TSC engagement in public threads.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links and reference-checkout paths were rewritten; findings and measured values are unchanged.
