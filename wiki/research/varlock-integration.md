# Varlock integration

How Nub should interoperate with [varlock](https://varlock.dev) (`dmno-dev/varlock`) — the `.env.schema` / `@env-spec` schema-and-secrets layer for `.env` files.

- **Status:** IMPLEMENTED on [PR #659](https://github.com/nubjs/nub/pull/659). Two defects found, one on each side.
- **Grounding:** varlock read at `.repos/varlock` `origin/main` = `36a44a4` (post-v1.16.0). The clone's *working tree* is stale at `6283b72` = v1.9.0 — always read via `git show origin/main:<path>`. All runtime claims below were reproduced against installed `varlock@1.16.0` on Node 26.5.0.

> **REVERSAL (2026-08-02).** The in-process design this doc argues for through §"FINAL DESIGN" was built, then abandoned. Nub now spawns `varlock run --path <schema-dir> -- <node> …` and resolves nothing itself. Sections below describing a Rust-primed graph, `init-server`, the install-shape split, or nub-side redaction are **historical**. The surviving reasoning is in §"Why nub cannot keep its own cascade", both defect sections, and §"Redaction: nub does NOT match varlock run". §"IMPLEMENTED DESIGN" below is what shipped.

## The short version

Nub's own `.env` cascade gives the wrong answer on a varlock project, and the obvious `preload` workaround fork bombs; the fix is to put varlock in front of Node and defer to it entirely.

1. **Nub silently ignores `.env.schema` today.** It runs its own `.env` cascade, which produces a *different and wrong* answer whenever the schema declares a custom current-env flag — no error, no warning.
2. **Setting `nub.jsonc` `preload: ["varlock/auto-load"]` is an infinite fork bomb.** Nub compiles `preload` into `NODE_OPTIONS`, which every Node descendant inherits; varlock's auto-load `execSync`s the varlock CLI, which is itself Node, which re-preloads auto-load, forever.
3. **The fix is to put varlock in FRONT of Node**, exactly as a user invoking `varlock run` would: Nub detects the schema, stands down from `.env` loading entirely, and spawns `varlock run --path <schema-dir> -- <node> …`. It resolves nothing, injects nothing, and redacts nothing.
4. This matches the varlock author's own guidance: detect varlock, then **defer completely** to it for loading.

## IMPLEMENTED DESIGN: nub → varlock run → node

The maintainer's call, on parity grounds: varlock reaches full capability only when invoked as `varlock run`. Everything environment-related is deferred to it, with no fallback.

The `SpawnConfig::env_owner` field is `Option<(cli, schema_dir)>`. When set, `spawn_node` builds the same Node command it always would — every flag, the whole `NODE_OPTIONS` augmentation chain — and puts the loader in front of it, skipping `arg0("node")` because the process Nub launches IS the loader.

Why this beats the in-process design it replaced:

- **Redaction works.** In-process patches cover `console` only. `varlock run` pipes the stream, so raw `process.stdout.write` and subprocess output are covered too (measured; see §"Redaction").
- **No partial product.** The install-shape split (importable vs standalone binary) collapses — `node_modules/.bin/varlock` exists in every install shape, verified against a real `npm install varlock@1.16.0`.
- **No coupling to varlock internals.** No graph parsing, no blob format, no `init-server`.
- **−1114 / +265 lines** against the in-process head.

Three non-obvious points, each found by running it rather than reading it:

1. **The `nub run`, `nubx`, `nub watch` and lifecycle-script paths need no wrapping code.** Their `node` resolves through nub's PATH shim, re-enters Nub, and gets wrapped there. This deleted the marker-stamping-across-five-launch-paths problem wholesale.
2. **The guard marker `__NUB_ENV_OWNER_WRAPPED` must be nub's own, never `__VARLOCK_RUN`.** Keying it off varlock's variable would make a user's `varlock run -- nub` in a *different* directory silently serve the outer project's environment. Measured; see §"varlock's env vars".
3. **The `--path` flag is load-bearing.** Without it a workspace member dies with `No .env files found in …/pkgs/web` against a schema Nub had just found at the root — Nub walked up to decide the loader owns the project, so it must say where it walked to. Also fixes `cd src && nub app.js`.

## Why nub cannot keep its own cascade

Nub loads `.env*` in Rust, in the parent, before spawning Node ([`crates/nub-core/src/workspace/env.rs`](../../crates/nub-core/src/workspace/env.rs)), selecting the mode from `APP_ENV` (primary) or a clamped `NODE_ENV`.

Varlock does not work that way: the schema's `@currentEnv=$X` root decorator names *which variable* selects the environment, and it is authoritative — `varlock run` has no `--env` flag, and where `--env` exists (`load`, `explain`, `reveal`) it is only a fallback that `@currentEnv` overrides.

Measured, with `# @currentEnv=$MY_STAGE` in the schema and `MY_STAGE=production` in the environment:

| | `API_PORT` | `GREETING` |
|---|---|---|
| `varlock run -- node app.js` | `8080` | `hello-from-production` |
| `nub app.js` (nub's own cascade) | `null` | `hello-from-dotenv` |

Nub keys on `APP_ENV`, never sees `MY_STAGE`, and so never loads `.env.production` — silently. Any design where Nub keeps resolving env files itself reproduces this class of bug, which is the author's point: the schema describes a *graph* with its own resolution rules, not a file list.

Varlock's full precedence, for reference (`data-source.ts`, `config-item.ts`):

```
real process.env → .env.{env}.local → .env.{env} → .env.local → .env → .env.schema → builtins
```

## Defect 1 (nub): `preload` leaks into every Node descendant

Entries in `nub.jsonc` `preload` are compiled into `NODE_OPTIONS` tokens ([`crates/nub-cli/src/cli.rs:3066`](../../crates/nub-cli/src/cli.rs)), which `spawn.rs` folds into the child's environment.

Every descendant Node process inherits `NODE_OPTIONS`, so a `preload` entry runs in the child, the grandchild, and every Node process spawned for the life of the tree.

Verified with `preload: ["./noop.mjs"]`:

```
PARENT sees noop: true
CHILD NODE_OPTIONS has noop: true
CHILD ran noop: true
```

This is a Nub bug independent of varlock: `preload` reads as "preload this run", and a user preloading an instrumentation or setup module gets it re-executed in every unrelated Node subprocess.

The inheritance is load-bearing in the other direction, so it cannot be removed outright. `nub run <script>` launches a shell, which launches Node; the preload reaches that Node *only* through `NODE_OPTIONS`. Switching `preload` to argv would fix the fork bomb but break `nub run` script coverage — verified working today via the inheritance path.

### Hijacked `node` descendants are NOT run in compat mode

A descendant that resolves `node` through nub's PATH shim is **augmented**, not compat — which changes the fork-bomb analysis. Measured, under `nub probe.mjs` with `preload: ["./noop.mjs"]`, spawning bare `node` (so PATH resolves the shim):

```json
{"viaPathShim":true,"nubVersion":"0.6.0","noopRan":true,"hasNubPreload":true,"hasUserPreload":true}
```

The child reports `process.versions.nub`, ran the user preload, and carries both preload tokens. Re-entrancy detection suppresses a **second augmentation pass** (`is_reentrant` → skip re-injecting), not augmentation itself — the child stays augmented by inheritance. Compat mode is entered only by explicit `--node` / `NODE_COMPAT=1`, which calls `restore_compat_environment` ([`spawn.rs:816`](../../crates/nub-core/src/node/spawn.rs)) and un-augments.

### Nub's env loading builds a CHILD env map — there is nothing to "undo"

The `merge_child_env` function ([`cli.rs:242`](../../crates/nub-cli/src/cli.rs)) returns a `HashMap` applied to the spawned command's environment. Nub's `.env` cascade **never mutates nub's own process env**.

Detecting `.env.schema` is therefore a `stat` at the project root that can run *before* the cascade, so Nub declines to load rather than loading and reversing. The "undo what we already did" problem exists only if the decision has to be made in JS, after the child has started with values already in `process.env`.

## Defect 2 (varlock): `execSyncVarlock` does not scrub `NODE_OPTIONS`

The `varlock/auto-load` module is a synchronous *subprocess wrapper* by design — the graph resolves asynchronously, and top-level await causes ESM hoisting-order problems, so it is forced through `execSync` (`src/auto-load.ts:13-16`, `:63`):

```ts
const { stdout } = execSyncVarlock('load --format json-full --compact', { … });
```

In `lib/exec-sync-varlock.ts` the parent environment passes through wholesale, `NODE_OPTIONS` included. The spawned CLI is a Node process, so it inherits `--import varlock/auto-load` and recurses. Nothing breaks the cycle: the `__VARLOCK_ENV` reuse fast-path is populated only *after* `execSync` returns, so every level re-resolves from scratch.

Observed process chain under Nub, each `ppid` the previous `pid`:

```
node …/.bin/varlock load --format json-full --compact
  node …/.bin/varlock load --format json-full --compact
    node …/.bin/varlock load --format json-full --compact   (… 11+ levels before kill)
```

Present identically in v1.9.0 — longstanding, not a regression. **It is not nub-specific:** plain `NODE_OPTIONS="--import varlock/auto-load" node app.js` hangs the same way (exit 124), while the same module passed as a real `--import` *argv* flag works fine, because argv is not inherited.

It will bite any tool that does env-based preloading, so it is worth reporting upstream.

### There are TWO recursion channels, not one

The second channel is easy to miss, and it is what makes user-space workarounds fragile.

1. **`NODE_OPTIONS` inheritance** — carries `--import <user-preload>` into every descendant Node.
2. **Nub's PATH shim** — Nub prepends a shim directory containing a `node` → nub symlink (verified: `PATH[0]` is `/var/folders/…/nub-node-shim-…`). `node_modules/.bin/varlock` is a `#!/usr/bin/env node` script, so spawning it re-enters **Nub**, which re-reads `nub.jsonc` and re-applies `preload` from scratch.

Channel 2 is normally suppressed by re-entrancy detection: `is_reentrant_in` ([`spawn.rs:809`](../../crates/nub-core/src/node/spawn.rs)) looks for nub's *own* preload token in the inherited `NODE_OPTIONS` and skips re-augmentation when it finds it.

The trap: **blanking `NODE_OPTIONS` entirely makes the fork bomb worse, not better.** Removing nub's own token defeats re-entrancy detection, so the shim fully re-augments every level. Observed as alternating pairs in the process tree — shim `node` (nub), then nub re-spawning the real node with augmentation flags, repeating:

```
node …/.bin/varlock load …
  node --enable-source-maps --experimental-… …/.bin/varlock load …
    node …/.bin/varlock load …
      node --enable-source-maps --experimental-… …/.bin/varlock load …
```

So a correct JS-side workaround must strip **only its own** preload token and deliberately **keep nub's**.

## DECIDED (2026-08-01): detect, skip, and warn

The maintainer's call. Nub does **not** load varlock, vendor it, or resolve the graph. It does two things:

1. **In Rust**, `stat` `.env.schema`. If present, skip nub's own `.env*` cascade entirely. No undo is needed — detection is a `stat` and the cascade builds a child env map that has not been produced yet.
2. **In the child preload**, if `.env.schema` was detected but `__VARLOCK_ENV` is absent from the environment, warn that env was not applied.

Nub is expected to grow its own schema equivalent, so deep varlock coupling is a liability. This design couples Nub to two strings — the filename `.env.schema` and the sentinel `__VARLOCK_ENV` — and to zero varlock APIs.

### `__VARLOCK_ENV` is a reliable sentinel — verified on every path

| How varlock was invoked | sentinel present |
|---|---|
| `varlock run -- nub …` (outer wrapper) | yes |
| preload → in-process `load()` | yes |
| preload → `varlock/auto-load` | yes |
| not invoked at all | **no** → warns correctly |

### The ordering trap — the warning must be its own `--import`, appended LAST

Measured startup order is `--require` → each `--import` in argv order → entry module. Nub's own preload is the fast-tier `--require`, so it runs **before** any user `--import`. Four placements and their outcomes:

| Placement | Result |
|---|---|
| Inside nub's `--require` preload | **False positive** — runs before varlock's preload has set the sentinel |
| `process.nextTick` from that preload | **False positive** — measured firing before `--import` |
| `setImmediate` from that preload | **Too late** — measured firing after the entry module ran |
| **Its own `--import`, appended after the user's preload tokens** | **Correct** — last preload, still before user code |

Nub controls `NODE_OPTIONS` token order, so appending its check token after the user's `preload` entries is deterministic.

### The warning earns its keep in three distinct failure modes

All three leave the user with *no* env loaded, and all three are otherwise silent:

1. Varlock is installed but the user never wired the preload;
2. **the workspace trap** — Nub detects `.env.schema` at the project root and skips, but varlock's cwd-only discovery cannot see it from a member directory (verified: warns correctly);
3. Varlock's preload failed for any reason.

### Open sub-decisions

Three things the detect-skip-warn call leaves unsettled: when the skip applies, what overrides it, and how the warning behaves.

- **Gate the skip on varlock being resolvable?** A `.env.schema` with no varlock installed is arguably aspirational — keep loading `.env` in that case rather than leaving the user with nothing.
- ~~**Precedence:** should an explicit `--env-file` flag, or an explicit `envFile: true` in `nub.jsonc`, override the auto-skip? (Explicit-beats-inferred says yes.)~~ **SETTLED 2026-08-14, as predicted.** Any `envFile` value from a project file, the environment layer, or the CLI displaces the hand-over; `envFile: false` and `--no-env-file` mean no environment at all, the loader included. Two carve-outs the question did not anticipate: `"varlock"` is a mode name that SELECTS the hand-over, and a GLOBAL `envFile` never displaces a project's schema, since a machine-wide default would otherwise empty the environment of every schema project on the box.
- Warning suppression knob, and stderr-once semantics.

## Variant: nub does the load itself (dependency-injection)

Same detection, but Nub appends its own preload module calling varlock's in-process API rather than only warning. The user's only obligation is `npm install varlock`; Nub vendors nothing and calls two public functions.

```
Rust: stat .env.schema → skip cascade → resolve `varlock` from the project
      → if unresolvable, hard error ("add varlock to your dependencies")
      → else append --import <nub-runtime>/varlock-load.mjs
```

**This variant fixes the workspace case, which no other design does:** Nub owns the load, so it can work around varlock's cwd-only discovery by hopping cwd for the duration of the call. Verified: a member directory picks up the root `.env.schema`, and cwd is restored before user code runs.

Two implementation hazards, both measured:

1. **A bare `import "varlock"` from nub's runtime directory fails** — `ERR_MODULE_NOT_FOUND`, because nub's runtime lives outside the user's `node_modules`. Nub must resolve varlock against the *project*, then import the resolved absolute path.
2. **The cwd hop is global process state.** Safe here only because nothing else is running at preload time. It should be commented as such, and it becomes unnecessary the moment varlock ships `load({ path })`.

```js
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

const root = process.env.__NUB_VARLOCK_ROOT;
const req = createRequire(pathToFileURL(root + "/package.json"));
const { load, patchGlobalConsole } = await import(pathToFileURL(req.resolve("varlock")).href);

const cwd = process.cwd();                       // varlock discovery is cwd-only
if (cwd !== root) process.chdir(root);
try { await load(); } finally { if (cwd !== root) process.chdir(cwd); }
patchGlobalConsole();
```

### Design for replaceability — the seam matters more than the integration

Build this integration assuming the loader gets swapped. Keep the split explicit:

**Durable (build it general, not varlock-shaped):**
- The Rust capability is *"an external owner is handling env loading — stand down"*, not *"varlock is present"*. One detection hook, one skip, one preload-token append.
- The preload slot is a **pluggable loader module**.
- The post-preload verification step — *did anything load?* — is loader-agnostic.

**Disposable (keep it tiny and quarantined):**
- The ~10-line adapter that resolves varlock, calls `load()`, and patches console. When nub's own engine lands, this file is replaced and nothing else moves.
- The cwd hop, which exists solely to work around varlock's cwd-only discovery.

**One concrete consequence:** `__VARLOCK_ENV` is a varlock-specific sentinel, so the warning would break the day the loader changes. Nub should set and check its **own** neutral marker for "env was loaded by an external owner", treating `__VARLOCK_ENV` as one recognized signal among others rather than the contract.

A native Nub equivalent needs a Rust `@env-spec` parser, and none exists today — varlock's is Peggy/JS, and the monogram branch is a draft that emits TypeScript. That gap, not the integration work, is what gates a replacement.

### Choosing between the two

Detect-and-warn costs nothing and couples to nothing; letting Nub do the load is the only shape that works in a workspace, at ~70 ms and a dependency on a young API.

| | Detect + warn | Detect + nub loads |
|---|---|---|
| User setup | install varlock **and** wire `nub.jsonc` `preload` | install varlock, nothing else |
| Coupling | 2 strings, 0 APIs | 2 strings + `load()` / `patchGlobalConsole()` |
| Workspaces | broken — varlock is cwd-only | **works** via the cwd hop |
| varlock missing | warn, no env loaded | hard error naming the fix |
| Cost | nub adds none | ~70 ms |
| Risk | none | `load()` is young (`// TODO: add some options`) |

## FINAL DESIGN: prime in-process, then hand off to varlock's own entry

**Supersedes the sections below.** There *is* a single unified entry point — `varlock/auto-load` — and Nub uses it rather than assembling the pieces itself.

```js
await load();              // in-process, no subprocess: populates __VARLOCK_ENV
await import(autoLoad);    // reuse fast-path: 0 spawns, installs everything
```

The `auto-load` module installs the console / ServerResponse / Response guards, applies `@encryptInjectedEnv`, and strips `@internal` keys — all gated on the schema's own settings. Nub cannot preload it alone, because it resolves its graph through a CLI subprocess whose `#!/usr/bin/env node` shebang re-enters Nub (**measured: 541 spawns from one plain-node run**). But it reuses an already-populated `__VARLOCK_ENV` and then skips the CLI entirely, which is what the in-process `load()` sets up.

Measured against the previous "call the patchers by hand" shape:

| | before | after |
|---|---|---|
| `@encryptInjectedEnv` | plaintext blob, secret exposed | `varlock:v1:…`, no plaintext |
| `@internal` keys | leaked into the child | stripped |
| Response/ServerResponse guards | added by hand, easy to miss | varlock's own set |
| Cost | ~100 ms | ~150 ms, still 0 subprocesses |

### Global installs: `npm i -g` is importable, standalone is not

The install shape decides whether redaction is available at all, so the adapter takes the import path only when the bin canonicalizes into a real package directory.

- **`npm i -g varlock`** — the bin is a symlink into `<prefix>/lib/node_modules/varlock/`. Canonicalize it, walk up to the `package.json` whose `name` is the loader, use that as the resolve root. Verified: a global-only install then gets values **and** redaction, where the CLI path could offer neither. Roughly fifteen lines.
- **Homebrew / curl** — a standalone executable with no module behind it. Nothing to import; the CLI path is the only option and redaction is unavailable.

**Promotion must test the same condition the import needs.** A first cut promoted on the strength of an ancestor `package.json` named after the loader, while the adapter resolves it as a *bare specifier* — which needs `exports` (self-reference) or `main`. Those two conditions can disagree, and when they do the run hard-exits 1 on a setup where the CLI path worked. Reproduced with a manifest carrying neither field. Promotion now requires both the name and a declared entry.

An `npm link`-style install works fine: the bin canonicalizes through both symlinks into the linked checkout, whose manifest has `exports`, so self-reference resolves. Tested.

### Two bugs this design introduced, both found by sweeping rather than reading

Both were silent: one hung the process, the other overwrote good values with nulls in a workspace member.

1. **Setting `_VARLOCK_FILTER` hung the process.** That knob *disables* blob reuse, so `auto-load` fell back to its CLI and the recursion returned. The adapter now scrubs both `PATH` (nub's shim) and its own `NODE_OPTIONS` tokens **before importing varlock at all** — varlock snapshots `process.env` at module load (`originalProcessEnv`) and hands that snapshot to its subprocess, so a later mutation is invisible to it. Both restored in a `finally`.
2. **A workspace member silently resolved an empty graph.** Restoring cwd before the handoff made `auto-load`'s reuse check — evaluated against cwd — reject the blob it had just been given, re-resolve from the member where no schema exists, and overwrite good values with nulls, with no error anywhere. Both steps now run inside the cwd hop.

### DECIDED (2026-08-02): dependency build scripts DO see the schema

The maintainer's call, taken because stamping the markers on the lifecycle overlay made the adapter effective inside third-party build scripts — `node-gyp` and `prebuild-install` reach Node directly rather than through the shim.

The reasoning is consistency, not appetite for exposure. Nub's own `.env` cascade **already** reaches those scripts, because a lifecycle script's `node` goes through nub's PATH shim and re-enters Nub, so this widens the non-shim route to match the shim route rather than adding a new class of access. Withholding the schema would have meant a project's environment behaving one way for its own code and another for a dependency's build.

The trade accepted with it: a validation error in the application's own schema surfaces while an unrelated dependency is building. Users who want neither can use `nub install --ignore-scripts`, which runs no dependency scripts at all.

Recorded at the stamping site in `pm_engine/mod.rs` so it is not later "fixed" as an oversight, and documented on the env-files page.

### `.env.schema` is a CONTESTED filename — do not claim it by name

Verified, because the integration originally warned on the filename alone:

- **`dotenv-extended`** has defaulted to `.env.schema` since **2016-02-10** — nine years before varlock's first publish — for an **incompatible** format: bare `NAME=` lines, no decorators, values still in `.env`. Its README documents `schema: '.env.schema'` as the default. It does **43,793 downloads/week** (measured 2026-08-01).
- Warning on the filename told those projects their schema "was not applied" while another tool was applying it correctly, and recommended installing something they never asked for.

Two consequences, both now in the code:

1. **Detection was never filename-driven** — `suppresses_env_files()` is false for `Missing`, so Nub stands down only when the `varlock` package resolves. The coupling sits on the dependency, not the filename.
2. **A declared `dotenv-extended` is the carve-out** — the one signal Nub has that this file belongs to something else, since the schema's contents are never read.

**REVERSAL (2026-08-07): the `@env-spec` content sniff is gone, and an unreadable schema is now FATAL.** The sniff briefly gated both the warning and the hand-over on a `# ---` divider or a `# @decorator` line. That put Nub in the business of guessing at a format it does not parse, on evidence weak in both directions — a decorator-free `@env-spec` schema reads as foreign, and a `# ---` comment in anyone's file reads as ours.

Two rules replace it, both the maintainer's call:

1. **A declared `dotenv-extended` is the sole carve-out.** Nothing else disclaims the file, and Nub never reads its contents.
2. **Resolvability is not evidence — it is a requirement.** A `.env.schema` with no rival declared and no resolvable varlock is an ERROR, not a fall-back to `.env*`. The old non-fatal "loaded .env files instead" hint is deleted, because falling back is a DIFFERENT answer wearing the same shape (no defaults, no validation, no providers, and for a schema-only project nothing at all), and it was silent. `SchemaProblem::is_fatal` is gone with it — both variants now bail, differing only in the fix they recommend (`nub install` vs `nub add -D varlock`).

The gate sits on `run_file_in_dir` and `build_script_command` only, so `nub install` and `nub add` stay reachable inside a blocked project — verified by running them there.

### The much larger sibling bug: nub injects ciphertext, silently

Reproduced against shipped behavior, and **not caused by this PR** — nub's `.env` cascade doing what it is told:

| | weekly downloads | bare `nub app.js` |
|---|---|---|
| `@dotenvx/dotenvx` | **9,766,684** | injects `encrypted:BLcj1ZZ4…` ciphertext, exit 0, silent |
| `varlock` | 170,359 | wrong file set under `@currentEnv` (what this PR fixes) |

Running `dotenvx run -- node app.js` returns `hello-plain`; bare `nub app.js` returns the ciphertext. That is the same silent-wrong-answer class this PR exists to close, at roughly **57×** the population.

It composes correctly both ways already — `dotenvx run -- nub app.js` is right, and `node --require @dotenvx/dotenvx/config app.js` decrypts in **0.19 s** with no subprocess. The defect is confined to the bare invocation.

The broad fix is **tool-agnostic and needs no adapter**: refuse to inject a value that obviously is not one (`encrypted:` alongside `DOTENV_PUBLIC_KEY`; `ENC[AES256_GCM`). It requires zero knowledge of any tool's API and covers tools Nub has never heard of. Tracked as a separate decision, since it changes nub's core env path for everyone.

Ecosystem precedent for preferring a generic mechanism over a blessing, both verified: Vite answered this exact ciphertext report ([#19373](https://github.com/vitejs/vite/issues/19373)) with a generic off switch, `envDir: false` ([#19503](https://github.com/vitejs/vite/pull/19503), merged 2025-03-31), not detection. Node removed Corepack's hardcoded PM knowledge ([nodejs/node#59835](https://github.com/nodejs/node/pull/59835), merged 2025-09-12).

### Type generation

Under this superseded in-process design the `@generateTypes` decorator wrote its file only when the CLI resolved the environment, so resolving in-process wrote nothing and users had to run `varlock codegen`.

The shipped design does not have that gap: `varlock run` calls `generateTypesIfNeeded()` (`run.command.ts:195`), so type generation happens on the normal path.

### Sweep coverage

Every scenario below ran against real varlock 1.16.0 with a built binary, all passing, with zero stray processes throughout.

Values + redaction, `@internal` stripping, `_VARLOCK_FILTER`, `@import`, `exec()` resolvers, Workers, validation failure, the Response leak guard, `@proxy` / `@proxyConfig` items (identical to `varlock run`, including matching schema-error text), workspace members, global-npm, standalone-CLI, encrypted blob.

## Reference: raw in-process `load()` measurements

**This supersedes the `varlock load` subprocess design below.** Varlock's root export has an in-process async loader, `load()` (`src/index.ts:19-30`), and the redaction patches sit on that same export.

Because Node evaluates `--import` modules — top-level await included — before the entry module runs, a preload can `await load()` and have `process.env` populated before any user code.

```js
// nub-owned preload module
import { load, patchGlobalConsole } from "varlock";
await load();
patchGlobalConsole();
```

Measured against the same fixture (`@currentEnv=$APP_ENV`, `APP_ENV=production`, a `@sensitive` value):

| | result |
|---|---|
| Values + `@currentEnv` selection | correct (`GREETING = from-production`) |
| `@sensitive` redaction | yes — `sk▒▒▒▒▒` |
| Subprocesses spawned | **0** |
| Fork bomb under `NODE_OPTIONS`, child **and** grandchild | **safe**, exit 0 |
| Overhead | **~70 ms** (0.10 s vs 0.03 s bare node) |

That is roughly **15× cheaper than the `varlock load` subprocess path** (~1 s), which removes the cache from the critical path and makes automatic detection viable on cost grounds.

Two limitations, both measured:

- **The `load()` function takes no arguments** (`load.length === 0`, and the source carries a literal `// TODO: add some options`). So there is **no path override**, and varlock's cwd-only discovery cannot be redirected — **the workspace case is not solvable on this path today.**
- **It throws `InvalidEnvError` with a raw stack trace** on validation failure (exit 1, correct diagnostic text, but not the CLI's clean presentation). Nub should catch and present it.

The upstream ask is small: **`load({ path })`**. That addition would let the fast in-process path serve monorepos too, collapsing the two designs into one.

## Fallback design: mirror `varlock run`

Use this where the in-process path cannot go — chiefly workspaces, until `load()` accepts a path.

Running `varlock run` hands the child two things — confirmed by dumping the child's environment:

- `__VARLOCK_ENV` — the full serialized graph JSON (same shape as `load --format json-full`)
- `__VARLOCK_RUN=1`

This is a documented handoff, not an implementation detail: `varlock run -i/--inject` takes `all` (default) / `vars` (individual vars only) / `blob` (only `__VARLOCK_ENV`).

So Nub should do what `varlock run` does:

**In the Rust parent**, when a `.env.schema` is detected and `varlock` resolves:
1. Skip nub's own `.env*` cascade entirely.
2. Run `varlock load --format json-full --compact` **once**, with `NODE_OPTIONS` scrubbed from that child's environment (this alone makes the fork bomb structurally impossible, regardless of whether varlock fixes defect 2).
3. Parse the JSON, inject the resolved values into the child, and set `__VARLOCK_ENV` + `__VARLOCK_RUN=1`.

**In the child**, preload `varlock/init-server` — *not* `auto-load`.

The `json-full` contract carries everything nub needs:

```json
{ "basePath": "…",
  "config":   { "API_PORT": { "value": "3000", "isSensitive": false } },
  "settings": { "redactLogs": true, "preventLeaks": true, "disableProcessEnvInjection": false },
  "errors":   { "configItems": { "API_PORT": "Value is required but is currently empty" } } }
```

### Why `init-server` is the right preload module

Measured, with the blob preset and each module imported:

| Module | Sets values | Redacts `@sensitive` | Subprocesses spawned |
|---|---|---|---|
| `varlock/auto-load` | yes | yes | **1 per Node process** (fork bomb under `NODE_OPTIONS`) |
| `varlock/config` | yes | yes | same — it is a one-line re-export of `auto-load` |
| `varlock/env` | yes | **no** | 0 |
| `varlock/patch-console` | no | no (bare exports only) | 0 |
| **`varlock/init-server`** | **yes** | **yes** | **0** |

The `varlock/env` module only consumes the handoff — with `__VARLOCK_ENV` unset it produces nulls rather than resolving. Only `init-server` consumes it *and* installs the console/Response/ServerResponse patches, and because it never spawns it is safe to inherit through `NODE_OPTIONS`, which is what keeps `nub run <script>` coverage working.

End-to-end simulation of the proposed design, child and grandchild, under `NODE_OPTIONS`:

```
$ __VARLOCK_ENV="$GRAPH" __VARLOCK_RUN=1 NODE_OPTIONS="--import varlock/init-server" node gc.mjs
SECRET value: sk▒▒▒▒▒
API_PORT: 3000
EXIT=0  spawned: 0
```

Matches `varlock run -- node secret-app.js` on VALUES and on `console.*` redaction, with zero extra processes. It does NOT match its stream redaction — see the redaction section below.

### Version floor

The blob handoff is **not** a new mechanism, despite v1.16.0's release notes listing "environment-blob reuse". That entry refers to `auto-load`'s fast-path for reusing an *ambient* blob instead of re-resolving; the `run` → child handoff predates it.

Verified working on **both varlock 1.9.0 and 1.16.0**: `varlock run` sets `__VARLOCK_ENV` + `__VARLOCK_RUN=1`, `./init-server` is in the exports map, and blob + `--import varlock/init-server` redacts correctly with zero spawns, so the design does not pin users to the newest varlock. The exact floor below 1.9.0 is untested.

## Redaction: nub does NOT match `varlock run` (measured)

**Correction.** This document previously claimed the preload path was "byte-identical to `varlock run`, redaction included". That was measured with a `console.log` probe only and does not generalize — the two use different mechanisms:

- **`varlock run` redacts the STREAM.** The parent pipes the child's stdout/stderr through a redactor (`cli/helpers/stdout-redaction.ts`, with cross-chunk holdback), so everything the child emits is covered regardless of how it was written — and so is everything its own subprocesses emit.
- **A preload redacts the CONSOLE.** `patchGlobalConsole()` wraps `console.*` in that process. Nothing else.

Measured on one fixture with a `@sensitive` value, printed four ways:

| | interactive TTY | piped / redirected |
|---|---|---|
| `varlock run` | **nothing** redacted | `console.log`, `process.stdout.write`, `process.stderr.write`, and **subprocess output** all redacted |
| nub (preload) | `console.*` only | `console.*` only |

Two consequences:

1. **The gap is widest where redaction matters most.** Piped output is CI logs and log files — the leak path redaction exists for — and there `varlock run` covers everything while Nub covers only `console.*`. A logger writing to `process.stdout` directly (pino and friends), or any script that shells out, leaks under Nub and does not under `varlock run`.
2. **Varlock deliberately does not redact on a TTY.** You are a human looking at your own secrets, and stream-rewriting would break interactive programs. Nub currently redacts `console.*` even there, so it is marginally *noisier* interactively and materially *thinner* when piped.

### Decided: nub defers, and builds no redaction of its own

Nub will not add stream-level redaction. The adapter installs exactly the protections the loader installs for itself — varlock's own `auto-load` calls these three, in this order:

```js
patchGlobalConsole();
patchGlobalServerResponse();
patchGlobalResponse();
```

Each is internally gated on the schema's own settings, so a project that set `@redactLogs=false` or `@preventLeaks=false` gets what it asked for. Nub makes no judgement about which to run.

**In-process leak prevention THROWS where `varlock run` redacts.** Verified on the same fixture, putting a `@sensitive` value into `Response.json`:

| | result |
|---|---|
| `varlock run -- node app.js` | body redacted, process continues — `{"leaked":"sk▒▒▒▒▒"}` |
| nub (in-process patches) | **throws** `DETECTED LEAKED SENSITIVE CONFIG - SECRET`, exit non-zero |
| nub, schema sets `@preventLeaks=false` | body redacted, process continues — identical to `varlock run` |

Under `varlock run` the child has no loader in it, so `Response.json` is never patched and the parent's stream redactor masks the bytes on the way out; the in-process path patches the real constructor and catches the leak at its source. It is the same difference a user would get from `varlock/auto-load` directly, and the schema controls it.

Stream-level coverage — raw `process.stdout.write`, or anything a subprocess prints — comes from `varlock run -- nub …`, which composes (verified). Nub does not reimplement it.

## What this buys, and what it still gives up

Achieved: one varlock resolution per Nub run (rather than one per Node process), redaction in the child and every descendant, and correct `@currentEnv` handling.

Validation failures also propagate faithfully — verified exit 1 with varlock's own message shape.

Not achieved by any preload-based approach, because these live in `varlock run`'s process supervision:

- **Stream redaction for subprocesses.** A Nub-run script that shells out to `sh -c 'echo $SECRET'` is redacted by `varlock run` and not by a preload.
- **`@internal` stripping for children**, and `--filter` ambient-carry stripping. Nub can close both by filtering what it injects, since it now owns injection.
- Signal forwarding / process groups — irrelevant here, Nub owns that already.

## Cost

Running `varlock load` is not free. Measured on a contended host (load ~33, so directional only):

| | wall clock |
|---|---|
| `varlock load --format json-full --compact` | 0.81 – 1.38 s |
| bare `node -e 0` baseline | 0.12 – 0.30 s |

Roughly 0.5–1.1 s added per Nub invocation. Two contributors: the CLI is a full Node boot, and its load path `execFileSync`s the `varlock-local-encrypt` native binary (`native-bins/`, on macOS a Secure-Enclave app) for cache-backend probing on every load, with a 30 s timeout.

That is too much to add unconditionally to every `nub` command. **Auto-detection needs a resolved-graph cache** keyed on the schema and env-file mtimes, the current-env value, and the varlock version — or the integration has to be opt-in.

The call is also not read-only: with a `@generate*` decorator it writes generated files (`runCodeGeneratorsIfNeeded` runs on the `load` path). Measured: `env.d.ts` is written on the first load and **not** re-touched on an unchanged second load, so it is not a write-storm — but Nub cannot treat the call as pure.

## Workspaces — the sharpest remaining gap

**Varlock's schema discovery is cwd-only. There is no ancestor walk.** Verified in a two-package monorepo with `.env.schema` only at the root:

```
$ cd packages/web && varlock run -- node app.js
🚨 No .env files found in /private/tmp/vlkmono/packages/web
```

Nub resolves the *project root*, not cwd, so the two disagree exactly where Nub is strongest. The escape hatch works — `varlock run --path <workspace-root>` from the member resolves correctly — so Nub should pass `--path <project-root>` when it invokes varlock. Varlock already carries an asymmetry here: `auto-load` passes a `callerDir` so the *binary* search walks up in a monorepo, while *schema* discovery stays anchored to cwd.

Users can also opt in per package via `@import(...)` in a local schema or `varlock.loadPath` in the package's `package.json` (also cwd-only).

## Recommended usage today (no nub changes)

All three of these work now and are verified.

**Outer wrapper — simplest, full `varlock run` semantics including subprocess stream redaction:**

```sh
varlock run -- nub app.js
```

**`nub.jsonc` deferral, with a shim.** `preload: ["varlock/auto-load"]` alone fork bombs; a shim that strips its own inherited token first does not:

```jsonc
// nub.jsonc
{ "envFile": false, "preload": ["./varlock-shim.mjs"] }
```

```js
// varlock-shim.mjs
import { fileURLToPath } from "node:url";
import { basename } from "node:path";

// Strip ONLY this module's own token. Nub's own preload token MUST stay — it is what
// makes nub's PATH shim treat the spawned varlock CLI as already-augmented and skip
// re-augmenting it (which would re-read nub.jsonc and re-add this preload).
const self = basename(fileURLToPath(import.meta.url));
process.env.NODE_OPTIONS = (process.env.NODE_OPTIONS || "")
  .split(/\s+/).filter((t) => !t.includes(self)).join(" ");

await import("varlock/auto-load");
```

Two ways to get this wrong, both found by testing:

- **Do not filter on the string `varlock`.** An earlier draft of this shim did, and passed only because the file happened to be named `varlock-shim.mjs` — so the filter matched its own path by coincidence. Renaming it to `shim.mjs` fork bombs.
- **Do not `delete process.env.NODE_OPTIONS`.** That removes nub's token too and defeats re-entrancy detection, re-opening channel 2 above. Verified: it hangs harder than doing nothing.

Setting `envFile: false` is the load-bearing half — it turns off nub's cascade so varlock is the only thing resolving env. Verified to match `varlock run -- node app.js` on values and `@currentEnv` selection, and it covers `nub run <script>` too. Redaction coverage is NARROWER than `varlock run` — see the redaction section above.

The fragility of both footguns is the argument for solving this in Nub rather than in userland.

**Blob handoff** — with a resolved graph already in hand, `preload: ["varlock/init-server"]` plus `__VARLOCK_ENV` needs no shim and spawns nothing. This is the pattern Nub should adopt internally.

## Answering the author's concerns

The objections the varlock author raised against a tool loading his schema, and where deferring completely leaves each one.

| Concern | Resolution |
|---|---|
| "It's loading a graph, resolvable different ways" | Conceded — Nub defers and does no resolution of its own. Verified that nub's cascade is silently wrong under `@currentEnv`. |
| "You still need `APP_ENV=prod nub build`" | Works unchanged. Nub passes ambient env through; varlock's `@currentEnv=$APP_ENV` reads it. `APP_ENV=production nub app.js` is byte-identical to `APP_ENV=production varlock run -- node app.js`. |
| "Load a specific configuration, or only a subset" | Varlock's `--filter` (key globs, `!` negation, `@decorator`, `#tag`) or `_VARLOCK_FILTER`; `-p/--path` for a specific entry point. Nub should surface both. |
| "Different env policies per script" | Reachable via `scriptsMeta` in `package.json` — per-script `--filter` / `--path` / current-env. Not designed yet. |
| "Detect varlock, defer completely" | Adopted as the design. |

## Open questions

Unresolved: whether detection is automatic, how a resolved-graph cache would be invalidated, the general `preload` leak, denylist coverage for varlock-sourced values, and the shape of per-script policy.

1. **Opt-in or automatic?** Bare `.env.schema` detection costs ~1 s per invocation without a cache. Options: cache the resolved graph; require `nub.jsonc` opt-in; or detect and warn rather than act.
2. **Cache invalidation**, if cached — schema + env-file mtimes, `@currentEnv` value, varlock version, and any `exec()` resolver (which is inherently non-deterministic and may raise a 1Password GUI prompt).
3. **Fixing nub's `preload` leak generally**, without losing `nub run <script>` coverage. The integration no longer depends on it once the blob handoff lands, but it remains a bug.
4. **Should nub's `ENV_FILE_DENYLIST` apply to varlock-sourced values?** It currently protects `NODE_OPTIONS`, `__NUB_RUNTIME_CONFIG` etc. from `.env`-sourced values; a varlock graph is a different path and is not covered.
5. **Per-script policy shape** via `scriptsMeta`.

## Monogram / a Rust `@env-spec` parser

The author mentioned likely switching the parser to [monogram](https://github.com/johnsoncodehk/monogram), which would yield a Rust `@env-spec` parser Nub could use directly. Status, checked:

- The `origin/main` branch still ships peggy 5.x. `git grep -i monogram origin/main` returns **no hits**. The real parser is `packages/env-spec-parser/` — a PEG.js (`grammar.peggy`) grammar compiled to JS/TS. No Rust, no WASM, no tree-sitter anywhere in the tree.
- The work is [PR #744](https://github.com/dmno-dev/varlock/pull/744), **still a draft** (opened 2026-06-03, last updated 2026-07-04), on branch `codex/monogram-default-parser`, pinned to a `dmno-dev/monogram` fork as a devDependency.
- **The parser on that branch is TypeScript, not Rust.**

Monogram is a **grammar-definition tool**, not a parser library. You write the grammar once in TypeScript combinators and it derives a lexer/CST parser, a TextMate grammar, a **tree-sitter** grammar, a Monaco tokenizer, and language-config files. The realistic artifact for a Rust consumer is therefore a tree-sitter grammar — C, reachable from Rust via the `tree-sitter` crate's bindings — not a native Rust `@env-spec` parser, which is a materially different and heavier integration than using a Rust crate directly.

Not to be confused with it: **varlock already ships Rust**, at `packages/encryption-binary-rust/` (`varlock-local-encrypt`) — the local-encryption helper, ECIES plus platform key protection (DPAPI/Windows Hello, TPM2/secret-service), spawned as a helper binary over JSON IPC. It is the same binary the load path probes on every run (see Cost), and it has nothing to do with parsing `@env-spec`.

So a Rust-native `@env-spec` parser is not available, and reimplementing `@env-spec` in Nub is rejected for now — it would re-introduce the "nub resolves it itself" failure mode this document argues against. Revisit only if #744 lands *and* a usable Rust parsing path materializes.

## Reproduction

Fixtures used: `/tmp/vlk` (single package), `/tmp/vlkmono` (workspace). Both are throwaway; recreate with `npm install varlock@1.16.0` plus a `.env.schema`.

Reproducing the fork bomb needs care: bound it with a short `timeout` and clean up with `pkill -f "varlock load"`. Do **not** bound it with `ulimit -u` — that limit counts all of the user's existing processes, so on a busy host it fires before anything runs and produces a false positive.

## varlock's env vars, and why nub does not skip on them

`varlock run` sets exactly **two** variables on its child (measured, not read from source):

- `__VARLOCK_ENV` — the serialized graph, which contains **`basePath`**, the directory it resolved from
- `__VARLOCK_RUN=1`

Every name in the source, for reference: `_VARLOCK_ENV_KEY`, `_VARLOCK_FILTER`, `_VARLOCK_REDACT_STDOUT`, `_VARLOCK_CACHE_KEY`, `__VARLOCK_INTEGRATION`, `__VARLOCK_EXECUTION_PHASE`, `_VARLOCK_USE_INJECTED_ENV`, `_VARLOCK_THROW_ON_LOAD_ERROR`, `_VARLOCK_FORCE_KILL_TIMEOUT_MS`, `__VARLOCK_SEA_BUILD__`, `__VARLOCK_BUILD_TYPE__`, `_VARLOCK_DYNAMIC_BUILD_ACCESS_MODE`, `_VARLOCK_FORCE_FILE_ENCRYPTION_FALLBACK`, and a proxy-session family. Convention (`auto-load.ts:124-127`): `__` = varlock-set, `_` = user-controllable.

**DECIDED (2026-08-02): nub does not use these to skip re-invoking varlock.** Measured the nested case — project A's `varlock run` wrapping a Nub that runs in project B:

| | value |
| --- | --- |
| control, nub in B alone | `GREETING=from-B`, `ONLY_A=null` |
| A's `varlock run` → nub in B | `GREETING=from-B`, `ONLY_A=yes` |

Double resolution is **idempotent for schema-declared values** — B's schema wins. (`ONLY_A` rides along as ambient, which is varlock's own passthrough behaviour and identical to `varlock run -- varlock run -- node`.) Skipping on `__VARLOCK_RUN` alone would have given B **A's** `GREETING`. A `basePath` comparison would catch that, but it is necessary and not sufficient:

1. An **encrypted blob** (`@encryptInjectedEnv`) is opaque `varlock:v1:…` — `basePath` is unreadable.
2. **`_VARLOCK_FILTER`** on the outer run ⇒ same `basePath`, a *subset* of variables injected.
3. **`--path`** or other outer flags ⇒ same `basePath`, different resolution.

Each failure mode is a silent wrong or incomplete environment, to save ~0.15 s on a deliberate and uncommon invocation. Not worth it.

## Changelog

Every revision to this document, with the date and what changed.

- 2026-08-14 — **REVERSAL: an explicit `envFile` beside a schema is no longer refused — it wins.** The hand-over shipped with a hard error naming both sides, on the reasoning that either winner silently drops half of what the project asked for. That reasoning held for the drop but not for the refusal: a schema is INFERRED intent and an `envFile` value is DECLARED, so there is a correct winner, and refusing left a project wanting a schema in CI and a plain `.env` locally unable to say so. Any `envFile` from a project file, the environment layer or the CLI now displaces the hand-over. Two further corrections fall out. `envFile: false` and `--no-env-file` were classified as non-conflicting because standing down already loads nothing — which read the hand-over as the ABSENCE of loading rather than as its own answer, so both did nothing at all in a schema project and handed a fully resolved environment to someone who asked for none; they now mean no environment, the loader included. And a declared `envFile` also clears the missing-varlock error, which had made a schema project unrunnable on a machine where varlock will not install unless the user gave up augmentation via `--node`. Scope is the one limit: a GLOBAL `envFile` never displaces, because `nub config set --global envFile false` would otherwise empty the environment of every schema project on the machine. Settles the precedence open question above. Separately, `envFile` dropped its bare-string path form (paths go in an array, matching every other list field), which freed the string slot for the mode name `"varlock"` — `$varlock` was not available, since `envFile` values already run through `${VAR}` expansion.
- 2026-08-07 — **REVERSAL: the `@env-spec` content sniff is removed.** Ownership is decided by whether varlock RESOLVES (`node_modules/.bin` up to the workspace root, then `PATH`), with a declared `dotenv-extended` as the sole carve-out. nub no longer reads the schema at all. Recorded in full under the contested-filename section above.
- 2026-08-02 — **REVERSAL: the in-process design is replaced by `nub → varlock run → node`.**
  Maintainer's call, on parity grounds: varlock only reaches full capability when invoked as
  `varlock run`. Implemented as `SpawnConfig::env_owner`; −1114 / +265 lines. Redaction now covers
  the whole stream rather than `console` alone, and the install-shape split collapses because
  `node_modules/.bin/varlock` exists in every shape. Two prior claims in this doc are now **false**
  and corrected in place: `varlock run` *does* run type generation (`run.command.ts:195` calls
  `generateTypesIfNeeded()`, and the decorator is `@generateTypes`, not `@generateTsTypes`), and the
  workspace gap is closed by passing `--path`. Also decided: nub does **not** skip re-invocation when
  it detects varlock already ran upstream — see the section above.
- 2026-08-01 — **DECIDED (maintainer): detect `.env.schema` → skip nub's cascade → warn from the preload if `__VARLOCK_ENV` is absent.** nub neither loads nor vendors varlock, keeping the coupling to two strings and zero APIs, because nub will grow its own schema equivalent. Verified `__VARLOCK_ENV` is set on all three invocation paths (`varlock run`, in-process `load()`, `auto-load`) and absent otherwise. Found and resolved an ordering trap: nub's `--require` preload runs before user `--import` preloads, and `nextTick`/`setImmediate` from it fire too early/too late respectively — the check must be its own `--import` token appended after the user's. The warning also catches the workspace trap (schema at root, run from a member). The in-process `load()` design is demoted to reference.
- 2026-08-01 — **Found a much better primary design: varlock's in-process `load()` via `--import`.** Zero subprocesses, redaction included, fork-bomb safe, and ~70 ms instead of ~1 s — roughly 15× cheaper than the `varlock load` subprocess path, which it supersedes as the recommendation. Limitations measured: `load()` takes no arguments, so there is no path override and workspaces still need the subprocess fallback; and it throws a raw `InvalidEnvError` stack on validation failure. Identified `load({ path })` as a small, high-value upstream ask.
- 2026-08-01 — **Corrected the recommended shim, which was wrong as first published.** It filtered `NODE_OPTIONS` tokens on the string `varlock` and only worked because the file was named `varlock-shim.mjs`; renaming it fork bombs. Now strips its own token by basename. Also found a **second recursion channel** — nub's PATH shim re-enters nub for any `node`-shebang CLI and re-applies `nub.jsonc` `preload`, normally suppressed by re-entrancy detection — so `delete process.env.NODE_OPTIONS` makes things worse rather than better. Recorded two facts that shape the options: hijacked `node` descendants are **fully augmented, not compat mode**, and nub's env cascade builds a **child env map**, so detection can precede loading and nothing needs undoing.
- 2026-07-31 — Established the version floor by testing rather than assuming: the blob handoff works on varlock **1.9.0 as well as 1.16.0**, so the design does not require the just-published release. v1.16.0's "environment-blob reuse" is `auto-load`'s ambient-blob fast-path, a different mechanism. Also sharpened the monogram section — monogram derives a **tree-sitter** grammar (C, with Rust bindings), not a native Rust parser, so the expected payoff is smaller than "varlock will hand us a Rust crate"; and noted that varlock's existing `packages/encryption-binary-rust` is the encryption helper, not a parser.
- 2026-07-31 — Initial write-up. Design settled on the `varlock run`-mirroring blob handoff (`varlock load` in the Rust parent + `varlock/init-server` in the child preload). Two defects recorded: nub's `preload` leaking into every Node descendant via `NODE_OPTIONS`, and varlock's `execSyncVarlock` not scrubbing `NODE_OPTIONS`.
