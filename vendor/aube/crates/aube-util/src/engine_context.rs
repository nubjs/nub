//! Runtime embedder seam — the per-invocation counterpart to [`Embedder`].
//!
//! [`Embedder`](crate::identity::Embedder) is the *compile-time* embedder
//! profile: branding plus the behavior toggles fixed for the life of the
//! binary. But an embedder (e.g. nub) also computes a handful of values
//! *per project / per invocation* — which override sources apply, whether a
//! pnpm-named file is the active PM's and may be read, the PATH/env overlay
//! that routes lifecycle scripts through a provisioned runtime. A compile-time
//! const cannot carry those. [`EngineContext`] is their home: a process-global
//! struct the embedder populates as a run progresses, which aube's seam
//! read-sites consult.
//!
//! Unlike [`Embedder`] (selected once, at the entry point, before any command
//! runs), the context's fields are computed at *different phases* of a run —
//! some at startup, some after the manifest is parsed, some after settings
//! resolve. So it is backed by a `RwLock` and populated incrementally:
//! [`update_engine_context`] mutates individual fields in place, while
//! [`set_engine_context`] replaces the whole struct. [`engine_context`] returns
//! a snapshot clone.
//!
//! **Default = upstream-neutral for every field.** Standalone aube (and any
//! test) that never touches the context gets exactly upstream behavior:
//! [`EngineContext::default`] reproduces it field-for-field. This is the
//! runtime analogue of [`AUBE`](crate::identity::AUBE) being the unset
//! [`Embedder`] fallback.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// Per-invocation values an embedder computes and hands to aube.
///
/// Every field defaults to aube's upstream behavior, so an unset context is
/// behavior-neutral. Populated incrementally across a run's phases via
/// [`update_engine_context`] (field-level) or [`set_engine_context`]
/// (whole-struct replace).
#[derive(Clone, Debug)]
pub struct EngineContext {
    /// Replacement dependency-override source. `Some(map)` makes the supplied
    /// map the *sole* override source — `PackageJson::overrides_map` returns it
    /// verbatim instead of folding the manifest's `resolutions` /
    /// `pnpm.overrides` / top-level `overrides` with the built-in precedence.
    /// `None` (default) leaves upstream behavior untouched (fold every source).
    ///
    /// The embedder seam for tools that scope which override dialects apply per
    /// project (e.g. honoring only the active package manager's native field).
    /// aube assigns no policy — it consumes whatever map the embedder computed,
    /// typically from `PackageJson::tagged_overrides`.
    pub embedder_overrides: Option<BTreeMap<String, String>>,

    /// Whether Bun's top-level `trustedDependencies` array contributes to the
    /// lifecycle build allowlist. `true` (default) preserves upstream behavior
    /// — `trustedDependencies` unions into the allowlist. `false` makes
    /// `PackageJson::trusted_dependencies` return an empty list, for embedders
    /// whose active package manager ignores the field (every PM except Bun, and
    /// Bun itself from the version that dropped it).
    pub trusted_dependencies_honored: bool,

    /// Whether aube reads the *branded pnpm* config-compat surface. `true`
    /// (default) is upstream behavior: aube consults pnpm's surfaces alongside
    /// its own. This single posture drives all three pnpm-branded read-sites
    /// together —
    ///
    /// 1. `pnpm-workspace.yaml` is included in the workspace-yaml candidate
    ///    list (probed/read for workspace settings);
    /// 2. the `pnpm` `package.json` config namespace is consulted (folded with
    ///    the tool's own `aube.*` namespace);
    /// 3. pnpm's global `~/.config/pnpm/auth.ini` is read and its tokens
    ///    merged.
    ///
    /// The actual branded values (`"pnpm-workspace.yaml"`, the `"pnpm"`
    /// namespace) are aube's own compiled-in pnpm-compat knowledge; this bool
    /// only *gates* whether they apply. An embedder whose active PM isn't pnpm
    /// sets `false`: under a non-pnpm incumbent those pnpm-named surfaces are
    /// another tool's state and must not be read (a name-based policy). The
    /// tool's own branded YAML/namespace, `.npmrc`, and `npmrcAuthFile`
    /// sources are unaffected.
    ///
    /// NOTE: this posture gates only the PROJECT-scoped pnpm surfaces (the
    /// workspace yaml + the `pnpm` package.json namespace). The GLOBAL /
    /// user-scope pnpm-named files (`<configDir>/config.yaml` and
    /// `<configDir>/auth.ini`) are gated by the separate
    /// [`read_pnpm_global_config`](Self::read_pnpm_global_config) posture —
    /// they must NOT ride a project-derived (cwd-dependent) gate, since global
    /// config has no project incumbent.
    pub read_branded_pnpm_config: bool,

    /// Whether aube reads pnpm's GLOBAL / user-scope pnpm-named config files:
    /// `<configDir>/config.yaml` (pnpm v11's global settings file) and
    /// `<configDir>/auth.ini` (pnpm's global auth file). `true` (default)
    /// preserves upstream behavior — standalone aube IS a pnpm-compatible PM
    /// and reads pnpm's global config unconditionally.
    ///
    /// This is DELIBERATELY SEPARATE from
    /// [`read_branded_pnpm_config`](Self::read_branded_pnpm_config). That
    /// posture is derived from the project's incumbent PM (a cwd-scoped
    /// concept) and correctly gates the PROJECT-scoped pnpm surfaces. Global
    /// config, by contrast, has no project and no incumbent — gating it on the
    /// cwd's incumbent would mean "read pnpm's global config only when you
    /// happen to be standing in a pnpm project", which is incoherent. This
    /// separate posture lets an embedder read the global pnpm-named files
    /// UNGATED by the cwd. nub sets this `true` unconditionally: it honors
    /// whatever global config the user already has from any tool (npm's
    /// `~/.npmrc`, pnpm's global `config.yaml` / `auth.ini`), independent of
    /// the cwd. (nub keeps global WRITES neutral — never writing back a
    /// pnpm-branded global file — but that is the embedder's write-path
    /// concern, not this read gate.)
    pub read_pnpm_global_config: bool,

    /// Whether aube reads Yarn Berry's `.yarnrc.yml` config surface and
    /// translates the subset that maps cleanly onto the existing npmrc-shaped
    /// registry/settings model. `false` (default) preserves upstream aube
    /// behavior. Embedders such as nub set this only when Yarn is the active
    /// incumbent; under nub identity or another PM, Yarn-named config is
    /// another tool's state and must not be read.
    pub read_yarn_config: bool,

    /// Whether the incumbent Yarn is *classic* (v1), gating the classic
    /// `.yarnrc` reader. `false` (default) preserves upstream behavior. Only
    /// meaningful when [`read_yarn_config`](Self::read_yarn_config) is `true`:
    ///
    /// - Classic Yarn (v1) reads `.yarnrc`; Yarn Berry (v2+) abandoned it and
    ///   reads only `.yarnrc.yml`. A Berry project can carry a stray legacy
    ///   `.yarnrc` that Berry itself ignores, so reading it under a Berry
    ///   incumbent silently diverges from Yarn (wrong registry/auth).
    /// - The Berry `.yarnrc.yml` surface is gated by `read_yarn_config` alone
    ///   and is unaffected by this flag; only the classic `.yarnrc` path
    ///   additionally requires `yarn_is_classic`.
    ///
    /// Embedders set this `true` only when the active Yarn is provably classic
    /// (v1). The embedder owns the classic-vs-Berry classification — aube
    /// assigns no policy.
    pub yarn_is_classic: bool,

    /// Whether aube honors Bun's `BUN_CONFIG_REGISTRY` / `BUN_CONFIG_TOKEN`
    /// install/registry environment variables, translating them onto the
    /// existing npmrc-shaped registry/token settings. `false` (default)
    /// preserves upstream aube behavior. Embedders such as nub set this only
    /// when Bun is the active incumbent; under nub identity or another PM,
    /// Bun-named config is another tool's state and must not be read.
    ///
    /// Bun's semantics (mirrored here): `BUN_CONFIG_REGISTRY` sets the default
    /// registry and is checked *before* `NPM_CONFIG_REGISTRY` /
    /// `npm_config_registry` (so it outranks them); `BUN_CONFIG_TOKEN` sets the
    /// default registry's auth token, checked before `NPM_CONFIG_TOKEN` /
    /// `npm_config_token`. Only these two — the high-impact CI-credentials
    /// pair — are mapped; the wider `BUN_CONFIG_*` install-behavior family
    /// (retry counts, lockfile toggles, …) is not honored.
    pub read_bun_config: bool,

    /// Whether manifest-root map settings owned by an embedder whose
    /// `manifest_namespace` is empty are read as the tool's native config
    /// surface. `false` (default) preserves upstream behavior and keeps
    /// top-level extension keys inert unless a specific cross-tool reader
    /// owns them. Embedders such as nub set this only under their own project
    /// identity, so root-native config is not accidentally honored while
    /// mirroring another package manager.
    pub read_manifest_root_config: bool,

    /// Whether the cwd-default `.pnpmfile` is detected. `true` (default) is
    /// upstream. An embedder under a non-pnpm incumbent sets `false`: a stray
    /// `.pnpmfile` is another tool's resolution-shaping config and is not
    /// honored. Explicit `pnpmfilePath` overrides are unaffected.
    pub pnpmfile_default_enabled: bool,

    /// Additional user-scope `.npmrc`-shaped entries supplied by an embedder.
    /// Empty by default. These are inserted below the real user `.npmrc`, so a
    /// developer's own npm config keeps normal precedence.
    pub synthetic_user_npmrc_entries: Vec<(String, String)>,

    /// Additional project-scope `.npmrc`-shaped entries supplied by an
    /// embedder. Empty by default. These are inserted below the real project
    /// `.npmrc`, preserving explicit project npm config precedence.
    pub synthetic_project_npmrc_entries: Vec<(String, String)>,

    /// PATH entries prepended (in order, ahead of the existing PATH) to every
    /// lifecycle spawn. An embedder places a runtime shim dir first so a bare
    /// `node` in a build script resolves to the augmented runtime. Default
    /// empty = no-op. The embedder owns the *source*; aube copies it into
    /// `ScriptSettings` at settings-resolution time and the spawn path composes
    /// it onto PATH.
    pub path_prepends: Vec<PathBuf>,

    /// Environment overlay applied verbatim to every lifecycle spawn (set last,
    /// so it outranks the settings-derived keys). Generic by design — aube
    /// assigns no meaning to the keys; an embedder fills it to route scripts
    /// through a provisioned/augmented runtime (e.g. point `NODE` at a shim,
    /// pin `npm_node_execpath`, inject a preload via `NODE_OPTIONS`). Default
    /// empty = behavior-preserving.
    pub env_overlay: Vec<(OsString, OsString)>,

    /// Replacement lifecycle `npm_config_user_agent` product token. `None`
    /// (default) falls back to the compile-time [`Embedder::user_agent`] —
    /// standalone aube reports `aube/<version>`. An embedder sets `Some` when
    /// the product string is genuinely *runtime*: nub emits a per-mode UA
    /// embedding the project's RESOLVED node version (e.g.
    /// `pnpm/x nub/x node/vX`), which can't be a compile-time literal. Read at
    /// the lifecycle-UA seam in `aube-scripts`.
    ///
    /// [`Embedder::user_agent`]: crate::identity::Embedder::user_agent
    pub lifecycle_user_agent_product: Option<String>,
}

impl Default for EngineContext {
    /// Upstream-neutral defaults — an unset context reproduces standalone aube
    /// behavior for every seam.
    fn default() -> Self {
        Self {
            embedder_overrides: None,
            trusted_dependencies_honored: true,
            read_branded_pnpm_config: true,
            read_pnpm_global_config: true,
            read_yarn_config: false,
            yarn_is_classic: false,
            read_bun_config: false,
            read_manifest_root_config: false,
            pnpmfile_default_enabled: true,
            synthetic_user_npmrc_entries: Vec::new(),
            synthetic_project_npmrc_entries: Vec::new(),
            path_prepends: Vec::new(),
            env_overlay: Vec::new(),
            lifecycle_user_agent_product: None,
        }
    }
}

static ACTIVE: OnceLock<RwLock<EngineContext>> = OnceLock::new();

fn active() -> &'static RwLock<EngineContext> {
    ACTIVE.get_or_init(|| RwLock::new(EngineContext::default()))
}

/// A snapshot clone of the active engine context, or
/// [`EngineContext::default`] when nothing was set. Never panics.
pub fn engine_context() -> EngineContext {
    match active().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Replace the whole engine context. Use when an embedder computes every field
/// at once; prefer [`update_engine_context`] when populating fields across the
/// different phases of a run.
pub fn set_engine_context(context: EngineContext) {
    match active().write() {
        Ok(mut guard) => *guard = context,
        Err(poisoned) => *poisoned.into_inner() = context,
    }
}

/// Mutate the active engine context in place. The closure receives a mutable
/// reference to the current context (its prior fields preserved), so an
/// embedder can populate one field per phase without clobbering the others.
pub fn update_engine_context(f: impl FnOnce(&mut EngineContext)) {
    match active().write() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With nothing set, every field is upstream-neutral. This is the
    /// behavior-neutrality contract: an embedder that sets nothing gets aube.
    /// (Mirrors `identity::tests::embedder_unset_is_aube`.)
    #[test]
    fn default_is_upstream_neutral() {
        let ctx = EngineContext::default();
        assert_eq!(ctx.embedder_overrides, None);
        assert!(ctx.trusted_dependencies_honored);
        assert!(ctx.read_branded_pnpm_config);
        assert!(ctx.read_pnpm_global_config);
        assert!(!ctx.read_yarn_config);
        assert!(!ctx.yarn_is_classic);
        assert!(!ctx.read_bun_config);
        assert!(!ctx.read_manifest_root_config);
        assert!(ctx.pnpmfile_default_enabled);
        assert!(ctx.synthetic_user_npmrc_entries.is_empty());
        assert!(ctx.synthetic_project_npmrc_entries.is_empty());
        assert!(ctx.path_prepends.is_empty());
        assert!(ctx.env_overlay.is_empty());
        assert_eq!(ctx.lifecycle_user_agent_product, None);
    }
}
