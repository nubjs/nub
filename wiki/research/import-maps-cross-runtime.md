# Import Maps across Node alternatives — runtime support, package-map.json, and spec compatibility

Research compiled 2026-05-22. Companion to [[research/import-maps-node-resistance]] (Node's resistance to WICG Import Maps and the `--experimental-package-map` sibling). Nub's runtime posture is package-map forward-implementation, not a WICG polyfill.

## TL;DR

**Only Deno among major Node alternatives ships native WICG Import Maps.** Browsers ship them via `<script type="importmap">`; Bun, Node.js, Cloudflare Workers, and typical edge/server runtimes do not.

Bun substitutes `tsconfig.json` `paths`, `package.json` `"imports"`, and plugins. Node is shipping **`package-map.json`** ([PR #62239](https://github.com/nodejs/node/pull/62239)) — a deliberately non-compatible JSON schema that solves package-manager problems import maps cannot (phantom deps, peer deps under workspaces, conditional `exports`). It is not a subset or profile of the Import Maps spec: arcanis rejected reusing `imports`/`scopes` because the semantics diverge. WinterTC is exploring whether package maps could be *translated into* import maps for hosts that only speak WICG, which is standardization work rather than today's reality.

**The Import Maps spec defines no filename convention** — only the JSON shape (`imports`, `scopes`, optional `integrity`) and the HTML delivery mechanism (`<script type="importmap">` inline or via `src`). Hosts pick filenames (`import_map.json`, `deno.json`) by convention only.

## Which runtimes support Import Maps?

Native WICG support, and the substitute mechanism where it is absent, across browsers, Deno, Node, Bun, Cloudflare Workers, and WinterCG-style edge hosts.

| Runtime / host | Native WICG Import Maps? | How bare-specifier remapping works instead |
|---|---|---|
| **Browsers** (Chrome, Edge, Firefox, Safari) | Yes — HTML living standard | `<script type="importmap">` inline JSON or `src=` fetch; applies to module scripts in documents, **not** workers/worklets ([MDN](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script/type/importmap)) |
| **Deno** | Yes — first-class, spec-based with Deno extensions | `deno.json` `imports`/`scopes` (extended semantics), or `--import-map=` / `importMap` field pointing at a strict JSON file |
| **Node.js** | No (draft [PR 50590](https://github.com/nodejs/node/pull/50590) stalled) | `package.json` `"exports"` / `"imports"`, `node_modules`, tsconfig paths (tooling only); **`--experimental-package-map`** incoming ([PR 62239](https://github.com/nodejs/node/pull/62239)) |
| **Bun** | No — [issue #9863](https://github.com/oven-sh/bun/issues/9863) explicitly notes absence; community asks for it | `tsconfig.json` `compilerOptions.paths`, `package.json` `"imports"` (`#`-prefixed), `bunfig.toml` `[resolve] conditions`, runtime plugins (`onResolve`) |
| **Cloudflare Workers** | No — import maps don't apply inside workers per HTML spec | Bundler/wrangler resolves at deploy time; runtime uses ES modules + `node_modules` compat layers, not document import maps |
| **Vercel Edge / WinterCG-style hosts** | Generally no | Same pattern: bundler-time resolution, WinterCG minimum API surface, no document import-map injection |

**Deno is the outlier among server-side Node alternatives.** It adopted import maps early because its module model is URL-centric (JSR, `npm:`, `https:`) and bare specifiers without a map are invalid. Deno is also pushing import-map *merging* ([deno#30689](https://github.com/denoland/deno/issues/30689)) to align with the HTML spec's multi-map merge algorithm — still one effective map per process today.

**Bun does not support import maps** as of 2026. Its module-resolution docs describe Node-style `node_modules`, `exports`, tsconfig paths, and `#` imports — no `--import-map`, no `bunfig` import-map section. Issue threads treat import maps as a possible future feature (CLI- or bunfig-delivered, runtime not just build).

## Deno vs the Import Maps spec — how compatible?

Deno has **two modes**:

1. **Strict mode** — `--import-map import_map.json` or `deno.json` `"importMap": "..."`. Follows the Import Maps Standard closely: trailing-slash paired entries required for package-directory mappings (`"@pkg/"` → `"jsr:/@pkg/"` with the extra `/` after the scheme — a Deno/JSR quirk documented in [deno modules docs](https://docs.deno.com/runtime/fundamentals/modules/)). Subpath resolution via trailing-slash prefix rules behaves per spec.

2. **Extended mode** — inline `"imports"` / `"scopes"` in `deno.json`, which most Deno projects use. Deno extends the standard: a single entry per package without the trailing-slash duplicate, auto-inferred directory mappings, and integration with `deno add` / lockfile / JSR publishing. It is not portable strict import-map JSON — copying `deno.json` `imports` verbatim into a browser `<script type="importmap">` may fail trailing-slash and URL-normalization rules.

Other Deno deviations from browser hosts: remote URL targets (`jsr:`, `npm:`, `https:`) are first-class (browsers use absolute URLs or site-relative paths); one import map per run (merging is proposed, not fully shipped); `compilerOptions.paths` applies only to bare specifiers as a TypeScript-compat layer, with `imports` authoritative at runtime.

**Compatibility rating for Deno strict mode vs HTML spec:** high for the core `imports`/`scopes` resolution algorithm and JSON shape; medium overall once Deno-specific URL schemes and host integration are counted. **For Deno `deno.json` inline `imports`:** medium — same keys, looser authoring rules, not byte-for-byte spec-compliant JSON.

## `package-map.json` vs Import Maps — same problem, different design

Node's [`--experimental-package-map`](https://github.com/nodejs/node/pull/62239) (`package-map.json`) addresses **package-manager strict resolution**, not browser-style bare-specifier aliasing. The PR description and the review thread with aduh95 and wesleytodd state the design choice plainly:

### Schema — not compatible

The two JSON schemas side by side across eight dimensions: top-level shape, keying, scope model, target values, strictness, multiple versions of one name, conditional exports, and remote targets.

| | WICG / HTML Import Maps | Node `package-map.json` |
|---|---|---|
| Top-level shape | `{ "imports": {...}, "scopes": {...}, "integrity": {...} }` | `{ "packages": { "<id>": { "path", "name?", "dependencies" } } }` |
| Keying | Bare specifier strings (`"react"`, `"lodash/"`) | Arbitrary package IDs (`"react"`, `"lodash@1"`, `"lodash@2"`) |
| Scope model | URL-prefix `scopes` map (referrer-path keyed) | Per-package `dependencies` allowlist (importer-package keyed) |
| Target values | URL strings (absolute, `/`, `./`, `../`) | Filesystem paths relative to map file, then **`exports`/`main`/`node_modules` resolution continues** |
| Strictness | Map rewrites specifier → URL; host resolves URL | Map picks package directory; **`ERR_PACKAGE_MAP_ACCESS_DENIED`** if importer lacks dependency entry |
| Multiple versions same name | Awkward — global `imports` can't express two `"lodash"` without scopes tricks that break on peer deps | Native — `"lodash@1"` and `"lodash@2"` as separate IDs |
| Conditional exports | Not in spec — flat one-entry-per-specifier | Preserved — map stops at package path, Node conditions apply after |
| HTTPS / remote targets | Spec allows; Node killed network imports | Not in schema |

**Renaming `package-map.json` to `importmap.json` does not make any import-map consumer able to parse it** — the JSON schemas are disjoint. WinterTC discussion ([WinterTC55/admin#173](https://github.com/WinterTC55/admin/issues/173)) floated defining package maps *in terms of* import-map translation for hosts like Deno, lowering a package map to generated `imports`/`scopes` entries, but that is a proposed interoperability layer rather than current behavior. Per an April 2026 WinterTC comment from arcanis, import maps would likely **override** package maps on overlapping paths in Deno-style hosts.

### Behavioral overlap — partial

Both mechanisms intercept bare-specifier resolution before (or instead of) naive lookup, and they overlap for the simple case "make `react` resolve to this directory". Beyond that:

- Import maps **replace the specifier with a URL** and the host loads that URL. Done.
- Package maps **select a package root** from an allowlisted dependency graph, then **defer to Node's existing resolver** (`exports`, extension probing, conditions). The map encodes *who may import whom*, not just *where files live*.

Import maps' `scopes` key by **referrer URL prefix** — one folder, one dependency set. Package maps key by **package identity** — same folder can appear as multiple IDs with different `dependencies` (the peer-dep / workspace fix). That semantic gap is why arcanis rejected reusing `scopes` "just for the name."

wesleytodd ([WinterTC55/admin#173](https://github.com/WinterTC55/admin/issues/173), [PR 62239](https://github.com/nodejs/node/pull/62239)) argues many package-map goals could be reconciled into an import-map-compatible form incrementally. arcanis argues the semantics are too different and reusing import-map field names with different meaning would mislead implementors. Both can be true: *conceptually related*, *not schema-compatible today*, *possible future translation layer*.

### Compatibility rating: package-map vs import maps spec

Ratings across five dimensions: JSON schema, authoring portability, conceptual problem overlap, future WinterTC alignment, and Nub's posture.

| Dimension | Rating | Notes |
|---|---|---|
| JSON schema | **0% — incompatible** | Different top-level keys, different value types, different algorithms |
| Authoring portability | **~0%** | No tool converts between them today; PMs will emit `package-map.json`, not WICG maps |
| Conceptual problem overlap | **~40%** | Both remap bare specifiers; package-map adds allowlists + multi-version IDs + exports composition |
| Future WinterTC alignment | **Unknown — exploratory** | Translation-to-import-maps discussed; not standardized |
| Nub posture | **Align with package-map** | Forward-implement PR 62239, not WICG |

## Does the Import Maps spec define a filename convention?

**No.** The WHATWG HTML living standard ([import maps section](https://html.spec.whatwg.org/multipage/webappapis.html#import-maps)) specifies:

- The **JSON representation**: optional top-level keys `imports`, `scopes`, `integrity`; module specifier map rules (non-empty keys, string URL values, trailing-slash pairing, longest-prefix wins).
- The **delivery mechanism in browsers**: a `<script>` element with `type="importmap"`, child text content = JSON, optionally `src` to fetch JSON.
- **Merge semantics** when multiple import maps exist in a document.
- **No requirement** for any particular filesystem filename, path, or discovery convention.

Filename choices are **host conventions**, not spec:

| Host | Convention | Spec strictness |
|---|---|---|
| Deno CLI | `--import-map=import_map.json` (any path) | Strict import-map JSON when using CLI flag |
| Deno config | `deno.json` `"importMap": "./import_map.json"` | Strict for external file; inline `imports` extends spec |
| Node (proposed, not shipped) | `--importmap` / `importmap.json` in [issue 49443](https://github.com/nodejs/node/issues/49443) discussion | Would have been host-defined |
| Node package-map (shipping) | `--experimental-package-map=./package-map.json`; PM suggestion `node_modules/.package-map.json` | **Not import maps** — different format entirely |
| Browsers | N/A — inline or `<script src="...">` | Spec governs content, not filename |

Historical note: WICG's earliest explainer ([initial commit](https://github.com/WICG/import-maps/commit/2ed3a0c)) used `<script type="packagemap" href="package-map.json">` with a **`packages`-shaped JSON** — an ancestor design that was **replaced** by the current `imports`/`scopes` import map before HTML integration. Today's `package-map.json` in Node PR 62239 is **not** a revival of that WICG draft; it is a new Node/package-manager design that coincidentally shares the word "package map."

## Implications for Nub

Four consequences: import maps are a Deno-and-browser mechanism, package-map is where the Node ecosystem is converging, a strict WICG map will not be read in package-map mode, and no filename is spec-backed.

1. **"Import maps" in the web-standard sense means Deno plus browsers**, not Bun, not Node, not Workers. Nub as a Node augmenter should not treat WICG import maps as the cross-runtime lingua franca for server-side remapping.

2. **Package-map is the Node-ecosystem convergence point** — Yarn (arcanis), pnpm (zkochan supportive), WinterTC interest-check. Nub's pivot to package-map matches that direction.

3. **A strict WICG `importmap.json` authored for Deno/browser portability** will not be read by Nub in package-map mode. For Deno-style remapping on Node, the portable paths are `package.json` `"imports"`, tsconfig paths, or a future package-map generator.

4. **No canonical import-map filename exists** — choosing `importmap.json` over `import_map.json` is host policy. Deno docs use `import_map.json`; Node issue threads used `importmap.json`. Neither is spec-backed.

## Sources

Specs, runtime docs, and issue threads behind the runtime-support table, the schema comparison, and the no-filename finding.

- [WHATWG HTML — Import Maps](https://html.spec.whatwg.org/multipage/webappapis.html#import-maps)
- [MDN — `<script type="importmap">`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script/type/importmap)
- [WICG/import-maps](https://github.com/WICG/import-maps) (historical; merged into HTML)
- [Deno modules docs — import maps](https://docs.deno.com/runtime/fundamentals/modules/)
- [denoland/deno#27539](https://github.com/denoland/deno/issues/27539) — strict vs extended import map behavior
- [denoland/deno#30689](https://github.com/denoland/deno/issues/30689) — import map merging proposal
- [oven-sh/bun#9863](https://github.com/oven-sh/bun/issues/9863) — no import map support
- [Bun module resolution docs](https://bun.sh/docs/runtime/modules)
- [nodejs/node#49443](https://github.com/nodejs/node/issues/49443) — Import Maps tracking
- [nodejs/node#62239](https://github.com/nodejs/node/pull/62239) — package maps implementation
- [WinterTC55/admin#173](https://github.com/WinterTC55/admin/issues/173) — package maps interest check
- [[research/import-maps-node-resistance]] — Node resistance deep-dive

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
