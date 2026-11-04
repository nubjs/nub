---
name: soak
description: Manages the repo's supply-chain soak window (SOAK_DAYS) — checks and fixes the derived surfaces, bumps or disables the window, adds dated per-package exclusions, and bumps pinned external tools. Use when a task touches minimumReleaseAge, min-release-age, min-publish-age, external-tools.json, renovate.json, or taze cooldowns, or when investigating why a freshly published version won't install.
---

# The soak window

One rule: a release must be at least `SOAK_DAYS` old before this repo adopts it. The delay gives the ecosystem time to catch a malicious or yanked release before we ever install it. The window is defined exactly once — read the current value from `scripts/soak/constants.mts` and never hardcode it elsewhere. Every surface derives from or is parity-checked against it:

| Surface | Key | Units |
|---|---|---|
| `.cargo/config.toml` | `global-min-publish-age` | `"N days"` |
| `tools/pnpm-workspace.yaml` | `minimumReleaseAge` | minutes |
| `.npmrc` | `min-release-age` | days |
| `tools/taze.config.mts` | `maturityPeriod` | imports `SOAK_DAYS` |
| `external-tools.json` | `soakBypass` annotations | days |
| `.github/renovate.json` | `minimumReleaseAge` (explicit — an `extends:` preset doesn't count) | `"N days"` |

## Commands (package.json scripts — the code lives in `scripts/soak/`)

- `pnpm run soak` — parity-check every surface (CI-gated in docs-links)
- `pnpm run soak:fix` — rewrite drifted windows, prune expired exclusions
- `pnpm run deps:update` — bump npm (taze) + cargo deps through the window
- `pnpm run tools:check` / `tools:fix` / `tools:install` — validate / prune-expired-bypasses / install the SRI-pinned external tools (`external-tools.json`)
- `pnpm run test:scripts` — the scripts' own unit tests

The gates fail closed on invalid states (missing, malformed, or wrong-arithmetic annotations) and WARN on expired ones — stale is not unsafe, and nobody has to watch for it: the scheduled `soak-autofix` workflow runs `soak:fix` + `tools:fix` daily and commits the pruning as a bot PR.

A soak change is done when `pnpm run soak` and `pnpm run test:scripts` both exit 0 — the same gates CI runs. Re-run them after every fix.

## Change the window (one place)

1. Edit `SOAK_DAYS` in `scripts/soak/constants.mts`.
2. `pnpm run soak:fix` (rewrites cargo/npmrc/yaml; taze follows by import).
3. `pnpm run soak` + `pnpm run test:scripts` — existing exclusion annotations encode the old window and will be flagged; re-date or remove them, then re-run until both pass.

**Opt out entirely**: set `SOAK_DAYS = 0` and run the same two steps — cargo, pnpm/nub (`minimumReleaseAge: 0`), npm, and taze all treat zero as disabled. There is deliberately no env-var bypass: opting out is a committed, reviewable change, never a silent one.

## Skip the soak for ONE package (dated, temporary)

Add to `minimumReleaseAgeExclude` in `tools/pnpm-workspace.yaml` with the annotation on the line above (block list only — flow `[..]` is rejected because a comment line can't attach to an inline entry):

```yaml
# published: YYYY-MM-DD | removable: YYYY-MM-DD
- 'name@1.2.3'
```

`removable` = `published + SOAK_DAYS`; `published` must be the real registry publish date (the placeholders above are schematic — copying them verbatim is rejected). Once `removable` passes, `pnpm run soak` warns until the pin is pruned (`soak:fix` or the soak-autofix workflow does it). Bare names / `@scope/*` globs are standing trust and need no annotation. External tools use the same shape via a `soakBypass` object in `external-tools.json`.

## The cargo soak needs nightly — the repo still must not pin one

`min-publish-age` is an `[unstable]` cargo feature: a stable cargo ignores it silently. The repo deliberately ships **no** `rust-toolchain.toml`, because a repo-root toolchain file outranks `rustup default` and would silently redirect the version-pinned CI jobs (the MSRV `Check` legs) and build released binaries on nightly.

The nightly is instead requested per-invocation, at the only step that picks versions: `scripts/soak/update-deps.mts` runs `cargo +nightly update`. Everything else — every CI job, every shipped binary — builds on stable. If you need the cargo soak somewhere new, call `cargo +nightly` there; do not add a toolchain file.

**Keep the nightly current — a merely-old one silently disables the window.** Cargo treats an `[unstable]` key it does not implement as a warning and exits 0, so an old nightly resolves with NO window while looking successful. Measured both sides: nightly 2026-03-21 (cargo 1.96.0-nightly) has no such `-Z` and skips the window silently; nightly 2026-07-27 (cargo 1.99.0-nightly) supports `-Z min-publish-age` and visibly holds a too-fresh release back (`available: v0.2.189, published 7 days ago`). `deps:update` detects the warning and fails with the fix (`rustup update nightly`) — if you see it, the lockfile changes it just made are unsoaked.

## Maintaining this skill

`scripts/soak/` is the law; this file only documents it — when they disagree, fix this file. Keep it concise (goal + constraints, not step enumeration), and keep the window value in `constants.mts` rather than restating it here.
