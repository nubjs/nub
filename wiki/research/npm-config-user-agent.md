# Package-manager user agent — what Nub sets, and how `create-*` scaffolders read it

Nub's run and install-lifecycle paths emit a valid pnpm-shaped `npm_config_user_agent`. The exec paths emit nothing, and the `nub/` leading token used in nub-identity projects is unrecognized by the common scaffolder detectors.

## Question

Does Nub set `npm_config_user_agent` correctly across its PM surfaces, and do `create-*` scaffolders (create-vite, create-next-app, …) recognize what Nub emits?

The `npm_config_user_agent` variable is what a package manager exports to its child processes so downstream tools can detect the invoking PM. Format: `<name>/<version> npm/<ver|?> node/<nodever> <os> <arch>[ extras]`. Scaffolders read it to print correct next-step commands ("Now run `pnpm dev`") and to pick an install command.

## TL;DR

Three surfaces set the string, and the leading token is a deliberate brand choice with a known consequence for scaffolder detection.

- **Nub's `run` and install-lifecycle paths are correct** — valid, role-aware, pnpm-shaped UA.
- **Two findings:**
  1. **The exec surface is three routes, not one, and the reference PMs set `npm_config_user_agent` on all of them.** The routes: `nub exec`/`nubx` (node-bin + non-node branches of `launch_bin`) and the engine dlx path (`nub x`/`nub dlx`/`nub create`, spawning in aube's `exec_bin`). **PR #260 fixes the `nub exec`/`nubx` routes; the engine dlx path is a separate aube-side follow-up.** Mechanical parity bug.
  2. **In nub-identity / fresh projects the UA leads with `nub/`**, which the common detectors don't recognize — `package-manager-detector.getUserAgent()` returns `null` and create-next-app's `startsWith` whitelist falls back to `npm`. (create-vite, which passes the token through raw, is the exception and prints correct `nub` commands.) This is a brand/product call.

## Reference PMs — empirical (pinned versions)

Captured on node v26.2.0, darwin arm64. Each PM spawns a child that prints `process.env.npm_config_user_agent`, in both a `postinstall` lifecycle script and an `exec`/`dlx` context. Strings are verbatim and stable across re-runs.

| PM | Version | UA (identical in lifecycle + exec) | Leading token |
|----|---------|-------------------------------------|---------------|
| npm | 11.13.0 | `npm/11.13.0 node/v26.2.0 darwin arm64 workspaces/false` | `npm/11.13.0` |
| pnpm | 10.15.1 | `pnpm/10.15.1 npm/? node/v26.2.0 darwin arm64` | `pnpm/10.15.1` |
| yarn (v1) | 1.13.0 | `yarn/1.13.0 npm/? node/v26.2.0 darwin arm64` | `yarn/1.13.0` |
| bun | 1.3.14 | `bun/1.3.14 npm/? node/v24.3.0 darwin arm64` | `bun/1.3.14` |

Observations:
- All four lead with `<name>/<version>` — the token consumers switch on.
- pnpm / yarn / bun all emit the literal `npm/?` placeholder for the npm-version slot (they don't know the host npm version); npm fills in its own.
- Only npm appends trailing fields (`workspaces/false`, `ci/true`, …); the others stop at `<os> <arch>`.
- bun reports its embedded Node compat version (`node/v24.3.0`), not the host's.
- All set it identically in lifecycle and exec. (yarn 1's `yarn node` is the one carve-out — it does NOT set it; `yarn exec` and lifecycle do.)

## Nub — code + empirical

Where Nub sets it:
- **Install lifecycle** (dep postinstalls): `vendor/aube/crates/aube-scripts/src/lib.rs` — `aube_user_agent()` → `cmd.env("npm_config_user_agent", …)`. Product token comes from the engine context's `lifecycle_user_agent_product`, set by the PM engine at `crates/nub-cli/src/pm_engine/mod.rs:733`.
- **`nub run` / `nub exec` (script)**: `crates/nub-core/src/workspace/scripts.rs:206` (`npm_env`), product threaded from `crates/nub-cli/src/pm_engine/mod.rs::run_lifecycle_ua_product` → `compose_lifecycle_ua`.

The composer (`compose_lifecycle_ua`, mod.rs:1164) is role-aware:
- **nub-identity / fresh** → `nub/<v> npm/? node/v<ver> <os> <arch>` (leads with `nub/`)
- **compat mode** (incumbent npm/pnpm/yarn/bun detected) → `<incumbent>/<ver> nub/<v> node/v<ver> <os> <arch>` (leads with the incumbent token)

Empirical capture (dev build at commit `ba6648a`, v0.2.10):

| # | Context | Set? | Verbatim UA | Leading token |
|---|---------|------|-------------|---------------|
| 1 | `nub run` — fresh / nub-identity | yes | `nub/0.2.10 npm/? node/v26.2.0 darwin arm64` | `nub/` |
| 2 | `nub run` — pnpm incumbent | yes | `pnpm/9.1.0 nub/0.2.10 node/v26.2.0 darwin arm64` | `pnpm/` |
| 3 | `nub run` — npm incumbent | yes | `npm/10.5.0 nub/0.2.10 node/v26.2.0 darwin arm64` | `npm/` |
| 4 | **`nub x` / `dlx` / `nubx` exec** | **NO** | **unset** | — |
| 5a | install postinstall — fresh | yes | `nub/0.2.10 npm/? node/v26.2.0 darwin arm64` | `nub/` |
| 5b | install postinstall — pnpm incumbent | yes | `pnpm/9.1.0 nub/0.2.10 node/v26.2.0 darwin arm64` | `pnpm/` |

The format is valid: it matches pnpm/yarn's shape byte-for-byte — same `npm/?` placeholder, same `node/v<ver> <os> <arch>` tail, Node's `process.platform`/`process.arch` vocabulary. The only issues are the exec path not setting it, and the nub-identity leading token.

### The exec surface — three routes, not one

Verified empirically and at source. All three left `npm_config_user_agent` unset:

1. **`nub exec` / `nubx`** → `run_exec_with_dlx` → `launch_bin` (`crates/nub-cli/src/cli.rs`). `launch_bin` has TWO branches: a **node-bin** branch (`is_node_bin` true → `run_file_in_dir`, spawned as `node <bin>` — the common `create-*` case, since a scaffolder's `.bin` entry is a node script) and a **non-node** branch (`apply_exec_augmentation`). `apply_exec_augmentation` set `NODE`/`NODE_OPTIONS`/`NODE_PATH`/`PATH` plus the localStorage signal but never the UA; `run_file_in_dir` built its child env without it.
2. **`nub x` / `nub dlx` / `nub create`** → the aube ENGINE (`aube::commands::dlx::run` / `create::run`), which spawns the resolved bin INSIDE aube (`vendor/aube/crates/aube/src/commands/exec.rs::exec_bin` for a local-bin hit, or a direct spawn for a fetched package). Neither aube spawn sets the UA. This is a distinct surface from (1) — `nub x`/`dlx` are aliases of the engine `dlx` verb, NOT of `nub exec`.

Reference tools all set it in exec. Nub's `launch_bin` branches set it the same way as `run`, reusing `run_lifecycle_ua_product` plus a shared `scripts::user_agent_string` helper (#260); the engine dlx path takes its product from the engine context.

## Consumers — how `create-*` detect the PM

Three parsers were fed the two candidate UAs: `getUserAgent()` from `package-manager-detector@1.6.0` (the shared dependency many scaffolders use), plus create-next-app's and create-vite's own.

| UA (leading token) | package-manager-detector `getUserAgent()` | create-next-app `getPkgManager()` | create-vite `pkgFromUserAgent()` → printed cmds |
|---|---|---|---|
| `nub/0.2.9 npm/? node/…` | **`null`** | **`npm`** (fallback) | `nub` → `nub install`, `nub run dev` (correct) |
| `pnpm/9.1.0 nub/0.2.9 node/…` | `"pnpm"` | `pnpm` | `pnpm` → `pnpm install`, `pnpm dev` |

Detector logic:
- **package-manager-detector** (`src/detect.ts`): `getUserAgent()` does `name = userAgent.split('/')[0]` then `AGENTS.includes(name) ? name : null`. `AGENTS` is a fixed compile-time array `['npm','yarn','yarn@berry','pnpm','pnpm@6','bun','deno']`. `nub` isn't in it → `null`. No plugin/registry/extension hook on this path; adding a PM means a PR to that array plus a release. (`detect()` itself is UA-independent — it walks lockfiles / `packageManager` / `devEngines`; `getUserAgent()` is the only UA entry point.)
- **create-next-app** (`helpers/get-pkg-manager.ts`): hardcoded `userAgent.startsWith('yarn'|'pnpm'|'bun')`, else `return 'npm'`. `nub/…` → `npm`.
- **create-vite** (`packages/create-vite/src/index.ts`): rolls its own `pkgFromUserAgent` with no whitelist — takes `split(' ')[0].split('/')[0]` raw, so `nub` passes through and it prints correct `nub install` / `nub run dev`.

There is no shared source of truth — every scaffolder that rolls its own detector must be updated individually to recognize a new token.

**Crux:** a `pnpm`-leading UA is recognized by every detector (the trailing `nub/` token is ignored). A `nub`-leading UA is recognized only by raw-passthrough tools (create-vite); the whitelist family misdetects it as npm. Scaffolders are typically run in a fresh/empty directory — nub-identity mode — so the `nub/`-lead is the case that reaches them in practice.

## Decision-record cross-check

Three settled positions bear on this: the PM-run compat decision that fixed the UA format, the brand boundary covering vars Nub sets for its children, and the pnpm-compatible CLI grammar.

- **The PM-run compat decision** settled that Nub SHOULD emit a nub-identifying UA `nub/<version> npm/? node/<v> <platform>`, on the grounds that the `npm/?` placeholder is what yarn-berry does and the field name is npm-canonical. That decision is honesty-first and is what the run/lifecycle paths implement. It was made **without the create-* consumer analysis above**, so it doesn't account for the misdetection cost or the exec-path gap — this doc extends it rather than overturning it.
- **Brand boundary:** `npm_config_user_agent` is a var Nub SETS for its children — internal mechanism, not a public API users import — so branding it `nub/` is allowed and here is the honest choice. The tension is UX (detector recognition), not brand-boundary compliance. Leading with `pnpm/` in nub-identity mode would be Nub advertising itself as pnpm on a string third parties read.
- **pnpm-compat axis:** Nub's CLI grammar is pnpm-compatible, so `pnpm`-shaped next-step commands a scaffolder prints (`pnpm install`, `pnpm dev`) all work under Nub — which is what makes the compat-mode incumbent-lead safe, and what a masquerade option would lean on.

## Current behavior

Nub sets a role-aware `npm_config_user_agent` on `nub run`, on install lifecycle scripts, and on the bin-exec routes.

In a nub-identity project the leading token is Nub's own — `nub/<version> npm/? node/<v> <platform>`, the shape yarn-berry uses for its `npm/?` placeholder — so scaffolders that match the token against a whitelist (create-next-app, the `package-manager-detector` family) fall back to npm's next-step commands until they recognize `nub`, while name-generic detectors (create-vite) print `nub` commands. In a compat project the incumbent's token leads, so every detector prints the incumbent's commands, which Nub's pnpm-compatible grammar runs as written. The honest lead is the deliberate choice: an incumbent-shaped token in a nub-identity project would advertise a package manager that is not there.

## Changelog

Two revisions, both 2026-06-30: the initial audit, then the exec-surface correction that found three bin-exec routes rather than one.

- 2026-06-30 — **Exec-surface correction (#260).** Empirical build showed the bin-exec surface is THREE routes, not the single `apply_exec_augmentation` the initial write-up named: `nub exec`/`nubx` split into a node-bin branch (`run_file_in_dir`) and a non-node branch (`apply_exec_augmentation`) under `launch_bin`, and `nub x`/`nub dlx`/`nub create` route through the aube engine's `exec_bin` — all three left `npm_config_user_agent` unset. PR #260 fixes the `nub exec`/`nubx` routes (both branches, reusing `run_lifecycle_ua_product` + a shared `scripts::user_agent_string`); the engine dlx path stays open as an aube-side follow-up. Gap 2 (nub/-lead misdetection) unchanged.
- 2026-06-30 — Initial write-up. Audited nub's `npm_config_user_agent` across run/lifecycle/exec vs npm 11.13.0 / pnpm 10.15.1 / yarn 1.13.0 / bun 1.3.14, and consumer behavior in package-manager-detector 1.6.0 / create-next-app / create-vite. Found (1) exec-path (`nub x`/`dlx`/`nubx`) sets nothing — parity gap; (2) nub-identity UA leads with unrecognized `nub/` → misdetected as npm by the whitelist-detector family (create-vite is the raw-passthrough exception).
- 2026-08-28 — Trimmed to the measured findings and current behavior.
