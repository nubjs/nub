# `npm_config_user_agent` — what nub sets, and the create-* scaffolder gap

## Question

Does nub set `npm_config_user_agent` correctly across its PM surfaces, and do `create-*` scaffolders (create-vite, create-next-app, …) recognize what nub emits?

`npm_config_user_agent` is the env var a package manager exports to its child processes so downstream tools can detect the invoking PM. Format: `<name>/<version> npm/<ver|?> node/<nodever> <os> <arch>[ extras]`. Scaffolders read it to print correct next-step commands ("Now run `pnpm dev`") and to pick an install command.

## TL;DR

- **nub's `run` and install-lifecycle paths are correct** — valid, role-aware, pnpm-shaped UA.
- **Two gaps:**
  1. **The exec paths do NOT set `npm_config_user_agent`** — a straight divergence from every reference PM, and the exact path scaffolders run through. The surface is THREE routes: `nub exec`/`nubx` (node-bin + non-node branches of `launch_bin`) and the engine dlx path (`nub x`/`nub dlx`/`nub create`, spawning in aube's `exec_bin`). **PR #260 fixes the `nub exec`/`nubx` routes; the engine dlx path is a separate aube-side follow-up.** Mechanical parity bug.
  2. **In nub-identity / fresh projects the UA leads with `nub/`**, which the common detectors don't recognize — `package-manager-detector.getUserAgent()` returns `null` and create-next-app's `startsWith` whitelist falls back to `npm`. (create-vite, which passes the token through raw, is the lucky exception and prints correct `nub` commands.) This is a brand/product call.

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

## nub — code + empirical

Where nub sets it:
- **Install lifecycle** (dep postinstalls): `vendor/aube/crates/aube-scripts/src/lib.rs` — `aube_user_agent()` → `cmd.env("npm_config_user_agent", …)`. Product token comes from the engine context's `lifecycle_user_agent_product`, set by the PM engine at `crates/nub-cli/src/pm_engine/mod.rs:733`.
- **`nub run` / `nub exec` (script)**: `crates/nub-core/src/workspace/scripts.rs:206` (`npm_env`), product threaded from `crates/nub-cli/src/pm_engine/mod.rs::run_lifecycle_ua_product` → `compose_lifecycle_ua`.

The composer (`compose_lifecycle_ua`, mod.rs:1164) is role-aware:
- **nub-identity / fresh** → `nub/<v> npm/? node/v<ver> <os> <arch>` (leads with `nub/`)
- **compat mode** (incumbent npm/pnpm/yarn/bun detected) → `<incumbent>/<ver> nub/<v> node/v<ver> <os> <arch>` (leads with the incumbent token)

Empirical capture (dev binary `~/.cache/nub/shared-target/fast/nub`, commit `ba6648a`, v0.2.10):

| # | Context | Set? | Verbatim UA | Leading token |
|---|---------|------|-------------|---------------|
| 1 | `nub run` — fresh / nub-identity | yes | `nub/0.2.10 npm/? node/v26.2.0 darwin arm64` | `nub/` |
| 2 | `nub run` — pnpm incumbent | yes | `pnpm/9.1.0 nub/0.2.10 node/v26.2.0 darwin arm64` | `pnpm/` |
| 3 | `nub run` — npm incumbent | yes | `npm/10.5.0 nub/0.2.10 node/v26.2.0 darwin arm64` | `npm/` |
| 4 | **`nub x` / `dlx` / `nubx` exec** | **NO** | **unset** | — |
| 5a | install postinstall — fresh | yes | `nub/0.2.10 npm/? node/v26.2.0 darwin arm64` | `nub/` |
| 5b | install postinstall — pnpm incumbent | yes | `pnpm/9.1.0 nub/0.2.10 node/v26.2.0 darwin arm64` | `pnpm/` |

**The format is valid** — it matches pnpm/yarn's shape byte-for-byte (same `npm/?` placeholder, same `node/v<ver> <os> <arch>` tail, Node's `process.platform`/`process.arch` vocabulary). The only issues are (a) the exec path not setting it, and (b) the nub-identity leading token.

**Exec-path gap (verified empirically + at source — the exec surface is THREE paths, not one):** the initial read named `apply_exec_augmentation` as the single fix site, but running the dev binary shows the bin-exec surface splits into three routes, all of which left `npm_config_user_agent` unset:

1. **`nub exec` / `nubx`** → `run_exec_with_dlx` → `launch_bin` (`crates/nub-cli/src/cli.rs`). `launch_bin` has TWO branches: a **node-bin** branch (`is_node_bin` true → `run_file_in_dir`, spawned as `node <bin>` — this is the common `create-*` case, since a scaffolder's `.bin` entry is a node script) and a **non-node** branch (`apply_exec_augmentation`). `apply_exec_augmentation` set `NODE`/`NODE_OPTIONS`/`NODE_PATH`/`PATH` + the localStorage signal but never the UA; `run_file_in_dir` built its child env without it.
2. **`nub x` / `nub dlx` / `nub create`** → the aube ENGINE (`aube::commands::dlx::run` / `create::run`), which spawns the resolved bin INSIDE aube (`vendor/aube/crates/aube/src/commands/exec.rs::exec_bin` for a local-bin hit, or a direct spawn for a fetched package). Neither aube spawn sets the UA. This is a distinct surface from (1) — `nub x`/`dlx` are aliases of the engine `dlx` verb, NOT of `nub exec`.

Reference tools all set it in exec; nub left it undefined on all three. **PR #260 fixes (1)** — both `launch_bin` branches, reusing `run_lifecycle_ua_product` + a shared `scripts::user_agent_string` helper. **(2) is aube-side and still open** (a separate follow-up: thread the UA via the engine context / embedder profile under fork-discipline).

## Consumers — how `create-*` detect the PM

`getUserAgent()` from `package-manager-detector@1.6.0` (the shared dependency many scaffolders use), plus create-next-app's and create-vite's own parsers, fed the two candidate UAs:

| UA (leading token) | package-manager-detector `getUserAgent()` | create-next-app `getPkgManager()` | create-vite `pkgFromUserAgent()` → printed cmds |
|---|---|---|---|
| `nub/0.2.9 npm/? node/…` | **`null`** | **`npm`** (fallback) | `nub` → `nub install`, `nub run dev` (correct) |
| `pnpm/9.1.0 nub/0.2.9 node/…` | `"pnpm"` | `pnpm` | `pnpm` → `pnpm install`, `pnpm dev` |

Detector logic:
- **package-manager-detector** (`src/detect.ts`): `getUserAgent()` does `name = userAgent.split('/')[0]` then `AGENTS.includes(name) ? name : null`. `AGENTS` is a fixed compile-time array `['npm','yarn','yarn@berry','pnpm','pnpm@6','bun','deno']`. `nub` isn't in it → `null`. No plugin/registry/extension hook on this path; adding a PM means a PR to that array + a release. (`detect()` itself is UA-independent — it walks lockfiles / `packageManager` / `devEngines`; `getUserAgent()` is the only UA entry point.)
- **create-next-app** (`helpers/get-pkg-manager.ts`): hardcoded `userAgent.startsWith('yarn'|'pnpm'|'bun')`, else `return 'npm'`. `nub/…` → `npm`.
- **create-vite** (`packages/create-vite/src/index.ts`): rolls its own `pkgFromUserAgent` with no whitelist — takes `split(' ')[0].split('/')[0]` raw, so `nub` passes through and it prints correct `nub install` / `nub run dev`.

There is no shared source of truth — every scaffolder that rolls its own detector must be updated individually to recognize a new token.

**Crux:** a `pnpm`-leading UA is recognized by every detector (the trailing `nub/` token is ignored). A `nub`-leading UA is recognized only by raw-passthrough tools (create-vite); the whitelist family misdetects it as npm. Note scaffolders are typically run in a fresh/empty directory — i.e. nub-identity mode — so in practice the `nub/`-lead is the case that actually reaches them.

## Decision-record cross-check

- **`wiki/research/pm-run-compat-scope.md`** already decided nub SHOULD emit a nub-identifying UA `nub/<version> npm/? node/<v> <platform>` ("the `npm/?` placeholder is what yarn-berry does — the field name is npm-canonical"). That decision is honesty-first and is what the run/lifecycle paths implement. It was made **without the create-* consumer analysis above**, so it doesn't account for the misdetection cost or the exec-path gap — this doc extends it rather than overturning it.
- **Brand boundary (AGENTS.md):** `npm_config_user_agent` is a var nub SETS for its children (internal-mechanism side of the boundary — not a public API users import), so branding it `nub/` is allowed and, here, is the *honest* choice. The tension is UX (detector recognition), not brand-boundary compliance. Leading with `pnpm/` in nub-identity mode would be nub advertising itself as pnpm on a string third parties read — a masquerade the honesty bar disfavors.
- **pnpm-compat axis:** nub's CLI grammar is pnpm-compatible, so `pnpm`-shaped next-step commands a scaffolder prints (`pnpm install`, `pnpm dev`) all actually work under nub — which is what makes the compat-mode incumbent-lead safe, and what a masquerade option would lean on.

## Is nub correct today?

**Partially.** The `run` and install-lifecycle paths are correct (valid format, role-aware). The **exec path is incorrect** (sets nothing — divergence from all reference PMs), and the **nub-identity leading token misdetects** in the whitelist-detector family.

## Options and recommendation

Two separable decisions.

### Decision A — the exec-path gap (recommend: fix, low-risk parity)

`nub x`/`dlx`/`nubx` should set `npm_config_user_agent` like every reference PM does. The role-aware machinery already exists (`run_lifecycle_ua_product` / `compose_lifecycle_ua`, taking `cwd` + node version — both available in the exec path); wire it into `apply_exec_augmentation` / `run_exec_with_dlx`. This is parity restoration, not a new product decision — it aligns with the existing "SHOULD set `npm_config_user_agent`" decision that simply wasn't threaded into the bin-exec path. Ship it with an exec-path UA integration test (the current test at `crates/nub-cli/tests/integration.rs:5655` covers only run/exec-*script*, not bin-exec). **Autonomous-safe** — but note it immediately surfaces Decision B, because fixing the gap in a fresh scaffolder dir means emitting the nub-identity UA, whose leading token is the open question.

### Decision B — the nub-identity leading token (needs sign-off — brand vs. compat)

- **(a) Honest `nub/` lead** (status quo of the run/lifecycle paths; matches the prior decision). Brand-forward and truthful. create-vite prints correct `nub` commands; create-next-app + the package-manager-detector family fall back to npm (they'll tell the user to run `npm install` / `npm run dev` — functional since those hit real npm, but wrong for a nub user). Remediation is upstream: PR `nub` into `package-manager-detector`'s `AGENTS` array (the highest-leverage single target, since many tools consume it) and, tool-by-tool, into the scaffolders that roll their own. Slow to propagate; honest throughout. This is the "turbo did it" path — earn recognition upstream rather than spoof it.
- **(b) Recognized lead (masquerade)** — emit an incumbent-shaped token even in nub-identity mode, e.g. `pnpm/<parityver> nub/<v> node/v<ver> …`. Every detector recognizes `pnpm` and prints working (pnpm-compatible → nub-compatible) commands immediately; the trailing `nub/` token keeps nub present in the string. Cost: nub advertising itself as pnpm on a surface other tools read — against the honesty bar, and confusing if a tool reports "detected pnpm."
- **(c) Hybrid** — honest `nub/` on run/lifecycle (unchanged), and in compat mode keep the already-correct incumbent-lead; only the fresh/nub-identity exec case is contested. Could scope a narrow, documented recognized-lead just for the scaffolder-facing exec path while pursuing upstream inclusion, then revert to honest `nub/` once upstream lands. Most moving parts.

**Recommendation:** do **Decision A now** (parity, autonomous). For **Decision B, lean (a) honest `nub/` + drive upstream inclusion in `package-manager-detector`**, accepting the transitional "shows npm commands" cost in whitelist-detector scaffolders — it holds the honesty bar and is the durable fix. Options (b) and (c) are the faster but brand-costly alternatives. This is a brand and product call.

## Changelog

- 2026-06-30 — **Exec-surface correction + Gap 1 partially fixed (PR #260).** Empirical build showed the bin-exec surface is THREE routes, not the single `apply_exec_augmentation` the initial write-up named: `nub exec`/`nubx` split into a node-bin branch (`run_file_in_dir`) and a non-node branch (`apply_exec_augmentation`) under `launch_bin`, and `nub x`/`nub dlx`/`nub create` route through the aube engine's `exec_bin` — all three left `npm_config_user_agent` unset. PR #260 fixes the `nub exec`/`nubx` routes (both branches, reusing `run_lifecycle_ua_product` + a shared `scripts::user_agent_string`); the engine dlx path stays open as an aube-side follow-up. Gap 2 (nub/-lead misdetection) unchanged.
- 2026-06-30 — Initial write-up. Audited nub's `npm_config_user_agent` across run/lifecycle/exec vs npm 11.13.0 / pnpm 10.15.1 / yarn 1.13.0 / bun 1.3.14, and consumer behavior in package-manager-detector 1.6.0 / create-next-app / create-vite. Found (1) exec-path (`nub x`/`dlx`/`nubx`) sets nothing — parity gap; (2) nub-identity UA leads with unrecognized `nub/` → misdetected as npm by the whitelist-detector family (create-vite is the raw-passthrough exception). Cross-checked against `pm-run-compat-scope.md`'s prior honest-`nub/` decision (extended, not overturned).
