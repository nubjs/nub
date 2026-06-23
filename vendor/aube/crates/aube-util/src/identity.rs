//! Compile-time embedder profile — the binary's identity and embedder-fixed
//! behavior, centralized.
//!
//! aube hardcodes its own name, version, lockfile filename, cache namespace,
//! env-var prefix, and so on across many crates, and bakes in a handful of
//! behavior choices that an embedding host would want to flip. [`Embedder`]
//! gathers those *embedder-fixed* values — branding plus the behavior toggles
//! that are the host's to set, not the user's — into one place, so the
//! binary's identity is selected once, at the entry point, instead of being
//! scattered as literals and policy checks. Standalone aube ships [`AUBE`],
//! which reproduces every value verbatim, and consumers read it through
//! [`embedder`].
//!
//! This struct holds *embedder-fixed* data: branding (pure naming constants)
//! plus the behavior toggles that an embedder — not an end user — owns
//! (`canonical_lockfile_always_wins`, `runtime_switching`, `self_engines_check`,
//! `self_update_enabled`, `warm_store_verify`, `read_branded_settings_env`).
//! Genuinely *user-tunable* knobs do not belong here; those stay settings.
//!
//! An embedder selects its profile by registering it with [`set_embedder`].
//! A host that goes through the library entry point `aube::cli_main` passes
//! its `&'static Embedder` there and `cli_main` registers it; a host that
//! drives the command layer in-process (`aube::commands::*::run`, bypassing
//! `cli_main` — the headline embedding use case) calls [`set_embedder`] itself
//! at startup. Internally the chosen profile is stored once in a private
//! [`OnceLock`]; [`embedder`] returns it, falling back to [`AUBE`] when nothing
//! was registered, so any caller or test that never sets one transparently
//! gets standalone-aube behavior.

use std::sync::OnceLock;

/// The binary's embedder profile — branding plus embedder-fixed behavior.
///
/// Branding fields are pure naming constants. The behavior toggles
/// (`canonical_lockfile_always_wins`, `runtime_switching`,
/// `self_engines_check`, `self_update_enabled`, `warm_store_verify`,
/// `read_branded_settings_env`) are embedder-fixed, not user-tunable: a host
/// that mirrors the project's incumbent package manager, owns Node
/// provisioning, lives outside aube's version namespace, owns its own
/// self-update, trusts the published store, or hides aube's branded settings
/// env family flips them. Genuinely user-tunable knobs stay settings.
#[derive(Clone, Copy, Debug)]
pub struct Embedder {
    /// Tool name, lowercase (e.g. `"aube"`). The proper noun users type and
    /// see in output, and the clap command name driving help/usage/errors.
    /// Must be filesystem- and command-safe (no spaces, slashes, or shell
    /// metacharacters); it is used verbatim in on-disk sidecar paths (e.g.
    /// `.<name>_patch_state.json`, `.<name>-deploy-injected/`) and in command
    /// invocations, so the embedder is responsible for supplying a safe slug.
    pub name: &'static str,
    /// High-visibility display name shown in the progress banner (e.g.
    /// `"aube"`). Usually equal to [`name`](Self::name); split out so an
    /// embedder can brand the banner independently of the command name.
    pub display_name: &'static str,
    /// Vendor attribution rendered after the version in the progress banner,
    /// e.g. `Some("by jdx.dev")`. `None` suppresses the attribution entirely
    /// (an embedder that doesn't want a third-party vendor tag).
    pub vendor: Option<&'static str>,
    /// Version string — `env!("CARGO_PKG_VERSION")` for standalone aube.
    pub version: &'static str,
    /// HTTP `User-Agent` product token, e.g. `"aube/1.19.0"`. Sent to the
    /// registry and exported as the lifecycle `npm_config_user_agent`
    /// product.
    pub user_agent: &'static str,
    /// Names this tool recognizes as *itself* in a `packageManager` field or
    /// a lockfile-kind detection. Standalone aube: `["aube"]`.
    pub self_names: &'static [&'static str],
    /// Names accepted as compatible drop-in targets in the `packageManager`
    /// guardrail. Standalone aube: `["pnpm"]`.
    pub compatible_names: &'static [&'static str],
    /// Canonical lockfile filename, e.g. `"aube-lock.yaml"`.
    ///
    /// Invariant (checked in [`set_embedder`]): must contain a `.` (so the
    /// stem/extension split the lockfile-candidate machinery relies on holds)
    /// and must not collide with a foreign package manager's lockfile name
    /// (`pnpm-lock.yaml`, `package-lock.json`, `bun.lock`, `yarn.lock`,
    /// `npm-shrinkwrap.json`). Aliasing a foreign name would make aube's own
    /// lockfile indistinguishable from the incumbent's in the
    /// lockfile-candidate set (`io.rs` / `clean.rs` / `pack.rs`).
    pub lockfile_basename: &'static str,
    /// The *branded* workspace-config YAML this tool reads and writes, e.g.
    /// `"aube-workspace.yaml"`. `None` disables the tool's own branded YAML
    /// entirely (the shared `pnpm-workspace.yaml` compatibility surface is
    /// handled separately and is not configured here).
    pub workspace_yaml: Option<&'static str>,
    /// The `package.json` object key this tool reads its own config under,
    /// e.g. `"aube"`. `""` means this tool has *no own* branded manifest
    /// namespace: config reads fold only the
    /// [`compatible_names`](Self::compatible_names) namespaces plus any
    /// top-level (manifest-root) entry, and setting *writes* go to the
    /// manifest **root** as top-level `package.json` keys — never under a
    /// foreign brand's namespace, and never as a literal `""` key.
    pub manifest_namespace: &'static str,
    /// Env-var prefix for the tool's *internal* debug / diagnostic / perf-bisect
    /// toggles, read through [`embedder_env`](crate::env::embedder_env), e.g.
    /// `Some("AUBE")` → `AUBE_DISABLE_CLONEDIR`, `AUBE_DIAG_PRINT`, … `None`
    /// means the tool exposes *no* branded debug-toggle family — every such
    /// toggle is simply unreadable, so an embedding host's brand never sprouts a
    /// dozen `<HOST>_DISABLE_*` perf switches. This gates the non-settings,
    /// non-user-facing toggle family only; the few user-facing config knobs go
    /// through [`config_env_prefix`](Self::config_env_prefix), and the settings
    /// table's branded aliases go through
    /// [`branded_env_alias_enabled`](crate::env::branded_env_alias_enabled).
    pub env_prefix: Option<&'static str>,
    /// Env-var prefix for the tool's small set of *first-class config* knobs —
    /// the cache dir, the fetch concurrency, the primer TTL — read through
    /// [`config_env`](crate::env::config_env), e.g. `Some("AUBE")` →
    /// `AUBE_CACHE_DIR` / `AUBE_CONCURRENCY` / `AUBE_PRIMER_TTL`, `Some("NUB")`
    /// → `NUB_CACHE_DIR` / `NUB_CONCURRENCY` / `NUB_PRIMER_TTL`. Distinct from
    /// [`env_prefix`](Self::env_prefix): these few knobs ARE legitimate config
    /// the host wants under its own brand, whereas the debug toggles vanish
    /// under an embedder that hides them. `None` reads no first-class config env.
    pub config_env_prefix: Option<&'static str>,
    /// Leaf directory name under the OS cache root, e.g. `"aube"` →
    /// `<XDG_CACHE_HOME>/aube`.
    pub cache_namespace: &'static str,
    /// Leaf directory name under the OS data/state root, e.g. `"aube"`.
    pub data_namespace: &'static str,

    // --- embedder-fixed behavior toggles (not user-tunable) ---
    /// When `true` (aube's default), this tool's canonical lockfile
    /// (`lockfile_basename`) outranks any foreign lockfile present in
    /// lockfile-kind detection. An embedder that mirrors the project's
    /// incumbent package manager sets this `false` so the incumbent's
    /// lockfile wins instead. Embedder-fixed: it's the host's call, not the
    /// user's.
    pub canonical_lockfile_always_wins: bool,
    /// When `true` (aube's default), this tool resolves and switches the Node
    /// runtime from version files / devEngines and prepends it to `PATH`. An
    /// embedder that owns Node provisioning itself sets this `false`, leaving
    /// the runtime resolver inert. Embedder-fixed.
    pub runtime_switching: bool,
    /// When `true` (aube's default), this tool validates a manifest's
    /// `engines.<self>` constraint against its own version. An embedder whose
    /// version isn't in aube's version namespace sets this `false` to avoid
    /// spurious `engines.aube` mismatches. The `engines.node` check is
    /// unaffected. Embedder-fixed.
    pub self_engines_check: bool,
    /// When `true` (aube's default), this tool owns its own self-update:
    /// the update notifier (and its `aube.jdx.dev` endpoints) runs. An
    /// embedder that owns its own upgrade path sets this `false` so those
    /// code paths never run. Embedder-fixed.
    pub self_update_enabled: bool,
    /// When `true` (aube's default), warm-relink store verification stats
    /// every cached file; when `false`, only the first file per package is
    /// stat'd (fast-trust). An embedder that trusts the atomically-published
    /// store (nub, Bun's model) sets this `false` to skip the per-file stat
    /// sweep. Independent of import-time SRI / `verifyStoreIntegrity`. A fixed
    /// embedder posture (it doesn't vary per project), so it lives here rather
    /// than on the runtime engine context.
    pub warm_store_verify: bool,
    /// When `false` (aube's default), the lockfile writer always writes —
    /// matching upstream aube and pnpm, which rewrite the lockfile whenever
    /// the resolution/write path is reached, even to byte-identical content.
    /// When `true`, the writer first compares the resolved graph's identity
    /// hash against the graph the existing on-disk lockfile encodes and
    /// *skips the write* when they are equal, so an install that didn't
    /// change the resolved graph leaves the lockfile's bytes/mtime untouched.
    ///
    /// This is a behavior an embedder opts into, NOT upstream pnpm behavior:
    /// pnpm achieves an untouched-on-no-op lockfile via an up-front
    /// skip-resolution short-circuit (`allProjectsAreUpToDate` + a deep-equal
    /// of the parsed current/wanted lockfiles) and, once it does resolve,
    /// writes unconditionally — it has no post-resolution "resolved graph ==
    /// on-disk, so skip the write" guard. An embedder that interoperates with
    /// another package manager on the same lockfile (e.g. nub round-tripping
    /// a `pnpm-lock.yaml`) sets this `true` to break the rewrite flip-flop
    /// where each tool rewrites a graph-equal lockfile back into its own
    /// serialization forever. Embedder-fixed: it's the host's call, not the
    /// user's, and it doesn't vary per project.
    pub no_churn_lockfile_write: bool,
    /// When `true` (aube's default), this tool honors its *branded* `AUBE_*`
    /// settings env-var family — the tool-prefixed aliases (`{env_prefix}_<NAME>`)
    /// for user-facing config knobs declared in `settings.toml`. An embedder
    /// whose users shouldn't reach aube's settings through a branded env family
    /// sets this `false`, and every tool-branded settings env var is ignored;
    /// the neutral `npm_config_*` / `NPM_CONFIG_*` aliases and bare external
    /// vars (`CI`, `HTTP_PROXY`, …) are unaffected. Distinct from, and composed
    /// with, [`env_prefix`](Self::env_prefix): `env_prefix` says *which* prefix
    /// is the brand (used to match a var to this tool), while this toggle says
    /// *whether the branded settings-env surface is read at all* — so an
    /// embedder can keep a branded `env_prefix` for identity yet read no branded
    /// settings env vars. This gates only aube's user-facing `AUBE_*` *settings*
    /// surface, never the internal cross-process env vars aube sets for its own
    /// plumbing, and never the error/exit codes in `aube-codes`. Symmetric with
    /// the runtime [`read_branded_pnpm_config`] posture; embedder-fixed.
    ///
    /// [`read_branded_pnpm_config`]: crate::engine_context::EngineContext::read_branded_pnpm_config
    pub read_branded_settings_env: bool,
    /// How long after the bundled primer's build date (`generated_at`) the
    /// offline metadata primer is consulted at all. `None` = unlimited (the
    /// primer never expires); `Some(d)` = consult the primer only while
    /// `now − generated_at < d`, and once the binary ages past `d` skip the
    /// primer entirely and resolve all-network.
    ///
    /// This *replaces* the old `primer_evergreen` boolean. The per-pick regime
    /// logic — a FROZEN pick (settled, immutable history) is served from the
    /// offline primer, a live-frontier pick keeps the freshness refetch — is now
    /// the always-on correctness layer beneath this TTL, not a thing the TTL
    /// switches on and off. Cooling (`minimumReleaseAge` / `trustPolicy`) is
    /// still enforced inside `pick_version` against the primer's own `time` map
    /// regardless of TTL, so the TTL is a staleness bound on the *bundled data*,
    /// never a security lever. The default `None` (unlimited) is correct because
    /// frozen resolution data is immutable: an aged binary's frozen picks are
    /// still right, so there is no reason for the primer to self-disable —
    /// "evergreen" is just an ∞ TTL, not a separate flag.
    ///
    /// Both standalone aube ([`AUBE`]) and nub default to `None` (unlimited).
    /// The `{config_env_prefix}_PRIMER_TTL` env var (`AUBE_PRIMER_TTL` /
    /// `NUB_PRIMER_TTL`) overrides it: `0`/`unlimited`/`inf`/`infinite`/`never`
    /// → unlimited; a duration like `30d` / `720h` / `45m` → finite. Embedder-
    /// fixed: it's the host's call, not the user's, and it doesn't vary per
    /// project.
    pub primer_ttl: Option<std::time::Duration>,
}

/// Standalone aube's embedder profile. Reproduces every hardcoded branding
/// constant and behavior default verbatim; this is the fallback whenever no
/// profile is registered.
pub const AUBE: Embedder = Embedder {
    name: "aube",
    display_name: "aube",
    vendor: Some("by jdx.dev"),
    version: env!("CARGO_PKG_VERSION"),
    user_agent: concat!("aube/", env!("CARGO_PKG_VERSION")),
    self_names: &["aube"],
    compatible_names: &["pnpm"],
    lockfile_basename: "aube-lock.yaml",
    workspace_yaml: Some("aube-workspace.yaml"),
    manifest_namespace: "aube",
    env_prefix: Some("AUBE"),
    config_env_prefix: Some("AUBE"),
    cache_namespace: "aube",
    data_namespace: "aube",
    canonical_lockfile_always_wins: true,
    runtime_switching: true,
    self_engines_check: true,
    self_update_enabled: true,
    warm_store_verify: true,
    no_churn_lockfile_write: false,
    read_branded_settings_env: true,
    primer_ttl: None,
};

static ACTIVE: OnceLock<&'static Embedder> = OnceLock::new();

/// Register the active embedder profile.
///
/// Call this **once at startup**, before invoking any `aube::commands`
/// directly. `aube::cli_main` calls it for you, so binaries that go through
/// `cli_main` don't need to; embedders that drive the command layer in-process
/// — calling `aube::commands::*::run` directly, bypassing `cli_main` (the
/// headline embedding use case) — call it themselves to register their
/// profile before the first command runs.
///
/// Set-once / first-wins: the first registration is the active profile for the
/// process; later calls are silently ignored. A process that never registers
/// one transparently gets standalone-aube behavior ([`AUBE`]) — which is also
/// why tests that don't register a profile see `AUBE`.
///
/// Validates the profile's lockfile invariant in debug builds: a profile whose
/// `lockfile_basename` has no extension or aliases a foreign package manager's
/// lockfile would silently corrupt the lockfile-candidate set, so it trips a
/// `debug_assert!` here — at registration, the single choke point — rather than
/// misbehaving deep inside `io.rs` / `clean.rs` / `pack.rs`.
pub fn set_embedder(embedder: &'static Embedder) {
    debug_assert!(
        embedder.lockfile_basename.contains('.'),
        "embedder lockfile_basename {:?} must contain a `.` (stem/extension split is load-bearing)",
        embedder.lockfile_basename,
    );
    debug_assert!(
        !FOREIGN_LOCKFILE_NAMES.contains(&embedder.lockfile_basename),
        "embedder lockfile_basename {:?} aliases a foreign package manager's lockfile; \
         pick a distinct name so aube's lockfile stays distinguishable in the candidate set",
        embedder.lockfile_basename,
    );
    let _ = ACTIVE.set(embedder);
}

/// Foreign package-manager lockfile names an embedder's `lockfile_basename`
/// must not alias. Aliasing one would make aube's own lockfile collide with
/// the incumbent's in the lockfile-candidate machinery.
const FOREIGN_LOCKFILE_NAMES: &[&str] = &[
    "pnpm-lock.yaml",
    "package-lock.json",
    "bun.lock",
    "yarn.lock",
    "npm-shrinkwrap.json",
];

/// The active embedder profile, or [`AUBE`] when none was registered. Never
/// panics: an unset profile transparently yields standalone-aube behavior.
pub fn embedder() -> &'static Embedder {
    ACTIVE.get().copied().unwrap_or(&AUBE)
}

/// The active tool's program name for *user-facing* output — the proper noun a
/// user types and reads (e.g. `"aube"` under the default profile, the host's
/// brand under an embedder).
///
/// This is the source-branding seam jdx approved over post-processing rendered
/// output: instead of an embedder string-rewriting `"aube"` out of finished
/// banners and error text, user-facing emission sites compose the program name
/// at the source via `prog()` / [`cmd`], so a library consumer (nub) gets its
/// own brand without any post-pass. Use it for bare program-name references in
/// `miette!` / `bail!` / `eprintln!` / `println!` strings (banners, "re-exec
/// into pinned {prog} version", …). For an `aube <verb>` command reference use
/// [`cmd`] instead, which also brands the verb's program prefix.
///
/// Default-preserving: under the default [`AUBE`] profile this returns exactly
/// `"aube"` byte-for-byte, so standalone aube's output is unchanged. Returns
/// [`Embedder::name`] — the command-safe slug, matching what the clap command
/// name and on-disk sidecars already use, so a user reads one consistent name.
pub fn prog() -> &'static str {
    embedder().name
}

/// A *user-facing* `"{prog} <verb>"` command reference, e.g. `cmd("install")`
/// renders `"aube install"` under the default profile and `"nub install"` under
/// a `nub`-branded embedder.
///
/// Use this wherever a user-facing `miette!` / `bail!` / `eprintln!` /
/// `println!` string tells the user to run a command — `"run `{}` first"` with
/// `cmd("install")`, `"`{}`: package has no script"` with `cmd("run")`, help
/// hints, and so on — so the program prefix follows the active brand instead of
/// being hardcoded to `aube`. The `verb` is the command spelling exactly as the
/// CLI accepts it (`"install"`, `"patch-commit"`, `"store prune"`); it is not
/// re-branded, only the leading program name is.
///
/// Default-preserving: under the default [`AUBE`] profile `cmd("install")` is
/// exactly `"aube install"` byte-for-byte, so standalone aube's error and help
/// text is unchanged. Allocates a `String`; for the bare program name with no
/// verb use [`prog`], which borrows.
pub fn cmd(verb: &str) -> String {
    format!("{} {verb}", prog())
}

/// The active embedder's canonical lockfile basename — the file a FRESH
/// install creates (`aube-lock.yaml` under the default profile, `lock.yaml`
/// under nub). Use it wherever a user-facing message names the lockfile the
/// engine writes, so the name follows the active brand instead of hardcoding
/// `aube-lock.yaml`.
///
/// Default-preserving: `aube-lock.yaml` byte-for-byte under [`AUBE`].
pub fn lockfile_basename() -> &'static str {
    embedder().lockfile_basename
}

/// The user-facing list of workspace-root marker files the engine recognizes,
/// composed for the active embedder — e.g. `` "`aube-workspace.yaml`,
/// `pnpm-workspace.yaml`" `` under the default profile and just
/// `` "`pnpm-workspace.yaml`" `` under a profile whose [`Embedder::workspace_yaml`]
/// is `None` (nub). Use it in user-facing `--filter requires a workspace root
/// (…)` / `no workspace root (…)` messages so the markers named follow the
/// active brand instead of hardcoding `aube-workspace.yaml`.
///
/// The compatible pnpm workspace yaml (`pnpm-workspace.yaml`) is always
/// included — the engine reads it under every profile. The embedder's own
/// `workspace_yaml` (when set) precedes it.
///
/// Default-preserving: under the default [`AUBE`] profile this is exactly
/// `` "`aube-workspace.yaml`, `pnpm-workspace.yaml`" `` byte-for-byte, so
/// standalone aube's messages are unchanged.
pub fn workspace_markers() -> String {
    match embedder().workspace_yaml {
        Some(own) if own != "pnpm-workspace.yaml" => {
            format!("`{own}`, `pnpm-workspace.yaml`")
        }
        _ => "`pnpm-workspace.yaml`".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no profile registered, `embedder()` is `AUBE` and every field
    /// reproduces aube's standalone branding and behavior defaults verbatim.
    /// This is the behavior-neutrality contract: an embedder that sets nothing
    /// gets aube.
    ///
    /// Relies on no other test in this binary calling `set_embedder` — the
    /// `ACTIVE` `OnceLock` is process-global and first-write-wins, so a test
    /// that registers a non-aube profile would flip the fallback this asserts.
    /// Keep profile registration out of this crate's unit tests.
    #[test]
    fn embedder_unset_is_aube() {
        let id = embedder();
        assert_eq!(id.name, "aube");
        assert_eq!(id.display_name, "aube");
        assert_eq!(id.vendor, Some("by jdx.dev"));
        assert_eq!(id.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(id.user_agent, concat!("aube/", env!("CARGO_PKG_VERSION")));
        assert_eq!(id.self_names, &["aube"]);
        assert_eq!(id.compatible_names, &["pnpm"]);
        assert_eq!(id.lockfile_basename, "aube-lock.yaml");
        assert_eq!(id.workspace_yaml, Some("aube-workspace.yaml"));
        assert_eq!(id.manifest_namespace, "aube");
        assert_eq!(id.env_prefix, Some("AUBE"));
        assert_eq!(id.config_env_prefix, Some("AUBE"));
        assert_eq!(id.cache_namespace, "aube");
        assert_eq!(id.data_namespace, "aube");
        assert!(id.canonical_lockfile_always_wins);
        assert!(id.runtime_switching);
        assert!(id.self_engines_check);
        assert!(id.self_update_enabled);
        assert!(id.warm_store_verify);
        assert!(!id.no_churn_lockfile_write);
        assert!(id.read_branded_settings_env);
        assert_eq!(id.config_env_prefix, Some("AUBE"));
        assert_eq!(id.primer_ttl, None);
    }

    /// Under the default (AUBE) profile the source-branding helpers reproduce
    /// aube's hardcoded user-facing strings byte-for-byte: `prog()` is `"aube"`
    /// and `cmd("install")` is `"aube install"`. This is the default-preserving
    /// contract for the helpers jdx approved — converting a literal `"aube
    /// install"` site to `cmd("install")` changes nothing for standalone aube.
    /// (The non-aube branch — a host brand flowing through `prog`/`cmd` — is
    /// covered by the `source_branding_brand_gate` integration test, which
    /// registers a real profile in its own process; doing it here would flip
    /// the process-global fallback the default-profile tests depend on.)
    #[test]
    fn prog_and_cmd_render_aube_under_default_profile() {
        assert_eq!(prog(), "aube");
        assert_eq!(cmd("install"), "aube install");
        assert_eq!(cmd("patch-commit"), "aube patch-commit");
        assert_eq!(cmd("store prune"), "aube store prune");
    }

    /// Default-preserving for the marker/lockfile-name source-branding helpers:
    /// under the default profile they reproduce aube's hardcoded user-facing
    /// strings byte-for-byte, so converting a literal `aube-workspace.yaml,
    /// pnpm-workspace.yaml` / `aube-lock.yaml` site to these helpers changes
    /// nothing for standalone aube. (The nub-profile branch — markers collapsed
    /// to just `pnpm-workspace.yaml`, lockfile `lock.yaml` — is covered by the
    /// nub-side CLI tests, which register the NUB profile in their own process;
    /// doing it here would flip the process-global fallback.)
    #[test]
    fn workspace_markers_and_lockfile_basename_under_default_profile() {
        assert_eq!(
            workspace_markers(),
            "`aube-workspace.yaml`, `pnpm-workspace.yaml`"
        );
        assert_eq!(lockfile_basename(), "aube-lock.yaml");
    }
}
