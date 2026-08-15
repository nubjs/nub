# Node-flag hijack compatibility: Nub as an argv0=node proxy

**Status:** v1, 2026-05-31.

**Scope:** the executable-hijack flag-compatibility contract. Under the hijack-by-default PATH shim, Nub's binary sits in front of every `node` invocation in a Nub-orchestrated process tree: it inspects `basename(argv[0])`, and when that is `node` it dispatches into an augmented top-level that auto-injects flags and ultimately spawns the real node. That makes Nub-as-node a full `node [node-flags] <script> [script-args]` argv proxy. For each flag category this doc answers how Nub must forward or intercept the flag, locate the script boundary, inject its own flags strictly before the script, and reproduce node's early-exit / identity / value-form semantics byte-for-byte. Getting the boundary, a value form (`--opt=val` vs `--opt val`), or an early-exit flag wrong silently breaks drop-in compat for the user and for every descendant `node` invocation.

**Relationship to [[research/node-flag-interactions]]:** that doc asks the inverse question — from Nub's augmentation surface (registerHooks, `--import` preload, env loading, auto-flag-injection), does Nub break this flag? — and produces a per-flag A–F verdict table (no-interaction / plays-nicely / conflict / plausibly-broken / subtly-broken / mooted). This doc asks how an argv0=node proxy must parse and forward each flag, and where exactly it injects. Where the other doc already carries a per-flag augmentation verdict (`--permission`, `--watch`, `--env-file`, `--conditions`, `--require`-vs-`--import`, `--inspect-brk` source maps), this one references it rather than re-deriving. What is added here: the boundary-parser contract, the value-form / arity table, the injected-flag collision matrix, and the early-exit/identity rules governing Nub-as-node's argv rewrite.

**Verification posture:** findings marked **confirmed** were reproduced by running real `node` — primarily v26.2.0, the development host's Node — with the cited flags, cross-checked against `node/src/node_options.cc`, `node_options-inl.h`, and `node/lib/internal/main/`. Findings marked **design-inferred** are read from source or behavior on the dev-box version but were not reproduced on the 18.19–22.14 / 22.15 augmentation floor; each carries an explicit re-verify note. Node's augmentation floor is 18.19.0, with sync `registerHooks` on 22.15+ and async `module.register` on 18.19–22.14. Several findings are tier-dependent and say so.

## 1. The load-bearing premise

Nub-as-node must replicate node's tokenizer exactly, because three things ride on the parse:

1. **The script boundary** — where node-flags end and the script plus its argv begin.
2. **The injection point** — Nub inserts its auto-flags (`--import` preload, `--enable-source-maps`, `--disable-warning=ExperimentalWarning`, version-gated `--experimental-*`) strictly before the script, and for `--import` specifically before any user `--import`.
3. **Early-exit / identity fidelity** — `--version`, `--help`, `--v8-options`, and `--completion-bash` must print the real node's output and exit, never Nub's identity.

A naive first-non-dash-token-is-the-script scan is wrong, because value-consuming flags (`-e CODE`, `--require MOD`, `--env-file PATH`) legitimately place a non-dash token immediately after a flag that is not the script, and because `-e`, `-p`, `-i`, `--`, `-`, and the early-exit flags each bend the rules.

The single most important parser fact (`node_options-inl.h:337-338`): node's option loop breaks the instant `args.first().size() <= 1 || args.first()[0] != '-'`. The first token that is not `-…`, or is a bare single `-`, terminates node-flag parsing, and that token plus everything after becomes `process.argv`. Everything in this doc is downstream of locating that break point after honoring value consumption, `--`, and the eval/REPL/early-exit modes.

## 2. Execution-mode flags & the script-boundary parser

Ten tokens decide where node-flags end and the script begins. Three are marked boundary-critical — the script positional, a bare `-`, and `--` — and `--prof-process` injects a synthetic `--` of its own.

| Flag | Value-form | Verdict | Nub-as-node handling | Risk |
|------|-----------|---------|----------------------|------|
| `[script positional]` | positional | boundary-critical | THE boundary. First token with `size<=1` or not starting `-`, after honoring value-consuming flags + `--`. Inject all auto-flags + user node-flags strictly before it; forward it + remainder verbatim. | high |
| `- ` (bare dash, stdin) | positional | boundary-critical | Lone `-` BREAKS the loop like a script path (`size<=1`), but routes to stdin-eval (not run-main); `-` and following tokens stay in argv. Transpile stdin TS Nub-side (no file URL for the hook). | high |
| `--` (end-of-options) | none | boundary-critical | Hard boundary. node DROPS `--` from `process.argv` (`pop_first` excludes it). Stop Nub's scan here; inject before it; forward `--`+rest verbatim (real node re-drops). kDisallowedInEnvvar. | high |
| `-e, --eval <code>` | `--eval=` or `-e <tok>` | intercept | `--eval=CODE` accepts `=`; `-e` takes the NEXT whole token (`-e=CODE` is REJECTED — `=`-split is `--`-only). Parsing CONTINUES after the value. Inline-code mode: transpile (possibly-TS) code Nub-side; honor `--input-type`. kDisallowedInEnvvar — never route via NODE_OPTIONS. | high |
| `-p, --print <code>` | `--print=` or `-p <tok>` | intercept | Alias of `--eval`+print. `-pe`/`-ep`… see false alarm below: only `-pe` is a real alias; `-ep` is rejected. Same transpile obligation as `-e`. kDisallowedInEnvvar. | high |
| `-i, --interactive` | none | intercept | force_repl. OVERRIDES eval: `-i -e CODE` runs REPL with the eval string preloaded, not eval-and-exit (`node.cc` requires `!force_repl`). Wire TS into REPL eval. kDisallowedInEnvvar. | med |
| `-c, --check` | none | intercept | Syntax-check-only (no run). Nub must syntax-check `.ts` via its own parser (node's check_syntax has no TS on the floor). Mutually exclusive with `--eval`/`--test`/`--watch`. Scope injection: preload side-effects shouldn't fire for a check-only run. kDisallowedInEnvvar. | med |
| `--input-type <type>` | space-or-equals | passthrough | `module|commonjs|module-typescript|commonjs-typescript`. Only affects `-e`/stdin. Nub honors it as the parse goal when doing its OWN eval-string transpile. kAllowedInEnvvar. | med |
| `inspect` (subcommand) | positional | early-exit | `argv[1]=="inspect"` → debugger CLI, bypasses script/eval. No user graph; do NOT inject `--import`/TS augmentation. | low |
| `--prof-process` | none + synthetic `--` | early-exit | Alias expands to `{--prof-process, --}`: appends `--`, dispatches to the tick processor. Alternate entrypoint, no user JS. Skip ALL injection; forward tail opaquely. | high |

### Confirmed boundary facts (reproduced)

Nine reproductions against real node v26.2.0, covering value consumption, `=`-splitting, underscore normalization, alias arity, and the fatal-unknown-flag rule.

- `node main.js --max-old-space-size=4096 xyz` → once the script is hit, ALL following dash-flags stay in `process.argv` verbatim (confirmed). The boundary is `main.js`; inject before it; forward the rest untouched.
- `node --require ./pre.js main.js a b` → `./pre.js` is the VALUE of `--require`, `main.js` is the script (confirmed). A naive "first non-dash token" scan mis-classifies `./pre.js` as the script.
- `node -e='code'` / `node -p='2+2'` → `node: bad option: -e=code` (confirmed). `--eval='code'` / `--print='2+2'` are ACCEPTED. `=`-split is restricted to `--`-prefixed tokens (`node_options-inl.h:350-356`). Only split `=` for `--` tokens; for `-e`/`-p` the value is the next whole token.
- `node -e ''` → exit 0 (empty eval valid); `node -e` → `-e requires an argument`, exit 9 (confirmed).
- `node -- main.js --not-a-node-flag x` → `process.argv.slice(2) == ['--not-a-node-flag','x']`, `main.js` runs (confirmed); `--` dropped from argv.
- `echo '' | node -i -e 'console.log("EVAL_RAN")'` → REPL launches AND the eval string is preloaded into the REPL (confirmed) — NOT eval-and-exit.
- `node --env_file a.env main.js` runs `main.js`, and `a.env` is CONSUMED as `--env-file`'s value (not treated as the script; confirmed). Underscores are normalized to dashes before arity lookup (`node_options-inl.h:369-373`), so a dash-only matcher mis-reads `a.env` as the script — Nub must normalize for arity. (Node quirk, also confirmed: under the `--env_file` spelling the value is consumed but the env-file is NOT actually loaded — `process.env.FOO` stays unset where `--env-file` would set it. Forwarding verbatim reproduces this; only the boundary normalization matters to the splitter.) The recognition/arity normalization also applies to `--max_old_space_size`, `--experimental_vm_modules`, `--no_warnings`, etc. (all confirmed accepted, exit 0).
- `node -r ./pre.js main.js` → `./pre.js` preloads, `main.js` runs (confirmed) — the `-r`→`--require` alias inherits `--require`'s arity-1 consumption. Alias arity is not optional knowledge.
- `node --totally-bogus-flag main.js` → `node: bad option: --totally-bogus-flag`, exit 9 (confirmed) — an unknown `--flag` on argv is FATAL (V8 rejects it), NOT warn-and-ignore. In `NODE_OPTIONS` it is a different fatal: `… is not allowed in NODE_OPTIONS`, exit 9 (confirmed). There is no "harmless unknown flag" path on either channel, which is why injection must stay within the target version's recognized set (§7).

### node's run-mode precedence (must reproduce exactly)

From `node/lib/internal/main/` + `node.cc`: `inspect` → `--help` → `--prof-process` → (`has_eval_string && !force_repl`) → `--check` → `--test` → `--watch` → script-positional → (`interactive || TTY`) → stdin-eval.

A dispatcher that checks `has_eval_string` before `force_repl` wrongly runs eval under `-i`.

### False alarms (do NOT implement)

Two rules that look real from the flag names and are not. Implementing either would make Nub accept argv that real node rejects.

- `-ep` is NOT a valid combined alias. `node -ep '3+3'` → `node: bad option: -ep`, exit 9 (confirmed). Only `-pe` expands to `{--print,--eval}`. Nub must REJECT `-ep` exactly as node does.
- `--max-old-space-size 100` does NOT consume `100` as a value (it is a V8 option, `=`-form only). The space-form-consumes-next-token rule is for node-NATIVE kString/kInteger flags (`--require`, `--import`, `--env-file`), NOT V8 options. See §6.

## 3. Module & loader flags (collide with the injected `--import` / hooks)

Per-flag augmentation verdicts — does the loader break — live in [[research/node-flag-interactions|`node-flag-interactions.md` §3 / §4.6 / §4.7]]. Here: how Nub-as-node orders and forwards them around its injected `--import`.

| Flag | Value-form | Verdict | Nub-as-node handling | Risk |
|------|-----------|---------|----------------------|------|
| `--import` | space-or-equals, string-list | inject-collision | PREPEND Nub's preload as the FIRST `--import`, before any user `--import` and before the script. node runs `--import` left-to-right; if Nub appended after a user `--import x.ts`, x.ts reaches the loader before registerHooks and fails. No negation form. | high |
| `--require, -r` | space-or-equals, string-list | passthrough | Forward verbatim. node runs ALL `--require` (CJS) before ALL `--import` (ESM), regardless of interleave. So user `-r` preloads run before Nub's hook registration — Nub's ESM/TS hooks don't cover user-`-r`-pulled code (it's CJS anyway). | med |
| `--experimental-loader, --loader` | space-or-equals, string-list | inject-collision | node rewrites each into a synthetic `--import register(...)` PREPENDED ahead of user `--import`. User loader + Nub hooks COEXIST; both fire. Pass Nub's preload as an absolute `file://` URL so a user resolve hook that rejects relative specifiers can't break it. | high |
| `--conditions, -C` | space-or-equals, string-list | passthrough | Forward verbatim; inject nothing. Never add a Nub-owned condition (brand-boundary + alters resolution). | low |
| `--input-type` | space-or-equals | passthrough | See §2 — eval/stdin parse goal only. | low |
| `--experimental-detect-module` / `--no-…` | none (bool) | passthrough | Default-on in supported range; nothing to inject. (`--experimental-default-type` does not exist as a registered option; forward unknown spellings and let node error.) | low |
| `--experimental-vm-modules` / `--no-…` | none (bool) | passthrough | Not injected by Nub. Honor `--no-` via the subtract step if ever added to the table. | low |
| `--require-module` / `--experimental-require-module` / `--no-…` | none (bool) | passthrough | `--experimental-require-module` Implies `--require-module`. Forward verbatim. | low |
| `--preserve-symlinks` / `--preserve-symlinks-main` / `--no-…` | none (bool) | passthrough | Forward verbatim. Governs USER resolution; Nub's preload path is an absolute path it controls, no interaction. | low |
| `--experimental-specifier-resolution` / `--es-module-specifier-resolution` | space-or-equals | passthrough | NoOp in current node; accept-and-ignore. Forward (do not reject as unknown). | low |

### Confirmed loader-ordering facts (reproduced)

Four reproductions fixing the injected preload's position and form: first among `--import`, an absolute `file://` URL, from a path that always exists — and with no `--no-import` opt-out available.

- `node --import a.mjs --import b.mjs main.mjs` → A, B, MAIN strictly left-to-right (confirmed). Nub's preload must be the first `--import` so its registerHooks is active when a later user `--import x.ts` resolves (confirmed: nub-preload-first transpiles a later user `--import x.ts`).
- `node --import ./does-not-exist.mjs main.mjs` → ERR_MODULE_NOT_FOUND, exit 1, MAIN never runs (confirmed). A non-resolvable injected preload aborts the WHOLE process. Nub's preload path must be a stable, always-present ABSOLUTE install-dir path (e.g. `<install>/runtime/preload.mjs`), NEVER a per-invocation `/tmp/nub-node-<pid>/…` file that a grandchild reading inherited NODE_OPTIONS might find already cleaned up.
- `node --no-import main.mjs` → `node: --no-import is an invalid negation because it is not a boolean option` (confirmed). String-list flags (`--import`/`--require`/`--loader`/`--conditions`) have NO `--no-X` form. The `--no-experimental-*` subtract-merge opt-out therefore does NOT cover the injected `--import`; the only escape is `nub run --node` / `nubx --node`, and the opt-out documentation must state that exclusion explicitly.
- User custom loader can intercept Nub's preload: with a loader that throws on `'a.mjs'`, `node --experimental-loader ./strict.mjs --import ./a.mjs main.mjs` → the preload never runs; passing `--import file:///abs/a.mjs` survives (confirmed). Use the absolute `file://` form.

### Entry-loader flip (documented divergence, observably benign on the CJS surface)

Injecting `--import` forces the main entry through the ESM loader (`run_main.js`: `shouldUseESMLoader` is true whenever `--import.length>0`).

Reproduced as BENIGN for the CJS surface: `require`/`module`/`__dirname` are identical with and without the injected `--import`. The TLA-detection effect on ambiguous `.js` entries could NOT be reproduced as a divergence on node 26 (default detect-module promotes ESM-syntax files regardless). **Design-inferred for the floor:** re-verify TLA-of-ambiguous-`.js`-entry on the 18.19 floor (Docker `node:18.19-slim`) before claiming full parity; on current node it is a documented internal routing divergence with no observable CJS-surface effect.

### Double-injection (self-mitigating, with a caveat)

node canonicalizes the `--import` specifier to a realpath before its ESM cache check, so the SAME preload runs ONCE across the NODE_OPTIONS + argv channels (confirmed).

That dedup holds across relative-vs-absolute spellings, `..` segments, and symlinks (`/tmp` vs `/private/tmp`). Double-registration occurs ONLY if Nub points the two channels at two physically distinct files.

Mitigation: emit ONE canonical absolute path on both channels; a registration sentinel inside the preload (a `globalThis` guard) is cheap insurance.

## 4. Inspector, profiling & diagnostic flags

The category's dominant trap is value-form asymmetry: the toggles take no value, while their settings take a space-or-equals value. One mis-classification swallows the script.

| Flag | Value-form | Verdict | Nub-as-node handling | Risk |
|------|-----------|---------|----------------------|------|
| `--inspect` | none (port via `--inspect=` alias only) | passthrough | Valueless toggle. `node --inspect -e …` does NOT eat `-e` (confirmed). Forward before script, consume zero tokens. | high |
| `--inspect-brk` | none (port via `=` alias) | boundary-critical | Implies `--inspect`. Pause-on-start fires before the main-exec callback; Nub's injected `--import` runs INSIDE that callback, so the debugger's first paused frame is Nub's loader shim, not the user script (source-inferred from `node.cc:312-319`). Ignore-list the preload frames or document. | high |
| `--inspect-wait` | none (port via `=` alias) | passthrough | Valueless; waits for attach (incl. before the preload). | med |
| `--inspect-port, --debug-port` | space-or-equals | passthrough | VALUE-TAKING. `node --inspect-port probe.js` consumes `probe.js` as host:port (confirmed). Consume the next token; do NOT treat the port as the script. | high |
| `--inspect-publish-uid` | space-or-equals | passthrough | Value-taking string (comma list). Consume value token. | low |
| `--cpu-prof` | none | passthrough | Boolean. `node --cpu-prof cp.js 9999` runs cp.js as ENTRYPOINT (confirmed) — does NOT take a filename. Filename is `--cpu-prof-name`. | high |
| `--cpu-prof-name/-dir/-interval` | space-or-equals | passthrough | Value-taking. Consume value token. | low |
| `--heap-prof` | none | passthrough | Boolean, symmetric with `--cpu-prof` (confirmed `node --heap-prof cp.js` runs cp.js). | high |
| `--heap-prof-name/-dir/-interval` | space-or-equals | passthrough | Value-taking. Consume value token. | low |
| `--prof` | none | passthrough | V8Option boolean (no value). Must reach the child (parent never starts V8). | med |
| `--perf-basic-prof[-only-functions]` | none | passthrough | V8Option booleans. Forward to child. | low |
| `--heapsnapshot-signal` | space-or-equals | passthrough | Value-taking; effect depends on the parent RELAYING the signal to the child. | med |
| `--heapsnapshot-near-heap-limit` | space-or-equals | passthrough | Value-taking integer. | low |
| `--diagnostic-dir` / `--report-dir[ectory]` / `--report-filename` | space-or-equals | passthrough | Value-taking strings. Consume value token. | low |
| `--report-signal` | space-or-equals | passthrough | Value-taking; Implies `--report-on-signal`; depends on signal relay. | med |
| `--report-on-*` / `--report-compact` / `--report-exclude-*` | none (bool) | passthrough | Valueless toggles. | low |
| `--trace-*` (deprecation, warnings, exit, sync-io, tls, uncaught, env, …) | none (bool) | passthrough | Valueless. `--trace-sync-io` will fire constantly under Nub's sync registerHooks — expected; see [[research/node-flag-interactions]]. | low |
| `--trace-event-categories` / `--trace-event-file-pattern` | space-or-equals | passthrough | Value-taking. | low |
| `--stack-trace-limit` | space-or-equals | boundary-critical | See §6 — node-owned kInteger that dual-pushes to V8; space form aborts but consumes the value either way. | high |

### Confirmed inspector/prof facts (reproduced)

Three reproductions separating the valueless toggles from their value-taking siblings, plus the `--prof-process` entrypoint's tolerance of an injected `--import`.

- `node --inspect-port probe.js` → consumes `probe.js` as the host:port value (errors "Unable to resolve…"), no entrypoint run; `node --inspect-port probe.js extra.js` → probe.js is the port, extra.js the entrypoint (confirmed).
- `node --cpu-prof cp.js` / `node --heap-prof cp.js` → cp.js is the ENTRYPOINT, profile written (confirmed); the next token is NOT a filename.
- `node --prof-process --some-bogus-flag` → errors INSIDE `node:internal/v8_prof_polyfill` ("Cannot access log file: --some-bogus-flag"), proving the synthetic `--` and tick-processor dispatch (confirmed). `node --import ./preload.mjs --prof-process LOG` → preload does NOT run, LOG read correctly (confirmed) — node silently ignores injected `--import` here, so Nub should skip injection rather than rely on that tolerance.

### Cross-cutting: signal relay & the V8 parent

Half this category's observable effect depends on the resident parent FORWARDING signals to the child node. If Nub's parent eats these instead of relaying, the diagnostic flags silently no-op.

The signals: `--inspect` (SIGUSR1), `--heapsnapshot-signal`, `--report-signal`/`--report-on-signal` (SIGUSR2 default), SIGINT to a paused `--inspect-brk`.

The inspector PORT double-bind is NOT a risk while the parent stays a thin non-V8 spawner — only the child binds the port (confirmed not reproducible; precluded by architecture).

## 5. V8 options & runtime/memory tuning — mass passthrough, value-form sensitive

Nub injects zero flags in this category, so it is pure passthrough and the entire risk is the splitter's value-form classification. The category splits into two disjoint, superficially identical parse behaviors.

| Flag | Value-form | Verdict | Nub-as-node handling | Risk |
|------|-----------|---------|----------------------|------|
| `--max-old-space-size` | equals-only | boundary-critical | V8Option. NEVER consumes the next token; `=`-glued only. `--max-old-space-size 100 b.js` → bare flag to V8 (aborts exit 9), `100` is the FIRST positional. Boundary is `100`, not `b.js`. | high |
| `--max-semi-space-size`, `--max-heap-size` | equals-only | boundary-critical | Identical V8Option contract. | high |
| `--stack-size` | equals-only | boundary-critical | Pure V8 flag (falls through node's unrecognized→V8Option path). Never consume next token. | high |
| `--jitless` / `--no-opt` / `--expose-gc` / `--disallow-code-generation-from-strings` / `--interpreted-frames-native-stack` / … | none | passthrough | V8Option bool toggles; next token is always the script. | low |
| `--harmony-*` / the ~971 `--v8-options` entries | none / `=`-only | passthrough | One bucket, one rule: any token Nub doesn't recognize as node-owned-valued is a V8/unknown token that consumes NO following arg; the next bareword is the boundary. | med |
| `--v8-options` | none | early-exit | Prints V8's flag dump, exit 0, no script. Forward verbatim; don't print ahead of node; don't let injection alter stdout/exit. | med |
| `--v8-pool-size` | space-or-equals | passthrough | NODE-owned kUInteger — DOES consume the following token (`--v8-pool-size 4 b.js` runs, boundary at b.js). Counterexample proving you can't infer arity from the `--v8-` prefix. | high |
| `--stack-trace-limit` | space-or-equals | boundary-critical | NODE-owned kInteger — consumes the next token AND dual-pushes the bare token to V8 (space form then aborts at V8). Boundary is the token AFTER the value. | high |
| `--max-old-space-size-percentage` | space-or-equals | passthrough | NODE-owned kString — consumes the following token. | med |

### Confirmed V8 facts (reproduced on v26.2.0)

Three reproductions establishing that arity cannot be inferred from a flag's name shape.

- `node --max-old-space-size 100 b.js A` → V8 "illegal value … size_t", exit 9; `100` becomes the first positional (confirmed). Equals form `--max-old-space-size=100 b.js A` runs with boundary at b.js. Prepending Nub's flags (`--enable-source-maps --disable-warning=ExperimentalWarning`) does NOT change this — each V8Option token is self-contained (confirmed).
- `node --v8-pool-size 4 b.js A B` → exit 0, `4` consumed, boundary at b.js, args=[A,B] (confirmed). DIVERGENT from `--max-old-space-size`.
- `node --stack-trace-limit 20 b.js` → node stores 20 (boundary correct) but dual-pushes a valueless `--stack-trace-limit` to V8 → "illegal value of type int", exit 9 (confirmed). `--stack-trace-limit=20` works. `node --stack-trace-limit` (no token) → "requires an argument", exit 9.

The classification must come from node's actual option-type table — typed-field binding / kUInteger / kInteger / kString vs `V8Option{}` — version-pinned, not from name shape. The safe default for any unknown `--flag` the version-pinned table does not recognize is V8 passthrough consuming no following token, matching `node_options-inl.h:441-444`.

## 6. Boundary parser contract

The single algorithm Nub-as-node lives or dies by, stated precisely and pinned to the detected node version.

**Inputs:** the argv after `argv[0]` (the spelled-as-`node` token), and the inherited `NODE_OPTIONS`.

**Algorithm (replicating node's left-to-right loop, `node_options-inl.h`):**

1. Walk tokens left to right. For each token `t`:
   - If `t == "--"`: STOP. `--` is the hard boundary; it is consumed/dropped (not pushed to argv). Everything after is positional (script + args), including `--`-looking tokens. (`--` is kDisallowedInEnvvar.)
   - If `t.size() <= 1` OR `t[0] != '-'` (i.e. a bare `-` or a non-dash token): STOP. `t` is the script boundary (or stdin sentinel for lone `-`). Do NOT consume it.
   - Else `t` is a flag. **First NORMALIZE the name, then look up arity — node does four pre-arity transforms (`node_options-inl.h:350-412`) that a naive splitter skips, and each one changes which token is the boundary:**
     - **(a) `=`-split is `--`-prefix ONLY.** For a `--long` token, `name` is everything before the first `=`; the value is glued. For a single-dash token, the whole token is the name (so `-e=x` does NOT split — it becomes the unknown flag `-e=x`, rejected by V8 as `bad option`; confirmed).
     - **(b) Underscore normalization.** `_` → `-` for every char from index 2 onward BEFORE lookup. This governs option RECOGNITION and ARITY: `--env_file a.env app.js` is recognized as `--env-file` and CONSUMES `a.env` as its value (so `app.js` is the script, not `a.env`; confirmed). A matcher that only knows the dash spelling mis-reads `a.env` as the script, so normalize before consulting the arity table. Caveat: normalization fixes the boundary but is not always wired through to the FEATURE — on node 26.2 the env-file under the `--env_file` spelling consumes its value yet silently fails to LOAD the file. Nub reproduces that node quirk for free by forwarding verbatim; only the arity normalization is load-bearing for the splitter.
     - **(c) `--no-` negation.** `--no-foo` → `foo` + negation. Legal only for kBoolean / kV8Option; `--no-<valued-flag>` (e.g. `--no-require`) is a hard error, not a boundary. Negated booleans consume no token.
     - **(d) Alias expansion (carries arity, and can inject a boundary).** Short and legacy spellings expand via node's alias map (`node_options.cc` `AddAlias`) BEFORE arity is known: `-r`→`--require`, `-e`→`--eval`, `-p`→`--print`, `-pe`→`{--print,--eval}`, `-c`→`--check`, `-i`→`--interactive`, `-C`→`--conditions`, `--loader`→`--experimental-loader`, `--inspect=`→`{--inspect-port,--inspect}`. The alias inherits the target's arity — `-r ./pre.js app.js` consumes `./pre.js` exactly like `--require` (confirmed). Two aliases expand to MULTIPLE tokens (`name + " <arg>"` aliases only match when a following non-dash arg exists), and one — `--prof-process` → `{--prof-process, --}` — **injects a `--` hard boundary**, so everything after `--prof-process` is positional. Nub's arity table MUST therefore include node's alias map, version-pinned, not just the canonical long flags.
   - Then determine its ARITY from the version-pinned option-type table:
     - **Value-consuming in space form (consume the NEXT token as the value, regardless of leading `-` only when node does):** node-owned kString / kInteger / kUInteger / kHostPort / string-list flags. The load-bearing members: `--require`/`-r`, `--import`, `--loader`/`--experimental-loader`, `--conditions`/`-C`, `--eval`/`-e`, `--print`/`-p`, `--input-type`, `--env-file`, `--env-file-if-exists`, `--disable-warning`, `--redirect-warnings`, `--watch-path`, `--watch-kill-signal`, `--inspect-port`/`--debug-port`, `--inspect-publish-uid`, `--cpu-prof-name`/`-dir`/`-interval`, `--heap-prof-name`/`-dir`/`-interval`, `--heapsnapshot-signal`, `--heapsnapshot-near-heap-limit`, `--diagnostic-dir`, `--report-dir`/`-directory`/`-filename`/`-signal`, `--trace-event-categories`/`-file-pattern`, `--v8-pool-size`, `--stack-trace-limit`, `--max-old-space-size-percentage`, `--title`, `--icu-data-dir`. Consume `t`'s value: if `t` is `--long=val`, the value is glued; else the value is the NEXT token (and if that token starts with `-`, node errors "requires an argument" — confirmed for `--require -p app.js` and `--import --version`; the one exception is a leading `\-`, which node un-escapes to `-` and accepts as the value, `node_options-inl.h:474`).
     - **Valueless (consume NO following token):** all booleans (`--inspect`, `--inspect-brk`, `--cpu-prof`, `--heap-prof`, `--prof`, `--check`/`-c`, `--interactive`/`-i`, every `--trace-*`/`--report-on-*` toggle, all V8Option booleans).
     - **V8Option value-takers (`=`-form ONLY, NEVER consume next token):** `--max-old-space-size`, `--max-semi-space-size`, `--max-heap-size`, `--stack-size`, the long tail of `--v8-options`. A bare V8Option token is terminal-for-itself; the following token can be the script boundary.
     - **Unknown `--flag`:** treat as V8/unknown — consume NO following token (default-safe per `node_options-inl.h:441-444`). Let real node produce its own `bad option` error if it's genuinely bad.
2. The first STOP token is the script boundary (or the eval/stdin/REPL sentinel — see the run-mode precedence in §2).
3. **Injection point:** insert Nub's auto-flags + the user's node-flags BEFORE the boundary. For `--import` specifically, Nub's preload `--import` must come BEFORE any user `--import` (which is itself before the boundary). For `--`, inject before the `--`. Forward the boundary token + remainder VERBATIM.

**Hard invariants:**
- Reproduce node's ABORTS and EXIT CODES byte-for-byte. `--max-old-space-size 100 app.js` must reproduce node's exit-9 V8 abort, NOT "helpfully" run app.js. Running code node refuses to run is the worst failure mode.
- Never consume the token after a V8Option as its value. Never fail to consume the token after a node-owned space-form value flag.
- Stop the scan at `--`; never strip `--` while continuing the scan (a later flag-named protected arg would be misclassified). Forwarding `--`+rest to real node is correct (node re-drops `--`).
- `=`-split only `--`-prefixed tokens.
- The version-pinned table is mandatory. The `--v8-pool-size` (consumes) vs `--max-old-space-size` (does not) pair proves name-shape inference is unsound. When the detected node version is unknown to the table, fall back to the default-safe "unknown `--flag` consumes nothing" rule.

## 7. Injected-flag collision matrix

Nub injects five flags, all of them kAllowedInEnvvar: an `--import` preload, `--enable-source-maps`, `--disable-warning=ExperimentalWarning`, and the version-gated `--experimental-sqlite` / `--experimental-websocket`.

The `--import` preload is always injected; source-maps and the warning disable are universal; the two experimental flags go in only on node versions where they are still flagged. The collisions, with the merge rule:

| Injected flag | Type | User collision | Merge rule | Confirmed |
|---------------|------|----------------|------------|-----------|
| `--import <preload>` | string-list | user `--import x.ts`; `--no-import` rejected | Prepend BEFORE all user `--import`. No per-flag opt-out (string-list); escape is `--node`. Emit ONE canonical absolute path across argv+NODE_OPTIONS. | yes |
| `--enable-source-maps` | bool, last-wins | user `--no-enable-source-maps` (argv or NODE_OPTIONS) | SUBTRACT: if `--no-…` appears in argv OR NODE_OPTIONS, drop `--enable-source-maps` from the would-inject set. Do NOT rely on last-wins. | yes |
| `--disable-warning=ExperimentalWarning` | string-list (accumulates) | user `--disable-warning=X` | Additive, safe to coexist. No `--no-` removal; Nub-side `--show-warnings` subtracts it from the would-inject set. | yes |
| `--experimental-sqlite` | bool, last-wins, version-gated | user `--no-experimental-sqlite` | SUBTRACT (same as source-maps). Inject only on versions where still flagged. | yes |
| `--experimental-websocket` | bool, last-wins, version-gated | user `--no-experimental-websocket` (truly removes the global) | SUBTRACT. Default-true in supported range so Nub injects nothing there; the negation is the meaningful user action. | yes |

### The load-bearing collision rule: NODE_OPTIONS is parsed BEFORE argv

Confirmed: `NODE_OPTIONS="--no-enable-source-maps" node --enable-source-maps -e 'process.sourceMapsEnabled'` → `true`.

The argv positive stomps the user's NODE_OPTIONS opt-out, because NODE_OPTIONS is parsed first and node is last-wins with argv last, and the same holds for every kBoolean Nub injects. The only correct mechanism is a three-stage subtract: scan both NODE_OPTIONS and argv for the `--no-X` of any would-inject flag, and remove the positive before emitting on either channel. Relying on node's last-wins silently re-enables features the user disabled in NODE_OPTIONS.

### Idempotency & dedup

Redundant injection of the same flag is safe; redundant injection of the same preload under two different spellings is not.

- Double `--enable-source-maps` / double `--disable-warning=ExperimentalWarning` are harmless (confirmed idempotent / accumulating).
- Double `--import` of the SAME canonical path runs once (node realpath-dedups, confirmed). Two DIFFERENT spellings run twice — emit one canonical absolute path on both channels; a preload-internal registration sentinel is cheap insurance.

### Disallowed-in-NODE_OPTIONS guard

Six things are kDisallowedInEnvvar: `-e`/`--eval`, `-p`/`--print`, `-c`/`--check`, `-i`/`--interactive`, `--`, and unknown flags.

Pushing any into NODE_OPTIONS makes node exit 9 "is not allowed in NODE_OPTIONS" for EVERY descendant (confirmed). Today's injected set is all kAllowedInEnvvar (confirmed: `--import`/`--require`/`--enable-source-maps`/`--disable-warning`/`--experimental-sqlite`/`--experimental-websocket` accepted). Any future table addition must be allowlist-checked against the target version's kAllowedInEnvvar set before going on the NODE_OPTIONS channel. Note the asymmetry: a POSITIONAL in NODE_OPTIONS is silently DROPPED, not fatal (confirmed), but an unknown FLAG is fatal exit-9. An unknown flag does not merely warn; redundant injection is safe only for flags the target version still recognizes, including ones demoted to NoOp.

### Env reconstruction hazard

node's own `child_process` force-propagates `NODE_V8_COVERAGE` (and `NODE_OPTIONS`) to children even when user code passes a custom `env`.

Nub-as-node now mediates every spawn; it must INHERIT the env and mutate only the keys it owns (append to `NODE_OPTIONS`, prepend the shim dir to `PATH`), NEVER rebuild from a fixed key set, or it silently drops these across the tree.

## 8. Early-exit & node-identity rules

**Identity mode is decided by `argv[0]`, not by a heuristic.** Nub dispatches on `basename(argv[0])`, which splits the two cases that would otherwise conflict on `--version`/`--help`:

- **Node-identity mode** — Nub was reached through the hijack shim, so a descendant process invoked `node` and `argv[0]` is `node`/`node.exe`. Every identity surface below must be the real spawned node's, byte-for-byte, because the caller is build tooling (node-gyp, npm, `child_process.spawn('node', …)`) that parses it.
- **Nub-identity mode** — the user typed `nub`/`nubx` directly, so `argv[0]` is `nub`, `nub --version` prints Nub's version, and `nub --help` prints Nub's help. Printing node's identity here would be the wrong answer.

The discriminator is deterministic and unspoofable, since node cannot rewrite the `argv[0]` Nub sees, so there is no detect-if-hijacked guesswork. The rules in this section govern node-identity mode; the direct-`nub` surface is Nub's own CLI. One corollary: in node-identity mode `--version` must reflect whatever node Nub actually spawns — forward the flag, or shell `node --version` — never a string baked into Nub, so it tracks the user's installed and selected Node.

Early-exit flags fire only when parsed as a node option: before the script boundary, before `--`, and not bound as a preceding option's value. They also fire after `-e`/`-p`/`--eval`/`--print`, which do not establish a boundary — node keeps parsing options after the eval value.

| Flag | Verdict | Identity / handling | Confirmed |
|------|---------|---------------------|-----------|
| `--version, -v` | early-exit, passthrough | Print the REAL spawned node's version string byte-for-byte (forward the flag, or shell `node --version`). NEVER Nub's package version — node-gyp/npm/install scripts grep it. `-v` is a pure alias (no "verbose" meaning). | yes |
| `--help, -h` | early-exit, passthrough | Stream real node's help verbatim (ends with the nodejs.org doc link + NODE_* table). Never regenerate/augment — would leak Nub flags into a node-branded surface. | yes |
| `--v8-options` | early-exit, passthrough | V8's own help dump (~1900 lines). Must reach real node. Boolean, no value. | yes |
| `--completion-bash` | early-exit, passthrough | Emits a bash script embedding the REAL node version's COMPLETE flag list inside `compgen -W` and `complete … node node_g`. NEVER synthesize/append — direct vector for leaking Nub flags + version drift. | yes |
| `--prof-process` / `inspect` | early-exit | Alternate entrypoints; skip all injection; forward tail opaquely (see §2/§4). | yes |
| `--print-help` / `--report-help` | n/a | Do NOT exist (only 4 print_* booleans exist: print_bash_completion, print_help, print_v8_help, print_version). Don't fabricate; forward unknown spellings so real node errors. | yes |

### Confirmed early-exit facts (reproduced)

Four reproductions, all of which say the same thing: whether a token is an early-exit flag or a positional argument is decided by node's own parse, never by pattern-matching argv.

- `node -e 'console.log("X")' --version` → prints `v26.2.0`, exit 0; the eval is NEVER run (confirmed). This directly refutes "early-exit only fires before the script boundary" — with `-e`/`-p` there is no script PATH, so option parsing continues and `--version` still fires. Only a real script PATH (`node t.js --version` → script runs, `--version` in argv) or `--` (`node -e x -- --version` → eval runs, `--version` in argv) demotes it.
- Value-escape: `node --require=--version -e 'ran'` → `--version` is the VALUE of `--require` (ERR_MODULE_NOT_FOUND), no version print (confirmed). `node --require --version t.js` → `--require requires an argument` (confirmed). So whether `--version` is early-exit depends on the value-form of the PRECEDING option. NEVER pattern-match argv for the literal `--version`; run node's parse.
- Precedence: `node -e x --completion-bash --version` AND the reverse both print the version (confirmed) — `print_version` resolves before `print_bash_completion` in `node.cc`. Forwarding to real node preserves this for free; don't reimplement the ordering.
- `node --import ./preload.mjs --version` → preload runs, then version prints (confirmed) — the preload PATH is `--import`'s value, so a trailing `--version` is the next option and fires. Injecting `--import <abs>` strictly before user argv is order-safe.

**The identity invariant:** every place node encodes its own version (`--version`/`-v`) or its own flag set (`--completion-bash`, `--help`, `--v8-options`) MUST come from the real spawned node. Printing Nub's identity here is simultaneously a brand-boundary violation and a hard compat break. Nub's auto-injected flags are inert when an early-exit flag triggers (node early-returns before running the preload — confirmed for `--v8-options` and parse-time aborts), but they must still be placed strictly before the boundary so they never shift node's option-vs-script-arg classification.

## 9. Confirmed high-severity risks (with mitigations)

Ranked, all reproduced.

1. **Boundary mis-location from value-consuming flags (critical).** `node --env-file /tmp/a.env /tmp/s.js` consumes `a.env` as the value and runs `s.js`; `node --require ./pre.js main.js` makes `pre.js` the value, `main.js` the script (both confirmed). A naive "first non-dash token" splitter mis-locates the boundary, injects before the wrong file, and can run the env-file/preload path as code. **Mitigation:** the §6 version-pinned arity table; encode `--env-file` and the sibling space-form value flags as arity-1; forward `--env-file` verbatim (it's node's namespace) and preserve its exit-9 missing-file abort + `--env-file-if-exists`'s "Continuing without it." notice byte-for-byte.

2. **V8Option boundary (high).** `--max-old-space-size 100 b.js` (and `--max-semi-space-size`/`--max-heap-size`/`--stack-size`) → V8Option does NOT consume `100`; boundary is `100`; node aborts exit 9 (confirmed). **Mitigation:** classify the whole V8Option family as never-consume; glue `=value`; reproduce the abort + exit code.

3. **`--v8-pool-size` / `--stack-trace-limit` reverse trap (high).** These DO consume the following space token (confirmed), unlike `--max-old-space-size`. **Mitigation:** explicit allowlist of node-owned valued numeric flags; never infer arity from the `--v8-` prefix.

4. **`--inspect-port` / `--cpu-prof` / `--heap-prof` value-form (high).** `--inspect-port probe.js` consumes probe.js as port; `--cpu-prof cp.js` runs cp.js as entrypoint (both confirmed). **Mitigation:** the §4 toggle-vs-setting table — `--cpu-prof`/`--heap-prof`/`--inspect*` toggles are valueless; only their `-name`/`-dir`/`-interval`/`-port` siblings consume.

5. **`--import` injection ORDER + abort-on-missing (high/critical).** Nub's preload must be the FIRST `--import` (before user `--import`); a non-resolvable preload aborts the WHOLE process before main (confirmed). **Mitigation:** prepend Nub's preload `--import`; use a stable absolute install-dir path (never a per-invocation temp file); pass it as a `file://` URL so a user custom loader can't reject it; emit one canonical path across both channels.

6. **NODE_OPTIONS-before-argv last-wins stomp (high).** `NODE_OPTIONS=--no-enable-source-maps` + injected `--enable-source-maps` re-enables what the user disabled (confirmed); same for every injected kBoolean. **Mitigation:** the three-stage subtract against PARSED NODE_OPTIONS + argv before emitting on either channel.

7. **`--` hard-boundary + drop (high).** `node -- main.js --not-a-node-flag x` → both after-`--` tokens are argv, `--` dropped (confirmed); kDisallowedInEnvvar. **Mitigation:** stop the scan at `--`; inject before it; forward `--`+rest verbatim; never emit `--` into NODE_OPTIONS.

8. **`--prof-process` alternate entrypoint (high).** Synthetic `--` + tick-processor dispatch; no user JS; node silently ignores injected `--import` (confirmed). **Mitigation:** detect before the boundary; skip ALL injection; forward tail opaquely.

9. **Early-exit fires after `-e`/`-p`; identity must be real node's (high/critical).** `node -e 'X' --version` prints the version, never runs the eval (confirmed). **Mitigation:** run node's parse (never grep argv); print the REAL node's version/help/completion/v8-options verbatim — never Nub's identity.

10. **`--watch` mutual-exclusion coupling (high).** `--watch` + `--eval`/`--check`/`--interactive`/`--test-force-exit` all hard-error exit-9 (confirmed). Nub's TS preload MUST use `--import` (compatible with `--watch`, confirmed) and NEVER `--eval`/`--check` alongside a user `--watch`. **Mitigation:** preload is `--import`-only; preserve the four mutual-exclusion errors verbatim. The "bare `--watch` with no file requires a file" early-exit could NOT be reproduced on shipping node, so forward bare `--watch` verbatim and do not synthesize that error.

11. **`--experimental-transform-types` version-gated hard removal (high).** Removed in node 24+ → `node: bad option`, exit 9 (confirmed), NOT warn-and-ignore. **Mitigation:** NEVER inject it; when a user passes it, forward verbatim and let real node decide (valid on 22.7–23.x, exit-9 on 24+). Nub provides the feature via its own transpiler, which is exactly why an implementer might wrongly intercept it — don't.

12. **`-e`/`-p`/stdin TS-eval transpile obligation (high).** The `--import`/registerHooks file-URL hook NEVER sees the `-e` string (no file URL); on the 18.19–22.14 / 22.15 floor node has no built-in TS-eval. **Mitigation:** detect inline-code mode (has_eval_string OR print_eval) and stdin mode (bare `-` / non-TTY stdin, no script), transpile the (possibly-TS) code Nub-side before handing plain JS to real node, honoring `--input-type`. Never route eval-mode through NODE_OPTIONS (kDisallowedInEnvvar).

13. **`--no-strip-types` failure-mode flip (med, user-mental-model).** `--no-strip-types` does NOT disable Nub's transpiler (Nub owns `.ts`/`.tsx`/`.mts`/`.cts` via its load hook); it only removes node's safety-net for hook-missed files, flipping their failure from "node-stripped, runs" to a SyntaxError/ERR_UNKNOWN_FILE_EXTENSION crash (confirmed; node never backstops `.tsx`). **Mitigation:** forward verbatim; document loudly that the disable knob is `nub run --node`, not the node flag.

14. **Permission-tier breakage (high, tier-dependent).** Under `--permission`: Nub's preload read needs `--allow-fs-read=<canonicalized-install-dir>` (a bare install-dir grant FAILS under a symlinked Homebrew install — node realpath's the `--import` path; confirmed); its N-API transpiler addon needs `--allow-addons` (refuse-early, don't auto-inject — widening); and on the async tier (18.19–22.14) `module.register` spawns an INTERNAL worker that throws "Use --allow-worker" (confirmed on node 26 that the check fires before is_internal is read). The sync registerHooks tier (22.15+) is clean. **Mitigation:** match node's FULL permission-active set (`--permission` AND `--permission-audit`, argv + NODE_OPTIONS); canonicalize the install dir via `current_exe()->canonicalize()` for the fs-read grant; refuse-early on missing `--allow-addons`; on the async tier under `--permission`, refuse-early or force the sync tier. Per-flag augmentation verdicts: [[research/node-flag-interactions|`node-flag-interactions.md` §4.1]].

## 10. Open questions

Six items unresolved, none of them contradicting a confirmed finding above. Most need either a fixture on the 18.19 augmentation floor or a decision record.

- **Version-pinned arity table source of truth.** The §6 table is read off node v26.2.0 source and behavior, while Nub augments node 18.19+, new V8 flags appear, and some flags migrate type across majors. Should Nub bake a per-major option-type table, or derive it at runtime from `node --v8-options` plus a node-owned-flag allowlist? The default-safe unknown-flag-consumes-nothing rule covers the long tail but not node-owned value flags added in a version Nub's table predates. (design-inferred; not yet decided)
- **TLA-of-ambiguous-`.js`-entry under the injected `--import` on the 18.19 floor.** The entry-loader flip is benign on node 26 and not reproduced on the floor, where detect-module differs. Needs a `node:18.19-slim` Docker fixture before claiming full parity. (design-inferred)
- **`--inspect-brk` first-frame.** Ignore-list the preload frames via the V8 inspector skip-list, defer the break, or document that the first paused frame is the loader shim? A user who passes their own `--import` already sees this, so it is augmentation-induced rather than non-conformant against `node + --import`. (design-inferred from `node.cc:312-319`; not reproduced live)
- **Async-tier `--permission` decision.** Refuse-early vs force-sync-tier vs documenting the `--allow-worker` requirement is undecided and needs a decision record.
- **Signal relay completeness.** Half of §4's flags depend on the resident parent forwarding SIGUSR1/SIGUSR2/SIGINT to the child. Whether that relay is implemented and tested is not a flag in this audit, but it gates the observable effect of the whole diagnostic category.
- **Worker-thread `execArgv` inheritance.** Do Nub's hooks register inside `Worker` threads, whose `execArgv` can override flags? Cross-references the same open question in [[research/node-flag-interactions]]; deserves its own fixture.

## Changelog

Revision history for this document. Both entries are from the original audit: the initial write-up, then the §6 boundary-contract and §8 identity-mode hardening reproduced on node 26.2.0.

- 2026-05-31 — Initial write-up (workflow: node-flag-hijack-compat-audit).
- 2026-05-31 — Hardened §6 boundary contract with node's pre-arity parse stages (underscore `_`→`-` normalization, `--no-` negation, alias expansion incl. multi-token `-pe` and the `--prof-process`→`{…,--}` boundary injection, `\-` value-escape), reproduced on node 26.2.0 against `node_options-inl.h` `Parse()`. Added §8 `argv[0]`-dispatch identity-mode discriminator (node-identity vs nub-identity) resolving the `--version`/`--help` tension. Confirmed unknown-flag-is-fatal on both argv and `NODE_OPTIONS` channels (drove the `auto-flag-injection.md` rationale fix). Golden-reference parity suite landed at `tests/flag-parsing/`.
