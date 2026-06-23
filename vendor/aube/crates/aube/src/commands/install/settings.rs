use super::super::{packument_cache_dir, packument_full_cache_dir};
use super::version_from_dep_path;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;

/// Accept pnpm's documented aliases (`highest`, `time-based`, `time`,
/// `lowest-direct`) and map them to our enum. Unknown values fall back
/// to `None` so the caller's `.npmrc` / default path still runs.
fn parse_resolution_mode(s: &str) -> Option<aube_resolver::ResolutionMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "highest" => Some(aube_resolver::ResolutionMode::Highest),
        // pnpm treats `lowest-direct` and `time-based` as distinct
        // modes; aube folds them into `TimeBased` and skips the cutoff
        // filter when there's no publish time to compare against, so
        // `lowest-direct` behavior emerges naturally from `TimeBased`
        // with `time:` absent. Close enough for the first pass.
        "time-based" | "time" | "lowest-direct" => Some(aube_resolver::ResolutionMode::TimeBased),
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
/// resolver's runtime enum. pnpm treats `lowest-direct` and
/// `time-based` as distinct modes, but aube folds them into
/// `TimeBased` and lets the `time:` cutoff filter handle the
/// difference — when publish times are missing the `lowest-direct`
/// behavior emerges naturally. Close enough for the first pass.
fn map_resolution_mode(
    m: aube_settings::resolved::ResolutionMode,
) -> aube_resolver::ResolutionMode {
    match m {
        aube_settings::resolved::ResolutionMode::Highest => aube_resolver::ResolutionMode::Highest,
        aube_settings::resolved::ResolutionMode::TimeBased
        | aube_settings::resolved::ResolutionMode::LowestDirect => {
            aube_resolver::ResolutionMode::TimeBased
        }
    }
}

/// Resolve the effective `minimumReleaseAge` configuration from a
/// pre-built resolve context. Every lookup goes through the
/// build-time-generated typed accessors in `aube_settings::resolved`
/// — `.npmrc` first, then `pnpm-workspace.yaml`. CLI override
/// (currently always `None`, no flag yet) wins over both.
fn resolve_minimum_release_age(
    ctx: &aube_settings::ResolveCtx<'_>,
    cli_minutes: Option<u64>,
) -> Option<aube_resolver::MinimumReleaseAge> {
    let minutes = cli_minutes.unwrap_or_else(|| aube_settings::resolved::minimum_release_age(ctx));
    if minutes == 0 {
        return None;
    }
    let exclude: std::collections::HashSet<String> =
        aube_settings::resolved::minimum_release_age_exclude(ctx)
            .unwrap_or_default()
            .into_iter()
            .collect();
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
    manifests: &[(String, aube_manifest::PackageJson)],
    triggers: &'a [String],
) -> Option<&'a str> {
    for (_, m) in manifests {
        for name in triggers {
            if m.dependencies.contains_key(name)
                || m.dev_dependencies.contains_key(name)
                || m.optional_dependencies.contains_key(name)
            {
                return Some(name.as_str());
            }
        }
    }
    None
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
/// Assumes a consistent `.aube/` tree (every entry the same type),
/// which is what a successful install produces. A crash mid-link
/// during a transition could leave a mixed tree; we classify from the
/// first entry `read_dir` yields and let the next install self-heal
/// — worst case is one extra wipe, which is identical to the cost of
/// the transition we're already handling.
pub(super) fn detect_aube_dir_gvs_mode(aube_dir: &std::path::Path) -> Option<bool> {
    let entries = std::fs::read_dir(aube_dir).ok()?;
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
        // Classify via `read_link`, not `file_type().is_symlink()`.
        // On Windows, `sys::create_dir_link` produces an NTFS junction
        // whose `is_symlink()` is `false` and `is_dir()` is `true`,
        // making a gvs-on entry indistinguishable from a per-project
        // real directory via the file-type bit. `read_link` succeeds on
        // both Unix symlinks and Windows junction reparse points, and
        // returns `Err(InvalidInput)` on a regular directory — exactly
        // the signal we need. Non-link IO errors just skip the entry
        // and move on to the next candidate.
        match std::fs::read_link(entry.path()) {
            Ok(_) => return Some(true),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => return Some(false),
            Err(_) => continue,
        }
    }
    None
}

/// Honor `cleanupUnusedCatalogs` by pruning declared-but-unreferenced
/// catalog entries from the workspace yaml. No-op when the setting is
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
    if !aube_settings::resolved::cleanup_unused_catalogs(ctx) {
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
            "cleanupUnusedCatalogs: pruned {} from {filename}",
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
) -> aube_resolver::DependencyPolicy {
    let mut policy = aube_resolver::DependencyPolicy::default();

    let package_extensions = effective_package_extensions(manifest, ctx);
    policy.package_extensions = parse_package_extensions(package_extensions);

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
    policy.trust_policy_ignore_after = aube_settings::resolved::trust_policy_ignore_after(ctx);
    policy.block_exotic_subdeps = aube_settings::resolved::block_exotic_subdeps(ctx);

    policy
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
    (os, cpu, libc)
}

/// Stamp pnpm's `packageExtensionsChecksum` / `pnpmfileChecksum` onto
/// `graph` so a written pnpm-lock.yaml matches what pnpm itself records,
/// keeping config-drift detection in sync (a wrong/absent value makes
/// pnpm re-resolve, or abort a frozen install). No-op for non-pnpm
/// lockfiles: aube-lock.yaml shares the writer and must not grow
/// pnpm-only fields.
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
    if !matches!(write_kind, aube_lockfile::LockfileKind::Pnpm) {
        return;
    }
    let package_extensions = effective_package_extensions(manifest, ctx);
    graph.package_extensions_checksum =
        aube_lockfile::pnpm::package_extensions_checksum(&package_extensions);

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
    let write_kind = aube_lockfile::detect_existing_lockfile_kind(cwd)
        .unwrap_or(aube_lockfile::LockfileKind::Aube);
    let files = crate::commands::FileSources::load(cwd);
    let (ws_config, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config for lockfile finalization")?;
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
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

fn json_string_map(map: BTreeMap<String, serde_json::Value>) -> BTreeMap<String, String> {
    map.into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect()
}

fn parse_package_extensions(
    raw: BTreeMap<String, serde_json::Value>,
) -> Vec<aube_resolver::PackageExtension> {
    raw.into_iter()
        .filter_map(|(selector, value)| {
            let obj = value.as_object()?;
            Some(aube_resolver::PackageExtension {
                selector,
                dependencies: read_json_string_map(obj.get("dependencies")),
                optional_dependencies: read_json_string_map(obj.get("optionalDependencies")),
                peer_dependencies: read_json_string_map(obj.get("peerDependencies")),
                peer_dependencies_meta: read_peer_dependencies_meta(
                    obj.get("peerDependenciesMeta"),
                ),
            })
        })
        .collect()
}

fn read_json_string_map(value: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn read_peer_dependencies_meta(
    value: Option<&serde_json::Value>,
) -> BTreeMap<String, aube_registry::PeerDepMeta> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(name, meta)| {
                    let optional = meta
                        .as_object()
                        .and_then(|m| m.get("optional"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    (name.clone(), aube_registry::PeerDepMeta { optional })
                })
                .collect()
        })
        .unwrap_or_default()
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
        minimum_release_age_override,
        target_lockfile_kind,
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
    let (sup_os, sup_cpu, sup_libc) =
        effective_supported_architectures(manifest, workspace_config, settings_ctx);
    // pnpm-lock.yaml, aube-lock.yaml, bun.lock, and package-lock.json are
    // all committed, cross-platform artifacts that carry per-package os/cpu
    // metadata. When the user hasn't declared
    // `pnpm.supportedArchitectures`, record EVERY optional-dep variant a
    // package declares (`accept_all`) so the committed lockfile installs
    // cleanly on every contributor's platform — withholding variants leaves
    // teammates with "Cannot find native binding". This matches what pnpm
    // AND bun both write verbatim (all 26 `@esbuild/*` / `@rollup/rollup-*`
    // natives, freebsd/ppc64/s390x and all), so a lockfile aube regenerates
    // stays diff-clean against the native tool. For package-lock.json the
    // stakes are higher still: a platform-mismatched *root* optional
    // dependency (fsevents on Linux) missing from the lockfile makes
    // `npm ci` refuse the whole install with EUSAGE "Missing:
    // fsevents@x.y.z from lock file", so npm is widened too. Install-time
    // filtering (`filter_graph`) and the streaming-fetch gate run against
    // the unmodified host triple, so `node_modules` and tarball downloads
    // stay trimmed to the host — the wider lockfile costs only bytes, never
    // extra installs. Yarn classic lockfiles have no per-package os/cpu
    // metadata, so widening there would only bloat them — keep pnpm's
    // host-only default.
    let manifest_set_supported_arch =
        !(sup_os.is_empty() && sup_cpu.is_empty() && sup_libc.is_empty());
    let writes_cross_platform_lock = matches!(
        target_lockfile_kind,
        Some(
            aube_lockfile::LockfileKind::Pnpm
                | aube_lockfile::LockfileKind::Aube
                | aube_lockfile::LockfileKind::Bun
                | aube_lockfile::LockfileKind::Npm
        )
    );
    let supported_architectures = if manifest_set_supported_arch {
        aube_resolver::SupportedArchitectures {
            os: sup_os,
            cpu: sup_cpu,
            libc: sup_libc,
            ..Default::default()
        }
    } else if writes_cross_platform_lock {
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
    let dependency_policy = resolve_dependency_policy(manifest, settings_ctx);
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
            "minimumReleaseAge: {} min, {} excluded, strict={}",
            mra.minutes,
            mra.exclude.len(),
            mra.strict
        );
    }
    let git_shallow_hosts = resolve_git_shallow_hosts(settings_ctx);
    let packument_concurrency = resolve_network_concurrency(settings_ctx);
    let mut resolver = resolver
        .with_packument_network_concurrency(packument_concurrency)
        .with_packument_cache(packument_cache_dir());
    if cache_full_packuments {
        resolver = resolver.with_packument_full_cache(packument_full_cache_dir());
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
    // Called from install flow after resolver, before linker phase.
    // Progress bar is active at this point. Raw eprintln smears
    // across bar frames. Route through safe_eprintln.
    crate::progress::safe_eprintln("error: Issues with peer dependencies found");
    for u in &unmet {
        let from_ver = version_from_dep_path(&u.from_dep_path, &u.from_name);
        let msg = match &u.found {
            Some(found) => format!(
                "error:   {}@{from_ver}: expected peer {}@{}, found {found}",
                u.from_name, u.peer_name, u.declared,
            ),
            None => format!(
                "error:   {}@{from_ver}: missing required peer {}@{}",
                u.from_name, u.peer_name, u.declared,
            ),
        };
        crate::progress::safe_eprintln(&msg);
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
        let policy = resolve_dependency_policy(&manifest, &ctx);

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
        std::process::Command::new(crate::runtime::node_program())
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

    /// aube-lock.yaml must never grow pnpm-only checksum fields — the
    /// stamp is a no-op when no pnpm lockfile is present.
    #[tokio::test]
    async fn finalize_skips_checksums_on_aube_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
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
        finalize_lockfile_graph(cwd, &mut graph, &manifest(), false, None)
            .await
            .unwrap();
        assert!(
            graph.package_extensions_checksum.is_none(),
            "aube-lock.yaml must not grow pnpm-only checksum fields"
        );
        assert!(graph.pnpmfile_checksum.is_none());
    }

    /// The pnpmfile half of the same regression: a local pnpmfile that
    /// exports hooks gets its `pnpmfileChecksum` recorded on a pnpm-lock
    /// rewrite (matching pnpm + a fresh `aube install`).
    #[tokio::test]
    async fn finalize_stamps_pnpmfile_checksum_on_pnpm_lock() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
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
