# Field-write vs. detection: does a generous version RANGE break Nub recognition?

## Question

Nub writes `"packageManager": "nub@<exact>"` on virgin install so tools detect Nub as the package manager. A self-shim that enforced the *exact* written version would delegate a user on a newer Nub back to the pinned patch, which is lock-in. The alternative is writing a **generous range** instead — either in `packageManager` (`nub@^0.2.0`, tolerated only if consumers don't strict-parse it) or in `devEngines.packageManager.version` (the spec's designated range field). **Does a range break external detection, and which field maximizes coverage?** Answered by reading the actual detection code of five representative consumers.

## TL;DR

- **A range is SAFE for the two dominant detector libraries** — `package-manager-detector` (~75M dl/mo) and `nypm` (merged Nub PR) both key on the **name before `@`** and never gate on the version. Range or exact, `packageManager` or `devEngines` — all detect Nub.
- **A range BREAKS turbo.** turbo enforces **exact 3-part semver** at the regex level on **both** `packageManager` *and* `devEngines.packageManager.version`. A `^` fails the regex → `InvalidPackageManager` → turbo falls back to lockfile detection, where Nub deliberately has no signal (Nub's PR #13187 *removed* lockfile-based Nub detection).
- **corepack hard-errors on Nub either way** — a range and an exact pin both make a corepack shim (`npm`/`pnpm`/`yarn`) exit 1 in a Nub project, because Nub is an unknown PM to corepack. This is not new and not a detection channel; it is the intended "other tooling can't run here" friction. corepack 0.35 does **not** read `devEngines`, so pinning Nub *only* in `devEngines` sidesteps the corepack hard-error.
- **The 12 create-\* scaffolder recognition PRs are UNAFFECTED.** They detect the invoking PM from `npm_config_user_agent` (a runtime signal), not a manifest field, and there is no manifest at scaffold time anyway.
- **Net:** a range costs exactly one detector — **turbo** — and nothing else.

## Verdict table

| Detector | Reads `packageManager` | Reads `devEngines.packageManager` | Version parse | Range in `packageManager`? | Range in `devEngines`? | Recognition-PR value-sensitive? |
|---|---|---|---|---|---|---|
| **corepack 0.35.0** | Yes (its own shims only) | **No** | `semver.valid()` — EXACT required | Hard-errors ("expected a semver version") | N/A (not read) | N/A — no Nub PR; Nub is intentionally unknown to corepack |
| **package-manager-detector (#72)** | Yes | Yes (only when `packageManager` absent) | name before `@`; version never gated (regex extracts digits for *display*) | **Works** | **Works** | No — #72 adds `'nub'` to the `AGENTS` set; parse is name-generic, range-agnostic |
| **nypm (#247, merged)** | Yes (primary) | Yes (fallback) | `.split("@")[0]` name; version passed through unvalidated | **Works** (cosmetic `majorVersion:"^0"`) | **Works** (regex `/\d+/` → clean `"0"`) | No — field+name based, range-agnostic |
| **turbo (#13187)** | Yes (primary) | Yes (fallback) | strict regex `\d+\.\d+\.\d+` EXACT on BOTH fields | **BREAKS** (`^` → `InvalidPackageManager` → lockfile fallback → not found) | **BREAKS** ("must be an exact semantic version") | **Yes** — needs an EXACT pin in whichever field |
| **4 scaffolders** (create-vue/qwik/t3/hono) | No (UA runtime) | No | N/A — reads `npm_config_user_agent` | N/A | N/A | No — UA-based, wholly unaffected |

## Evidence (file:line)

### corepack 0.35.0 — hard-errors on Nub, range or exact

Source: `node/deps/corepack/dist/lib/corepack.cjs` (bundled).

- Known set `{"npm","pnpm","yarn"}` (L13412-13414); `isSupportedPackageManager("nub") → false` (L13419-13421).
- `parseSpec` runs the **version-format check FIRST** (L13444, `enforceExactVersion && !semver.valid(range)`), then the **name check** (L13446, `!isSupportedPackageManager(name)`).
  - Exact `nub@0.2.9`: passes version-format → fails name → `UsageError("Unsupported package manager specification (nub@0.2.9)")`.
  - Range `nub@^0.2.0`: fails version-format first → `UsageError("Invalid package manager specification … expected a semver version")` (never reaches the name check).
- Both propagate to `runMain` (L14570-14574) → `console.error` + `process.exit(1)`.
- `COREPACK_ENABLE_STRICT=0` does **not** help (the throw precedes the `transparent` fallback, L13782/13811); only `COREPACK_ENABLE_PROJECT_SPEC=0` bypasses the field entirely.
- corepack only intercepts its own shims (`npm`/`pnpm`/`yarn`/`npx`/`pnpx`/`yarnpkg`); there is no `nub` shim, so running `nub` bypasses corepack.

### package-manager-detector — name-generic, range-safe (needs #72 merged)

Source: `package-manager-detector/src/detect.ts` (checkout at #66, pre-#72).

- `packageManager` parse (L123-127): `const [name, ver] = pkg.packageManager.replace(/^\^/,'').split('@')`; `handelVer` = `version?.match(/\d+(\.\d+){0,2}/)?.[0] ?? version` — extracts digits **for display only**, never gates. Name is everything before `@`.
- Reads `devEngines.packageManager` (L129-134) — but only when `packageManager` is absent.
- Also a UA path `getUserAgent()` (L22-30, inlined `npm_config_user_agent`, gated on `AGENTS`).
- Sole hard requirement: `'nub'` ∈ `AGENTS` (`src/constants.ts`) — **exactly what PR #72 adds** (touches the `packageManager`, `devEngines`, and UA paths at once via the one array). Verified by evaluating the real parse: `nub@^0.2.0`/`~`/`>=`/exact all → `{name:"nub"}`.

### nypm #247 — field+name based, range-safe

Source: `nypm/src/package-manager.ts`, `src/_utils.ts`.

- The Nub entry has **no `lockFile`** (deliberate; L48-54) — detected only via `packageManager` / `devEngines` / `argv` (`/nub/`), never a lockfile.
- `parsePackageManagerField` (`_utils.ts:278-301`): `.split("@")` name; version passed through, no semver validation. `majorVersion = version.split(".")[0]` (L109) → `"^0"` for a range, but the name lookup `packageManagers.find(pm => pm.name === name)` (L113) still matches → **Nub detected**. `majorVersion` is cosmetic (no per-major Nub variant to gate).
- `devEngines` fallback (`package-manager.ts:127-146`) via `parseDevEnginesPackageManager` (`_utils.ts:312-341`): major via `version?.match(/\d+/)?.[0]` → clean `"0"` for `"^0.2.0"`.

### turbo #13187 — EXACT semver required on BOTH fields (range breaks it)

Source: `turborepo/crates/turborepo-repository/src/package_manager/mod.rs` (the #13187 branch, single commit `4334f6d`).

- `PACKAGE_MANAGER_PATTERN` (L252-253): `…(?P<manager>aube|bun|npm|nub|pnpm|yarn)@(?P<version>\d+\.\d+\.\d+…|https?://\S+)…` — version is **exact 3-part semver** (or a URL). A `^`/`~` prefix fails the regex → `InvalidPackageManager`.
- `DEV_ENGINES_VERSION_PATTERN` (L256-257): `\A\d+\.\d+\.\d+…\z` — same exact requirement; error at L643 literally says *"must be an exact semantic version"*.
- Cascade: `packageManager` (L483-537) → `devEngines` fallback when absent (L540-657) → lockfiles (L910-944). Crucially, an *invalid* `packageManager` value is caught and falls to **lockfile** detection (L947-958), **not** to `devEngines` — and #13187 **removed** Nub from the lockfile chain (Nub's `lock.yaml` no longer implies Nub). So a range in `packageManager` → invalid → lockfile fallback → **Nub not found**.
- The exact-semver enforcement is turbo's own pre-existing behavior (the version alternation is identical for all PMs); #13187 only added `nub`/`aube` to the name alternation, removed lockfile detection, and fixed `link_workspace_packages` (`Nub => true`). So this constraint will not relax on Nub's account.

### 4 scaffolders — UA-runtime, field-agnostic

- create-vue `utils/packageManager.ts:8` — `process.env.npm_config_user_agent`.
- create-t3-app `cli/src/utils/getUserPkgManager.ts:5` — `process.env.npm_config_user_agent`.
- create-hono `src/hooks/dependencies.ts:136` — `process.env.npm_config_user_agent`.
- create-qwik `packages/qwik/src/cli/utils/utils.ts:120` → `which-pm-runs@1.1.0` → `index.js:4` reads `npm_config_user_agent`.

None reads `packageManager`/`devEngines`; there is no manifest at scaffold time. The manifest field-write choice is orthogonal to all 12 scaffolder recognition PRs.

## Which field maximizes coverage

| Strategy | pmd | nypm | turbo | corepack (Nub project) | Spec-correct |
|---|---|---|---|---|---|
| `packageManager: nub@<exact>` (today, #255) | detect | detect | **detect** | hard-error (intended) | field wants exact ✓ |
| `packageManager: nub@^range` | detect | detect | **broken** | hard-error (format msg) | field spec is exact-only ✗ |
| `devEngines.packageManager.version: <exact>` only | detect | detect | **detect** | no hard-error (not read) | ✓ |
| `devEngines.packageManager.version: ^range` only | detect | detect | **broken** | no hard-error | range field ✓ (but turbo stricter than spec) |

- **An exact `packageManager`** is the widest-honored, single-field win: it is the only field some tools read at all, and it satisfies turbo. Its cost is the corepack hard-error, which is by design for Nub.
- **An exact `devEngines`** matches turbo's now-preferred field and dodges the corepack hard-error, but it is a *fallback* for pmd/turbo (only read when `packageManager` is absent), and some consumers read only `packageManager`. Dropping `packageManager` to avoid corepack would lose those consumers.
- **No field lets a RANGE keep turbo.** turbo requires exact on both. A range is only free if you are willing to give up turbo recognition.

## Bottom line

1. **A generous range costs exactly one detector: turbo.** If turbo/monorepo recognition is in scope, a range is a real regression there.
2. **Decouple detection-value from self-shim-enforcement instead of loosening the written pin.** The lock-in worth avoiding comes from the *self-shim* honoring the exact string. The clean fix: keep an **exact** `packageManager: nub@<exact>` for maximum external detection (turbo included), and have Nub's own self-shim satisfy against a **range** it derives itself — Nub already writes a `^<version>` range into `devEngines.packageManager` in `nub pm use` (`use_nub.rs:561`) and its resolver already range-checks devEngines (`pm/resolve.rs`). The virgin stamp (`install_family.rs:1125`) writes only the exact `packageManager` today; adding the matching `devEngines` `^` range there (parity with `nub pm use`) gives the self-shim a range to satisfy against **without** touching the exact `packageManager` field external detectors depend on.
3. **If a range must live in a manifest field, put it ONLY in `devEngines.packageManager.version`** (spec-correct) and keep `packageManager` exact — a range in `packageManager` gains nothing over `devEngines` and risks stricter consumers.
4. **Recognition PRs affected by a field-write change:** none go out of date. Only **turbo** is *value-sensitive* — it needs an exact pin in whichever field carries the signal. Every other PR (scaffolders UA-based; pmd/nypm name-based) is invariant to exact/range/devEngines.

## Changelog

- 2026-07-30 — Migrated from the internal research corpus; the deliberation framing was rewritten to state the finding. The five-detector source read has not been re-run against newer versions of those tools.

- 2026-06-30 — Initial write-up. Five-detector differential source read (corepack 0.35.0,
  package-manager-detector #72, nypm #247, turbo #13187, 4 UA scaffolders). Finding: a range is
  safe for package-manager-detector, nypm and the scaffolders but breaks turbo (exact semver enforced on both `packageManager`
  and `devEngines`); corepack hard-errors on nub regardless. Recommend keeping an exact
  `packageManager` pin and decoupling self-shim satisfaction (range) from the written value.
