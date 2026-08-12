---
**Status:** v1, 2026-05-18. Write-once research doc.

**Question:** Why did Node.js pick SWC (via `@swc/wasm-typescript`, wrapped as `amaro`) rather than Oxc for `--experimental-strip-types`? Does anything in that decision trail change Nub's commitment to oxc-based transpilation?

**Headline answer:** No. Node's choice was driven by build/toolchain constraints (no Rust in the Node build), an existing maintainer relationship with SWC's author, and timing — the Oxc transformer wasn't production-ready in mid-2024. Oxc was effectively not on Node's list of candidates, and no public comparison was performed. For Nub, which links Rust crates directly and ships a Rust binary, every constraint that pushed Node to SWC-via-wasm points the other way.

**Builds on:** [`node-strip-types-interaction.md`](node-strip-types-interaction.md).
---

# Why Node picked SWC (not Oxc) for `--strip-types`

## 1. TL;DR

- **Node didn't pick SWC over Oxc — Oxc wasn't really considered.** The strip-types PR (nodejs/node#53725, July 2024) was Marco Ippolito's individual choice, and his stated rationale was "simplicity": `@swc/wasm-typescript` ships as wasm+JS glue, so Node's build doesn't grow a Rust toolchain. The only alternatives named in the loaders WG discussion were SWC itself (chosen), `typescript-go` (rejected later, by design — no blank-spacing mode), `tsc` (rejected — slow, no semver), and ts-blank-space (which inspired SWC's blank-space codegen mode rather than replacing it).
- **Oxc was raised exactly once in public, by an external commenter, and no maintainer engaged.** A comment on nodejs/loaders#217 asked "would it not be better to use oxc.rs?" (Oct 2024). No response — amaro had shipped and the discussion had moved on.
- **The decision was timing-bound.** The Oxc transformer hit alpha 2024-09-29, after strip-types landed in 22.6 on 2024-08-06. Oxc's ts-blank-space-inspired stripping path, now 4x faster than `swc_fast_ts_strip`, landed early 2025 (Evan You tweet 2025-02-14). During the May–July 2024 decision the production options for fast Rust-backed TS stripping were SWC and esbuild (Go).
- **The wasm-vs-native build constraint is load-bearing for Node and irrelevant for Nub.** Node distributes a single C++ binary built by a conservative GYP-based build system with no Rust in it. Pulling Rust into that build would have been a multi-month infra effort with TSC-level approval costs; a pre-compiled wasm blob through V8 cost ~nothing. Nub is a Rust binary that links Rust crates directly, so none of this gating applies.
- **Implication for Nub: zero pressure to switch.** Node's choice doesn't validate SWC over Oxc on technical merits — it validates SWC-as-wasm over Rust-in-Node's-build. The secondary observation is amaro's emerging role as the de-facto "TS stripper" reference, which matters for source-map conventions and error-message shapes (§5).

## 2. The decision trail (chronological)

### 2024-05 — Marco joins TSC; type stripping prototyping begins

Marco Ippolito (then transitioning from NearForm to HeroDevs) becomes a Node.js TSC member and starts prototyping native TS support. His blog ("The Summer I Shipped Type Stripping," [satanacchio.hashnode.dev](https://satanacchio.hashnode.dev/the-summer-i-shipped-type-stripping)) records the path: Ashley Claymore suggests the ts-blank-space approach (Bloomberg's "replace types with whitespace" trick for source-map-free stripping), and Marco finds `@swc/wasm-typescript` on GitHub — it has "basically zero downloads at the time."

### 2024-07-02 — loaders WG issue #208 opened, SWC named upfront

[nodejs/loaders#208](https://github.com/nodejs/loaders/issues/208) ("Support typescript with --experimental-strip-types") is Marco's tracking issue; its opening message names SWC from the start:

> The main idea is to use type-stripping. This is achieved ideally through the use of an external dependency, in my opinion `@swc/wasm-typescript`.

Matteo Collina (mcollina) raises non-blocking concerns:

> 1. Is swc on board with following our LTS? This seems a _massive_ dependency we should not take on lightly. 2. TypeScript does not follow semver, and the language evolves relatively quickly. Is this approach stable long term?

Marco's reply addresses LTS and licensing only — no transpiler comparison was performed:

> SWC seems to be a mature project, it is already used by Deno for our same purpose, has Apache-2 license which is fine. I'm using @swc/wasm, the wasm version to avoid compiling rust.

The SWC maintainer Donny/강동윤 (`@kdy1`) joins the thread same day:

> I can configure a publish pipeline for a separate package/binary, or a Wasm publish pipeline if node.js will use SWC for this task.

A day later (2024-07-03) he creates `@swc/wasm-typescript` with documentation for Node's use case — bespoke infrastructure built by the SWC team for Node. No equivalent outreach from the Oxc side is in the public record from this period.

### 2024-07-04 — PR #53725 opens

Marco opens [nodejs/node#53725](https://github.com/nodejs/node/pull/53725) ("module: add --experimental-strip-types"). Its body has a section titled **"Why I chose @swc/wasm-typescript"** — the only public rationale document:

> Because of *simplicity*. I have considered other tools but they require either rust or go to be added to the toolchain. `@swc/wasm-typescript` is a small package with a wasm and a js file to bind it. Swc is currently used by Deno for the same purpose, it's battle tested. In the future I see this being implemented in **native layer**. Massive shoutout to @kdy1 for releasing a swc version for us.

The unnamed "other tools" are esbuild (Go) and the official `tsc`. Oxc appears neither as considered nor as rejected, so the de facto comparison set was {SWC, esbuild, tsc}.

Comments on the PR over the next two weeks focus on:
- Source maps (Geoffrey Booth, Matteo) → resolved by adopting ts-blank-space's whitespace approach.
- `node_modules` handling (ljharb, ChALkeR) → resolved by reject-in-node_modules.
- Scope of features (TS team, Daniel Rosenwasser, Jake Bailey) → resolved by "type stripping only, no transforms" for the initial ship.

The TypeScript team (Daniel Rosenwasser, Jake Bailey, Ryan Cavanaugh, Aaron Frost) participates from issue #208 and the TSC meeting as scope advisors, not transpiler advisors; none weighs in on SWC vs alternatives.

### 2024-07-17 — TSC meeting

[nodejs/TSC meetings/2024-07-17.md](https://github.com/nodejs/TSC/blob/main/meetings/2024-07-17.md). Strip-types is discussed at length, but only **whether** to ship, **how** scoped, and **what** to tell users. The transpiler is taken as settled:

> Marco: Currently blocked as swc having trouble with some architectures so still some time before it would land anyway. Feedback from TypeScript team is that there are some concerns but they can be solved. Created package to wrap swc which would go under the Node.js organization.

The "created package to wrap swc" is amaro. SWC is the only candidate discussed.

### 2024-07-24 — TSC meeting with TypeScript team

[nodejs/TSC meetings/2024-07-24.md](https://github.com/nodejs/TSC/blob/main/meetings/2024-07-24.md). The TS team is invited and presents concerns about:
- TS-files-in-`node_modules` (Rosenwasser: "don't think it's a good idea to publish runnable ts in modules")
- Monorepo support (Ryanca: "how to support monorepo without running ts files in node_modules")
- Type-checking-at-runtime (Bailey: "the answer to 'do we want type checking at runtime' is 'no'")

The transpiler choice is not raised; SWC is treated as given.

### 2024-07-24 — PR merged

Marco's PR merges, pulling `@swc/wasm-typescript` in via amaro — a brand-new package under the nodejs org that wraps the wasm blob with a stable API surface. His blog attributes the wrapper to the TS team's concern about tight coupling between Node and SWC version evolution.

### 2024-08-06 — strip-types ships in Node 22.6 (flagged)

[Node 22.6.0 release notes](https://nodejs.org/en/blog/release/v22.6.0). `--experimental-strip-types` available; default off.

### 2024-09-29 — Oxc transformer alpha

[oxc.rs/blog/2024-09-29-transformer-alpha](https://oxc.rs/blog/2024-09-29-transformer-alpha). The Oxc transformer hits alpha, the first public pitch of it as a usable transformer. Benchmarks vs SWC: 3-5x faster, 20% less memory, 2 MB vs 37 MB package size. **Two months after Node's strip-types landed**, so Oxc was not a viable production candidate when the decision was made.

### 2024-10-28 — Oxc raised, ignored

[nodejs/loaders#217 comment 2433489253](https://github.com/nodejs/loaders/issues/217#issuecomment-2433489253):

> thernstig: I am unsure if this comment is fitting here, but I see mentions of swc in the original post. Would it not be better to use https://oxc.rs/ and its parser as it is 3x faster than swc?

No reply; nobody from the TSC engages. By then amaro is in stable maintenance and the loaders WG discussion has moved on to scope and monorepo concerns.

### 2024-12-26 — strip-types unflagged in 23.6

[nodejs/node#56350](https://github.com/nodejs/node/pull/56350) unflags `--experimental-strip-types` for Node 23.6. The PR has zero discussion of transpiler choice; the choice is locked in.

### 2025-02-14 — Oxc ships ts-blank-space-inspired stripping

[Evan You tweet](https://x.com/youyuxi/status/1890701933767246117):

> Cool stuff: ts-blank-space inspired TypeScript type stripping implemented on OXC - strip TS types at the speed of parsing, 4x faster than swc_fast_ts_strip which is used in Node.js /cc @robpalmer2 @satanacchio

If Node were re-deciding from scratch, the performance argument would flip here. **It does not flip retroactively** — no Node TSC discussion of switching has occurred.

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

1. **Build-toolchain friction.** Node's build is C++ via GYP, so adding a Rust dep is a project-level decision touching CI infrastructure, reproducibility, and supply-chain provenance, while pre-built wasm + JS glue needs no new build step — V8 already runs wasm. **The dominant factor**, explicit in the PR body: "other tools require either rust or go to be added to the toolchain."

2. **Maintainer relationship.** SWC's primary author committed to building `@swc/wasm-typescript` as a Node-specific wasm distribution within days of being asked, to following Node's LTS cadence, and to maintaining a separate publish pipeline. That is a real long-term cost for the SWC team, and the offer de-risked the choice.

3. **Prior art.** Deno had been using SWC for a similar role for years. The PR body cites this explicitly. "Battle-tested" is the appeal.

4. **License.** SWC is Apache-2.0, which is compatible with Node's MIT. Oxc is MIT (would have been equivalent or marginally simpler). Not a differentiator.

5. **ts-blank-space approach feasibility.** The blank-space codegen technique (replace types with whitespace, no source-map generation needed) was suggested mid-PR by Bloomberg's team. Marco asked SWC to add a `blank-space` mode (swc-project/swc#9144) and SWC shipped it within days. Oxc had no equivalent path at the time.

### What Node didn't weigh

Notably absent from any public discussion:

- **Performance comparison.** No benchmarks were referenced in the PR, the loaders WG issues, or the TSC meeting notes. The decision wasn't "SWC is faster" — it was "SWC is here and works."
- **Oxc.** The single instance of someone (an external commenter) raising Oxc is unanswered.
- **AST compatibility / ecosystem fit.** Both SWC and Oxc emit ESTree-compatible-ish ASTs; no discussion of whether Node's future loader hooks or addon ecosystem would benefit from alignment with one or the other.
- **Native binding option.** The PR's "in the future I see this being implemented in **native layer**" line suggests openness to a native (non-wasm) integration eventually, but no concrete plan exists in any tracking issue. A comment on amaro#200 leaves the question open: "Or why not use `napi-rs` as wrapper but `WASM` now?"

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

Oxc is meaningfully faster than SWC for stripping today; that gap did not exist in mid-2024, and Node has not acted on it.

### Non-erasable syntax

Both SWC and Oxc transformers handle enums, namespaces, parameter properties, and decorators; amaro's `transform-types` mode exposes this via SWC. Oxc-transformer alpha had partial coverage in late 2024 and is more complete in 2026 — sufficient for Nub's non-erasable-syntax needs.

Node chose to ship **only the erasable subset** by default — strip-types mode, not transform-types — for **ecosystem stability reasons** (TS team concerns about fragmenting "valid TS"), not because of SWC API constraints. Either transpiler could have implemented either policy.

## 4. Project-level dynamics

### Maintainer relationships

SWC's author appeared on Marco's tracking issue within hours, agreed to build custom infrastructure (`@swc/wasm-typescript`), shipped requested features (blank-space mode in swc#9144) within days, and continues to maintain that pipeline. amaro's CI breaks regularly when SWC's output format shifts (amaro#329 "swc update workflow is broken"), but the relationship absorbs that.

No public record shows Oxc's maintainers engaging with the Node TSC, and there is no equivalent "we'll build Oxc-wasm-typescript for Node" offer. Oxc was a younger project in mid-2024, not yet at the alpha-transformer stage, and Node's choice was over before Oxc could have competed.

### amaro's role

The actual integration surface is amaro, not raw SWC. It:
- Wraps `@swc/wasm-typescript` with a stable API.
- Versions independently from Node (so Node can ship amaro@1.x and amaro@1.y can be bumped in a patch release).
- Provides both `strip-types` (default) and `transform-types` modes.
- Is published under the nodejs GitHub org.
- Has a CODEOWNERS file with Marco Ippolito as primary maintainer.

The wrapper exists because the TypeScript team pushed back on Node tightly coupling to SWC's release cadence: Node depends on amaro, amaro depends on SWC. amaro could swap engines under its API without Node noticing; the swap hasn't happened and isn't on its roadmap.

### TSC commitment posture

The Node TSC's posture on TS-runtime support is deliberate scope minimization: "ship the minimum that solves the user complaint; defer transforms to the loader ecosystem." Node ships bare-minimum stripping and points at loaders for the rest.

A Node TSC reconsideration of transpiler choice would require:
- A concrete user-visible problem with SWC (none has materialized).
- A maintainer willing to do the swap PR (no one volunteered).
- TSC consensus that the swap is worth the churn (would require burning credibility on a non-user-facing change).

None of these conditions are present, so the choice is institutionally sticky regardless of merit.

## 5. Implications for Nub

### Does Node's choice change Nub's commitment to Oxc?

**No.** The constraints that drove Node to SWC-via-wasm don't apply to Nub:

1. **No Rust-toolchain barrier.** Nub is a Rust binary. Linking `oxc_parser` and `oxc_transformer` crates directly is the path of least resistance, not a special accommodation.
2. **No "maintainer relationship" lock-in.** Nub needs no wasm-publish pipeline and no bespoke infrastructure from any transpiler team. Oxc is on crates.io and is depended on like any other crate.
3. **No build-system gating.** No GYP, no Python build scripts, no TSC-level review for adding a dep. `cargo add oxc_parser` is the whole story.
4. **Ecosystem direction.** Rolldown ships on Oxc and Vite 8 ships on Rolldown, so aligning Nub's transpiler with the dominant future bundler stack puts its diagnostics, source-map shape, and AST conventions in the same family as the user's bundler.

### What about source-map and error-shape compatibility with amaro?

The JS ecosystem is settling on amaro as the de-facto "TypeScript runtime stripper," so differences between amaro's source-map shape and Oxc's could create user-visible inconsistencies as more tooling assumes amaro:
- amaro uses **whitespace replacement** (the ts-blank-space approach) by default in `strip-types` mode. Source positions are preserved 1:1; no source map needed.
- amaro's `transform-types` mode emits source maps with SWC-shape mappings.
- Oxc's transformer emits source maps with Oxc-shape mappings, which are similar but not byte-identical.

Nub's TS pipeline already commits to source-map emission via Oxc. So long as those source maps are well-formed — V8, Chrome DevTools, and the Node debugger all accept them — byte-identical output with amaro does not matter for end users. The one place this could bite is a downstream tool that **assumes amaro-shape mappings**; none is known, and if one emerges the fix is at the source-map-emit stage.

### Could Oxc replace amaro inside Node someday?

Plausible but not imminent. The path would require:
- Someone with a stake in Oxc — a Node TSC member, an Oxc maintainer, or someone from the Vite team — to do the swap PR.
- A demonstration that the swap is risk-free (byte-compatible output for the strip-types subset, equivalent error messages, equivalent edge-case handling).
- TSC buy-in on rocking a load-bearing dep for performance reasons alone.

Realistic timeline: not before 2027.

The reverse pressure — amaro improving enough that Nub should switch to it — is unlikely: Nub's integration is in-process Rust crates, amaro's is through V8's wasm boundary, giving up roughly 100x speed for no benefit.

### One thing to keep on watch

Compat mode passes everything through to Node, including Node's strip-types, so if amaro starts handling cases Nub's transpiler doesn't, or vice versa, compat-mode behavior diverges from default-mode behavior. That divergence is intentional — compat mode is Node's behavior — but the cases are worth testing:

- Edge-case TS syntax that amaro accepts but Oxc doesn't (probably none; verify).
- Edge-case TS syntax that Oxc accepts but amaro doesn't (Oxc tends to be ahead on newer TS features, so Nub might accept syntax that fails under `--node`).

This is a test-surface item, not a strategic concern.

## 6. Open questions

These could not be answered from public sources:

- **Did Marco or any TSC member privately evaluate Oxc?** The PR language ("I have considered other tools") suggests some evaluation happened, but Oxc isn't named and no public artifact exists.
- **Was there a corporate driver?** Marco was at NearForm/HeroDevs; SWC's author was at Vercel at the time. The public record does not distinguish a developer-to-developer collaboration from a strategic one, so any motivation ascribed here would be speculation.
- **Will Node revisit?** No public indication. Marco's amaro#200 comment ("we should keep using SWC for the foreseeable future") is the most recent on-record statement, and no revisit is scheduled.
- **Could amaro switch engines under its own API?** Technically yes — amaro is a wrapper, and a swap to Oxc internally would be invisible to Node consumers modulo source-map diffs. No public proposal exists.
- **How much of the SWC choice was relational vs technical?** A counterfactual where the Oxc team made the same offer at the same time would have produced a different conversation, and the record does not let that be tested.

## 7. Bottom line for Nub

Continue with Oxc. The Node TSC decision validates SWC's wasm-distribution and maintainer-relationship shapes as fits for Node's specific build constraints, none of which apply to Nub.

One action follows: add an "interop with amaro" smoke test to the TS pipeline, verifying that a `.ts` file transpiled by Nub via Oxc and the same file transpiled by Node via amaro produce **functionally equivalent runtime behavior** for the erasable subset. Byte-identical output is not the bar; the bar is that the same file gives the same answer through `nub script.ts` and `nub node script.ts`.

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
