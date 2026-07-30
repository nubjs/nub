# Why Node hasn't shipped Import Maps — and what that means for Nub

Research compiled 2026-05-22 to test the hypothesis that the Node maintainers' non-implementation of WICG Import Maps is sound, and to interrogate whether Nub's polyfill at `runtime/import-maps.md` is filling a real gap or replicating a browser surface that doesn't fit Node. Sibling refs: [`tsconfig-paths.md`](tsconfig-paths.md), [`exports-map-ts-swap.md`](exports-map-ts-swap.md), [`module-resolution.md`](module-resolution.md). Architecture context: `../architecture/augmenter-not-fork.md`.

## TL;DR — the thesis

**Position 2 of the three: "sound but historically contingent".** Node's six-year non-shipping of Import Maps is not political inertia or ignorance — there is a substantive, principled technical argument that the WICG Import Maps spec is keyed and scoped in ways that don't fit Node's resolution model (conditional exports, peer dependencies under workspaces, multiple package IDs sharing a directory). That argument has been made cleanly by Bedford (2020), jkrems/Hagberg (2020, 2024), and most recently arcanis (2026), and it is the operative reason WICG Import Maps haven't landed. But there *isn't* uniform "resistance": Booth, Bedford, and wesleytodd have repeatedly said "we'd ship this if the PR were finished," and as of 2026-03 arcanis landed [`--experimental-package-map`][PR 62239] — a Node-shaped sibling of the import-map idea, explicitly framed as "as close to import maps as possible, with a light design difference making it possible to stay compatible with other Node.js resolution features." The de-facto outcome is that **Node is shipping the *underlying need* import maps tries to address, but with Node-native semantics, not the WICG spec.**

For Nub, this materially changes the calculus on the `runtime/import-maps.md` polyfill. **Recommendation: scope down (preferred) or drop.** The bare-specifier-remap subset that Nub's polyfill uniquely solves overlaps heavily with `package.json` `imports`, tsconfig paths, and the incoming `--experimental-package-map`. The HTTPS-URL-target portion is already out of scope per the 2026-05-18 decision (and would be dead-on-arrival anyway because [`--experimental-network-imports` was removed][PR 53822] in 2024). What's left is a small, brittle surface that risks teaching Nub users to author `importmap.json` files that won't transfer to plain Node, won't compose with the package-manager's `package-map.json`, and don't compose cleanly with conditional exports. The reversibility-filter pressure on this feature is real.

## The Node discussion — canonical sources

### Tracking issue and PR

- **[#49443] — "Support for Import Maps"** (wesleytodd, opened 2020-01-24, still open as of 2025-12, labeled `feature request | esm | never-stale`). 74 👍 / 15 ❤️. This is *the* Node-side tracking issue; everything else points back at it. Bedford explicitly removed the `stale` bot's auto-close in 2024-10 with the comment: *"I don't know of any resistance to implementing this feature in Node.js, other than just getting the PR to a mergeable state."*
- **[PR 50590] — "module: add import map support"** (wesleytodd, draft, opened 2023-11-07, last activity 2025-08-04). Behind `--experimental-import-map`. Has never reached mergeable state — wesleytodd cited time constraints repeatedly through 2024-2025.
- **[loaders 168] — "Import Maps Implementation Plan"** (wesleytodd, 2023-10). The implementation plan thread. Booth's load-bearing comment: *"Import maps is basically an alternative ESM resolution algorithm, that can be run sync just like the existing resolution algorithm is. … I would put the import maps handling in this function [`defaultResolve`], so that it's handled within the `defaultResolve` that's already at the end of the `resolve` hook chain."* This is the architectural positioning Nub's plan inherits.
- **[modules 544] — "Import Maps and Node.js"** (Bedford, 2020-08, closed). Older Modules-WG context; Bedford's framing of import maps as *"ephemeral views into the project, representing a caching / precomputation of the resolution operation"* originates here.
- **[#62239] — "loader: implement package maps"** (arcanis/Maël Nison, opened 2026-03-13, in progress, on TSC agenda). The *outcome*: a Node-native alternative behind `--experimental-package-map=` that solves the same user problems (phantom deps, peer-dep correctness in monorepos, I/O-free resolution) without taking the spec.

### Who said what — the substantive arguments

The arguments mapped, with attribution and weight (TSC voting members in **bold**):

| Argument | Who | Where | Position |
|---|---|---|---|
| Conditional exports collapse — import maps have one entry per specifier, Node packages can resolve differently per `import`/`require`/`node-addons`/user conditions | Jan Krems (jkrems / "hybrist"), **wesleytodd** acknowledged | [#49443], comment 2024-11-12 | The concrete blocker. Example: `"exports": { "node-addons": "./addon.node", "default": "./slow.js" }` — no flat import map can pre-resolve this without knowing `--no-addons` at install time. wesleytodd: *"This is great input @jkrems and for sure not a use case I had considered."* |
| Filesystem-keyed scopes can't represent multiple package IDs sharing a directory (peer deps under workspaces) | **arcanis** | [PR 62239] (Mar 2026), [#49443] (2020-01) | *"The scopes field is keyed by filesystem path. This is a problem because it precludes a same folder from having multiple package IDs each with their own dependency set, necessary to represent peer dependencies with workspaces."* This is why package-maps is keyed by arbitrary IDs in a flat `packages` table, not by URL prefix. |
| Import maps "take full ownership of the resolution pipeline" by spec | **arcanis** | [PR 62239] description (2026-03) | *"In practice these aren't a good fit for runtimes like Node.js … import maps take full ownership of the resolution pipeline by spec, thus preventing implementing additional runtime-specific behaviours such as exports or imports fields."* wesleytodd disputes this in the PR thread, citing *"A Document can have multiple import maps processed"* from the HTML spec — but the dispute is unresolved. |
| No package-level semantics — can't express "this dep isn't reachable because no parent provides peer X" | arcanis | [#49443], 2020-01 | *"Yarn is able to throw semantic exception when a package isn't reachable, explaining why the package cannot be found. It's quite harder for import maps, which don't have package-level semantics — just folder-level semantics."* |
| Mocking / vendoring / resolution-inlining are the *real* useful goals, not "import maps as the only resolver" | Guy Bedford | [#49443], 2020-01 | *"I do see having import maps as being the default and only form of resolution in Node.js as a non-goal. Rather an approach where the import map can allow intercepting the resolution and then having that work with userland approaches."* — that is, partial mapping, falling back to Node's resolver. The Nub plan matches this shape. |
| Spec uncertainty was a real concern in 2020 but isn't now | ljharb, then trusktr/Bedford | [#49443], 2020 then 2024-25 | ljharb 2020: *"I'd hope node core would never ship any form of import maps … when those things happen [browser standardization], node core would obviously ship import maps."* Resolved as of 2024 — Chrome, Edge, Firefox, Safari, Deno all ship import maps. So the 2020 wait-and-see argument no longer applies. |
| `--experimental-policy` / policy-redirect is the prior art doing the same job badly | bmeck | [#49443], 2020-01 | *"We already have this feature of static resolution mappings to some extent using policy.html. Even though I wrote the feature and have spent some time tuning performance, the benchmarks there are not pleasant … particularly regarding startup time."* — and the policy feature was [later removed entirely][next-10 285] in 2024 as unmaintained. |
| HTTPS imports are dead — the import-map URL-target use case is structurally blocked | implicit | [PR 53822] (network-imports drop, 2024), [#49443] throughout | `--experimental-network-imports` was removed 2024 because *"this feature is lacking a champion, and the documentation is not clear enough to define security expectations or boundaries."* An import map entry like `"react": "https://esm.sh/react"` cannot work in modern Node. |
| Whether to ship it anyway, for spec-alignment / influence reasons | **Booth** | [#49443], 2020-01 | *"Node should absolutely support any standards; and we would be in a stronger position to influence the standard as it develops if we have our own flagged implementation … If import maps never end up getting standardized, we can just remove ours."* — the most pro-import-map TSC voice. |
| It's at the top of the loaders to-do list, PRs welcome | **Booth** | [#49443] (2023-10), [loaders milestones] | The current open posture: not blocked on principle, blocked on author availability. |

[#49443]: https://github.com/nodejs/node/issues/49443
[PR 50590]: https://github.com/nodejs/node/pull/50590
[PR 62239]: https://github.com/nodejs/node/pull/62239
[PR 53822]: https://github.com/nodejs/node/pull/53822
[modules 544]: https://github.com/nodejs/modules/issues/544
[loaders 168]: https://github.com/nodejs/loaders/issues/168
[loaders milestones]: https://github.com/nodejs/loaders#milestone-3-usability-improvements
[next-10 285]: https://github.com/nodejs/next-10/issues/285

### The de-facto outcome

Reading the timeline: import maps was discussed in earnest 2020, a PR was drafted 2023, the PR stalled on author bandwidth and on the conditional-exports problem (jkrems 2024-11), and in March 2026 arcanis landed [PR 62239] which is *the* current direction — a Node-shaped sibling. The arcanis design is **deliberately different** from WICG import maps where the spec doesn't fit Node:

- **Keyed by package ID**, not by bare specifier — allows multiple packages with the same name (multiple lodash versions in one tree).
- **Per-package `dependencies` table**, not global `imports` — encodes the "phantom dependency" / strict-resolution problem that flat import-maps can't.
- **Path field**, not URL — file: only, no `https:` (which is dead anyway post-[PR 53822]).
- **Composes with `exports` and `imports`**, not replaces them — solves the "last-mile resolution" wesleytodd had been arguing for since 2020.

ruyadorno and wesleytodd in the PR 62239 thread (March 2026) explicitly worried this would *block* import-maps from later landing. arcanis's reply: *"Despite similar names package maps and import maps have non-overlapping designs intended for very different hosts. Them landing doesn't prevent you in any way from pursuing your own work implementing import maps, which will address different use cases than the ones we're focusing on here."* — which reads as "we are shipping the Node problem; the WICG spec is for browser problems." This is the most candid statement available from a TSC-adjacent contributor about the spec/host mismatch.

So the answer to "did Node formally reject import maps?" is **no**. The answer to "did Node de-facto decide the WICG spec wasn't the right shape for the Node-side problem?" is **yes, and the alternative is shipping right now under an experimental flag.**

## The design tension — Import Maps vs Node's existing mechanisms

Mechanism-by-mechanism overlap:

### `package.json` `"exports"` field with conditions

`exports` is the closest sibling. Both name an entry point for `"some-package"`. Critical differences:

- **`exports` is the *package author's* contract; import maps are the *consumer's* config.** A package author publishes one `exports` map to npm; a consumer can have one project-wide `importmap.json` overriding many packages' exports.
- **`exports` carries conditions** — `import`/`require`/`node`/`browser`/`development`/`production`/`workerd`/user-defined via `--conditions`. Import maps have no condition mechanism; the spec deliberately omitted it as out-of-scope for the browser host.
- **`exports` has subpath patterns** (`"./*": "./dist/*.js"`); import maps have trailing-slash mappings. Different syntax for similar idea; not interchangeable as authored.
- **`exports` runs *first* in the resolver chain** (it's where package bare-specifier resolution lands). Import maps in Booth's loaders-168 positioning would run *above* `defaultResolve` but bypass the exports lookup when matched — which is exactly the "takes ownership of resolution" objection arcanis raised. Implementations have to choose: does the import-map entry win and skip exports? Or does the entry rewrite the specifier, which then re-enters exports?

The jkrems 2024-11 collapse-on-conditions argument is the operative tension. A package with `"exports": { "node-addons": "./addon.node", "default": "./slow.js" }` can't be pre-resolved into a single import-map entry without knowing `--no-addons` at install time, and that flag is per-process. The package-manager-emits-import-map workflow breaks at this exact point.

### `package.json` `"imports"` field (subpath imports)

Direct functional overlap for the "this project wants to remap a bare specifier to a local file" case. `imports` requires `#`-prefixed keys (`"#vendor/lodash": "./vendor/lodash.js"`); import maps don't. Both support conditions in `imports` but only `imports`'s conditions are honored by Node.

The user problem `imports` solves (project-internal aliasing without webpack-style config) is exactly the user problem the *project-author* writes an import map to solve, minus the `#` prefix and minus the WICG-spec compatibility. Most "I want `lodash` to mean `./vendor/lodash`" cases under Nub are already addressable with `package.json` `imports`. The main gap: `imports` is per-package (one map per `package.json`), import maps can be cross-package.

### `node_modules` lookup

Largely independent. Import maps and `node_modules` resolution are typically composed: an import map intercepts before `node_modules` lookup; unmatched specifiers fall through to `node_modules`. This is the partial-mapping use case Bedford described in 2020 and the design wesleytodd's [PR 50590] adopted.

The tension: `node_modules` lookup respects symlinks, `--preserve-symlinks`, realpath canonicalization, and per-directory `package.json` discovery. Import map targets are URLs that don't reproduce any of that. A `file:` URL target loses the package-scope context the importing file would otherwise inherit. bakkot's symlink question in [PR 62239] thread is the same issue in microcosm.

### `tsconfig.json` `paths` (at compile time)

Functionally overlaps the project-internal aliasing use case. `paths` is more expressive (multiple substitution targets, `*` wildcards, baseUrl indirection). But `paths` is compile-time-only by spec — runtime tools (tsx, Nub, ts-node) honor it as a courtesy.

For Nub, this is the operative comparison: **Nub is already shipping tsconfig-paths runtime support in v0**, which covers the project-internal alias case import maps would otherwise address. The remaining import-map-unique use case is *cross-package* remapping (vendoring, mocking) — which is genuinely outside the tsconfig-paths scope.

### The removed `--experimental-network-imports`

[PR 53822] (richardlau/RafaelGSS, merged 2024-07) removed `--experimental-network-imports` because of lack of champion, security-model gap, and Security WG load. The removal *forecloses* the import-maps "URL targets pointing at CDNs" use case — there's nothing for `"react": "https://esm.sh/react"` to map *to* in Node. Either Node ships network imports again (no champion, no plan) or import maps in Node are restricted to file-URL targets, which is the same scope as `imports` plus cross-package reach.

Nub's `runtime/import-maps.md` already excludes HTTPS targets, decided 2026-05-18. That's correct under the augmenter-not-fork posture: doing HTTPS imports would require network fetch, integrity, caching, versioning — *package-manager*-shaped work that Nub is explicitly not in scope for.

### Summary table

| Use case | `exports`/`imports` | `node_modules` | `tsconfig paths` | `--experimental-package-map` (#62239) | WICG Import Maps |
|---|:-:|:-:|:-:|:-:|:-:|
| Bare-specifier → local file (project-internal) | ✓ (`imports`) | — | ✓ | ✓ | ✓ |
| Bare-specifier → cross-package remap | partial | — | partial | ✓ (with strict mode) | ✓ |
| Bare-specifier → HTTPS URL | — | — | — | — | ✓ (dead in Node post [PR 53822]) |
| Conditional resolution (`import` vs `require`, `--conditions`) | ✓ | — | — | not yet | ✗ (spec gap) |
| Peer-dep correctness in monorepos | partial | partial | — | ✓ | ✗ (scope-by-path) |
| Multiple package IDs at one path | — | — | — | ✓ | ✗ (scope-by-path) |
| Mocking specific deps | — | — | — | partial | ✓ |
| Package-manager-emitted | — | implicit | — | ✓ (designed for) | partial (collapse problem) |

The pattern is clear: **WICG Import Maps overlap with Node's existing mechanisms on the easy cases and lose to them on the hard cases** (conditions, peer deps, package identity). The arcanis PR 62239 is shaped to handle the hard cases that import maps can't.

## Thesis development

The three positions under consideration:

1. **Sound and well-reasoned** — Node has good architectural reasons, hypothesis holds.
2. **Sound but historically contingent** — Reasonable at the time, possibly revisitable.
3. **Possibly wrong** — Weak reasoning or political inertia, Nub's polyfill fills a real gap.

**The evidence is solidly Position 2, slightly leaning Position 1.**

Position 3 doesn't survive contact with the record. The arguments against direct WICG-Import-Maps adoption are substantive and made by people who have built the surrounding infrastructure: Bedford designed conditional exports; arcanis built Yarn PnP and is now shipping package-maps; jkrems is a long-time loader-WG voice who raised the conditional-resolution collapse problem concretely with an unanswerable example (`node-addons` condition). The arguments aren't "we don't like WHATWG specs" (though bmeck would say that) or "we hate browsers" — they're specific, mechanism-level, and the alternative they motivate (package-maps PR 62239) is *more capable* than the WICG spec for Node's resolution model, not less.

Position 1 is *almost* right but for one detail. The framing "Node has rejected import maps" is wrong — Node hasn't rejected anything. [#49443] is still labeled `never-stale`. [PR 50590] is still open as a draft. Booth and Bedford are on record as supportive. The de-facto outcome — "we'd ship it if the PR got finished, but in the meantime someone is shipping a Node-shaped alternative that handles the cases import maps can't" — is more like a soft-fork-of-the-design than a rejection.

Position 2 captures the truth: there was a window where shipping the WICG spec might have been the right call (Bedford's 2020 "ephemeral view, partial mapping, falls through to Node resolver" framing was approximately this), but the spec didn't evolve to accommodate Node's host model (no condition mechanism, scope-by-path keying), and Node moved on to package-maps. The 2020 conversation reads now as Node trying to figure out whether the WICG group would adopt Node's constraints; the answer turned out to be no, so Node is shipping its own.

**The hypothesis is essentially right.** The two specific claims hold up: (1) WICG Import Maps were designed for the browser-side bare-specifier-to-URL problem; (2) Node has solved the equivalent problem differently and better via `exports`/conditions. The one place the hypothesis overshoots: it characterizes Node's posture as "resistance," when the more accurate framing is "deferral until the spec fits Node's host model, with active work on a Node-shaped sibling in the meantime."

## Implication for Nub's posture

`runtime/import-maps.md` currently positions WICG Import Maps as a v0.1 ship-in-Phase-1 feature, reversed from the earlier "defer past v0" stance on 2026-05-18 with the reasoning: *"It's now stabilized and web standard."*

Re-examining that reasoning in light of this research:

**The "stabilized web standard" framing is true but partial.** WICG Import Maps are stable in browsers (Chrome, Edge, Firefox, Safari, Deno). They are *not* stable as a Node-host feature — Node's de-facto position is "we'd implement them if the spec accommodated our host, and since it doesn't, we're shipping a sibling that does." Nub adopting WICG Import Maps imports a spec that **the Node maintainers explicitly chose not to adopt and built an alternative to.** That's a different ecosystem position than "we polyfill what Node will ship eventually."

The three sub-questions posed:

### Is Nub's polyfill addressing a real ecosystem gap?

**Marginally.** The gap is narrow:

- The HTTPS-URL-target use case is **out of scope** by the 2026-05-18 decision (and dead in Node anyway).
- The project-internal bare-specifier-alias use case is **already covered by tsconfig paths + `package.json` `imports`**. Nub ships tsconfig paths in v0; users with `imports` get them for free via Node's resolver.
- The cross-package remap / vendoring / mocking use case is **the real gap**, and it's narrow — it's the Bedford 2020 list (mocking specific deps, vendoring specific deps). For these, the WICG spec does work, modulo the conditional-resolution collapse problem.
- The "package-manager generates an import map" use case is **going to be `package-map.json`**, not WICG `importmap.json`, if anything. No package manager has shipped Import Map generation; arcanis at Yarn is committed to package-maps; pnpm (zkochan on PR 62239) signaled support.

So the real ecosystem gap Nub would fill: a user wants to vendor `react` to `./vendor/react.js` across their whole project. That's solvable with `package.json` `imports` per-package (one entry per package), with tsconfig paths if they accept the TS-only abstraction, or with a Nub-specific `importmap.json`. The Nub-specific surface is the most uniform and the most lock-in-prone.

### Does the polyfill create lock-in?

**Yes, modestly.** A user who writes `importmap.json` is writing a file that:

- Plain Node doesn't read (Nub could publish a `module.registerHooks()` shim package, but that's "Nub-on-Node," not "Node native").
- Plain Node + `package-map.json` doesn't compose with — different shape, different keying.
- Browser doesn't read at runtime (browsers read `<script type="importmap">` inline, not a file).
- Deno reads but with different semantics (`importMap` in `deno.json`).

This is exactly the brand-boundary risk [`CLAUDE.md`](../../CLAUDE.md) and the reversibility filter call out. A user who adopts Nub's import-map feature has written a config that **only Nub reads** and that doesn't transfer.

Compare to tsconfig paths: a user who writes `tsconfig.json` `paths` has written a config that tsc reads at compile time, that IDE tooling reads, that tsx/ts-node/Bun read, and that Nub also reads. That's a Nub *augmentation* of an existing artifact, not a Nub-specific artifact. The reversibility cost is much lower.

The brand-boundary check from [`CLAUDE.md`](../../CLAUDE.md): *"would a user on plain Node, plus the corresponding `module.register()` / `--import` / npm-addon, get the same result?"* For import maps, the answer is yes — Nub could publish `@nub/import-maps-loader` (well, not under `@nub/*` per brand rules, so under some neutral name) and a plain-Node user could install it. So the mechanism-level test passes. The *config-level* test (would the user's `importmap.json` be a portable artifact?) is what fails.

### Could the polyfill be scoped down to bare-specifier remapping only?

**Yes, and this is probably the right answer if we keep it at all.** Concrete scope-down:

- **File targets only** (already decided).
- **`imports` map only**, no `scopes` map. Scopes are the part that breaks under Node's peer-dep semantics and that have no straightforward Nub use case. Project-internal aliasing doesn't need scopes; cross-package vendoring doesn't either.
- **Skip trailing-slash patterns initially.** They're the part that overlaps with `exports` subpath patterns and creates the most "wait, which mechanism wins?" confusion.
- **Document explicitly that this is a transitional polyfill.** Add to the docstring: *"Prefer `package.json` `imports` for project-internal aliasing; prefer tsconfig paths for TS-family projects; the import-map polyfill is for cases that neither of those covers. If Node's `--experimental-package-map` stabilizes, expect Nub to honor that instead."*

This scoped-down version is small, well-defined, doesn't pretend to be a faithful WICG implementation (because Nub's resolution model has the same condition / peer-dep collapse problems Node's does), and is reversibly droppable if usage data shows nobody adopts it.

### Recommendation

In priority order, the recommendation:

1. **Strongly preferred: scope down the polyfill.** Keep `imports`-map only (no `scopes`, no trailing-slash patterns for v0.1), file-target only (already decided), document as transitional, monitor `--experimental-package-map` stabilization for a possible pivot. This preserves the goodwill of "we ship the web standard" without the lock-in cost of the full surface.

2. **Acceptable alternative: drop entirely from v0.1, revisit post-1.0.** tsconfig paths + Node's `package.json` `imports` covers ~95% of real user needs. The remaining 5% (cross-package vendoring without `imports`) is a small enough user base that "wait until either WICG or Node-package-maps stabilizes" is defensible. Cuts a small surface that has real lock-in risk and modest user value.

3. **Not recommended: ship full WICG-spec import maps (current plan).** Reasons:
   - Sets up Nub users to author `importmap.json` files that don't transfer to plain Node, plain `package-map.json`, or browsers (without polyfill).
   - Imports a spec the Node maintainers have chosen not to adopt, in a way that doesn't compose with Node's planned alternative (`--experimental-package-map`).
   - The conditional-resolution collapse problem (jkrems' `node-addons` example) is unsolved in Nub's plan; we'd inherit the same defect.
   - Misuses "web standard" as a justification — WICG Import Maps are a *browser* standard; Node's host-shape says no, and Nub is a Node augmenter, not a browser-on-the-server.

A scoped-down (option 1) `runtime/import-maps.md` would amount to: keep the doc, narrow the "What's supported" list, add the scope-down rationale, link to this research doc, set up a follow-up to monitor `--experimental-package-map` and decide whether to honor it under Nub.

## Sources

- **[Node issue 49443]** — Support for Import Maps (wesleytodd, 2020-01 → 2025-12, open, `never-stale`). The canonical tracking issue.
- **[PR 50590]** — module: add import map support (wesleytodd, 2023-11, draft, stalled).
- **[PR 62239]** — loader: implement package maps (arcanis, 2026-03, in progress). The Node-shaped sibling.
- **[loaders 168]** — Import Maps Implementation Plan (wesleytodd, 2023-10). Architectural positioning.
- **[modules 544]** — Import Maps and Node.js (Bedford, 2020-08, closed). Original Modules-WG context.
- **[PR 53822]** — drop `--experimental-network-imports` (richardlau, 2024-07, merged). HTTPS-import removal that forecloses URL-target use case.
- **[next-10 285]** — Re-evaluating Node.js Experimental Features (2024). Context for the `--experimental-policy` removal too.
- **[guybedford/import-maps-extensions]** — Bedford's WICG-extensions proposal (worker maps, integrity, isolated scopes, lazy loading). The work that *would* close some gaps if it landed; hasn't.
- **WHATWG HTML Living Standard — Import Maps** ([https://html.spec.whatwg.org/multipage/webappapis.html#import-maps](https://html.spec.whatwg.org/multipage/webappapis.html#import-maps)). Spec text; relevant for the "multiple import maps processed" debate between wesleytodd and arcanis in [PR 62239].
- **WICG/import-maps** ([https://github.com/WICG/import-maps](https://github.com/WICG/import-maps)). The original WICG group's repo.
- **MDN Import Maps** ([https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script/type/importmap](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script/type/importmap)). Browser-side documentation; used as reference point by aduh95 in [PR 62239] review.
- **`runtime/import-maps.md`** — the Nub runtime doc this research targets.
- **`runtime/tsconfig-paths.md`** — sibling resolve-hook feature that covers project-internal aliasing.

[Node issue 49443]: https://github.com/nodejs/node/issues/49443
[guybedford/import-maps-extensions]: https://github.com/guybedford/import-maps-extensions

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
