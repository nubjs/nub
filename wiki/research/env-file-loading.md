# Research: eager `.env` file loading

Compiled 2026-05-16 by reading Bun's source (`src/dotenv/env_loader.zig`, `src/runtime/cli/run_command.zig` on `main`), Node's `--env-file` docs and changelog, Deno's CLI reference, and a long tail of GitHub issues across Bun, Vite, Next.js, dotenv, and the framework ecosystem.

## Contents

- [TL;DR](#tldr)
- [Bun's behavior in detail](#buns-behavior-in-detail)
  - [Which files](#which-files)
  - [Precedence](#precedence)
  - [NODE_ENV handling](#node_env-handling)
  - [Search location](#search-location)
  - [Which commands load env files](#which-commands-load-env-files)
  - [Parser / expansion syntax](#parser--expansion-syntax)
  - [Disable mechanisms](#disable-mechanisms)
- [Ecosystem conventions](#ecosystem-conventions)
  - [dotenv](#dotenv)
  - [dotenv variants](#dotenv-variants)
  - [Next.js](#nextjs)
  - [Vite (and Astro / SvelteKit by extension)](#vite-and-astro--sveltekit-by-extension)
  - [Remix / Nuxt](#remix--nuxt)
  - [Node `--env-file`](#node---env-file)
  - [Deno](#deno)
  - [CI runners and hosts](#ci-runners-and-hosts)
- [Conflict surface](#conflict-surface)
  - [Eager load vs user `dotenv.config()`](#eager-load-vs-user-dotenvconfig)
  - [Eager load vs framework dev servers](#eager-load-vs-framework-dev-servers)
- [Maintainer engagement on the six issues](#maintainer-engagement-on-the-six-issues)
- [Bun footguns (do not copy)](#bun-footguns-do-not-copy)
- [Synthesis](#synthesis)
- [Sources](#sources)

## TL;DR

- The `.env` + `.env.local` + `.env.[NODE_ENV]` + `.env.[NODE_ENV].local` taxonomy with **first-writer-wins** precedence is the ecosystem consensus (Next.js, Vite, dotenv-flow). Bun copied it.
- **Shell `process.env` always wins** in every userspace loader and in Bun. No-override-against-existing is the universal default. Node's built-in `--env-file` is the outlier — multi-flag invocations later-override earlier, but they still don't beat the live environment.
- `.env.local` is **skipped under `NODE_ENV=test`** everywhere (Next, dotenv-flow, Vite-via-mode, Bun). Universal.
- `${VAR}` expansion is **not** default in `dotenv` v17 or Node `--env-file`. Bun does expand by default and it bites people whose `.env` contains literal `$` (shell passwords).
- Bun's worst footguns: (1) `std.fs.cwd()` with no walk-up — root `.env` invisible from monorepo subpackages or git worktrees; (2) defaults `.env.development` suffix even when `NODE_ENV` is unset; (3) `bun run <script>` skips defaults and relies on the spawned child being `bun` — breaks for `bun run prisma`, `bun run next dev` (Next handles its own), `bunx some-cli`.
- CI/hosts (GitHub Actions, Vercel, Fly, Cloudflare, Netlify) never auto-load `.env`. It is unambiguously a dev-time artifact, which means aggressive eager loading is safe in dev and irrelevant in prod hosts (no `.env` to load).
- Eager pre-loading is **safe from a `dotenv` standpoint** (their default no-override makes user `dotenv.config()` calls into no-ops for overlapping keys), but **fights framework loaders** when the runtime loads mode-specific files using its own definition of "mode" — Vite, Next, Astro all break this way.

## Bun's behavior in detail

### Which files

Bun's `Loader` struct declares eight named slots (`src/dotenv/env_loader.zig` 11–18):

```
.env.local
.env.development
.env.production
.env.test
.env.development.local
.env.production.local
.env.test.local
.env
```

Exactly one suffix (`development` / `production` / `test`) is active per process, picked from `BUN_ENV` or `NODE_ENV`. The docs publicly mention only four files; `.env.{NODE_ENV}.local` is loaded but undocumented.

### Precedence

Documented (lowest → highest):

1. `.env`
2. `.env.{NODE_ENV}`
3. `.env.local`
4. `.env.{NODE_ENV}.local` (undocumented)

Implementation reads files **highest-first** with `override: false` on every call → **first writer wins** → same effective ordering as "load lowest first and overwrite," but cheaper. Inside a single file later definitions of a key do override earlier ones.

`loadProcess()` populates the map from `std.os.environ` *before* any file is read, so shell env beats every `.env` file unconditionally. There is no flag to invert this for default loading.

Multiple `--env-file` args process **right-to-left** so the last flag wins. Comma-separated paths inside one flag follow the same right-to-left rule.

In `test` mode, `.env.local` is **deliberately skipped** (env_loader.zig line 685: `if (comptime suffix != .@"test")`).

Final effective precedence (highest wins):

```
shell environ / process.env
  └─ --env-file=… args (right-to-left, comma-separated)
      └─ .env.{NODE_ENV}.local
          └─ .env.local (skipped if NODE_ENV=test)
              └─ .env.{NODE_ENV}
                  └─ .env
```

### NODE_ENV handling

- `process.env.NODE_ENV` is **left undefined** if shell + `.env` files don't set it (since PR #9695; before that Bun defaulted to `development` and broke Vite/Next builds).
- But the **file-selection suffix** still defaults to `development` when `NODE_ENV` is unset, because `isProduction()`/`isTest()` fall through to `false`. So `process.env.NODE_ENV === undefined` *and* Bun loaded `.env.development`. This surprises people. See oven-sh/bun#13377.
- `bun test` auto-sets `NODE_ENV=test` unless the user set it elsewhere (including in a `.env` file — meaning `.env` with `NODE_ENV=development` makes `bun test` run with `NODE_ENV=development` and read `.env.development`, not `.env.test`).
- `bun build` and `bunx` do **not** auto-set NODE_ENV (unlike Vite, whose `build` command implies `mode=production`).
- Setting `NODE_ENV=production` *inside* `.env` doesn't retroactively switch the file-selection suffix — by the time `.env` is parsed, the suffix was already chosen from the empty pre-file environment.

### Search location

`std.fs.cwd()`. No walk-up, no package.json detection, no git-root detection, no workspace-root awareness. Six related open issues: oven-sh/bun#5836, #10358, #11190 (a 1.1.8 regression where root `.env` stopped working from subpackages), #27493 (git worktree cwd issue).

### Which commands load env files

- `bun <file>` — loads defaults
- `bun run <file>` — loads defaults
- `bun test` — loads defaults with `test` suffix
- `bun build` — loads defaults
- **`bun run <package.json script>` — does NOT load defaults.** The comment in env_loader.zig lines 617–623 spells it out: *"it is the responsibility of the script's instance of bun to load .env"*. Works fine when the script is `"dev": "bun server.ts"` (inner bun loads). **Breaks** when the script is `"dev": "next dev"` (next handles its own with different rules) or `"dev": "prisma migrate"` (nobody loads → oven-sh/bun#23962, open Oct 2025).
- `bunx <pkg>` — same problem as above when the resolved binary's shebang is `#!/usr/bin/env node` rather than bun.
- `bun install` — calls `loadProcess()` (reads shell) but does NOT parse `.env`. Users frequently misunderstand this — `NPM_TOKEN` in `.env` doesn't help `bun install`.

### Parser / expansion syntax

- Quoting: backtick, double-quote, single-quote. Inside `"..."`, `\n`/`\r`/`\\` decoded; inside `'...'` and backticks, no escape processing. Multi-line: yes, inside quotes.
- Comments: `#` starts a line comment or inline comment after an unquoted value. `#` inside quotes is literal.
- Key charset `[a-zA-Z0-9_\-.]+` (includes `-` and `.`, unusual).
- `export KEY=VALUE` prefix is stripped.
- **`KEY: VALUE` YAML-style accepted** — quirky and undocumented; writing `URL: https://...` parses as key `URL` value `https://...`.
- `$VAR` and `${VAR}` both expand. Escape literal `$` with `\$`.
- Expansion runs **after** the file is fully parsed, and only against keys added by *this* file (`.env.local` cannot re-template a value defined in `.env`).
- No `${VAR:-default}` / `${VAR:?error}` operators (dotenv-expand supports them, Bun does not).

### Disable mechanisms

- `--no-env-file` (PR #24767, merged 2025-11-17). All-or-nothing for defaults. `--env-file=foo.env` still works alongside it.
- bunfig.toml: `env = false`, `env.file = false`.
- `BUN_OPTIONS="--no-env-file"` prepends args.
- No per-file disable. Cannot "load `.env` but skip `.env.local`."

## Ecosystem conventions

### dotenv

- ~121–126M weekly downloads (mid-2026). ~76k direct dependents. Snyk-classified "Key ecosystem project."
- Loads `path.resolve(process.cwd(), '.env')`. Single file.
- **Never overrides** existing env vars by default. Opt-in via `{ override: true }`.
- No `${VAR}` expansion. README points users at `dotenvx` for that.
- Classic preload: `node -r dotenv/config app.js` or `import 'dotenv/config'`.
- v17.0.0 (early 2026): `quiet` flipped to `false` — now logs *"loaded X env vars from .env"* on every import. Users hate this.

### dotenv variants

- **`dotenv-expand`** — adds `${VAR}` and `$VAR` interpolation. v12.0.0 stopped expanding against `process.env` (only file values).
- **`dotenv-flow`** — defines the multi-file order everyone copied: `.env` < `.env.local` < `.env.${NODE_ENV}` < `.env.${NODE_ENV}.local`. Skip `.env.local` when `NODE_ENV=test`.
- **`dotenvx`** — by motdotla (original `dotenv` author). Drop-in replacement; adds ECIES encryption (commit `.env` safely), expansion with defaults/command substitution, and a CLI. This is where the official `dotenv` README now points for expansion. Direction the ecosystem is steering.

### Next.js

Lookup order (highest → lowest, stop at first hit):

1. `process.env`
2. `.env.$(NODE_ENV).local`
3. `.env.local` *(skipped if `NODE_ENV=test`)*
4. `.env.$(NODE_ENV)`
5. `.env`

`NODE_ENV` allowed values: `development | production | test`. `next dev` → `development`. Other `next` commands → `production`.

`NEXT_PUBLIC_` prefix → baked into the client bundle at build time (not runtime). Expansion: `$VAR` only, no `${}` documented.

`@next/env` exports `loadEnvConfig(projectDir)` — the actual implementation, used by Jest/Drizzle/Prisma configs.

### Vite (and Astro / SvelteKit by extension)

Files (lowest → highest):

1. `.env`
2. `.env.local`
3. `.env.[mode]`
4. `.env.[mode].local`

**`mode` vs `NODE_ENV` are deliberately decoupled.** `mode` is a Vite CLI concept (`vite --mode staging`). `NODE_ENV` controls React/Vue dev-vs-prod. `vite build --mode staging` builds with `NODE_ENV=production` while loading `.env.staging`.

Only `VITE_`-prefixed (configurable) vars are exposed to client code on `import.meta.env`. Vite uses `dotenv` + `dotenv-expand` internally.

Astro inherits Vite's loader; client prefix `PUBLIC_`. SvelteKit also inherits Vite's loader; client prefix `PUBLIC_`.

### Remix / Nuxt

- **Remix v2**: auto-loads `.env` via built-in dotenv during `remix dev` only. No multi-file/mode. Production (`remix serve`) reads host env only.
- **Nuxt**: auto-loads `.env` at dev, build, and generate time only. Production runtime does **not** read `.env` — host env only. Custom file via `--dotenv .env.local`. Public env requires `NUXT_` prefix.

### Node `--env-file`

- Added v20.6.0. **Stable as of v24.10.0 / v22.21.0** (declared non-experimental late 2025).
- `--env-file-if-exists` added v22.9.0, also stable v24.10.0.
- Missing file: `--env-file` throws; `--env-file-if-exists` silently skips.
- Multiple flags: **later overrides earlier** (per docs). Opposite of `dotenv`'s default and opposite of Deno's first-wins.
- **No `${VAR}` expansion.**
- Configures Node itself: `NODE_OPTIONS` set via `--env-file` is applied to Node startup (userspace `dotenv` can't do this).
- Open issues: nodejs/node#54134 (inner quotes terminate parse, divergent from `dotenv`), #59897 (large multi-line values silently fail).

### Deno

- **No auto-load.** Must pass `--env-file` (or `Deno.loadEnv`).
- `--env-file` with no value defaults to `.env` in cwd; also **walks up parents** to find one (unique among runtimes).
- Multiple `--env-file` flags: **first-wins** (`*"Only the first environment variable with a given key is used."`*). Opposite of Node, matches `dotenv`.
- No `${VAR}` expansion.

Cleanest behavior in the ecosystem.

### CI runners and hosts

**None auto-load `.env`.** GitHub Actions (env from workflow YAML + `GITHUB_ENV`), Vercel (dashboard / `vercel env`), Fly (`fly secrets`
+ `[env]` in `fly.toml`), Cloudflare Pages/Workers (dashboard or
`wrangler.toml`; local dev uses `.dev.vars`), Netlify, Railway, Render, AWS Amplify — all the same pattern: env from runner config, not from repo `.env`.

`.env*` is unambiguously a **dev-only artifact**. Aggressive eager loading is safe in dev because production hosts won't have a `.env` to load anyway.

## Conflict surface

### Eager load vs user `dotenv.config()`

If the runtime loads `.env` into `process.env` before user code, and the user code calls `dotenv.config()`:

- **Same keys**: `dotenv` no-overrides existing env by default, so its call is a no-op for overlapping keys. Works fine.
- **`{ override: true }`**: user expects the file to win and clobbers whatever the runtime set. Usually fine; can confuse if the runtime loaded *more* files than `dotenv` does (e.g., runtime loaded `.env.development` too, user clobbers back to `.env`).
- **`dotenv-expand`**: post-v12, doesn't expand against `process.env`. Runtime pre-population breaks any `${VAR}` interpolation that depended on the file value being processed by dotenv-expand. Users switch to `dotenvx` to fix.
- **Extra keys**: runtime loaded `.env.local`, user only loaded `.env`. User sees "where did this come from?" keys. Acceptable trade.
- **Tests**: if the runtime loads `.env.local` without the test-mode exclusion, machine-local creds leak into CI tests. Easily fixed.

### Eager load vs framework dev servers

This is the real foot-shoot. Bun-vs-Vite incident (vitejs/vite#20942, oven-sh/bun#13377):

1. `bun run vite build` invokes the bun runtime.
2. Bun defaults the file-selection suffix to `development` (NODE_ENV unset → falls through to development), pre-loads `.env.development` into `process.env`.
3. Vite then calls `loadEnv('production', ...)` to load `.env.production`. But Vite respects existing env — its loader has the same no-override default as `dotenv`.
4. `vite build` ships with **development** env values, silently.

Root cause: a runtime that loads mode-specific env files using its own definition of "mode" will fight any framework with a different definition. Vite's `mode` ≠ `NODE_ENV`.

**Safe under any framework**: pre-loading plain `.env` (and `.env.local` with test-mode exclusion). The framework would have loaded those anyway, no-override means same values either way.

**Dangerous**: pre-loading `.env.[NODE_ENV]` based on the runtime's choice of NODE_ENV. The framework defaults to no-override and can't correct the runtime's mistake.

Surgical fix candidate: detect framework binaries in `node_modules/.bin` (`vite`, `next`, `astro`, `nuxt`, `svelte-kit`, `remix`) and skip eager loading entirely — let the framework own env.

## Maintainer engagement on the six issues

Followed up on 2026-05-16 by reading the comment threads on the six Bun issues cited above (#5836, #10358, #11190, #13377, #23962, #27493). **None of them have any maintainer comments.** No Jarred Sumner, no Dylan Conway, no Ciro Spaciari, no Ashcon Partovi. The only Bun-org activity is the dedupe-bot on #27493 flagging it as a duplicate of #11190.

The only substantive design commentary anywhere on the six threads is from a community contributor (theoephraim) on #23962, who read the source and surfaced the actual rule:

> "bun special cases package.json scripts, and skips loading env vars. This is to help handle the situation where the package.json script is another call to bun, which might have a conflicting NODE_ENV set. So for example `bun run build:prod` where that script is `\"build:prod\": \"NODE_ENV=production bun build.ts\"`. The outer invocation would default to 'development' mode and loads .env.development, when the script itself should be reading .env.production. While the current solution does fix that specific case, it introduces additional special behaviour and adds a fair bit of confusion."

Two implications:

1. **Bun's `bun run <script>` skip is narrower than "scripts skip defaults."** It specifically skips when the argument resolves to a `package.json` script entry. `bun run /path/to/file.ts` (file-path argument) still loads defaults. The skip exists to prevent the outer process pre-loading `.env.development` from clobbering the inner process's NODE_ENV-from-script-string.
2. **The skip's existence depends on the #13377 footgun.** If Bun didn't default the suffix to `development` when NODE_ENV is unset, the outer-process pre-load would only touch `.env` + `.env.local` — and adding `.env.production*` in the inner process would not conflict. The two bugs prop each other up. Nub fixing #13377 makes the script-skip rule unnecessary.

The broader read: across 2.5 years and ~25 comments, **none of the six issues has been defended by Bun maintainers as an intentional design constraint.** They appear to be neglected, not defended. So Nub's divergences here (walk-up, no-suffix-when-unset, outer-process loading) aren't fighting a deliberate Bun invariant — they're fixing known gaps that nobody has been resourced to address.

## Bun footguns (do not copy)

1. **`std.fs.cwd()` only.** Six open issues from monorepo / workspace / git-worktree users. The trivial fix is "walk up to the nearest `package.json` (workspace root if present)" — purely additive.
2. **`.env.development` loaded when NODE_ENV is unset.** #13377. Surprises everyone.
3. **`bun run <pkg.json script>` skips defaults**, relying on the spawned child being bun. Breaks for `prisma`, `next`, non-bun-shebang `bunx` binaries. #23962 open.
4. **Setting `NODE_ENV=production` inside `.env`** doesn't retroactively switch the suffix.
5. **`KEY: VALUE` YAML syntax accepted.** Footgun with no upside.
6. **No `process.loadEnvFile()` polyfill.** Node has it; Bun #6618 open.
7. **`$VAR` expansion by default** differs from Node's `--env-file`. Cross-runtime test suites get bitten by `PASSWORD=foo$bar`.
8. **No granular disable.** Can't say "load `.env` but skip `.env.local`."
9. **`.env.local` silently skipped in test mode** (correct, but undocumented in Bun).

## Synthesis

Universal conventions (don't break):

1. `.env` exists, loaded into `process.env`, no-override default.
2. `.env.local` is gitignored, user-machine-specific, highest non-mode precedence, **skipped under `NODE_ENV=test`**.
3. Shell env always wins.
4. `${VAR}` expansion is **not** default in `dotenv` or Node `--env-file`. Users opt in.

Where Nub can pick its side without breaking the universal rules:

- Whether to load `.env.[NODE_ENV]` files automatically when NODE_ENV is explicit. Bun does; user expectations are mostly fine here. When NODE_ENV is **unset**, do not pick a suffix (fixes the #13377 class).
- Whether to expand `${VAR}` in defaults. Bun does, Node doesn't. Splitting it down the middle: expand in *defaults*, don't expand in `--env-file=` (matches Node behavior for the Node-compat flag).
- Whether to walk up to package.json / workspace root. Bun doesn't, Deno does (for explicit `--env-file`). Walking up is the right call — no compat cost, fixes Bun's #5836 / #27493 / #10358 / #11190.
- Whether to load defaults under `nub run <script>`. Bun's "skip outer, rely on inner" design is the source of #23962. Loading in the outer runtime and letting children inherit via the live env is unambiguous; `NODE_ENV=production nub run dev` still wins because shell-env precedence is preserved.

Where Nub must diverge from Bun for **additivity** (no breakage of vanilla Node programs):

- `process.env.NODE_ENV` stays undefined unless the user / a file sets it. (Bun got this right in PR #9695.)
- Don't pick a NODE_ENV-suffix file when NODE_ENV is undefined.
- Don't accept `KEY: VALUE` YAML.
- All of this must be inside the scope of `--node` compat mode, which reverts to plain-Node identity (no eager loading at all, since stock Node doesn't do it).

## Sources

Bun:

- https://bun.sh/docs/runtime/env
- https://bun.sh/docs/test/runtime-behavior
- https://bun.sh/docs/cli/run
- https://github.com/oven-sh/bun/blob/main/src/dotenv/env_loader.zig
- https://github.com/oven-sh/bun/blob/main/src/runtime/cli/run_command.zig
- PRs and issues: oven-sh/bun#1262, #1265, #4118, #4594, #4630, #5230, #5836, #6338, #6618, #6840, #7829, #9635, #9695, #9877, #10358, #11190, #13377, #23962, #24767, #27493
- vitejs/vite#20942

Ecosystem:

- https://github.com/motdotla/dotenv#readme
- https://github.com/motdotla/dotenv-expand
- https://github.com/kerimdzhanov/dotenv-flow
- https://github.com/dotenvx/dotenvx
- https://nextjs.org/docs/app/guides/environment-variables
- https://vite.dev/guide/env-and-mode
- https://v2.remix.run/docs/guides/envvars
- https://docs.astro.build/en/guides/environment-variables/
- https://nuxt.com/docs/guide/directory-structure/env
- https://svelte.dev/docs/kit/$env-static-private
- https://nodejs.org/api/cli.html#--env-fileconfig
- https://github.com/nodejs/node/issues/54134
- https://github.com/nodejs/node/issues/59897
- https://docs.deno.com/runtime/reference/cli/run/
- https://docs.github.com/en/actions/learn-github-actions/variables
- https://vercel.com/docs/environment-variables
- https://developers.cloudflare.com/pages/configuration/build-configuration/
- https://fly.io/docs/flyctl/config-env/

## Changelog

- 2026-07-30 — Migrated from the internal research corpus. Internal planning links, private attributions and reference-checkout paths were rewritten; findings and measured values are unchanged.
