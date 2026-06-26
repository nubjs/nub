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
/// - `lockfile_basename` = `"lock.yaml"` — nub's generic, unbranded canonical
///   lockfile (pnpm-lock v9 bytes); was
///   `set_aube_lock_base_filename(NUB_LOCKFILE)`.
/// - `workspace_yaml` = `None` — nub has no branded workspace YAML of its own.
///   The shared `pnpm-workspace.yaml` compat surface is gated separately on the
///   `EngineContext` (`read_branded_pnpm_config`), per the role.
/// - `manifest_namespace` = `""` — nub reads its config from the manifest
///   ROOT (top-level `workspaces`/`overrides`/`allowBuilds`), not a branded
///   `"nub"` object.
/// - `env_prefix` = `None` — nub exposes NONE of aube's internal debug /
///   diagnostic / perf-bisect toggle family (`AUBE_DISABLE_*`, `AUBE_DIAG_*`,
///   `AUBE_CAS_*`, …). Those route through `aube_util::env::embedder_env`, which
///   reads `{env_prefix}_<SUFFIX>` — so `None` makes every such toggle simply
///   unreadable under nub, and the branded `AUBE_*` forms never leak into nub's
///   surface. (The user-facing settings-class `AUBE_*` aliases are gated
///   separately by `read_branded_settings_env = false`.)
/// - `config_env_prefix` = `Some("NUB")` — nub's three FIRST-CLASS config knobs
///   are read under nub's own brand via `aube_util::env::config_env`:
///   `NUB_CACHE_DIR` (the PM cache dir), `NUB_CONCURRENCY` (the tarball-fetch
///   fan-out, the env override of the neutral `network-concurrency` setting),
///   and `NUB_PRIMER_TTL` (the offline-primer staleness bound). These are the
///   deliberate, minimal exception to the brand boundary: a handful of knobs nub
///   legitimately owns under its own name. The corresponding `AUBE_*` forms are
///   never read under nub.
/// - `cache_namespace` = `"nub/pm"` — engine cache lands at
///   `$XDG_CACHE_HOME/nub/pm` (a `/pm` sibling of nub's own runtime caches
///   under `$XDG_CACHE_HOME/nub/`), reproducing the old
///   `set_cache_root($XDG_CACHE/nub/pm)`. Covers packument caches, the git
///   clone cache, and the node-gyp tool cache (all derive from
///   `aube_store::dirs::cache_dir()`).
/// - `data_namespace` = `"nub"` — global CAS store at
///   `$XDG_DATA_HOME/nub/store/v1`, nub's own XDG namespace (matches the
///   `storeDir` embedder default and `store path` output).
/// - `canonical_lockfile_always_wins` = `false` — `lock.yaml` never silently
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
pub(crate) const NUB: aube_util::Embedder = aube_util::Embedder {
    name: "nub",
    display_name: "nub",
    vendor: Some("by jdx.dev"),
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("nub/", env!("CARGO_PKG_VERSION")),
    self_names: &["nub"],
    compatible_names: &["pnpm"],
    lockfile_basename: super::use_align::NUB_LOCKFILE,
    workspace_yaml: None,
    manifest_namespace: "",
    env_prefix: None,
    config_env_prefix: Some("NUB"),
    cache_namespace: "nub/pm",
    data_namespace: "nub",
    canonical_lockfile_always_wins: false,
    runtime_switching: false,
    self_engines_check: false,
    self_update_enabled: false,
    warm_store_verify: false,
    no_churn_lockfile_write: true,
    read_branded_settings_env: false,
    primer_ttl: None,
    // Cap aube's CPU-bound pools (linker rayon pool, tokio worker seed) to the
    // real cgroup CFS-CPU-quota budget on a constrained box; `None` on an
    // unconstrained box leaves them at full cores. Same detector the nub-side
    // install runtime uses (`build_runtime`), so both stay consistent.
    cpu_budget: Some(super::resource_limits::cpu_budget),
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
    assert!(matches!(NUB.lockfile_basename.as_bytes(), b"lock.yaml"));
    assert!(matches!(NUB.cache_namespace.as_bytes(), b"nub/pm"));
    assert!(matches!(NUB.data_namespace.as_bytes(), b"nub"));
    assert!(matches!(NUB.manifest_namespace.as_bytes(), b""));
    assert!(NUB.workspace_yaml.is_none());
    assert!(NUB.env_prefix.is_none());
    assert!(matches!(NUB.config_env_prefix, Some(p) if matches!(p.as_bytes(), b"NUB")));
    assert!(NUB.vendor.is_some());
    assert!(!NUB.self_engines_check);
    assert!(!NUB.canonical_lockfile_always_wins);
    assert!(!NUB.runtime_switching);
    assert!(!NUB.warm_store_verify);
    assert!(!NUB.self_update_enabled);
    assert!(NUB.no_churn_lockfile_write);
    assert!(!NUB.read_branded_settings_env);
    assert!(NUB.primer_ttl.is_none());
};
