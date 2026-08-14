# Research: `${VAR}` expansion semantics + the `.env.local`-in-test convention

Companion to [[research/env-file-loading]]. Two questions raised on 2026-05-18:

1. **Variable expansion** — exact behavior across Vite, Bun, Next.js, Astro, SvelteKit, Nuxt, Remix, Node `--env-file`, `dotenv`, `dotenv-expand`, `dotenvx`. Nub expands; this establishes the precise rules.
2. **`.env.local` skipped under `NODE_ENV=test`** — where did this come from, is it universal, should Nub ship it?

## Contents

Two questions in order: what `${VAR}` expansion does across the ecosystem, and where the rule that skips `.env.local` under `NODE_ENV=test` came from.

- [TL;DR](#tldr)
- [Expansion feature matrix](#expansion-feature-matrix)
- [Expansion behavior, tool by tool](#expansion-behavior-tool-by-tool)
- [Origin of the `.env.local`-skip-in-test rule](#origin-of-the-envlocal-skip-in-test-rule)
  - [Patient zero: Ruby `dotenv` (2015)](#patient-zero-ruby-dotenv-2015)
  - [CRA picks it up: `react-scripts@1.0.1` (May 2017)](#cra-picks-it-up-react-scripts101-may-2017)
  - [Next.js inherits it](#nextjs-inherits-it)
  - [The convention spreads (and fragments)](#the-convention-spreads-and-fragments)
  - [Ruby dotenv reversed itself in 3.0 (2024)](#ruby-dotenv-reversed-itself-in-30-2024)
- [Test-skip per-tool status](#test-skip-per-tool-status)
- [Recommendation for Nub](#recommendation-for-nub)
  - [Expansion ruleset](#expansion-ruleset)
  - [`.env.local` in test](#envlocal-in-test)
  - [Edge cases and footguns to document](#edge-cases-and-footguns-to-document)
- [Sources](#sources)

## TL;DR

**Expansion.** The "just works" ecosystem rules — what Vite, Next.js, Bun, CRA, `dotenv-expand`, and `dotenvx` all converge on — are:

- Both `$VAR` and `${VAR}` syntax are accepted.
- `\$` escapes a literal dollar.
- Expansion sources are: shell `process.env` first, then keys previously defined in the same file or in higher-precedence `.env*` files. Whichever has a value wins; the file's own definition is used if shell didn't set it.
- Undefined variables expand to the empty string (not an error).
- Nested expansion works — `dotenv-expand` and Bun both walk the result of an expansion looking for more `$`s and re-expand. This is iterative, not lexical; cycles are guarded.
- Direct cycles (`A=$A`, or `A=$B`/`B=$A`) terminate via a "seen" set or short-circuit-on-no-change. Result is empty/last-value, not a stack overflow.
- Shell-style `${VAR:-default}` is supported by `dotenv-expand` and `dotenvx`. It is **not** in the explicit-Vite-docs surface, but Vite uses `dotenv-expand` so it works there too. Bun does **not** support it. Next.js does **not** document it.

**Test-skip.** The `.env.local`-skipped-when-`NODE_ENV=test` rule traces to a single decision in **Ruby `dotenv` (bkeepers), 2015–2017**. It was ported to **Create React App on 2017-05-19** (PR [facebook/create-react-app#2250](https://github.com/facebook/create-react-app/pull/2250), authored by Dan Abramov, citing "matches what bkeepers/dotenv does"). **Next.js** copied it from there. **Bun** copied it from Next.js / CRA. **`dotenv-flow`** copied it. **The Ruby original was reversed in `dotenv` 3.0 (early 2024)** — the file is now loaded by default, opt-out via Rails config. So the JS ecosystem is currently more opinionated about this rule than the source-of-truth Ruby library that birthed it.

**Vite, Astro, SvelteKit, Nuxt, and Remix do NOT skip `.env.local` in test.** Vite explicitly loads `.env.local` in every mode, including `--mode test`. The skip is a Next.js / Bun / CRA-lineage behavior, not a universal convention.

**Recommendation:** Ship the universally-agreed expansion subset (both syntaxes, `\$` escape, shell-then-file precedence, undefined → empty, nested with cycle guard). Defer `${VAR:-default}` to post-v1. **Do ship the test-skip rule** despite Vite's divergence — it is the safer default, it matches what Next.js / CRA / Bun / `dotenv-flow` users expect, and the Vite divergence is a known ecosystem complaint, not a deliberate stance. Document loudly that this differs from Vite; provide an off-switch for Vite-shaped projects (project-scope `package.json` field, not a `NUB_*` env var).

## Expansion feature matrix

Status per tool, derived from official docs + source. "Y" = yes, "N" = no, "—" = not documented and behavior is library-dependent.

| Feature                                | `dotenv` (core) | `dotenv-expand` | `dotenvx`   | Node `--env-file` | Vite (via dotenv-expand) | Next.js (`@next/env`) | Bun         | Astro (Vite) | SvelteKit (Vite) | Nuxt (`dotenv`) | Remix (`dotenv`) |
| -------------------------------------- | --------------- | --------------- | ----------- | ----------------- | ------------------------ | --------------------- | ----------- | ------------ | ---------------- | --------------- | ---------------- |
| `$VAR` syntax                          | N               | Y               | Y           | N                 | Y                        | Y                     | Y           | Y            | Y                | N               | N                |
| `${VAR}` syntax                        | N               | Y               | Y           | N                 | Y                        | — (likely Y)          | Y           | Y            | Y                | N               | N                |
| `\$` escape                            | N/A             | Y               | Y           | N/A               | Y                        | Y                     | Y           | Y            | Y                | N/A             | N/A              |
| Nested expansion (A→B→C)               | N/A             | Y (iterative)   | Y           | N/A               | Y                        | Y                     | Y           | Y            | Y                | N/A             | N/A              |
| Cycle detection                        | N/A             | Y (seen-set)    | Y           | N/A               | Y                        | — (likely)            | — (likely)  | Y            | Y                | N/A             | N/A              |
| `${VAR:-default}`                      | N/A             | Y               | Y           | N                 | Y (via dotenv-expand)    | N (not documented)    | N           | Y            | Y                | N/A             | N/A              |
| `${VAR:?error}`                        | N/A             | N               | Y           | N                 | N                        | N                     | N           | N            | N                | N/A             | N/A              |
| `$(cmd)` command substitution          | N/A             | N               | Y           | N                 | N                        | N                     | N           | N            | N                | N/A             | N/A              |
| Expand against `process.env`           | N/A             | N (v12+)        | Y           | N/A               | Y (config) / no (runtime) | Y                     | Y           | Y            | Y                | N/A             | N/A              |
| Expand against other file keys         | N/A             | Y               | Y           | N/A               | Y                        | Y                     | Y           | Y            | Y                | N/A             | N/A              |
| Order: shell-first vs file-first       | N/A             | file-only (v12) | shell-first | N/A               | shell-first              | shell-first           | shell-first | shell-first  | shell-first      | N/A             | N/A              |
| Undefined var → empty string           | N/A             | Y               | Y           | N/A               | Y                        | Y                     | Y           | Y            | Y                | N/A             | N/A              |

Three callouts on the matrix:

1. **`dotenv-expand` v12 (early 2026)** stopped expanding against `process.env` — now expands only against the parsed file's own keys. This was a breaking change driven by security concerns (preventing shell injection via env values that themselves contain `$REFS`). Vite users who relied on shell-env interpolation were the loudest complainants; the project now points them at `dotenvx`. Nub should not copy this: shell-first is what users expect.
2. **Nuxt and Remix use `dotenv` core**, not `dotenv-expand`, so they do *no* expansion by default. Users who want `${VAR}` behavior in Nuxt/Remix wire up `dotenv-expand` (or now `dotenvx`) themselves. This is a known source of "why doesn't `${X}` work in my Nuxt project" Stack Overflow threads.
3. **Node `--env-file` does no expansion at all**, deliberately — the Node team has rejected expansion proposals citing the `PASSWORD=foo$bar`-silently-truncates footgun. Nub's `--env-file=` preserves this for byte-identical `nub --env-file=` → `node --env-file=` behavior, but Nub's *default eager load* expands. Already decided; the open question was resolved 2026-05-17.

## Expansion behavior, tool by tool

Per-tool behavior, from Vite's `dotenv-expand` pairing through Bun, Next.js, the Vite-derived frameworks, Nuxt and Remix on bare `dotenv`, Node's non-expanding `--env-file`, and `dotenvx`.

**Vite.** Uses `dotenv` + `dotenv-expand` internally. Both `$VAR` and `${VAR}` accepted. `\$` escapes a literal `$`. The Vite docs explicitly mention "reverse-order expansion" (a variable referencing one defined later in the file), warning it's interop-hostile. Expansion against `process.env`: yes during `loadEnv` calls in the config phase, but Vite's runtime `import.meta.env` exposes post-expansion values regardless. Undefined variables produce empty strings.

**Bun.** Documented in [[research/env-file-loading#Bun's behavior in detail#Parser / expansion syntax|`research/env-file-loading.md`]] already. Both syntaxes. `\$` escape. No `${VAR:-default}`. Expansion runs after the file is fully parsed and references shell env + this-file values. A `.env.local` cannot re-template a value defined in `.env` cross-file (it can only see *this file's* values plus the shell env that was loaded first).

**Next.js.** Docs only show `$VAR`; `${VAR}` is not in the official example but is supported because `@next/env` uses `dotenv-expand` under the hood. `\$` escape documented. Expansion against `process.env` is on (Next loads shell env first, then files). Multi-file expansion works (a value in `.env.local` can reference a key from `.env`, because higher-precedence files are loaded first but expansion runs against the accumulated state).

**Astro, SvelteKit.** Inherit Vite's loader verbatim. Same expansion semantics as Vite.

**Nuxt, Remix.** Use `dotenv` core. No expansion. Users who want it add `dotenv-expand` or `dotenvx` themselves.

**Node `--env-file`.** No expansion. `PASSWORD=foo$bar` is the literal seven-character string `foo$bar`. Stable as of v24.10 / v22.21.

**`dotenv` (motdotla, core).** No expansion. README explicitly redirects users to `dotenvx`.

**`dotenv-expand`.** Both syntaxes. `\$` escape. Nested expansion via iterative resolution. Cycle detection via a `seen` Set; if a variable references itself (directly or indirectly), the result is empty string rather than infinite recursion. `${VAR:-default}` and `${VAR-default}` supported. `${VAR:+alt}` and `${VAR+alt}` supported. Since v12, expansion no longer consults `process.env`.

**`dotenvx`.** Superset of `dotenv-expand`. Adds `$(cmd)` command substitution, `${VAR:?error}`, and encrypted-file support. This is where motdotla (the original `dotenv` author) is pushing the ecosystem. Maintained, actively developed, but not the v0 baseline for any framework runtime yet.

## Origin of the `.env.local`-skip-in-test rule

The rule traces to Ruby `dotenv` in 2015, entered JavaScript through Create React App in 2017, spread from there to Next.js, Bun and `dotenv-flow`, and was reversed in Ruby `dotenv` 3.0.

### Patient zero: Ruby `dotenv` (2015)

The earliest implementation of the "skip `.env.local` in test" convention is in `bkeepers/dotenv` (the Ruby gem).

The Rails integration (`dotenv-rails`) added logic that loaded different files depending on `Rails.env`, with the explicit rule that the `test` environment skipped `.env.local`. The rationale, as stated across the issue tracker and PR threads: **tests should be reproducible across machines and CI runners, and `.env.local` exists precisely to hold machine-local overrides that should not participate in the reproducible test environment.**

### CRA picks it up: `react-scripts@1.0.1` (May 2017)

CRA's PR [#1344](https://github.com/facebook/create-react-app/pull/1344) (merged 2017-04, shipped in `react-scripts@1.0.0` 2017-05) added multi-file `.env*` support, modeled on Ruby dotenv. The initial implementation included `.env.local` in the test priority list.

Less than a month later, Dan Abramov filed [#2250](https://github.com/facebook/create-react-app/pull/2250) ("Ignore `.env.local` in test environment"), merged 2017-05-19, shipped in `react-scripts@1.0.1`. The PR body cites the Ruby behavior directly: *"matches both what [bkeepers/dotenv](https://github.com/bkeepers/dotenv) does, and what we claim we do in the docs."*

This is the canonical entry point into the JavaScript ecosystem. Every subsequent JS implementation traces back to either this CRA decision or to Ruby dotenv via independent porting.

### Next.js inherits it

Next.js added `.env*` support in v9.4 (2020). The documentation inherits the CRA model wholesale, including the test-skip rule. The current Next.js docs state, verbatim:

> There is a small difference between `test` environment, and both `development` and `production` that you need to bear in mind: `.env.local` won't be loaded, as you expect tests to produce the same results for everyone. This way every test execution will use the same env defaults across different executions by ignoring your `.env.local` (which is intended to override the default set).

That paragraph is *the* canonical statement of the rationale in JS-ecosystem terms; every other doc citing this rule either paraphrases it or links it.

`@next/env`, the underlying package, hard-codes the skip — there is no flag. Jest configs, Drizzle configs, Prisma configs that use `@next/env` inherit the skip.

### The convention spreads (and fragments)

- **`dotenv-flow`** (kerimdzhanov): copied the rule explicitly. Docs state *"the `.env.local` file is not listed for 'test' environment, since normally you expect tests to produce the same results for everyone."* This is the third independent statement of the same rationale.
- **Bun**: copied from CRA/Next. `env_loader.zig` line 685: `if (comptime suffix != .@"test")`. No documentation in Bun's user-facing docs; the behavior is silent.
- **Vite**: did **not** copy. Vite's `loadEnv` loads `.env.local` in every mode including `test`. The Vite docs are explicit: *"the following two files are loaded in all cases: `.env`, `.env.local`."* Vite's design choice was instead to make `mode` distinct from `NODE_ENV`, putting the burden on the user to be explicit about which env file they want loaded for tests.
- **Astro, SvelteKit**: inherit Vite — no skip.
- **Nuxt, Remix**: only load `.env` (single-file), so the question doesn't arise.
- **Node `--env-file`**: no multi-file handling at all — the question doesn't arise.
- **Deno**: same.

So the JS-ecosystem distribution is roughly:

- **Skip in test**: CRA, Next.js, `@next/env`, `dotenv-flow`, Bun.
- **Load in test**: Vite, Astro, SvelteKit (all via Vite).
- **N/A** (no multi-file): Nuxt, Remix, raw `dotenv`, Node `--env-file`, Deno.

The Next.js side is bigger by user-share but Vite's user base is non-trivial. **The convention is not universal.**

### Ruby dotenv reversed itself in 3.0 (2024)

Ruby `dotenv` 3.0 (released early 2024) **reversed** the test-skip rule. The current README shows `.env.local` as loaded in *every* environment including test.

The reversal was driven by issue [bkeepers/dotenv#418](https://github.com/bkeepers/dotenv/issues/418) and PR [#417](https://github.com/bkeepers/dotenv/pull/417). The core argument from the issue submitter:

> Most Rails applications require a database, and the URL of that database may vary between different developer machines and between developer machines and a CI service. Preventing `.env.local` loading while still allowing `.env.test.local` forces developers to duplicate variables that legitimately belong in a single local-overrides file.

bkeepers closed #417 without merging because dotenv 3.0 instead shipped a more general "user picks which files load" mechanism for Rails integration. But the *practical effect* on default behavior is that the **source-of-truth library that started this convention no longer follows it as a default.**

The JS ecosystem hasn't responded — CRA is in maintenance, Next.js docs haven't updated, `dotenv-flow` is stable. So the JS convention as it stands in 2026 is "skip in test," frozen from a Ruby decision that has since been reversed in Ruby.

## Test-skip per-tool status

Whether each tool skips `.env.local` under `NODE_ENV=test`, whether it documents that behavior, and whether the behavior is configurable.

| Tool                | Skips `.env.local` in test? | Documented? | Configurable? |
| ------------------- | --------------------------- | ----------- | ------------- |
| CRA                 | Y                           | Y           | N             |
| Next.js / `@next/env` | Y                         | Y           | N             |
| Bun                 | Y                           | N           | N             |
| `dotenv-flow`       | Y                           | Y           | Y (via opts)  |
| `@vercel/style-guide` & most Next templates | Y | Y     | N             |
| Vite                | N                           | Y (loads)   | Y (loadEnv)   |
| Astro               | N (via Vite)                | N           | Y (loadEnv)   |
| SvelteKit           | N (via Vite)                | N           | Y (loadEnv)   |
| Nuxt                | N/A (`.env` only)           | N           | Y (--dotenv)  |
| Remix v2            | N/A (`.env` only)           | Y           | N             |
| Ruby `dotenv` ≥3.0  | N (reversed)                | Y           | Y             |
| Ruby `dotenv` <3.0  | Y                           | Y           | N             |
| Node `--env-file`   | N/A (single file)           | Y           | N             |
| Deno                | N/A                         | Y           | N             |

## Recommendation for Nub

Ship the universal expansion subset and the test-skip rule; defer `${VAR:-default}`, `$(cmd)` and `${VAR:?error}`; document the footguns the expansion inherits.

### Expansion ruleset

Ship the "universal ecosystem" subset. The parser implements:

- **Both `$VAR` and `${VAR}` syntax.** No reason to pick one — every expanding tool supports both.
- **Escape `\$` for literal dollar.** Matches Bun, Next.js, Vite, `dotenv-expand`. The escape is the single most-used parser feature besides expansion itself.
- **Resolution order: shell `process.env` first, then accumulated `.env*` values, then this-file values.** A variable defined in `.env` is visible to `.env.local`'s expansion because `.env` is loaded first into the accumulator. Shell wins everywhere; this matches every tool except `dotenv-expand` v12.
- **Undefined variable → empty string.** Matches every tool. Document loudly — this is a silent footgun. (Anything else breaks too many existing `.env` files.)
- **Iterative nested expansion with cycle guard.** Walk the result of an expansion looking for more `$`. Stop when no `$` remains, or when the same intermediate has been seen before. Cycles resolve to empty string, not stack overflow.
- **Do NOT support `${VAR:-default}` in v0.** Vite supports it (via `dotenv-expand`), Bun does not, Next.js doesn't document it. Cost to ship is low but it expands the contract; deferring keeps the parser smaller and it can be added later if a real use case shows up.
- **Do NOT support `$(cmd)` command substitution.** This is `dotenvx`-only. Security hazard (arbitrary command execution at env-load time, before user code), and no demand from the Vite / Next / Bun users being targeted.
- **Do NOT support `${VAR:?error}`.** Same reasoning as `:-` defaults.
- **Expansion does NOT run for `--env-file=`.** Already decided: `nub --env-file=` matches Node `--env-file=` byte-for-byte (no expansion). The eager default-load path is the only place expansion runs — the same split users implicitly understand, that the Node-compat flag gets Node behavior.

This is the minimal viable expansion that satisfies the Vite / Next / Bun user base (the three biggest target populations), and the matrix above confirms the subset is the universal intersection.

### `.env.local` in test

**Recommendation: ship the skip rule. Default on.**

1. **It's the safer default.** The failure mode the rule prevents is real: a developer's `.env.local` has `STRIPE_KEY=sk_live_…` (their local dev account), CI runs tests, one of the tests accidentally hits Stripe live. Reproducibility beats convenience for tests.
2. **It matches Next.js, CRA, Bun, `dotenv-flow`** — collectively the largest user base for `.env*` conventions in the JS ecosystem.
3. **It diverges from Vite / Astro / SvelteKit** — non-trivial. The Vite-shaped user will be surprised that "my `.env.local` isn't loaded when I run `nub --node-env=test ...`." Mitigation: document the divergence prominently and provide a project-scope off-switch.
4. **The Ruby reversal is a yellow flag, not a red one.** Ruby's reversal happened because `dotenv-rails`'s test environment is coupled to ActiveRecord database URLs in a way that has no JS analog — Rails test env *requires* a different DB URL than dev, and forcing duplication into `.env.test.local` is friction for a shape of project the JS ecosystem doesn't have. The Rails-specific motivation does not translate.
5. **The cost of the rule is low and the cost of *not* shipping it is high.** Without it, Next.js / CRA users file bugs like "Nub loaded my local Stripe key into the CI test run" and the rule gets added retroactively. With it, Vite-shaped users hit a documented divergence and have an off-switch.

**Off-switch shape.** Per the no-`NUB_*`-env-var policy, this lives as a flat `package.json` field (final spelling TBD per the brand-namespace rule). Something like:

```json
{
  "envFileLoadLocalInTest": true
}
```

— exposed flatly, no `"nub"` namespace, opt-in (default is the skip). Document it alongside the other disable mechanisms.

### Edge cases and footguns to document

These belong in user-facing docs:

1. **Undefined `${VAR}` expands to empty string, not an error.** A `.env` with `DATABASE_URL=postgres://${USER}:${PASS}@host` where `USER` is unset produces `postgres://:@host`. This is the most common cause of cryptic connection failures in `dotenv-expand`-shaped tools and Nub inherits it. A one-time warning on first observed empty expansion is cheap and a high-value diagnostic.
2. **Escape literal `$` with `\$`, not `$$`.** Shell users sometimes try `$$` (which works in `make` and `docker-compose` but not in dotenv-expand). Document the difference.
3. **`PASSWORD=foo$bar` truncates silently to `foo`.** The single biggest footgun the Node team cited when rejecting expansion for `--env-file`. Nub accepts the hit (it's the only way to match Vite/Next user expectations) but should call it out in docs and migration guides. Recommended fix in user docs: quote the value, `PASSWORD="foo$bar"`, AND escape, `PASSWORD="foo\$bar"`.
4. **`.env.local` skip is silent.** No log line in default mode. Users debugging "why isn't my env loaded?" should run with `--print-env-load` (a diagnostic flag, not in v0 scope but worth considering). At minimum, document the test-skip prominently alongside the precedence stack.
5. **Cycles silently produce empty strings.** A → B → A becomes empty for both. Cycle-detection-via-seen-set means there's no crash; the user sees mysteriously empty values. This is `dotenv-expand`-conformant behavior; document it.

## Sources

PR / issue / source-code primary sources:

- [facebook/create-react-app#1344](https://github.com/facebook/create-react-app/pull/1344) — initial multi-file `.env` support in CRA (April 2017)
- [facebook/create-react-app#2250](https://github.com/facebook/create-react-app/pull/2250) — "Ignore `.env.local` in test environment" (Dan Abramov, merged 2017-05-19)
- [facebook/create-react-app#3387](https://github.com/facebook/create-react-app/pull/3387) — `dotenv-expand` support in CRA
- [facebook/create-react-app CHANGELOG-1.x](https://github.com/facebook/create-react-app/blob/main/CHANGELOG-1.x.md) — `react-scripts@1.0.1`, `1.1.0`, `1.1.1`
- [bkeepers/dotenv#280](https://github.com/bkeepers/dotenv) — original Ruby skip-in-test PR
- [bkeepers/dotenv#418](https://github.com/bkeepers/dotenv/issues/418) — "should be loaded in test" issue
- [bkeepers/dotenv#417](https://github.com/bkeepers/dotenv/pull/417) — proposed reversal (closed in favor of dotenv 3.0 reconfig)
- [motdotla/dotenv-expand#55](https://github.com/motdotla/dotenv-expand/issues/55) — "Bring your own process.env" (v12 context)
- [motdotla/dotenv-expand#3](https://github.com/motdotla/dotenv-expand/pull/3) — recursive expand
- [DeepWiki: dotenv-expand expand() function](https://deepwiki.com/motdotla/dotenv-expand/5.1-expand()-function) — cycle detection via seen-Set

Documentation:

- [Vite — Env Variables and Modes](https://vite.dev/guide/env-and-mode)
- [Next.js — Environment Variables](https://nextjs.org/docs/app/guides/environment-variables) — canonical test-skip rationale prose
- [Bun — Environment variables](https://bun.com/docs/runtime/env)
- [Create React App — Custom env vars](https://create-react-app.dev/docs/adding-custom-environment-variables/)
- [Astro — Environment Variables](https://docs.astro.build/en/guides/environment-variables/)
- [SvelteKit — `$env/static/private`](https://svelte.dev/docs/kit/$env-static-private)
- [Nuxt — `.env`](https://nuxt.com/docs/4.x/directory-structure/env)
- [Remix v2 — Env vars](https://v2.remix.run/docs/guides/envvars)
- [dotenv (motdotla) README](https://github.com/motdotla/dotenv)
- [dotenvx — env file interpolation](https://dotenvx.com/docs/env-file)
- [dotenv-flow README](https://github.com/kerimdzhanov/dotenv-flow)
- [Node `--env-file`](https://nodejs.org/api/cli.html#--env-fileconfig)

Companion Nub docs:

- [[research/env-file-loading|`research/env-file-loading.md`]] — broader survey, Bun footguns, precedence stack

## Changelog

Every revision to this document, with the date and what changed.

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
