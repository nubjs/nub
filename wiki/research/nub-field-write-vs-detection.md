# Field-write vs. detection: does a generous version RANGE break Nub recognition?

Writing a version range rather than an exact pin into the manifest costs exactly one detector — turbo — and leaves every other consumer detecting Nub. Established by reading the detection code of five representative consumers.

## Question

Does a version range in the manifest break Nub recognition, and which field should carry it?

Nub's canonical lockfile is deliberately neutral (`nub.lock`), so — unlike a PM whose branded lockfile is itself the repo's signal — Nub leaves nothing downstream tools can read unless it declares itself in the manifest. An *exact* pin is the obvious declaration, but a self-shim enforcing that exact version would delegate a user on a newer Nub back to the pinned patch, which is lock-in.

The alternative is writing a **generous range** instead — either in `packageManager` (`nub@^0.2.0`, tolerated only if consumers don't strict-parse it) or in `devEngines.packageManager.version` (the spec's designated range field). **Does a range break external detection, and which field maximizes coverage?** Answered by reading the actual detection code of five representative consumers.

## TL;DR

The consumers split three ways: name-keyed detectors that ignore the version entirely, turbo which enforces exact semver, and corepack which rejects Nub on any value.

- **A range is SAFE for the two dominant detector libraries** — `package-manager-detector` (~75M dl/mo) and `nypm` (merged Nub PR) both key on the **name before `@`** and never gate on the version. Range or exact, `packageManager` or `devEngines` — all detect Nub.
- **A range BREAKS turbo.** turbo enforces **exact 3-part semver** at the regex level on **both** `packageManager` *and* `devEngines.packageManager.version`. A `^` fails the regex → `InvalidPackageManager` → turbo falls back to lockfile detection, where Nub deliberately has no signal (Nub's PR #13187 *removed* lockfile-based Nub detection).
- **corepack hard-errors on Nub either way** — a range and an exact pin both make a corepack shim (`npm`/`pnpm`/`yarn`) exit 1 in a Nub project, because Nub is an unknown PM to corepack: corepack has no `nub` shim, so it never intercepts `nub` itself, but a corepack-shimmed `npm`/`pnpm`/`yarn` reads the field and exits 1. This is not a detection channel either way. corepack 0.35 does **not** read `devEngines`, so pinning Nub *only* in `devEngines` sidesteps the corepack hard-error.
- **Scaffolder detection is UNAFFECTED either way.** The `create-*` scaffolders detect the invoking PM from `npm_config_user_agent` — a runtime signal, not a manifest field — and there is no manifest at scaffold time anyway.
- **Net:** a range costs exactly one detector — **turbo** — and nothing else.

## Verdict table

One row per detector: which of the two manifest fields it reads, how strictly it parses the version, and whether a range survives in each field.

| Detector | Reads `packageManager` | Reads `devEngines.packageManager` | Version parse | Range in `packageManager`? | Range in `devEngines`? | Value-sensitive? |
|---|---|---|---|---|---|---|
| **corepack 0.35.0** | Yes (its own shims only) | **No** | `semver.valid()` — EXACT required | Hard-errors ("expected a semver version") | N/A (not read) | N/A — Nub is an unknown PM to corepack |
| **package-manager-detector** | Yes | Yes (only when `packageManager` absent) | name before `@`; version never gated (regex extracts digits for *display*) | **Works** | **Works** | No — parse is name-generic and range-agnostic |
| **nypm** | Yes (primary) | Yes (fallback) | `.split("@")[0]` name; version passed through unvalidated | **Works** (cosmetic `majorVersion:"^0"`) | **Works** (regex `/\d+/` → clean `"0"`) | No — field+name based, range-agnostic |
| **turbo** | Yes (primary) | Yes (fallback) | strict regex `\d+\.\d+\.\d+` EXACT on BOTH fields | **BREAKS** (`^` → `InvalidPackageManager` → lockfile fallback → not found) | **BREAKS** ("must be an exact semantic version") | **Yes** — needs an EXACT pin in whichever field |
| **4 scaffolders** (create-vue/qwik/t3/hono) | No (UA runtime) | No | N/A — reads `npm_config_user_agent` | N/A | N/A | No — UA-based, wholly unaffected |

## Evidence (file:line)

Each subsection cites the lines in the consumer's own source that decide its verdict above.

### corepack 0.35.0 — hard-errors on Nub, range or exact

Source: `node/deps/corepack/dist/lib/corepack.cjs` (bundled).

- Known set `{"npm","pnpm","yarn"}` (L13412-13414); `isSupportedPackageManager("nub") → false` (L13419-13421).
- `parseSpec` runs the **version-format check FIRST** (L13444, `enforceExactVersion && !semver.valid(range)`), then the **name check** (L13446, `!isSupportedPackageManager(name)`).
  - Exact `nub@0.2.9`: passes version-format → fails name → `UsageError("Unsupported package manager specification (nub@0.2.9)")`.
  - Range `nub@^0.2.0`: fails version-format first → `UsageError("Invalid package manager specification … expected a semver version")` (never reaches the name check).
- Both propagate to `runMain` (L14570-14574) → `console.error` + `process.exit(1)`.
- `COREPACK_ENABLE_STRICT=0` does **not** help (the throw precedes the `transparent` fallback, L13782/13811); only `COREPACK_ENABLE_PROJECT_SPEC=0` bypasses the field entirely.
- corepack only intercepts its own shims (`npm`/`pnpm`/`yarn`/`npx`/`pnpx`/`yarnpkg`); there is no `nub` shim, so running `nub` bypasses corepack.

### package-manager-detector — name-generic, range-safe

Source: `package-manager-detector/src/detect.ts`.

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

Four write strategies, scored against the same consumers and against what each field's own spec asks for.

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

What Nub writes follows from the detector table, and it is the inverse of the doc's first recommendation: the value-sensitive detector is turbo, and turbo also treats an exact pin as a hard lock, so the exact pin is reserved for an explicit opt-in.

1. **A virgin install stamps a caret range into `devEngines.packageManager`** — `{name:"nub", version:"^<x.y.z>", onFail:"ignore"}` (`install_family.rs`, `stamp_virgin_dev_engines`). Name-keyed detectors (`package-manager-detector`, `nypm`) read it; the exact, corepack-visible `packageManager` field is never written implicitly.
2. **`nub pm use nub@<exact>` writes the hard pin.** That explicit opt-in sets `packageManager: "nub@<exact>"` and keeps the `devEngines` range beside it (`use_nub.rs`), so turbo's exact-semver regex is satisfied only where the user asked for a lock.
3. **A range never goes in `packageManager`.** It gains nothing over `devEngines` and risks stricter consumers.
4. **Recognition is invariant to the field-write choice** for every detector except turbo, which needs the exact pin in whichever field carries the signal.

## Changelog

Dated revisions, newest first. The 2026-07 entry flags that the five-detector source read has not been repeated against newer releases of those tools.

- 2026-07-30 — Initial publication.

- 2026-06-30 — Initial write-up. Five-detector differential source read (corepack 0.35.0,
  package-manager-detector #72, nypm #247, turbo #13187, 4 UA scaffolders). Finding: a range is
  safe for package-manager-detector, nypm and the scaffolders but breaks turbo (exact semver enforced on both `packageManager`
  and `devEngines`); corepack hard-errors on nub regardless. Recommend keeping an exact
  `packageManager` pin and decoupling self-shim satisfaction (range) from the written value.
- 2026-08-28 — **REVERSAL:** the virgin stamp writes a `^` range into `devEngines.packageManager`, not an exact `packageManager` pin; the exact pin is reserved for `nub pm use nub@<exact>`. Restated the section as current behavior.
