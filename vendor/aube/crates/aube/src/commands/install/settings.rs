use super::version_from_dep_path;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;

/// Curated manifest repairs shipped by Yarn and pnpm for standalone aube.
///
/// Keep these out of `packageExtensionsChecksum`: updates to bundled
/// compatibility data must not invalidate an existing project lockfile.
// Integer offsets keep the generated tables free of per-entry pointer
// relocations, which otherwise add measurable work to every CLI startup.
type BundledStringRef = (u32, u32);
type BundledTableRef = (u32, u32);

struct BundledPackageExtension {
    selector: BundledStringRef,
    dependencies: BundledTableRef,
    optional_dependencies: BundledTableRef,
    peer_dependencies: BundledTableRef,
    peer_dependencies_meta: BundledTableRef,
}

include!(concat!(env!("OUT_DIR"), "/bundled_package_extensions.rs"));

/// Accept pnpm's documented aliases (`highest`, `time-based`, `time`,
/// `lowest-direct`). Unknown values fall back to `None` so the caller's
/// `.npmrc` / default path still runs.
fn parse_resolution_mode(s: &str) -> Option<aube_resolver::ResolutionMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "highest" => Some(aube_resolver::ResolutionMode::Highest),
        "time-based" | "time" => Some(aube_resolver::ResolutionMode::TimeBased),
        "lowest-direct" => Some(aube_resolver::ResolutionMode::LowestDirect),
        _ => None,
    }
}

/// Resolve the effective `ResolutionMode` from the settings chain
/// (CLI > env > `.npmrc` > `aube-workspace.yaml` > default). The `.cli`
/// source carries `--resolution-mode` via `to_cli_flag_bag`, so every
/// caller feeds the same ctx and gets the same answer.
pub(crate) fn resolve_resolution_mode(
    ctx: &aube_settings::ResolveCtx<'_>,
) -> aube_resolver::ResolutionMode {
    // Legacy alias: pnpm's CLI / `.npmrc` / env accept the shorthand
    // `time` for `time-based`. The generator-side `from_str_normalized`
    // only knows the canonical variants declared in `settings.toml`,
    // so walk the same sources one more time for the untyped string
    // and feed it through `parse_resolution_mode`. Retire this once
    // the generator grows per-setting variant aliases.
    let raw = aube_settings::values::string_from_cli("resolutionMode", ctx.cli)
        .or_else(|| aube_settings::values::string_from_env("resolutionMode", ctx.env))
        .or_else(|| {
            aube_settings::values::string_from_npmrc("resolutionMode", ctx.project_aube_config)
        })
        .or_else(|| aube_settings::values::string_from_npmrc("resolutionMode", ctx.project_npmrc))
        .or_else(|| {
            aube_settings::values::string_from_workspace_yaml("resolutionMode", ctx.workspace_yaml)
        })
        .or_else(|| {
            aube_settings::values::string_from_npmrc("resolutionMode", ctx.user_aube_config)
        })
        .or_else(|| aube_settings::values::string_from_npmrc("resolutionMode", ctx.user_npmrc));
    if let Some(raw) = raw
        && let Some(m) = parse_resolution_mode(&raw)
    {
        return m;
    }
    map_resolution_mode(aube_settings::resolved::resolution_mode(ctx))
}

/// Translate the settings-side `ResolutionMode` enum into the
/// resolver's runtime enum.
fn map_resolution_mode(
    m: aube_settings::resolved::ResolutionMode,
) -> aube_resolver::ResolutionMode {
    match m {
        aube_settings::resolved::ResolutionMode::Highest => aube_resolver::ResolutionMode::Highest,
        aube_settings::resolved::ResolutionMode::TimeBased => {
            aube_resolver::ResolutionMode::TimeBased
        }
        aube_settings::resolved::ResolutionMode::LowestDirect => {
            aube_resolver::ResolutionMode::LowestDirect
        }
    }
}

/// Resolve the effective `minimumReleaseAge` configuration from a
/// pre-built resolve context. Every lookup goes through the
/// build-time-generated typed accessors in `aube_settings::resolved`
/// — `.npmrc` first, then `pnpm-workspace.yaml`. CLI override
/// (currently always `None`, no flag yet) wins over both.
pub(crate) fn resolve_minimum_release_age(
    ctx: &aube_settings::ResolveCtx<'_>,
    cli_minutes: Option<u64>,
) -> Option<aube_resolver::MinimumReleaseAge> {
    let minutes = cli_minutes.unwrap_or_else(|| aube_settings::resolved::minimum_release_age(ctx));
    if minutes == 0 {
        return None;
    }
    // Parse pattern-by-pattern (bare names, `*` name globs, exact-version
    // unions) so one malformed entry doesn't silently drop the rest and
    // weaken the gate. Same engine pnpm uses for both exclude settings.
    let (exclude, parse_errors) = aube_resolver::PackageVersionPolicy::parse_lossy(
        aube_settings::resolved::minimum_release_age_exclude(ctx).unwrap_or_default(),
    );
    for err in parse_errors {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_INVALID_MINIMUM_RELEASE_AGE_EXCLUDE,
            error = %err,
            "ignoring malformed minimumReleaseAgeExclude entry"
        );
    }
    // `paranoid=true` forces the gate to be hard, not advisory.
    let strict = aube_settings::resolved::minimum_release_age_strict(ctx)
        || aube_settings::resolved::paranoid(ctx);
    Some(aube_resolver::MinimumReleaseAge {
        minutes,
        exclude,
        strict,
    })
}

/// Resolve the effective `autoInstallPeers` setting from a
/// pre-built resolve context. Delegates to the build-time-generated
/// accessor in `aube_settings::resolved`, which walks `.npmrc` and
/// then `pnpm-workspace.yaml` using the source aliases declared in
/// `settings.toml`.
///
/// Takes the context by reference instead of re-reading the files
/// so the caller can share one read of `pnpm-workspace.yaml` across
/// this resolve, the drift check, and the build-policy load.
pub(super) fn resolve_auto_install_peers(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::auto_install_peers(ctx)
}

/// Resolve `excludeLinksFromLockfile` from `.npmrc` / workspace yaml.
/// Controls only lockfile serialization — the resolver still builds
/// the same graph regardless.
pub(super) fn resolve_exclude_links_from_lockfile(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::exclude_links_from_lockfile(ctx)
}

/// Scan `manifests` for any `trigger` name that appears in an
/// importer's `dependencies`, `devDependencies`, or
/// `optionalDependencies`, and return the first match. Used to power
/// `disableGlobalVirtualStoreForPackages` — some tools (Next.js's
/// Turbopack is the canonical example) canonicalize every
/// `node_modules/<pkg>` symlink and reject targets that escape the
/// project's filesystem root, which aube's global virtual store
/// produces by default. When a manifest declares one of those
/// packages, the install driver falls back to per-project
/// materialization. `peerDependencies` intentionally doesn't count —
/// a library that declares `next` as a peer isn't itself a Next.js
/// app.
pub(super) fn find_gvs_incompatible_trigger<'a>(
    manifests: &'a [(String, aube_manifest::PackageJson)],
    triggers: &[String],
) -> Option<&'a str> {
    for (_, m) in manifests {
        for pattern in triggers {
            if let Some(name) = m
                .dependencies
                .keys()
                .chain(m.dev_dependencies.keys())
                .chain(m.optional_dependencies.keys())
                .find(|name| aube_linker::package_name_matches(pattern, name))
            {
                return Some(name.as_str());
            }
        }
    }
    None
}

#[cfg(test)]
mod gvs_trigger_pattern_tests {
    use super::find_gvs_incompatible_trigger;

    #[test]
    fn wildcard_and_literal_triggers_match_declared_packages() {
        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .dependencies
            .insert("is-number".to_string(), "7.0.0".to_string());
        let manifests = vec![(".".to_string(), manifest)];

        assert_eq!(
            find_gvs_incompatible_trigger(&manifests, &["is-*".to_string()]),
            Some("is-number")
        );
        assert_eq!(
            find_gvs_incompatible_trigger(&manifests, &["is-number".to_string()]),
            Some("is-number")
        );
        assert_eq!(
            find_gvs_incompatible_trigger(&manifests, &["left-*".to_string()]),
            None
        );
    }
}

/// Classify the existing `.aube/` tree as built with the global virtual
/// store (entries are symlinks into the shared store) or with
/// per-project materialization (entries are real directories holding
/// the package files). Returns `None` when the tree is missing or has
/// no inspectable package entries — a fresh checkout or a prior
/// `--lockfile-only` run.
///
/// The linker can't reconcile a mode switch in place: a non-gvs install
/// that lands on a gvs tree silently re-uses stale symlinks into the
/// shared store, and a gvs install that lands on a per-project tree
/// fails to unlink the populated directories before creating its
/// symlinks. Callers use this to detect the transition and wipe
/// `node_modules/` before the linker runs.
///
/// Symlink-PRIORITY classification: any single symlink entry means the tree
/// is in global-virtual-store mode. GVS-on trees are NOT necessarily uniform
/// — `diskMaterializePackages` and the project-local legacy-Vite copies make
/// a handful of entries real dirs while the rest stay shared-store symlinks, a
/// legitimately MIXED tree — so classifying from the first `read_dir` entry
/// would be order-dependent (a forced real dir landing first would misread the
/// whole tree as per-project and trigger a spurious wipe on every install).
/// Per-project mode is the only mode with NO symlink entries, so `false` is
/// returned only after the full scan finds real dirs and zero symlinks. For a
/// consistent tree (every entry the same type — standalone aube's default) the
/// result is identical to the first-entry classification; only the mixed tree
/// differs.
pub(super) fn detect_aube_dir_gvs_mode(aube_dir: &std::path::Path) -> Option<bool> {
    let entries = std::fs::read_dir(aube_dir).ok()?;
    let mut saw_real_dir = false;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Skip the hidden hoist tree and sidecar dotfiles
        // (`.modules.yaml`, etc.). Scoped packages are encoded as
        // `@scope+name@version` on disk, so `@`-prefixed entries are
        // real package entries and must not be skipped.
        if name_str == "node_modules" || name_str.starts_with('.') {
            continue;
        }
        // `read_link` is the positive test for a link: it succeeds on
        // both a Unix symlink and a Windows junction reparse point,
        // which is what `sys::create_dir_link` writes on each platform.
        // The negative half is NOT its inverse — a failed `read_link`
        // reports a DIFFERENT error kind per platform, and reading a
        // plain directory off that kind is why this function could never
        // return `Some(false)` on Windows (nub#566; see
        // `aube_util::fs::is_real_dir`). With the mode change therefore
        // invisible there, `reset_on_mode_change` never wiped a
        // per-project tree and the gvs link pass that followed collided
        // with the surviving directories (`ERROR_ALREADY_EXISTS`).
        // Anything that is neither a link nor a real directory is
        // skipped.
        let path = entry.path();
        if std::fs::read_link(&path).is_ok() {
            return Some(true);
        }
        if aube_util::fs::is_real_dir(&path) {
            saw_real_dir = true;
        }
    }
    saw_real_dir.then_some(false)
}

#[cfg(test)]
mod gvs_mode_tests {
    use super::detect_aube_dir_gvs_mode;

    #[test]
    fn mixed_project_local_and_linked_entries_still_classify_as_gvs() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let aube_dir = tmp.path().join(".aube");
        let global = tmp.path().join("global/foo");
        std::fs::create_dir_all(aube_dir.join("vite@7.3.6"))
            .expect("project-local entry should be created");
        std::fs::create_dir_all(&global).expect("global entry should be created");
        aube_linker::sys::create_dir_link(&global, &aube_dir.join("foo@1.0.0"))
            .expect("global-store link should be created");

        assert_eq!(detect_aube_dir_gvs_mode(&aube_dir), Some(true));

        std::fs::remove_dir(aube_dir.join("foo@1.0.0"))
            .or_else(|_| std::fs::remove_file(aube_dir.join("foo@1.0.0")))
            .expect("global-store link should be removed");
        assert_eq!(detect_aube_dir_gvs_mode(&aube_dir), Some(false));
    }
}

pub(crate) fn resolve_catalog_prune(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::catalog_prune(ctx)
        .unwrap_or_else(|| aube_settings::resolved::cleanup_unused_catalogs(ctx))
}

/// Honor `catalogPrune` (or its deprecated `cleanupUnusedCatalogs` alias) by
/// pruning declared-but-unreferenced catalog entries from the workspace yaml. No-op when the setting is
/// off, when there is no workspace yaml file on disk, or when every
/// declared entry was referenced by an importer.
pub(super) fn maybe_cleanup_unused_catalogs(
    cwd: &std::path::Path,
    ctx: &aube_settings::ResolveCtx<'_>,
    declared: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    used: &std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, aube_lockfile::CatalogEntry>,
    >,
) -> miette::Result<()> {
    if !resolve_catalog_prune(ctx) {
        return Ok(());
    }
    if declared.is_empty() {
        return Ok(());
    }
    let Some(ws_path) = aube_manifest::workspace::workspace_yaml_existing(cwd) else {
        return Ok(());
    };
    let dropped = super::super::catalogs::prune_unused_catalog_entries(&ws_path, declared, used)?;
    if !dropped.is_empty() {
        let filename = ws_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| ws_path.display().to_string());
        tracing::info!(
            "catalogPrune: pruned {} from {filename}",
            pluralizer::pluralize("entry", dropped.len() as isize, true)
        );
    }
    Ok(())
}

/// Resolve `networkConcurrency` from cli / env / `.npmrc` /
/// workspace yaml. Returns `None` on miss so the caller can fall
/// back to its own hardcoded default (different sites intentionally
/// ship different defaults).
pub(super) fn resolve_network_concurrency(ctx: &aube_settings::ResolveCtx<'_>) -> Option<usize> {
    aube_settings::resolved::network_concurrency(ctx).and_then(|n| {
        if n == 0 {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_CONCURRENCY,
                "ignoring network-concurrency=0 (must be >= 1)"
            );
            None
        } else {
            Some(n as usize)
        }
    })
}

pub(super) fn resolve_link_concurrency(ctx: &aube_settings::ResolveCtx<'_>) -> Option<usize> {
    aube_settings::resolved::link_concurrency(ctx).and_then(|n| {
        if n == 0 {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_INVALID_CONCURRENCY,
                "ignoring link-concurrency=0 (must be >= 1)"
            );
            None
        } else {
            Some(n as usize)
        }
    })
}

pub(super) fn default_lockfile_network_concurrency() -> usize {
    default_network_concurrency()
}

pub(super) fn default_streaming_network_concurrency() -> usize {
    default_network_concurrency()
}

fn default_network_concurrency() -> usize {
    let workers = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4);
    network_concurrency_for_workers(workers)
}

fn network_concurrency_for_workers(workers: usize) -> usize {
    // 128 ceiling chosen empirically. The npm registry advertises
    // ~100 concurrent HTTP/2 streams per connection; with prior
    // knowledge of h2 multiplexing a single TCP connection absorbs
    // most of this and we never spawn 128 sockets. The old 64 cap
    // queued the second wave on cold installs >500 packages.
    workers.saturating_mul(3).clamp(16, 128)
}

/// Resolve `verifyStoreIntegrity` from cli / env / `.npmrc` /
/// workspace yaml. Defaults to `true` (pnpm parity) so the tarball
/// SHA-512 is checked against the lockfile integrity at import time.
pub(super) fn resolve_verify_store_integrity(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::verify_store_integrity(ctx)
}

/// Resolve `strictStoreIntegrity` from `.npmrc` / workspace yaml.
/// Defaults to `false` so ecosystem parity with pnpm is preserved
/// (pnpm only warns on a missing `dist.integrity`). Flipping this on
/// promotes the warning to a hard error, which matters when a
/// registry proxy or MITM could be stripping the integrity field.
pub(super) fn resolve_strict_store_integrity(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    // `paranoid=true` promotes "missing dist.integrity" to a hard fail.
    aube_settings::resolved::strict_store_integrity(ctx) || aube_settings::resolved::paranoid(ctx)
}

/// Resolve `strictStorePkgContentCheck` from `.npmrc`. Defaults to
/// `true` (pnpm parity): after each registry tarball lands in the CAS
/// we read its `package.json` and verify the embedded `name`/`version`
/// match the resolver's expectation, defending against a registry
/// substituting a tarball under one (name, version) but containing a
/// different package on disk.
pub(super) fn resolve_strict_store_pkg_content_check(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::strict_store_pkg_content_check(ctx)
}

/// Resolve `useRunningStoreServer` from `.npmrc`. aube has no
/// store-daemon, so this is accept-and-warn: a `true` value triggers a
/// single one-line warning at install start so a `.npmrc` ported from
/// a pnpm store-server setup keeps working unchanged. Returning the
/// raw value lets the caller decide whether to emit the warning.
pub(super) fn resolve_use_running_store_server(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::use_running_store_server(ctx)
}

/// Resolve `symlink` from cli / env / `.npmrc`. aube's isolated layout
/// is defined by the symlink graph under `node_modules/.aube/`, so the
/// only supported value is the default `true`. This is accept-and-warn:
/// `false` is read without failing the install (so a `.npmrc` ported
/// from a hard-copy pnpm setup keeps loading) but triggers a single
/// one-line warning at install start. Returning the raw value lets the
/// caller decide whether to emit the warning.
pub(super) fn resolve_symlink(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::symlink(ctx)
}

/// Resolve the `git-shallow-hosts` list from cli / env / `.npmrc` /
/// workspace yaml. Falls back to pnpm's built-in default list when no
/// configuration sets it — the accessor's own default already reflects
/// that, so the call site never has to duplicate the list.
pub(super) fn resolve_git_shallow_hosts(ctx: &aube_settings::ResolveCtx<'_>) -> Vec<String> {
    aube_settings::resolved::git_shallow_hosts(ctx)
}

/// Resolve `sideEffectsCache` from cli / env / `.npmrc` / workspace
/// yaml.
pub(super) fn resolve_side_effects_cache(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::side_effects_cache(ctx)
}

pub(super) fn resolve_side_effects_cache_readonly(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::side_effects_cache_readonly(ctx)
}

/// Resolve `strictPeerDependencies` from `.npmrc` / workspace yaml.
/// When true, any peer the resolver couldn't satisfy (missing or
/// out-of-range) fails the install instead of only printing a warning.
pub(super) fn resolve_strict_peer_dependencies(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::strict_peer_dependencies(ctx)
}

/// Resolved `peersSuffixMaxLength` — cap on lockfile peer-ID suffix byte
/// length before the resolver hashes it with SHA-256. Returns `usize` for
/// direct comparison against `String::len()` inside the resolver. A cast
/// from `u64` on 32-bit platforms saturates safely: pnpm's default is 1000
/// and no sane value comes close to `usize::MAX`.
pub(super) fn resolve_peers_suffix_max_length(ctx: &aube_settings::ResolveCtx<'_>) -> usize {
    let raw = aube_settings::resolved::peers_suffix_max_length(ctx);
    usize::try_from(raw).unwrap_or(usize::MAX)
}

/// Resolve `dedupePeerDependents` from `.npmrc` / workspace yaml.
/// When true (pnpm's default), peer-context post-pass collapses
/// peer-equivalent subtree variants into one canonical dep_path.
pub(super) fn resolve_dedupe_peer_dependents(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::dedupe_peer_dependents(ctx)
}

/// Resolve `dedupePeers` from `.npmrc` / workspace yaml. When true,
/// lockfile peer suffixes drop the peer name and emit just the version
/// — `(18.2.0)` instead of `(react@18.2.0)`.
pub(super) fn resolve_dedupe_peers(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::dedupe_peers(ctx)
}

/// Resolve `resolvePeersFromWorkspaceRoot` from `.npmrc` / workspace
/// yaml. When true (pnpm's default), unresolved peers fall back to
/// the root importer's direct deps before the graph-wide scan.
pub(super) fn resolve_peers_from_workspace_root(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::resolve_peers_from_workspace_root(ctx)
}

/// Resolve `registrySupportsTimeField` from `.npmrc` / workspace
/// yaml. When true, aube keeps the abbreviated-packument fetch on
/// the hot path under `resolutionMode=time-based` and
/// `minimumReleaseAge`, trusting the registry to embed `time` in
/// corgi responses. Default false (pnpm's and npmjs.org's behavior).
fn resolve_registry_supports_time_field(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::registry_supports_time_field(ctx)
}

pub(crate) fn resolve_force_metadata_primer(ctx: &aube_settings::ResolveCtx<'_>) -> bool {
    aube_settings::resolved::force_metadata_primer(ctx)
}

pub(crate) fn resolve_dependency_policy(
    manifest: &aube_manifest::PackageJson,
    ctx: &aube_settings::ResolveCtx<'_>,
) -> miette::Result<aube_resolver::DependencyPolicy> {
    let mut policy = aube_resolver::DependencyPolicy::default();

    validate_package_extension_containers(manifest, ctx)?;
    let package_extensions = effective_package_extensions(manifest, ctx);
    // User/project extensions first, then standalone and embedder-supplied
    // bundled ecosystem defaults appended LAST. `apply_package_extensions`
    // iterates this Vec in order and `extend_missing` is first-write-wins per
    // dependency key, so user extensions take precedence over bundled ones.
    // Bundled data is NEVER routed through `effective_package_extensions`,
    // which feeds the lockfile
    // `packageExtensionsChecksum`: routing bundled defaults through the
    // checksum would drift every existing lockfile on each bundled-list bump
    // and abort `--frozen-lockfile` under `enforce_package_extensions_checksum`.
    let mut extensions = parse_package_extensions(package_extensions)?;
    if !aube_settings::resolved::ignore_compatibility_db(ctx) {
        // The vendored ecosystem catalogs apply to EVERY embedder, not only
        // standalone aube. They are pnpm's own bundled data — Yarn's
        // `packageExtensions` database plus pnpm's three additions — and pnpm
        // merges it into every install unless `ignoreCompatibilityDb` is set. An
        // embedder that skipped it resolves a strictly smaller dependency graph
        // than the tool it mirrors, which is a compatibility difference users
        // hit as a missing module rather than as a warning: `reactcss@1.2.3`
        // requires `react` and declares it nowhere, so it installs and runs
        // under pnpm (the catalog adds the peer, and `auto-install-peers`
        // supplies it) and fails with `Cannot find module 'react'` without the
        // catalog.
        //
        // Catalog order is significant when semver selectors overlap: the first
        // matching extension wins per dependency key.
        extensions.extend(standalone_bundled_package_extensions());
        if let Some(bundled) = aube_util::engine_context().bundled_package_extensions {
            // Bundled extensions are embedder-supplied data, not user config.
            // A malformed entry should warn and be skipped, not abort the install
            // with an error naming a selector the user never wrote.
            extensions.extend(parse_bundled_package_extensions(bundled));
        }
    }
    policy.package_extensions = extensions;

    let mut allowed_deprecated = manifest.allowed_deprecated_versions();
    merge_string_map_setting(ctx, "allowedDeprecatedVersions", &mut allowed_deprecated);
    policy.allowed_deprecated_versions = allowed_deprecated;

    // `paranoid=true` forces no-downgrade regardless of the explicit
    // `trustPolicy` value — that's the whole point of the bundle switch.
    let paranoid = aube_settings::resolved::paranoid(ctx);
    policy.trust_policy = if paranoid {
        aube_resolver::TrustPolicy::NoDowngrade
    } else {
        match aube_settings::resolved::trust_policy(ctx) {
            aube_settings::resolved::TrustPolicy::NoDowngrade => {
                aube_resolver::TrustPolicy::NoDowngrade
            }
            aube_settings::resolved::TrustPolicy::Off => aube_resolver::TrustPolicy::Off,
        }
    };
    // Parse trustPolicyExclude pattern-by-pattern so one malformed entry
    // doesn't drop the rest. Silently dropping every rule on a typo
    // would turn the opt-in into a security regression.
    let trust_excludes = aube_settings::resolved::trust_policy_exclude(ctx);
    let (user_rules, parse_errors) = aube_resolver::TrustExcludeRules::parse_lossy(trust_excludes);
    for err in parse_errors {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_INVALID_TRUST_POLICY,
            error = %err,
            "ignoring malformed trustPolicyExclude entry"
        );
    }
    policy.trust_policy_exclude =
        aube_resolver::TrustExcludeRules::with_defaults_and_user_rules(user_rules);
    // An explicit user `trustPolicyIgnoreAfter` wins (including `0`, which
    // re-enables the full check); when unset, fall back to the embedder-fixed
    // default — `None` for standalone aube (unchanged: check every version), a
    // finite window for nub (see `Embedder::trust_policy_ignore_after_default`).
    policy.trust_policy_ignore_after = aube_settings::resolved::trust_policy_ignore_after(ctx)
        .or(aube_util::embedder().trust_policy_ignore_after_default);
    policy.block_exotic_subdeps = aube_settings::resolved::block_exotic_subdeps(ctx);

    Ok(policy)
}

/// Assemble the effective `packageExtensions` object — the root
/// manifest's `pnpm.packageExtensions` merged with every config source
/// (`.npmrc`, `pnpm-workspace.yaml`, env), later sources winning per
/// key. This is the object the resolver parses into typed
/// `PackageExtension`s *and* the one pnpm hashes into
/// `packageExtensionsChecksum`, so both must read it from here to stay
/// in agreement.
pub(crate) fn effective_package_extensions(
    manifest: &aube_manifest::PackageJson,
    ctx: &aube_settings::ResolveCtx<'_>,
) -> BTreeMap<String, serde_json::Value> {
    let mut package_extensions = manifest.package_extensions();
    merge_json_object_setting(ctx, "packageExtensions", &mut package_extensions);
    package_extensions
}

/// Effective `(os, cpu, libc)` platform-widening triple: the
/// `package.json`/`pnpm-workspace.yaml` value from
/// [`aube_manifest::effective_supported_architectures`] unioned with the
/// config-sourced `supportedArchitectures` object setting. The latter is
/// where a Yarn `.yarnrc.yml` `supportedArchitectures:` lands — yarnrc
/// translation emits it as the JSON-object `supportedArchitectures` npmrc
/// key, which flows through `merge_json_object_setting` exactly like
/// `packageExtensions`. Unioning (rather than overriding) matches how the
/// manifest and workspace-yaml sources already combine, and keeps the
/// arch set additive across every config home.
///
/// Command-line `--os`/`--cpu`/`--libc`
/// ([`aube_util::CliSupportedArchitectures`]) are applied LAST and per axis,
/// REPLACING the unioned config value for each axis named rather than adding
/// to it. Union is right between config homes, which all answer "what does
/// this project support"; the flags answer "what should this invocation
/// fetch", and a union could only ever widen — so `--os=linux` on a project
/// whose config says `darwin` has to mean linux, not both. Every filter site
/// routes through this function, so the override lands on the resolver, the
/// streaming-fetch gate, and `filter_graph` alike.
pub(crate) fn effective_supported_architectures(
    manifest: &aube_manifest::PackageJson,
    ws_config: &aube_manifest::workspace::WorkspaceConfig,
    ctx: &aube_settings::ResolveCtx<'_>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut os, mut cpu, mut libc) =
        aube_manifest::effective_supported_architectures(manifest, ws_config);
    let mut obj: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    merge_json_object_setting(ctx, "supportedArchitectures", &mut obj);
    let extend_field = |dst: &mut Vec<String>, key: &str| {
        let Some(serde_json::Value::Array(arr)) = obj.get(key) else {
            return;
        };
        for v in arr {
            if let Some(s) = v.as_str()
                && !dst.iter().any(|existing| existing == s)
            {
                dst.push(s.to_string());
            }
        }
    };
    extend_field(&mut os, "os");
    extend_field(&mut cpu, "cpu");
    extend_field(&mut libc, "libc");

    let cli = aube_util::engine_context().cli_supported_architectures;
    if let Some(v) = cli.os {
        os = v;
    }
    if let Some(v) = cli.cpu {
        cpu = v;
    }
    if let Some(v) = cli.libc {
        libc = v;
    }
    (os, cpu, libc)
}

/// Stamp pnpm's `packageExtensionsChecksum` / `pnpmfileChecksum` onto
/// `graph` so a written pnpm-lock.yaml matches what pnpm itself records,
/// keeping config-drift detection in sync (a wrong/absent value makes
/// pnpm re-resolve, or abort a frozen install).
///
/// `packageExtensionsChecksum` is stamped on `pnpm-lock.yaml` always
/// (registry byte-parity with pnpm) and on the embedder's own generic
/// lockfile (the `Aube` kind, pnpm-v9 bytes) when the embedder enforces the
/// checksum (`enforce_package_extensions_checksum` — nub) — so its own
/// packageExtensions drift check reaches a fixpoint. Standalone aube leaves
/// that posture off, so its aube-lock.yaml never grows the field (unchanged).
/// `pnpmfileChecksum` stays pnpm-lock-only: the generic lockfile never
/// carried it, and nub identity disables the default pnpmfile anyway.
///
/// `local_pnpmfile` is the project-local pnpmfile that participates in
/// the checksum — the caller resolves it via `crate::pnpmfile::detect`
/// so this stays agnostic to `--ignore-pnpmfile` and the global-pnpmfile
/// exclusion (pnpm hashes only the local file). Both checksums derive
/// from the same inputs pnpm uses.
pub(crate) async fn stamp_pnpm_config_checksums(
    graph: &mut aube_lockfile::LockfileGraph,
    write_kind: aube_lockfile::LockfileKind,
    manifest: &aube_manifest::PackageJson,
    ctx: &aube_settings::ResolveCtx<'_>,
    local_pnpmfile: Option<&std::path::Path>,
) {
    let is_pnpm = matches!(write_kind, aube_lockfile::LockfileKind::Pnpm);
    let stamp_package_extensions = is_pnpm
        || (matches!(write_kind, aube_lockfile::LockfileKind::Aube)
            && aube_util::engine_context().enforce_package_extensions_checksum);
    if !stamp_package_extensions {
        return;
    }
    let package_extensions = effective_package_extensions(manifest, ctx);
    graph.package_extensions_checksum =
        aube_lockfile::pnpm::package_extensions_checksum(&package_extensions);
    if !is_pnpm {
        return;
    }

    // Always reflect the *current* pnpmfile state: a missing, hook-less,
    // or unreadable pnpmfile must clear any checksum the graph carried
    // over (e.g. from a parsed lockfile), otherwise the written lockfile
    // keeps a stale `pnpmfileChecksum` that pnpm treats as config drift.
    //
    // pnpm records the checksum only when the loaded pnpmfile actually
    // exports a `hooks` object (`requireHooks` gates
    // `calculatePnpmfileChecksum` on `entries.some(e => e.hooks != null)`).
    // A pnpmfile that exists but exports no hooks — e.g. an empty
    // `.pnpmfile.cjs` — gets no checksum from pnpm; stamping one anyway
    // aborts pnpm's frozen install with ERR_PNPM_LOCKFILE_CONFIG_MISMATCH.
    // So gate on the export, not on file existence.
    graph.pnpmfile_checksum = match local_pnpmfile {
        Some(path) => match crate::pnpmfile::exports_hooks(path).await {
            Ok(true) => match aube_lockfile::pnpm::pnpmfile_checksum(&[path.to_path_buf()]) {
                Ok(checksum) => checksum,
                Err(e) => {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_PNPMFILE_CHECKSUM_FAILED,
                        "failed to read pnpmfile {} for checksum: {e}",
                        path.display()
                    );
                    None
                }
            },
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_PNPMFILE_CHECKSUM_FAILED,
                    "failed to inspect pnpmfile {} for hooks: {e}",
                    path.display()
                );
                None
            }
        },
        None => None,
    };
}

/// Finalize a freshly resolved graph for the lockfile write paths that
/// live *outside* `install` (`update`/`upgrade`, `remove`, `dedupe`,
/// `audit --fix`). Mirrors the install path's pre-write sequence:
/// stamp pnpm's config checksums (`packageExtensionsChecksum` +
/// `pnpmfileChecksum`) then apply the pnpm-parity snapshot passes
/// (`optional: true`, `transitivePeerDependencies`).
///
/// Before this existed, those commands resolved a graph with both
/// checksum fields `None` and wrote it straight to disk, so e.g.
/// `aube upgrade` dropped the `packageExtensionsChecksum` /
/// `pnpmfileChecksum` a prior `aube install` had recorded — and the
/// chained frozen-prefer install reused the now-stale lockfile without
/// restamping, so the fields never came back. pnpm writes these on
/// every command that rewrites the lockfile; matching that keeps
/// config-drift detection (ours and pnpm's) honest.
///
/// `ignore_pnpmfile` / `cli_pnpmfile` mirror the install flags: when a
/// caller honors `--ignore-pnpmfile` the local pnpmfile is excluded
/// from the checksum (pnpm clears it in that mode); `cli_pnpmfile` is
/// the `--pnpmfile` override (only `update` exposes one today).
///
/// Fails fast if `pnpm-workspace.yaml` is present but malformed: the
/// stamped checksums are derived from that config, so falling back to an
/// empty workspace would persist a checksum computed from the wrong
/// inputs and desync config-drift detection. This matches the install
/// entry path, which also propagates the parse error (a missing or empty
/// workspace file is `Ok(default)`, not an error, so single-package
/// projects are unaffected).
pub(crate) async fn finalize_lockfile_graph(
    cwd: &std::path::Path,
    graph: &mut aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    ignore_pnpmfile: bool,
    cli_pnpmfile: Option<&std::path::Path>,
) -> miette::Result<()> {
    let files = crate::commands::FileSources::load(cwd);
    let (ws_config, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config for lockfile finalization")?;
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    let write_kind = crate::commands::lockfile_kind_for_write_with_ctx(cwd, &ctx);
    let local_pnpmfile = if ignore_pnpmfile {
        None
    } else {
        crate::pnpmfile::detect(cwd, cli_pnpmfile, ws_config.pnpmfile_path.as_deref())
    };
    stamp_pnpm_config_checksums(graph, write_kind, manifest, &ctx, local_pnpmfile.as_deref()).await;
    crate::commands::prepare_resolved_graph_for_lockfile_write(graph);
    Ok(())
}

fn merge_json_object_setting(
    ctx: &aube_settings::ResolveCtx<'_>,
    setting: &str,
    out: &mut BTreeMap<String, serde_json::Value>,
) {
    // Walk file sources in low-to-high precedence order so later
    // `.extend` calls overwrite earlier ones for shared keys.
    // `workspace_yaml` sits between user-scope and project-scope —
    // it's project-scope locality.
    if let Some(value) = object_setting_from_npmrc(setting, ctx.user_npmrc) {
        out.extend(value);
    }
    if let Some(value) = object_setting_from_npmrc(setting, ctx.user_aube_config) {
        out.extend(value);
    }
    if let Some(value) = object_setting_from_workspace_yaml(setting, ctx.workspace_yaml) {
        out.extend(value);
    }
    if let Some(value) = object_setting_from_npmrc(setting, ctx.project_npmrc) {
        out.extend(value);
    }
    if let Some(value) = object_setting_from_npmrc(setting, ctx.project_aube_config) {
        out.extend(value);
    }
    if let Some(value) = object_setting_from_env(setting, ctx.env) {
        out.extend(value);
    }
}

fn merge_string_map_setting(
    ctx: &aube_settings::ResolveCtx<'_>,
    setting: &str,
    out: &mut BTreeMap<String, String>,
) {
    if let Some(value) = object_setting_from_npmrc(setting, ctx.user_npmrc) {
        out.extend(json_string_map(value));
    }
    if let Some(value) = object_setting_from_npmrc(setting, ctx.user_aube_config) {
        out.extend(json_string_map(value));
    }
    if let Some(value) = object_setting_from_workspace_yaml(setting, ctx.workspace_yaml) {
        out.extend(json_string_map(value));
    }
    if let Some(value) = object_setting_from_npmrc(setting, ctx.project_npmrc) {
        out.extend(json_string_map(value));
    }
    if let Some(value) = object_setting_from_npmrc(setting, ctx.project_aube_config) {
        out.extend(json_string_map(value));
    }
    if let Some(value) = object_setting_from_env(setting, ctx.env) {
        out.extend(json_string_map(value));
    }
}

fn deprecated_dollar_override_refs(overrides: &BTreeMap<String, String>) -> Vec<(&str, &str)> {
    overrides
        .iter()
        .filter_map(|(key, value)| {
            value
                .strip_prefix('$')
                .filter(|dep| !dep.is_empty())
                .map(|dep| (key.as_str(), dep))
        })
        .collect()
}

fn object_setting_from_npmrc(
    setting: &str,
    entries: &[(String, String)],
) -> Option<BTreeMap<String, serde_json::Value>> {
    let meta = aube_settings::find(setting)?;
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str()) {
            return parse_json_object(raw);
        }
    }
    None
}

fn object_setting_from_env(
    setting: &str,
    env: &[(String, String)],
) -> Option<BTreeMap<String, serde_json::Value>> {
    let meta = aube_settings::find(setting)?;
    for (key, raw) in env.iter().rev() {
        if meta.env_vars.contains(&key.as_str()) {
            return parse_json_object(raw);
        }
    }
    None
}

fn object_setting_from_workspace_yaml(
    setting: &str,
    raw: &BTreeMap<String, yaml_serde::Value>,
) -> Option<BTreeMap<String, serde_json::Value>> {
    let meta = aube_settings::find(setting)?;
    for key in meta.workspace_yaml_keys {
        let Some(value) = aube_settings::workspace_yaml_value(raw, key) else {
            continue;
        };
        if let Ok(serde_json::Value::Object(obj)) = serde_json::to_value(value) {
            return Some(obj.into_iter().collect());
        }
    }
    None
}

fn parse_json_object(raw: &str) -> Option<BTreeMap<String, serde_json::Value>> {
    let serde_json::Value::Object(obj) = serde_json::from_str(raw).ok()? else {
        return None;
    };
    Some(obj.into_iter().collect())
}

fn validate_package_extension_containers(
    manifest: &aube_manifest::PackageJson,
    ctx: &aube_settings::ResolveCtx<'_>,
) -> miette::Result<()> {
    for value in manifest.package_extension_values() {
        if !value.is_object() {
            return Err(invalid_package_extension(
                "packageExtensions",
                "setting must be an object",
            ));
        }
    }

    let meta = aube_settings::find("packageExtensions").ok_or_else(|| {
        invalid_package_extension("packageExtensions", "setting is not registered")
    })?;
    for entries in [
        ctx.user_npmrc,
        ctx.user_aube_config,
        ctx.project_npmrc,
        ctx.project_aube_config,
    ] {
        for (key, raw) in entries.iter().rev() {
            if !meta.npmrc_keys.contains(&key.as_str()) {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
                invalid_package_extension("packageExtensions", "setting must be valid JSON")
            })?;
            if !value.is_object() {
                return Err(invalid_package_extension(
                    "packageExtensions",
                    "setting must be an object",
                ));
            }
            break;
        }
    }
    for key in meta.workspace_yaml_keys {
        let Some(value) = aube_settings::workspace_yaml_value(ctx.workspace_yaml, key) else {
            continue;
        };
        let value = serde_json::to_value(value).map_err(|_| {
            invalid_package_extension("packageExtensions", "setting must be an object")
        })?;
        if !value.is_object() {
            return Err(invalid_package_extension(
                "packageExtensions",
                "setting must be an object",
            ));
        }
        break;
    }
    for (key, raw) in ctx.env.iter().rev() {
        if !meta.env_vars.contains(&key.as_str()) {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
            invalid_package_extension("packageExtensions", "setting must be valid JSON")
        })?;
        if !value.is_object() {
            return Err(invalid_package_extension(
                "packageExtensions",
                "setting must be an object",
            ));
        }
        break;
    }
    Ok(())
}

fn json_string_map(map: BTreeMap<String, serde_json::Value>) -> BTreeMap<String, String> {
    map.into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect()
}

fn parse_package_extensions(
    raw: impl IntoIterator<Item = (String, serde_json::Value)>,
) -> miette::Result<Vec<aube_resolver::PackageExtension>> {
    raw.into_iter()
        .map(|(selector, value)| parse_package_extension(selector, value))
        .collect()
}

fn parse_bundled_package_extensions(
    raw: impl IntoIterator<Item = (String, serde_json::Value)>,
) -> Vec<aube_resolver::PackageExtension> {
    raw.into_iter()
        .filter_map(
            |(selector, value)| match parse_package_extension(selector, value) {
                Ok(extension) => Some(extension),
                Err(err) => {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_BUNDLED_PACKAGE_EXTENSION,
                        error = %err,
                        "ignoring malformed bundled package extension"
                    );
                    None
                }
            },
        )
        .collect()
}

fn standalone_bundled_package_extensions() -> Vec<aube_resolver::PackageExtension> {
    STANDALONE_BUNDLED_PACKAGE_EXTENSIONS
        .iter()
        .map(|extension| aube_resolver::PackageExtension {
            selector: bundled_string(extension.selector).to_owned(),
            dependencies: bundled_string_map(extension.dependencies),
            optional_dependencies: bundled_string_map(extension.optional_dependencies),
            peer_dependencies: bundled_string_map(extension.peer_dependencies),
            peer_dependencies_meta: bundled_peer_meta(extension.peer_dependencies_meta),
        })
        .collect()
}

fn bundled_string(reference: BundledStringRef) -> &'static str {
    let start = reference.0 as usize;
    &BUNDLED_STRINGS[start..start + reference.1 as usize]
}

fn bundled_string_map(reference: BundledTableRef) -> BTreeMap<String, String> {
    let start = reference.0 as usize;
    let mut map = BTreeMap::new();
    for &(name, value) in &BUNDLED_STRING_PAIRS[start..start + reference.1 as usize] {
        map.insert(
            bundled_string(name).to_owned(),
            bundled_string(value).to_owned(),
        );
    }
    map
}

fn bundled_peer_meta(reference: BundledTableRef) -> BTreeMap<String, aube_registry::PeerDepMeta> {
    let start = reference.0 as usize;
    let mut map = BTreeMap::new();
    for &(name, optional) in &BUNDLED_PEER_META[start..start + reference.1 as usize] {
        map.insert(
            bundled_string(name).to_owned(),
            aube_registry::PeerDepMeta { optional },
        );
    }
    map
}

fn parse_package_extension(
    selector: String,
    value: serde_json::Value,
) -> miette::Result<aube_resolver::PackageExtension> {
    let obj = value
        .as_object()
        .ok_or_else(|| invalid_package_extension(&selector, "entry must be an object"))?;
    let dependencies_path = format!("{selector}.dependencies");
    let optional_dependencies_path = format!("{selector}.optionalDependencies");
    let peer_dependencies_path = format!("{selector}.peerDependencies");
    let peer_dependencies_meta_path = format!("{selector}.peerDependenciesMeta");
    Ok(aube_resolver::PackageExtension {
        selector,
        dependencies: read_json_string_map(obj.get("dependencies"), &dependencies_path)?,
        optional_dependencies: read_json_string_map(
            obj.get("optionalDependencies"),
            &optional_dependencies_path,
        )?,
        peer_dependencies: read_json_string_map(
            obj.get("peerDependencies"),
            &peer_dependencies_path,
        )?,
        peer_dependencies_meta: read_peer_dependencies_meta(
            obj.get("peerDependenciesMeta"),
            &peer_dependencies_meta_path,
        )?,
    })
}

fn read_json_string_map(
    value: Option<&serde_json::Value>,
    field: &str,
) -> miette::Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let obj = value
        .as_object()
        .ok_or_else(|| invalid_package_extension(field, "field must be an object"))?;
    obj.iter()
        .map(|(name, value)| {
            let range = value.as_str().ok_or_else(|| {
                invalid_package_extension(
                    &format!("{field}.{name}"),
                    "dependency range must be a string",
                )
            })?;
            Ok((name.clone(), range.to_string()))
        })
        .collect()
}

fn read_peer_dependencies_meta(
    value: Option<&serde_json::Value>,
    field: &str,
) -> miette::Result<BTreeMap<String, aube_registry::PeerDepMeta>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let obj = value
        .as_object()
        .ok_or_else(|| invalid_package_extension(field, "field must be an object"))?;
    obj.iter()
        .map(|(name, meta)| {
            let meta = meta.as_object().ok_or_else(|| {
                invalid_package_extension(
                    &format!("{field}.{name}"),
                    "peer metadata must be an object",
                )
            })?;
            let optional = match meta.get("optional") {
                Some(value) => value.as_bool().ok_or_else(|| {
                    invalid_package_extension(
                        &format!("{field}.{name}.optional"),
                        "optional must be a boolean",
                    )
                })?,
                None => false,
            };
            Ok((name.clone(), aube_registry::PeerDepMeta { optional }))
        })
        .collect()
}

fn invalid_package_extension(path: &str, reason: &str) -> miette::Report {
    miette::miette!(
        code = aube_codes::errors::ERR_AUBE_INVALID_PACKAGE_EXTENSION,
        "invalid packageExtensions entry at {path:?}: {reason}"
    )
}

/// Apply the install-time resolver configuration that's shared between
/// the streaming main path and the `--lockfile-only` short-circuit.
/// Both paths must produce identical lockfiles, so any new resolver
/// option should land here rather than only in one branch.
///
/// Also reused by `add`/`remove`/`update`/`dedupe`/`audit` via
/// `super::build_resolver` so those commands resolve under the same
/// settings install does — historically those went through a stripped
/// `Resolver::new + with_dependency_policy` shim that silently dropped
/// `supportedArchitectures`, `resolutionMode`, `minimumReleaseAge`,
/// `autoInstallPeers`, overrides, and friends. Concrete fallout:
/// `aube update` was rewriting the lockfile with host-only optional
/// deps (collapsing `@biomejs/biome` / `rollup` platform variants) and
/// dropping `time:` entries for not-updated direct deps.
pub(crate) struct ResolverConfigInputs<'a> {
    pub(crate) settings_ctx: &'a aube_settings::ResolveCtx<'a>,
    pub(crate) workspace_config: &'a aube_manifest::workspace::WorkspaceConfig,
    pub(crate) workspace_catalogs:
        &'a std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    /// Merged `namedRegistries` alias→URL map (builtin `gh:` + global
    /// `config.yaml` + workspace yaml), from `discover_named_registries`.
    /// Empty under any non-pnpm posture, which makes the resolver's
    /// named-registry branch inert.
    pub(crate) named_registries: &'a std::collections::BTreeMap<String, String>,
    /// CLI-supplied `--minimum-release-age` override in minutes. Only
    /// `aube install` exposes the flag today; every other caller passes
    /// `None` and gets the settings-chain value.
    pub(crate) minimum_release_age_override: Option<u64>,
    /// Lockfile format aube will write on the way out, or `None` when
    /// `lockfile=false` and no lockfile will be written at all. Drives
    /// whether the resolver widens its platform filter to cover every
    /// common OS/CPU/libc combination: formats whose native tools
    /// record every optional-dep platform variant regardless of host
    /// (`Some(Aube | Pnpm | Bun | Npm)`) opt in to the wide default so
    /// aube's output matches what the native tool would have written.
    /// Yarn classic carries no per-package os/cpu metadata, so it
    /// keeps the host-only default, and `None` skips widening entirely
    /// — nothing consumes the extra resolutions. Callers compute this
    /// as `lockfile_enabled.then(|| source_kind_before.unwrap_or(Aube))`.
    pub(crate) target_lockfile_kind: Option<aube_lockfile::LockfileKind>,
    pub(crate) dependency_policy: aube_resolver::DependencyPolicy,
    /// When `true`, the resolver caches full (non-corgi) packuments on
    /// disk so the next install/update can reuse them without a
    /// round-trip. Install opts in (`true`) to amortize the cost of
    /// fetching potentially thousands of full packuments. Update /
    /// add / dedupe / audit (via `super::build_resolver`) opt out
    /// (`false`) so that re-resolving immediately after a registry
    /// dist-tag change picks up the new latest instead of serving the
    /// previous run's cached packument within its `Cache-Control`
    /// freshness window. The abbreviated cache stays on either way —
    /// it's keyed off `(name, registry)` and revalidates per request,
    /// so dist-tag drift is observed there too, but the freshness
    /// window only matters when `needs_time` routes through the full
    /// cache.
    pub(crate) cache_full_packuments: bool,
    pub(crate) ignore_scripts: bool,
}

pub(crate) fn configure_resolver(
    resolver: aube_resolver::Resolver,
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    inputs: ResolverConfigInputs<'_>,
    read_package_hook: Option<Box<dyn aube_resolver::ReadPackageHook>>,
) -> aube_resolver::Resolver {
    let ResolverConfigInputs {
        settings_ctx,
        workspace_config,
        workspace_catalogs,
        named_registries,
        minimum_release_age_override,
        target_lockfile_kind,
        dependency_policy,
        cache_full_packuments,
        ignore_scripts,
    } = inputs;
    let auto_install_peers = resolve_auto_install_peers(settings_ctx);
    let exclude_links_from_lockfile = resolve_exclude_links_from_lockfile(settings_ctx);
    let peers_suffix_max_length = resolve_peers_suffix_max_length(settings_ctx);
    let dedupe_peer_dependents = resolve_dedupe_peer_dependents(settings_ctx);
    let dedupe_peers = resolve_dedupe_peers(settings_ctx);
    let resolve_peers_from_workspace_root_opt = resolve_peers_from_workspace_root(settings_ctx);
    let registry_supports_time_field = resolve_registry_supports_time_field(settings_ctx);
    let force_metadata_primer = resolve_force_metadata_primer(settings_ctx);
    // The full funnel, not the manifest-only reader: this is a filter input
    // like every other site, so an `.npmrc`-sourced object and a `--os`/`--cpu`
    // /`--libc` flag have to reach it too. Only the non-portable-lockfile arm
    // below consumes it — the portable kinds take `accept_all` instead.
    let (sup_os, sup_cpu, sup_libc) =
        effective_supported_architectures(manifest, workspace_config, settings_ctx);
    // pnpm-lock.yaml, aube-lock.yaml, bun.lock, package-lock.json, and
    // npm-shrinkwrap.json are committed, cross-platform artifacts that
    // carry per-package os/cpu metadata.
    // Record EVERY optional-dep variant a package declares (`accept_all`) in
    // portable lockfiles, even when the user configured
    // `supportedArchitectures`. pnpm applies that setting when deciding what
    // to install, not when deciding what its lockfile records; narrowing the
    // resolver here silently removes the variants another platform needs.
    // This also matches what npm and bun write verbatim (all 26 `@esbuild/*`
    // / `@rollup/rollup-*` natives, freebsd/ppc64/s390x and all), so a
    // lockfile aube regenerates stays diff-clean against the native tool.
    // Install-time filtering (`filter_graph`) and the streaming-fetch gate
    // still use the configured architectures, so the wider lockfile costs
    // only bytes, never extra installs. Yarn lockfiles don't carry the same
    // per-package os/cpu metadata, so widening there would only bloat them.
    let writes_cross_platform_lock = matches!(
        target_lockfile_kind,
        Some(
            aube_lockfile::LockfileKind::Pnpm
                | aube_lockfile::LockfileKind::Aube
                | aube_lockfile::LockfileKind::Bun
                | aube_lockfile::LockfileKind::Npm
                | aube_lockfile::LockfileKind::NpmShrinkwrap
        )
    );
    let supported_architectures = if writes_cross_platform_lock {
        aube_resolver::SupportedArchitectures {
            accept_all: true,
            ..Default::default()
        }
    } else {
        aube_resolver::SupportedArchitectures {
            os: sup_os,
            cpu: sup_cpu,
            libc: sup_libc,
            ..Default::default()
        }
    };
    let mut effective_overrides = manifest.overrides_map();
    merge_string_map_setting(settings_ctx, "overrides", &mut effective_overrides);
    for (key, dep) in deprecated_dollar_override_refs(&effective_overrides) {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_OVERRIDE_DOLLAR_REF_DEPRECATED,
            "override {key:?} uses deprecated $ reference ${dep}; use a catalog entry instead"
        );
    }
    let unresolved_refs = manifest.resolve_override_refs(&mut effective_overrides);
    for key in &unresolved_refs {
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_OVERRIDE_MISSING_DEP,
            "override {key:?} uses a $ reference to a package that is not in \
             dependencies, devDependencies, or optionalDependencies — \
             dropping the override"
        );
    }
    if !effective_overrides.is_empty() {
        tracing::debug!("applying {} overrides", effective_overrides.len());
    }
    if !dependency_policy.package_extensions.is_empty() {
        tracing::debug!(
            "applying {} packageExtensions",
            dependency_policy.package_extensions.len()
        );
    }
    let ignored_optional =
        aube_manifest::effective_ignored_optional_dependencies(manifest, workspace_config);
    if !ignored_optional.is_empty() {
        tracing::debug!(
            "ignoring {} optional dependencies (pnpm.ignoredOptionalDependencies)",
            ignored_optional.len()
        );
    }
    let resolution_mode = resolve_resolution_mode(settings_ctx);
    let minimum_release_age =
        resolve_minimum_release_age(settings_ctx, minimum_release_age_override);
    if let Some(ref mra) = minimum_release_age {
        tracing::debug!(
            "minimumReleaseAge: {} min, {} exclude rules, strict={}",
            mra.minutes,
            mra.exclude.len(),
            mra.strict
        );
    }
    let git_shallow_hosts = resolve_git_shallow_hosts(settings_ctx);
    let packument_concurrency = resolve_network_concurrency(settings_ctx);
    let cache_dir = crate::commands::resolved_cache_dir_with_ctx(cwd, settings_ctx);
    let mut resolver = resolver
        .with_packument_network_concurrency(packument_concurrency)
        .with_packument_cache(cache_dir.join("packuments-v1"));
    if cache_full_packuments {
        resolver = resolver.with_packument_full_cache(cache_dir.join("packuments-full-v1"));
    }
    let mut resolver = resolver
        .with_auto_install_peers(auto_install_peers)
        .with_peers_suffix_max_length(peers_suffix_max_length)
        .with_exclude_links_from_lockfile(exclude_links_from_lockfile)
        .with_dedupe_peer_dependents(dedupe_peer_dependents)
        .with_dedupe_peers(dedupe_peers)
        .with_resolve_peers_from_workspace_root(resolve_peers_from_workspace_root_opt)
        .with_registry_supports_time_field(registry_supports_time_field)
        .with_force_metadata_primer(force_metadata_primer)
        .with_supported_architectures(supported_architectures)
        .with_overrides(effective_overrides)
        .with_ignored_optional_dependencies(ignored_optional)
        .with_resolution_mode(resolution_mode)
        .with_minimum_release_age(minimum_release_age)
        .with_catalogs(workspace_catalogs.clone())
        .with_named_registries(named_registries.clone())
        .with_project_root(cwd.to_path_buf())
        .with_ignore_scripts(ignore_scripts)
        .with_dependency_policy(dependency_policy)
        .with_git_shallow_hosts(git_shallow_hosts);
    if let Some(hook) = read_package_hook {
        resolver = resolver.with_read_package_hook(hook);
    }
    resolver
}

/// Check the resolved graph for declared required peer deps whose
/// version doesn't satisfy the declared range, or that aren't in the
/// tree at all. Prints the list of unmet peers and returns an `Err`
/// so the install fails.
///
/// Only called under `strict-peer-dependencies=true`. The default
/// install path skips this entirely — aube is silent about peer
/// mismatches by default, matching bun/npm/yarn. Peers that match one
/// of the `PeerDependencyRules` escape hatches (`ignoreMissing`,
/// `allowAny`, `allowedVersions`) are filtered out before the check,
/// same as pnpm.
pub(super) fn check_unmet_peers(
    graph: &aube_lockfile::LockfileGraph,
    rules: &PeerDependencyRules,
) -> miette::Result<()> {
    let unmet: Vec<_> = aube_resolver::detect_unmet_peers(graph)
        .into_iter()
        .filter(|u| !rules.silences(u))
        .collect();
    if unmet.is_empty() {
        return Ok(());
    }
    super::control::output(
        super::InstallOutputLevel::Error,
        None,
        "Issues with peer dependencies found",
    );
    for u in &unmet {
        let from_ver = version_from_dep_path(&u.from_dep_path, &u.from_name);
        let msg = match &u.found {
            Some(found) => format!(
                "  {}@{from_ver}: expected peer {}@{}, found {found}",
                u.from_name, u.peer_name, u.declared,
            ),
            None => format!(
                "  {}@{from_ver}: missing required peer {}@{}",
                u.from_name, u.peer_name, u.declared,
            ),
        };
        super::control::output(super::InstallOutputLevel::Error, None, msg);
    }
    Err(miette!(
        "{} unmet peer dependenc{} (strict-peer-dependencies is enabled)\n  \
         help: set strict-peer-dependencies=false in .npmrc to warn instead, or \
         pin the peer version via pnpm.peerDependencyRules.allowedVersions",
        unmet.len(),
        if unmet.len() == 1 { "y" } else { "ies" }
    ))
}

/// Resolved `pnpm.peerDependencyRules` — the three escape hatches pnpm
/// exposes for quieting or widening peer-dependency checks.
///
/// Sources, merged in precedence order (later sources overwrite):
/// 1. `pnpm.peerDependencyRules` in the root `package.json`
/// 2. `peerDependencyRules` in `pnpm-workspace.yaml`
/// 3. `peerDependencyRules.{ignoreMissing,allowAny,allowedVersions}` in
///    `.npmrc`
/// 4. env (`npm_config_peer_dependency_rules_*` aliases)
///
/// Glob patterns are compiled once at resolve time — malformed patterns
/// are dropped with a warning rather than failing the install, matching
/// pnpm's tolerance for config typos.
#[derive(Debug, Default)]
pub(crate) struct PeerDependencyRules {
    ignore_missing: Vec<glob::Pattern>,
    allow_any: Vec<glob::Pattern>,
    /// Keys are pnpm selectors: either a bare peer name (`react`) or a
    /// scoped `parent>peer` pair (`styled-components>react`). Values are
    /// additional semver ranges; a peer resolving inside *either* the
    /// declared range or this override is treated as satisfied.
    allowed_versions: BTreeMap<String, String>,
}

impl PeerDependencyRules {
    pub(crate) fn resolve(
        manifest: &aube_manifest::PackageJson,
        ctx: &aube_settings::ResolveCtx<'_>,
    ) -> Self {
        // Lists: package.json is the base, overridden wholesale if any
        // higher-precedence source (cli/env/npmrc/workspaceYaml) sets
        // a value. Matches pnpm's "most specific file wins" semantics
        // for list-shaped config — we never concatenate across
        // sources.
        let ignore_missing_raw = aube_settings::resolved::peer_dependency_rules_ignore_missing(ctx)
            .unwrap_or_else(|| manifest.pnpm_peer_dependency_rules_ignore_missing());
        let allow_any_raw = aube_settings::resolved::peer_dependency_rules_allow_any(ctx)
            .unwrap_or_else(|| manifest.pnpm_peer_dependency_rules_allow_any());

        // Map: package.json is the base, then workspaceYaml / npmrc /
        // env merge on top (later sources win per-key). Same shape the
        // `overrides` and `allowedDeprecatedVersions` settings use.
        let mut allowed_versions = manifest.pnpm_peer_dependency_rules_allowed_versions();
        merge_string_map_setting(
            ctx,
            "peerDependencyRules.allowedVersions",
            &mut allowed_versions,
        );

        Self {
            ignore_missing: compile_peer_patterns("ignoreMissing", &ignore_missing_raw),
            allow_any: compile_peer_patterns("allowAny", &allow_any_raw),
            allowed_versions,
        }
    }

    /// True when an `UnmetPeer` should be suppressed from warn/error
    /// output because one of the three rules covers it.
    pub(crate) fn silences(&self, u: &aube_resolver::UnmetPeer) -> bool {
        if u.found.is_none() && self.ignore_missing.iter().any(|p| p.matches(&u.peer_name)) {
            return true;
        }
        if self.allow_any.iter().any(|p| p.matches(&u.peer_name)) {
            return true;
        }
        if let Some(found) = u.found.as_deref()
            && self.allowed_versions_permit(&u.from_name, &u.peer_name, found)
        {
            return true;
        }
        false
    }

    fn allowed_versions_permit(&self, parent: &str, peer: &str, found: &str) -> bool {
        let scoped_key = format!("{parent}>{peer}");
        let candidates = [
            self.allowed_versions.get(&scoped_key),
            self.allowed_versions.get(peer),
        ];
        let Ok(found_v) = node_semver::Version::parse(found) else {
            return false;
        };
        candidates
            .into_iter()
            .flatten()
            .any(|range| matches_range(range, &found_v))
    }
}

fn matches_range(range: &str, found: &node_semver::Version) -> bool {
    match node_semver::Range::parse(range) {
        Ok(r) => r.satisfies(found),
        Err(_) => false,
    }
}

fn compile_peer_patterns(field: &str, raw: &[String]) -> Vec<glob::Pattern> {
    raw.iter()
        .filter_map(|p| match glob::Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(err) => {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_INVALID_PEER_PATTERN,
                    "ignoring invalid peerDependencyRules.{field} pattern {p:?}: {err}"
                );
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod bundled_compat_tests {
    use super::*;

    #[test]
    fn the_catalog_carries_the_rule_an_embedder_gate_used_to_withhold() {
        // Until this gate came off, the vendored catalogs applied only when the
        // embedder was standalone aube, so every embedder resolved a smaller
        // graph than pnpm for the same project. `reactcss` is the cheapest
        // demonstration: it requires `react` and declares it nowhere, so without
        // this rule the peer is never added, `auto-install-peers` has nothing to
        // install, and the package fails at require time with `Cannot find
        // module 'react'` — under a tool whose whole claim is pnpm parity.
        let extensions = standalone_bundled_package_extensions();
        let reactcss = extensions
            .iter()
            .find(|extension| extension.selector == "reactcss@*")
            .expect("the bundled catalog must carry reactcss@*");
        assert_eq!(
            reactcss.peer_dependencies["react"], "*",
            "reactcss must gain react as a peer for auto-install-peers to supply it"
        );
    }

    #[test]
    fn catalog_matches_curated_upstreams_and_preserves_order() {
        let extensions = standalone_bundled_package_extensions();

        assert_eq!(extensions.len(), 161);
        let extension = |selector: &str| {
            extensions
                .iter()
                .find(|extension| extension.selector == selector)
                .unwrap_or_else(|| panic!("missing bundled selector {selector}"))
        };
        assert_eq!(
            extension("@angular/build@*").dependencies["tslib"],
            "^2.3.0"
        );
        assert_eq!(
            extension("@nuxt/vite-builder@>=4.5.0").dependencies["unplugin"],
            "^3.3.0"
        );
        let selector_position = |selector: &str| {
            extensions
                .iter()
                .position(|extension| extension.selector == selector)
                .unwrap_or_else(|| panic!("missing bundled selector {selector}"))
        };
        assert!(
            selector_position("consolidate@<=0.16.0") < selector_position("consolidate@<0.16.0")
        );
    }

    #[test]
    fn catalog_does_not_inject_type_only_or_singleton_runtimes() {
        // These are the bare runtime targets the reverted scanner rules
        // injected. Their `@types/*` counterparts are legitimate packages and
        // intentionally remain allowed in curated repairs.
        let extensions = standalone_bundled_package_extensions();
        for target in ["estree", "typescript", "react", "eslint"] {
            for extension in &extensions {
                assert!(
                    !extension.dependencies.contains_key(target),
                    "{} must not inject {target}",
                    extension.selector
                );
            }
        }
    }

    #[test]
    fn malformed_bundled_entry_does_not_discard_valid_entries() {
        let raw = vec![
            ("broken@*".to_string(), serde_json::Value::Null),
            (
                "valid@*".to_string(),
                serde_json::json!({"dependencies": {"left-pad": "^1.3.0"}}),
            ),
        ];

        let parsed = parse_bundled_package_extensions(raw);

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].selector, "valid@*");
        assert_eq!(parsed[0].dependencies["left-pad"], "^1.3.0");
    }

    #[test]
    fn ignore_compatibility_db_skips_bundled_but_keeps_project_extensions() {
        let npmrc = [("ignoreCompatibilityDb".to_string(), "true".to_string())];
        let workspace = BTreeMap::new();
        let ctx = aube_settings::ResolveCtx::files_only(&npmrc, &workspace);
        let manifest = serde_json::from_value(serde_json::json!({
            "pnpm": {
                "packageExtensions": {
                    "project-extension@*": {"dependencies": {"left-pad": "^1.3.0"}}
                }
            }
        }))
        .expect("test manifest should parse");

        let policy =
            resolve_dependency_policy(&manifest, &ctx).expect("test policy should resolve");

        assert_eq!(policy.package_extensions.len(), 1);
        assert_eq!(policy.package_extensions[0].selector, "project-extension@*");
    }
}

#[cfg(test)]
mod resolution_mode_tests {
    use super::*;

    #[test]
    fn lowest_direct_maps_to_the_public_resolver_mode() {
        assert_eq!(
            parse_resolution_mode("lowest-direct"),
            Some(aube_resolver::ResolutionMode::LowestDirect)
        );
    }

    #[test]
    fn time_alias_keeps_time_based_mode() {
        assert_eq!(
            parse_resolution_mode("time"),
            Some(aube_resolver::ResolutionMode::TimeBased)
        );
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    #[test]
    fn deprecated_dollar_override_refs_reports_only_ref_values() {
        let overrides = BTreeMap::from([
            ("left-pad".to_string(), "$left-pad".to_string()),
            ("react".to_string(), "^19.0.0".to_string()),
            ("empty".to_string(), "$".to_string()),
        ]);

        assert_eq!(
            deprecated_dollar_override_refs(&overrides),
            vec![("left-pad", "left-pad")]
        );
    }
}

#[cfg(test)]
mod yarn_package_extensions_tests {
    use super::*;

    // End-to-end for the Yarn `packageExtensions` route. The registry layer
    // translates a `.yarnrc.yml` `packageExtensions:` block into a single
    // `("packageExtensions", <json-object-string>)` settings entry (covered by
    // the translator's own unit test in aube-registry). This test starts from
    // that exact entry shape and asserts it flows through the SAME
    // object-setting parser pnpm uses, reaching the resolver's
    // `PackageExtension` model — proving the field is wired all the way
    // through to `resolve_dependency_policy`, not merely parsed in isolation.
    #[test]
    fn yarnrc_package_extensions_reach_the_dependency_policy() {
        // Byte-for-byte the entry the Yarn translator emits for the block:
        //   packageExtensions:
        //     "is-even@*":
        //       dependencies: { is-odd: "^1.0.0" }
        //       peerDependencies: { react: "*" }
        //       peerDependenciesMeta: { react: { optional: true } }
        let yarnrc_entries = vec![(
            "packageExtensions".to_string(),
            r#"{"is-even@*":{"dependencies":{"is-odd":"^1.0.0"},"peerDependencies":{"react":"*"},"peerDependenciesMeta":{"react":{"optional":true}}}}"#
                .to_string(),
        )];

        let workspace_yaml = std::collections::BTreeMap::new();
        let ctx = aube_settings::ResolveCtx::files_only(&yarnrc_entries, &workspace_yaml);
        let manifest = aube_manifest::PackageJson::default();
        let policy = resolve_dependency_policy(&manifest, &ctx).unwrap();

        let ext = policy
            .package_extensions
            .iter()
            .find(|e| e.selector == "is-even@*")
            .expect("Yarn packageExtensions selector must reach the resolver policy");
        assert_eq!(ext.dependencies.get("is-odd").unwrap(), "^1.0.0");
        assert_eq!(ext.peer_dependencies.get("react").unwrap(), "*");
        assert!(ext.peer_dependencies_meta.get("react").unwrap().optional);
        // Yarn's schema has no optionalDependencies in packageExtensions, so
        // the parser leaves that map empty rather than inventing entries.
        assert!(ext.optional_dependencies.is_empty());
    }
}

#[cfg(test)]
mod network_concurrency_tests {
    use super::*;

    #[test]
    fn dynamic_default_matches_pnpm_worker_clamp() {
        assert_eq!(network_concurrency_for_workers(1), 16);
        assert_eq!(network_concurrency_for_workers(8), 24);
        assert_eq!(network_concurrency_for_workers(24), 72);
        assert_eq!(network_concurrency_for_workers(64), 128);
        assert_eq!(network_concurrency_for_workers(usize::MAX), 128);
    }
}

// These ran `unix`-only, which is what let the Windows half of the
// detector stay broken (nub#566): on Windows the real-dir arm never
// fired, `all_real_dirs_reads_as_per_project` was the test that would
// have caught it, and it was compiled out. Every entry is now built
// through `create_dir_link` / `create_dir_all`, so the same three cases
// assert against a real junction on Windows and a real symlink on Unix.
#[cfg(test)]
mod gvs_mode_detect_tests {
    use super::*;

    // A link target that does not exist — the detector classifies by the
    // entry's own shape and must not care whether the target resolves.
    fn dangling_link(store_leaf: &str, at: std::path::PathBuf) {
        let target = at
            .parent()
            .expect("entry has a parent")
            .join("nonexistent-store")
            .join(store_leaf);
        aube_linker::sys::create_dir_link(&target, &at).unwrap();
    }

    // `diskMaterializePackages` produces a MIXED `.aube` tree — some entries
    // are shared-store links, some are disk-materialized real dirs. The
    // detector must classify such a tree as GVS-on regardless of `read_dir`
    // order; a forced real dir landing first must NOT misread it as per-project
    // (which would wipe node_modules on every install).
    #[test]
    fn mixed_tree_with_any_link_reads_as_gvs() {
        let dir = tempfile::tempdir().unwrap();
        let aube = dir.path();
        std::fs::create_dir_all(aube.join("real@1.0.0/node_modules/real")).unwrap();
        dangling_link("dep@2.0.0", aube.join("dep@2.0.0"));
        assert_eq!(detect_aube_dir_gvs_mode(aube), Some(true));
    }

    #[test]
    fn all_real_dirs_reads_as_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let aube = dir.path();
        std::fs::create_dir_all(aube.join("a@1.0.0/node_modules/a")).unwrap();
        std::fs::create_dir_all(aube.join("b@1.0.0/node_modules/b")).unwrap();
        assert_eq!(detect_aube_dir_gvs_mode(aube), Some(false));
    }

    #[test]
    fn all_links_reads_as_gvs() {
        let dir = tempfile::tempdir().unwrap();
        let aube = dir.path();
        dangling_link("a@1.0.0", aube.join("a@1.0.0"));
        dangling_link("b@1.0.0", aube.join("b@1.0.0"));
        assert_eq!(detect_aube_dir_gvs_mode(aube), Some(true));
    }
}

#[cfg(test)]
mod peer_dependency_rules_tests {
    use super::*;

    fn unmet(
        parent: &str,
        peer: &str,
        declared: &str,
        found: Option<&str>,
    ) -> aube_resolver::UnmetPeer {
        aube_resolver::UnmetPeer {
            from_dep_path: format!("{parent}@0.0.0"),
            from_name: parent.to_string(),
            peer_name: peer.to_string(),
            declared: declared.to_string(),
            found: found.map(String::from),
        }
    }

    fn rules(
        ignore_missing: &[&str],
        allow_any: &[&str],
        allowed_versions: &[(&str, &str)],
    ) -> PeerDependencyRules {
        PeerDependencyRules {
            ignore_missing: ignore_missing
                .iter()
                .map(|p| glob::Pattern::new(p).unwrap())
                .collect(),
            allow_any: allow_any
                .iter()
                .map(|p| glob::Pattern::new(p).unwrap())
                .collect(),
            allowed_versions: allowed_versions
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    #[test]
    fn ignore_missing_silences_only_missing_matches() {
        let r = rules(&["react*"], &[], &[]);
        assert!(r.silences(&unmet("parent", "react", "^18.0.0", None)));
        assert!(r.silences(&unmet("parent", "react-dom", "^18.0.0", None)));
        // present-but-wrong-version is NOT silenced by ignore_missing.
        assert!(!r.silences(&unmet("parent", "react", "^18.0.0", Some("19.0.0"))));
        // Non-matching name is not silenced.
        assert!(!r.silences(&unmet("parent", "vue", "^3.0.0", None)));
    }

    #[test]
    fn allow_any_silences_both_missing_and_wrong_version() {
        let r = rules(&[], &["react"], &[]);
        assert!(r.silences(&unmet("parent", "react", "^18.0.0", None)));
        assert!(r.silences(&unmet("parent", "react", "^18.0.0", Some("19.0.0"))));
        assert!(!r.silences(&unmet("parent", "vue", "^3.0.0", Some("2.0.0"))));
    }

    #[test]
    fn allowed_versions_bare_key_widens_range_regardless_of_parent() {
        let r = rules(&[], &[], &[("react", "^19.0.0")]);
        assert!(r.silences(&unmet(
            "styled-components",
            "react",
            "^18.0.0",
            Some("19.0.0")
        )));
        assert!(r.silences(&unmet("other-lib", "react", "^18.0.0", Some("19.5.0"))));
        // Found outside both the declared range AND the override — still fires.
        assert!(!r.silences(&unmet("lib", "react", "^18.0.0", Some("20.0.0"))));
        // Missing peers are not silenced by allowed_versions.
        assert!(!r.silences(&unmet("lib", "react", "^18.0.0", None)));
    }

    #[test]
    fn allowed_versions_scoped_key_only_matches_named_parent() {
        let r = rules(&[], &[], &[("styled-components>react", "^19.0.0")]);
        assert!(r.silences(&unmet(
            "styled-components",
            "react",
            "^18.0.0",
            Some("19.0.0")
        )));
        // Different parent — not silenced.
        assert!(!r.silences(&unmet("other-lib", "react", "^18.0.0", Some("19.0.0"))));
    }

    #[test]
    fn invalid_override_range_does_not_silence() {
        // A malformed range in allowedVersions falls through to "no
        // match" rather than panicking or silencing everything.
        let r = rules(&[], &[], &[("react", "not-a-range")]);
        assert!(!r.silences(&unmet("parent", "react", "^18.0.0", Some("19.0.0"))));
    }
}

#[cfg(test)]
mod finalize_lockfile_graph_tests {
    use super::*;

    fn node_available() -> bool {
        std::process::Command::new(crate::runtime::internal_node_program())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn manifest() -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some("x".to_string()),
            version: Some("1.0.0".to_string()),
            ..Default::default()
        }
    }

    /// Regression for `aube upgrade`/`dedupe`/`remove`/`audit` dropping
    /// `packageExtensionsChecksum`: every command that rewrites a
    /// pnpm-lock.yaml must stamp the checksum just like `aube install`.
    #[tokio::test]
    async fn finalize_stamps_package_extensions_checksum_on_pnpm_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(cwd.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"name":"x","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join("pnpm-workspace.yaml"),
            "packageExtensions:\n  foo@*:\n    dependencies:\n      bar: 1.0.0\n",
        )
        .unwrap();

        let mut graph = aube_lockfile::LockfileGraph::default();
        assert!(graph.package_extensions_checksum.is_none());
        // ignore_pnpmfile=true keeps this assertion node-free.
        finalize_lockfile_graph(cwd, &mut graph, &manifest(), true, None)
            .await
            .unwrap();
        assert!(
            graph.package_extensions_checksum.is_some(),
            "packageExtensions checksum must be stamped on pnpm-lock writes"
        );
    }

    /// The generic (`Aube`) lockfile grows `packageExtensionsChecksum` ONLY
    /// when the embedder enforces the checksum (nub). Standalone aube leaves
    /// the posture off, so its aube-lock.yaml never grows the pnpm-only field
    /// — the byte-for-byte default. `pnpmfileChecksum` stays pnpm-lock-only
    /// under either posture. Both branches run in one test fn (with a
    /// save/restore of the process-global posture) so the enforce window can't
    /// race the sibling pnpm-lock stamp tests.
    #[tokio::test]
    async fn finalize_stamps_generic_lock_checksum_only_when_embedder_enforces() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(cwd.join("aube-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"name":"x","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join("pnpm-workspace.yaml"),
            "packageExtensions:\n  foo@*:\n    dependencies:\n      bar: 1.0.0\n",
        )
        .unwrap();

        // Default posture (standalone aube): no checksum on aube-lock.yaml.
        let mut graph = aube_lockfile::LockfileGraph::default();
        finalize_lockfile_graph(cwd, &mut graph, &manifest(), false, None)
            .await
            .unwrap();
        assert!(
            graph.package_extensions_checksum.is_none(),
            "standalone aube must not stamp packageExtensionsChecksum on aube-lock.yaml"
        );
        assert!(graph.pnpmfile_checksum.is_none());

        // Enforcing embedder (nub): the generic lockfile grows the checksum,
        // but never the pnpmfile checksum.
        aube_util::update_engine_context(|c| c.enforce_package_extensions_checksum = true);
        let mut graph = aube_lockfile::LockfileGraph::default();
        finalize_lockfile_graph(cwd, &mut graph, &manifest(), false, None)
            .await
            .unwrap();
        aube_util::update_engine_context(|c| c.enforce_package_extensions_checksum = false);
        assert!(
            graph.package_extensions_checksum.is_some(),
            "an enforcing embedder must stamp packageExtensionsChecksum on its generic lockfile"
        );
        assert!(
            graph.pnpmfile_checksum.is_none(),
            "pnpmfileChecksum stays pnpm-lock-only even under an enforcing embedder"
        );
    }

    /// The pnpmfile half of the same regression: a local pnpmfile that
    /// exports hooks gets its `pnpmfileChecksum` recorded on a pnpm-lock
    /// rewrite (matching pnpm + a fresh `aube install`).
    // The pnpmfile default-gate lock is a std mutex held across the finalize
    // await on purpose: this test binary is single-threaded per `.cargo/config`
    // and the gate must stay held for the whole detect+finalize sequence.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn finalize_stamps_pnpmfile_checksum_on_pnpm_lock() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        // Resolving `local_pnpmfile` reaches the cwd-default arm of
        // `pnpmfile::detect`, whose gate is a process-global shared with
        // pnpmfile's own tests across this one lib test binary.
        let _lock = crate::pnpmfile::default_gate_lock();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        std::fs::write(cwd.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"name":"x","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".pnpmfile.cjs"),
            "module.exports = { hooks: { readPackage: (pkg) => pkg } }\n",
        )
        .unwrap();

        let mut graph = aube_lockfile::LockfileGraph::default();
        finalize_lockfile_graph(cwd, &mut graph, &manifest(), false, None)
            .await
            .unwrap();
        assert!(
            graph.pnpmfile_checksum.is_some(),
            "pnpmfile checksum must be stamped when a hook-exporting pnpmfile is present"
        );

        // --ignore-pnpmfile clears it, matching pnpm.
        let mut ignored = aube_lockfile::LockfileGraph::default();
        finalize_lockfile_graph(cwd, &mut ignored, &manifest(), true, None)
            .await
            .unwrap();
        assert!(
            ignored.pnpmfile_checksum.is_none(),
            "--ignore-pnpmfile must not record a pnpmfile checksum"
        );
    }
}
