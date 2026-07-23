//! Nub's compile-time embedder profile for the vendored aube engine.
//!
//! The engine (vendor/aube) selects its branding and embedder-fixed behavior
//! from a single `&'static aube_util::Embedder` registered once per process via
//! [`aube_util::set_embedder`]. Standalone aube ships `aube_util::AUBE`; nub
//! ships [`NUB`] here (aube stays nub-free — the profile is nub's). The runtime,
//! per-project counterpart — the config-surface posture, the scoped override
//! source, the lifecycle PATH/env overlay — lives on `aube_util::EngineContext`
//! and is populated across a run's phases (see `engine_brand_preflight` /
//! `apply_config_scope` / `apply_lifecycle_augmentation`).
//!
//! This const carries only the values that are *fixed for the life of the
//! nub binary*: branding plus the five embedder-fixed behavior toggles. It
//! replaces the old scatter of `aube::set_user_agent_product` /
//! `aube_lockfile::set_aube_lock_base_filename` /
//! `set_detection_self_names` / `set_canonical_lockfile_always_wins` /
//! `set_aube_engine_check` / `set_runtime_switching_enabled` /
//! `set_warm_store_verify` / `set_package_manager_names` seam calls — those
//! removed setters are now this one compile-time profile.

/// Nub's embedder profile. Registered once via [`register`].
///
/// Field choices, with the seam call each replaces:
///
/// - `name` / `display_name` = `"nub"` — the running tool.
/// - `vendor` = `Some("by jdx.dev")` — nub credits jdx for the vendored
///   engine ("powered by aube" ethos); the attribution is deliberately kept,
///   not stripped to `None`.
/// - `version` / `user_agent` — `nub/<CARGO_PKG_VERSION>` (was
///   `set_user_agent_product`). The *lifecycle* UA is genuinely runtime (it
///   embeds the project's resolved Node version) and is set per-invocation on
///   the `EngineContext` instead; this const is the registry/telemetry UA.
/// - `self_names` = `["nub"]`, `compatible_names` = `["pnpm"]` — nub is the
///   tool, pnpm the compatible drop-in (was `set_detection_self_names` +
///   `set_package_manager_names`).
/// - `lockfile_basename` = `"nub.lock"` — nub's generic, unbranded
///   canonical lockfile (pnpm-lock v9 bytes), pairing with `package.json`; was
///   `set_aube_lock_base_filename(NUB_LOCKFILE)`.
/// - `lockfile_legacy_basenames` = `["lock.yaml"]` — nub's pre-rename name,
///   still recognized on read during the transition and migrated to
///   `nub.lock` on the next mutating PM op.
/// - `workspace_yaml` = `None` — nub has no branded workspace YAML of its own.
///   The shared `pnpm-workspace.yaml` compat surface is gated separately on the
///   `EngineContext` (`read_branded_pnpm_config`), per the role.
/// - `manifest_namespace` = `""` — nub reads its config from the manifest
///   ROOT (top-level `workspaces`/`overrides`/`allowBuilds`), not a branded
///   `"nub"` object.
/// - `env_prefix` = `None` — nub exposes NONE of aube's internal debug /
///   perf-bisect toggle family (`AUBE_DISABLE_*`, `AUBE_CAS_*`, `AUBE_INTERNAL_*`,
///   …). Those route through `aube_util::env::embedder_env`, which reads
///   `{env_prefix}_<SUFFIX>` — so `None` makes every such toggle simply
///   unreadable under nub, and the branded `AUBE_*` forms never leak into nub's
///   surface. (The user-facing settings-class `AUBE_*` aliases are gated
///   separately by `read_branded_settings_env = false`. The diagnostics `DIAG_*`
///   knobs are split out onto `diag_env_prefix` so nub can expose THEM under its
///   own brand without re-admitting this whole family — see below.)
/// - `config_env_prefix` = `Some("NUB")` — nub's three FIRST-CLASS config knobs
///   are read under nub's own brand via `aube_util::env::config_env`:
///   `NUB_CACHE_DIR` (the PM cache dir), `NUB_CONCURRENCY` (the tarball-fetch
///   fan-out, the env override of the neutral `network-concurrency` setting),
///   and `NUB_PRIMER_TTL` (the offline-primer staleness bound). These are the
///   deliberate, minimal exception to the brand boundary: a handful of knobs nub
///   legitimately owns under its own name. The corresponding `AUBE_*` forms are
///   never read under nub.
/// - `diag_env_prefix` = `Some("NUB")` — exposes aube's rich diagnostics layer
///   (per-phase/per-op spans, JSONL output, end-of-run summary table,
///   critical-path analyzer, `getrusage` kernel deltas) under nub's OWN brand
///   via `aube_util::env::diag_env`: `NUB_DIAG_FILE` (JSONL events to a file),
///   `NUB_DIAG_PRINT`, `NUB_DIAG_SUMMARY`, `NUB_DIAG_CRITPATH`,
///   `NUB_DIAG_THRESHOLD_MS`, `NUB_DIAG_KERNEL`, plus `NUB_BENCH_PHASES_FILE`.
///   These are dev/perf-tracing knobs (a sanctioned internal `NUB_*` surface),
///   off by default — unset, the diag layer's `ENABLED` atomic stays false and
///   the instrumentation no-ops exactly as today, so there is zero hot-path
///   cost. Carved out from `env_prefix` deliberately: routing JUST the `DIAG_*`
///   knobs here lets nub reach the diagnostics layer without also exposing
///   aube's ~30 unrelated internal `AUBE_*` toggles under `NUB_*`. The `AUBE_*`
///   forms are never read under nub.
/// - `cache_namespace` = `"nub/pm"` — engine cache lands at
///   `$XDG_CACHE_HOME/nub/pm` (a `/pm` sibling of nub's own runtime caches
///   under `$XDG_CACHE_HOME/nub/`), reproducing the old
///   `set_cache_root($XDG_CACHE/nub/pm)`. Covers packument caches, the git
///   clone cache, and the node-gyp tool cache (all derive from
///   `aube_store::dirs::cache_dir()`).
/// - `data_namespace` = `"nub"` — global CAS store at
///   `$XDG_DATA_HOME/nub/store/v1`, nub's own XDG namespace (matches the
///   `storeDir` embedder default and `store path` output).
/// - `managed_config_system_dir` = `Some("nub")` — the admin-managed config
///   file is read from nub's OWN system path (`/etc/nub/managed.toml`), never
///   aube's `/etc/aube/managed.toml`. A machine with a co-installed standalone
///   aube cannot make nub silently inherit its `/etc/aube` policy: the brand
///   boundary holds on the system path the same way it already held on the env
///   override (`NUB_MANAGED_CONFIG_PATH`, via `config_env_prefix`).
/// - `config_namespace` = `None` — nub has NO branded user/project config file.
///   The engine never reads `~/.config/aube/config.toml` or
///   `<cwd>/.config/aube/config.toml` (the leak this profile closes), and nub
///   does NOT substitute a `~/.config/nub/` home of its own: a nub project's
///   config surface is the neutral `.npmrc` + the sanctioned `NUB_*` env knobs,
///   so there is no bespoke nub config file to author or read. Mirrors how
///   `managed_config_system_dir = None` would skip the system read — here the
///   user/project branded-file read/write is skipped entirely. Standalone aube
///   keeps `Some("aube")`, so its `~/.config/aube/config.toml` path is unchanged.
/// - `canonical_lockfile_always_wins` = `false` — `nub.lock` never silently
///   outranks a foreign lockfile beside it; that state is the loud
///   ambiguity/contradiction error (was
///   `set_canonical_lockfile_always_wins(false)`).
/// - `runtime_switching` = `false` — Node provisioning is nub's job; aube's
///   runtime resolver stays inert (was `set_runtime_switching_enabled(false)`).
/// - `self_engines_check` = `false` — an `engines.nub` pin is NEVER validated
///   (the decided default; `engines.node` is unaffected). Was
///   `set_aube_engine_check(false)`.
/// - `self_update_enabled` = `false` — nub owns its own upgrade path; the
///   engine's `aube.jdx.dev` update notifier never runs. (nub bypasses
///   `cli_main`, so this path is already unreachable through nub's dispatch;
///   `false` keeps it inert for any future engine path nub might touch.)
/// - `warm_store_verify` = `false` — nub trusts the atomically-published CAS
///   and skips the per-file warm-relink stat sweep (was
///   `set_warm_store_verify(false)`). Import-time SHA-512 / SRI is untouched.
/// - `no_churn_lockfile_write` = `true` — nub opts INTO the no-churn write
///   guard: when an install doesn't change the resolved graph, the lockfile's
///   bytes/mtime are left untouched. This breaks the rewrite flip-flop where
///   nub and the project's other PM keep rewriting a graph-equal lockfile into
///   their own serialization, since nub round-trips a foreign lockfile rather
///   than imposing its own.
/// - `read_branded_settings_env` = `false` — nub does NOT read aube's branded
///   `AUBE_*` settings env-var family; the neutral `npm_config_*` /
///   `NPM_CONFIG_*` aliases and bare external vars are unaffected. (Mirrors the
///   brand boundary on the settings-env surface — symmetric with nub's
///   `read_branded_pnpm_config` posture.)
/// - `primer_ttl` = `None` (unlimited) — nub ships an evergreen offline
///   metadata primer. The pick-site regime gate (a FROZEN pick is served from
///   the primer, a live-frontier pick refetches when stale) is the always-on
///   correctness layer; `primer_ttl` only governs whether the primer is
///   consulted at all, keyed on the binary's age relative to the primer build
///   date. `None` = never expire — frozen resolution data is immutable, so an
///   aged binary's frozen picks are still correct ("evergreen" is just an ∞
///   TTL). This replaces the old `primer_evergreen` boolean and its
///   `AUBE_PRIMER_PICK_GATE` override. Cooling (`minimumReleaseAge`) is still
///   enforced inside the pick against the primer's own `time` map regardless of
///   TTL, so this is a cold-install correctness fix, not a security weakening. A
///   user can set a finite `NUB_PRIMER_TTL` (e.g. `30d`) to make the primer
///   expire after that window.
/// - `tty_progress` = `true` — nub makes the in-place single-line animated
///   install bar its first-class UX on an interactive, non-CI terminal (the
///   `uv`-for-Node feel), instead of standalone aube's append-only default.
///   CI / piped / non-TTY output stays append-only regardless, so logs never
///   carry cursor-control escapes. The animated renderer is also brand-safe
///   under nub: the `AUBE_TTY_PROGRESS` opt-in is no longer required to reach
///   it (and nub does not read that `AUBE_*` var), since the profile enables it.
pub(crate) const NUB: aube_util::Embedder = aube_util::Embedder {
    name: "nub",
    display_name: "nub",
    vendor: Some("by jdx.dev"),
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("nub/", env!("CARGO_PKG_VERSION")),
    self_names: &["nub"],
    compatible_names: &["pnpm"],
    lockfile_basename: super::use_align::NUB_LOCKFILE,
    // `lock.yaml` (nub's pre-rename name) stays readable through the transition
    // and is migrated to `nub.lock` on the next mutating PM op; sunset at
    // the next major.
    lockfile_legacy_basenames: &[super::use_align::NUB_LEGACY_LOCKFILE],
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: Some("NUB"),
    // Exposes the diagnostics layer under nub's own brand (`NUB_DIAG_*` +
    // `NUB_BENCH_PHASES_FILE`) without re-admitting the broad `AUBE_*` toggle
    // family that `env_prefix: None` shuts off. Off by default — zero hot-path
    // cost when unset.
    diag_env_prefix: Some("NUB"),
    cache_namespace: "nub/pm",
    data_namespace: "nub",
    // Machine-global store named `store` (→ `~/.cache/nub/pm/store/`). It is the
    // real, shared store packages materialize into — only "virtual" in that
    // project `node_modules` symlink OUT of it — so `virtual-store` (pnpm's term
    // for the flattened per-project tree) never fit. `store` reads clean and,
    // unlike `.store`, is not the path segment simple-git-hooks slices on.
    // Cosmetic: the shared-store behavior is identical.
    virtual_store_subdir: "store",
    managed_config_system_dir: Some("nub"),
    // No branded user/project config file: nub never reads `~/.config/aube/`
    // (or `<cwd>/.config/aube/`) and authors no `~/.config/nub/` of its own —
    // a nub project's config surface is `.npmrc` + the `NUB_*` env knobs.
    config_namespace: None,
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    warm_store_verify: false,
    no_churn_lockfile_write: true,
    read_branded_settings_env: false,
    // The GVS-incompatible auto-fallback notice (e.g. Next.js drops the install
    // to per-project) is unactionable by the user — the only fix is upstream in
    // the package — so nub demotes it from a default-shown warning to a
    // `debug`-level detail: silent under `nub i`, reachable with
    // `--loglevel debug` / `RUST_LOG`. The per-project fallback behavior itself
    // is unchanged; only the notice is silenced.
    gvs_incompatible_warning: false,
    // GVS-precedence hoisting (#286): a DEFAULT hoist no longer vetoes the
    // shared virtual store, so GVS engages wherever it's active (off-CI, no
    // trigger, no explicit opt-out) with no hidden tree, and the pnpm-parity
    // hidden hoist tree is built wherever GVS is OFF (CI, `nub ci`, a
    // next/nuxt/parcel trigger, an explicit `enableGlobalVirtualStore=false`,
    // dlx) — restoring ambient `@types/*` resolution for store-resident
    // packages. Only an EXPLICIT `hoist=true` (nub's injected-deps push, or a
    // user setting) vetoes GVS. Nub therefore no longer pushes `hoist=false`;
    // see `nub_setting_defaults`.
    gvs_over_default_hoist: true,
    primer_ttl: None,
    // Cap aube's CPU-bound pools (linker rayon pool, tokio worker seed) to the
    // real cgroup CFS-CPU-quota budget on a constrained box; `None` on an
    // unconstrained box leaves them at full cores. Same detector the nub-side
    // install runtime uses (`build_runtime`), so both stay consistent.
    cpu_budget: Some(super::resource_limits::cpu_budget),
    // The animated single-line install bar is nub's first-class TTY UX; the
    // engine still falls back to append-only for CI / piped / non-TTY output.
    tty_progress: true,
    // `nub update -i` uses the tri-state table picker (keep → range → latest
    // radio cells per row, defaulting to "keep" so updates are opt-in),
    // instead of standalone aube's flat everything-pre-selected multiselect.
    rich_update_picker: true,
    // nub's guarantee is "the installed tree equals the incumbent PM's, or an
    // eager precise refusal" — so a lockfile source nub can't resolve (a
    // git/jsr/unknown protocol in a yarn.lock or bun.lock) aborts at plan time
    // for a non-optional dep, instead of reclassify→404 / silent drop.
    strict_unsupported_source: true,
    // A fully-satisfied warm install (node_modules present, lockfile/manifest/
    // settings/layout all match) short-circuits to an instant "Already up to
    // date" regardless of `trustPolicy`, matching npm/pnpm/bun and aube's own
    // offline + `aube run` auto-install paths. The trust gate is a resolve-time
    // downgrade defense with nothing to validate on a no-op (zero resolve/fetch/
    // link); any install that does REAL work misses the short-circuit and still
    // trips the gate during resolution. Standalone aube keeps the re-validation.
    warm_trust_revalidate: false,
    // Default `trustPolicyIgnoreAfter` to a 14-day window (in minutes) when the
    // user hasn't set it. A legitimate maintenance backport on an old major —
    // published later in wall-clock than a newer major that adopted OIDC
    // provenance — trips the date-ordered downgrade scan on first resolve (the
    // #270 false positive: `@modelcontextprotocol/inspector` → `tailwind-merge`).
    // Once such a version has aged past the window un-yanked it is
    // overwhelmingly a real backport and is exempted; a freshly published
    // weak-evidence version is still scanned against the full history, so the
    // downgrade attack (a stolen-token publish into an old line) is still caught
    // in the window that matters. An explicit user `trustPolicyIgnoreAfter=0`
    // opts back into the full strict check. Standalone aube keeps `None`.
    trust_policy_ignore_after_default: Some(14 * 24 * 60),
    // Fold nub's phantom-eject state into aube's install-state `settings_hash`: it
    // shapes which packages materialize but rides no aube setting. For users the
    // token is constant-on and folds the scanner version, so a scanner-logic bump
    // invalidates a warm tree and re-links; standalone aube's `None` skips the fold.
    extra_settings_fingerprint: Some(crate::dynamic_phantom::settings_fingerprint),
};

/// Register [`NUB`] as the active embedder profile. Idempotent (the engine's
/// `set_embedder` is a set-once `OnceLock`), so calling it once per command
/// from the brand preflight is correct and cheap. Must run before any engine
/// code reads branding — i.e. at the very start of `engine_brand_preflight`,
/// before the project-state walk.
pub(crate) fn register() {
    aube_util::set_embedder(&NUB);
}

// The profile reproduces nub's identity: generic unbranded lockfile,
// `nub/<v>` UA, jdx credit kept, the engines-self check OFF (an `engines.nub`
// pin is never validated — the decided default), and every other
// embedder-fixed toggle OFF. Compile-time assertions: the const is fixed, so a
// drift is a build break, not a test-run failure (and runtime `assert!` on a
// const trips clippy's `assertions_on_constants`).
const _: () = {
    assert!(matches!(NUB.lockfile_basename.as_bytes(), b"nub.lock"));
    assert!(matches!(NUB.lockfile_legacy_basenames, [b] if matches!(b.as_bytes(), b"lock.yaml")));
    assert!(matches!(NUB.cache_namespace.as_bytes(), b"nub/pm"));
    assert!(matches!(NUB.data_namespace.as_bytes(), b"nub"));
    assert!(matches!(NUB.managed_config_system_dir, Some(d) if matches!(d.as_bytes(), b"nub")));
    assert!(NUB.config_namespace.is_none());
    assert!(matches!(NUB.manifest_namespace.as_bytes(), b""));
    assert!(NUB.workspace_yaml.is_none());
    assert!(NUB.env_prefix.is_none());
    assert!(matches!(NUB.config_env_prefix, Some(p) if matches!(p.as_bytes(), b"NUB")));
    assert!(matches!(NUB.diag_env_prefix, Some(p) if matches!(p.as_bytes(), b"NUB")));
    assert!(NUB.vendor.is_some());
    assert!(!NUB.self_engines_check);
    assert!(!NUB.canonical_lockfile_always_wins);
    assert!(!NUB.runtime_switching);
    assert!(!NUB.warm_store_verify);
    assert!(!NUB.self_update_enabled);
    assert!(NUB.no_churn_lockfile_write);
    assert!(!NUB.read_branded_settings_env);
    assert!(!NUB.gvs_incompatible_warning);
    assert!(NUB.gvs_over_default_hoist);
    assert!(NUB.primer_ttl.is_none());
    assert!(NUB.tty_progress);
    assert!(!NUB.warm_trust_revalidate);
    assert!(matches!(NUB.trust_policy_ignore_after_default, Some(20160)));
    assert!(NUB.extra_settings_fingerprint.is_some());
};
