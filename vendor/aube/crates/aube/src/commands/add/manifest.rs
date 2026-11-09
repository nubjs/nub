use super::AddArgs;
use super::spec::{ParsedPkgSpec, parse_pkg_spec};
use crate::commands::catalogs::{
    CatalogRewrite, CatalogUpsert, decide_add_rewrite, range_compatible,
};
use aube_settings::resolved::CatalogMode;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone)]
pub(super) struct AddManifestOptions {
    pub(super) save_dev: bool,
    pub(super) save_exact: bool,
    pub(super) save_optional: bool,
    pub(super) save_peer: bool,
    /// `None` follows the `saveTypes` setting.
    pub(super) save_types: Option<bool>,
    pub(super) network_mode: aube_registry::NetworkMode,
    /// Target catalog for `--save-catalog` / `--save-catalog-name`.
    /// `None` means neither flag was passed and the catalog yaml is
    /// left untouched. `Some("default")` is `--save-catalog`;
    /// `Some(other)` is `--save-catalog-name=<other>`.
    pub(super) save_catalog: Option<String>,
    /// `--save-workspace-protocol` / `--no-save-workspace-protocol`
    /// per-invocation override. `None` defers to the resolved
    /// `saveWorkspaceProtocol` setting; `Some(true)` forces the
    /// `workspace:` form regardless of the setting; `Some(false)`
    /// forces a registry-style spec even when `linkWorkspacePackages`
    /// matched a sibling.
    pub(super) workspace_protocol_override: Option<bool>,
    /// Highest-precedence settings supplied by an embedding host.
    pub(super) setting_overrides: Vec<(String, String)>,
}

impl AddManifestOptions {
    pub(super) fn from_args(args: &AddArgs) -> Self {
        Self {
            save_dev: args.save_dev,
            save_exact: args.save_exact,
            save_optional: args.save_optional,
            save_peer: args.save_peer,
            save_types: types_flag(args.save_types, args.no_save_types),
            network_mode: aube_registry::NetworkMode::Online,
            save_catalog: args.save_catalog_name.clone().or_else(|| {
                if args.save_catalog {
                    Some("default".to_string())
                } else {
                    None
                }
            }),
            workspace_protocol_override: workspace_protocol_override_from_flags(
                args.save_workspace_protocol,
                args.no_save_workspace_protocol,
            ),
            setting_overrides: Vec::new(),
        }
    }
}

fn package_not_found_error_for_registry(
    client: &aube_registry::client::RegistryClient,
    name: String,
) -> miette::Report {
    let corpus = aube_resolver::popular_package_names_are_ranked()
        .then(aube_resolver::popular_package_names);
    package_not_found_error_for_registry_with_corpus(client, name, corpus)
}

fn package_not_found_error_for_registry_with_corpus(
    client: &aube_registry::client::RegistryClient,
    name: String,
    public_npm_corpus: Option<&str>,
) -> miette::Report {
    let corpus = client
        .uses_default_npm_registry_for(&name)
        .then_some(public_npm_corpus)
        .flatten();
    crate::commands::add_supply_chain::package_not_found_error_with_corpus(name, corpus)
}

/// Map the paired `--save-workspace-protocol` / `--no-save-workspace-protocol`
/// flags to a tri-state. `clap`'s `overrides_with` ensures only the
/// last-typed flag survives, so at most one of the two is `true` at a
/// time and we don't need to disambiguate. Takes the two flag bools
/// directly so the call site in `run()` (which destructures `AddArgs`
/// into locals) and `from_args` can share the logic without going
/// back through the struct.
pub(super) fn workspace_protocol_override_from_flags(save: bool, no_save: bool) -> Option<bool> {
    if save {
        Some(true)
    } else if no_save {
        Some(false)
    } else {
        None
    }
}

/// Scrub `name` from every dependency section, then write `specifier`
/// into the section(s) the `--save-*` flags select. Shared by all four
/// manifest-write paths (registry, linked-workspace, git, file/link)
/// so the section-routing rules live in exactly one place.
///
/// `--save-peer` matches pnpm: the package lands in BOTH
/// `peerDependencies` (the downstream contract) AND `devDependencies`
/// (so the local project actually installs it) — pnpm's
/// `getSaveType` maps `savePeer` to `devDependencies` and writes the
/// peer entry on top, regardless of whether `--save-dev` was also
/// passed. The peer section is therefore not scrubbed when
/// `--save-peer` is set, and the dev section is scrubbed only when we
/// are not about to re-add to it.
fn place_dep_in_manifest(
    manifest: &mut aube_manifest::PackageJson,
    name: &str,
    specifier: String,
    opts: &AddManifestOptions,
) {
    manifest.dependencies.remove(name);
    manifest.optional_dependencies.remove(name);
    if !opts.save_peer {
        manifest.peer_dependencies.remove(name);
    }
    if !opts.save_peer && !opts.save_dev {
        manifest.dev_dependencies.remove(name);
    }

    let dep_name = name.to_string();
    if opts.save_peer {
        manifest
            .peer_dependencies
            .insert(dep_name.clone(), specifier.clone());
        manifest.dev_dependencies.insert(dep_name, specifier);
    } else if opts.save_dev {
        manifest.dev_dependencies.insert(dep_name, specifier);
    } else if opts.save_optional {
        manifest.optional_dependencies.insert(dep_name, specifier);
    } else {
        manifest.dependencies.insert(dep_name, specifier);
    }
}

/// `--save-types` / `--no-save-types`, or `None` to follow the setting.
pub(super) fn types_flag(save_types: bool, no_save_types: bool) -> Option<bool> {
    match (save_types, no_save_types) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

/// The name a local spec's package declares. pnpm saves a local dependency
/// under this; the path's basename is only the fallback for a package with
/// no readable name. `cwd` is the directory the spec is relative to.
fn local_package_name(cwd: &Path, spec: &str) -> Option<String> {
    let path = spec
        .strip_prefix("file:")
        .or_else(|| spec.strip_prefix("link:"))?;
    let path = cwd.join(path);
    let manifest = if path.is_dir() {
        std::fs::read(path.join("package.json")).ok()?
    } else {
        aube_resolver::read_tarball_package_json(&std::fs::read(path).ok()?).ok()?
    };
    let name = serde_json::from_slice::<serde_json::Value>(&manifest).ok()?;
    name["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(String::from)
}

/// DefinitelyTyped's name for `name`: `@scope/pkg` → `@types/scope__pkg`.
fn types_package_name(name: &str) -> String {
    match name.strip_prefix('@') {
        Some(scoped) => format!("@types/{}", scoped.replacen('/', "__", 1)),
        None => format!("@types/{name}"),
    }
}

/// Whether a version's registry manifest declares its own TypeScript types.
/// A root `index.d.ts` with none of these fields is invisible here: the
/// registry metadata doesn't list a tarball's files.
fn has_bundled_types(version: &serde_json::Value) -> bool {
    fn exports_have_types(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(conditions) => conditions.iter().any(|(key, target)| {
                ((key == "types" || key.starts_with("types@")) && !target.is_null())
                    || exports_have_types(target)
            }),
            serde_json::Value::Array(targets) => targets.iter().any(exports_have_types),
            _ => false,
        }
    }
    // `import "pkg"` sees only the root entry. A `.`-keyed object is a
    // subpath map, any other object the root's own conditions.
    let exports = &version["exports"];
    let root = match exports.as_object() {
        Some(map) if map.keys().any(|key| key.starts_with('.')) => &exports["."],
        _ => exports,
    };
    ["types", "typings"]
        .iter()
        .any(|field| version[field].as_str().is_some_and(|path| !path.is_empty()))
        || exports_have_types(root)
}

pub(super) async fn update_manifest_for_add(
    cwd: &Path,
    packages: &[String],
    opts: AddManifestOptions,
    print_updated: bool,
) -> miette::Result<()> {
    // Resolve settings (savePrefix, tag, catalogMode, link/save
    // workspace protocol) from .npmrc / workspace yaml. `catalog_mode`
    // decides whether a newly-added dep that already lives in the
    // default workspace catalog gets rewritten to `catalog:` (see
    // `commands::catalogs::decide_add_rewrite`).
    //
    // The two settings the workspace yaml owns end-to-end
    // (`linkWorkspacePackages`, `saveWorkspaceProtocol`) read from
    // the workspace yaml root so a sub-project's `aube add` honors
    // the workspace-wide policy declared in
    // `pnpm-workspace.yaml`/`aube-workspace.yaml`. Everything else
    // (`tag`, `savePrefix`, `catalogMode`) reads from the project's
    // own dir so a sub-project's `.npmrc` still wins — switching the
    // entire context to the workspace root would silently drop those
    // overrides, since `load_npmrc_entries` doesn't walk up.
    let (
        default_tag,
        default_prefix,
        catalog_mode,
        minimum_release_age,
        registry_supports_time,
        cache_dir,
        save_types,
    ) = crate::commands::with_settings_ctx_and_cli(cwd, &opts.setting_overrides, |ctx| {
        let tag = aube_settings::resolved::tag(ctx);
        // `--save-exact` (CLI) OR a resolved `.npmrc`/env `save-exact=true`
        // both pin to the exact version (empty prefix). npm and pnpm both
        // honor the `save-exact` config knob, so reading it here is the
        // npm/pnpm-compatible behavior, not nub-specific.
        let prefix = if opts.save_exact || aube_settings::resolved::save_exact(ctx) {
            String::new()
        } else {
            let raw = aube_settings::resolved::save_prefix(ctx);
            // Validate: only ^, ~, or empty are valid prefixes.
            match raw.as_str() {
                "^" | "~" | "" => raw,
                _ => {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_SAVE_PREFIX,
                        "ignoring invalid save-prefix={raw:?}, falling back to ^"
                    );
                    "^".to_string()
                }
            }
        };
        let catalog_mode = aube_settings::resolved::catalog_mode(ctx);
        // The version this function writes into the manifest must
        // honor `minimumReleaseAge` the same way full resolution
        // does: dist-tag adds and `--save-exact` pin a concrete
        // version here, and a pinned fresh version would sail past
        // the resolver's gate via its lenient exact-range fallback.
        let minimum_release_age = crate::commands::install::resolve_minimum_release_age(ctx, None);
        let registry_supports_time_field =
            aube_settings::resolved::registry_supports_time_field(ctx);
        let cache_dir = crate::commands::resolved_cache_dir_with_ctx(cwd, ctx);
        let save_types = aube_settings::resolved::save_types(ctx);
        (
            tag,
            prefix,
            catalog_mode,
            minimum_release_age,
            registry_supports_time_field,
            cache_dir,
            save_types,
        )
    });
    let workspace_settings_cwd = crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let (
        link_workspace_packages,
        save_workspace_protocol_setting,
        root_catalog_mode,
        root_save_types,
    ) = crate::commands::with_settings_ctx(&workspace_settings_cwd, |ctx| {
        (
            aube_settings::resolved::link_workspace_packages(ctx),
            aube_settings::resolved::save_workspace_protocol(ctx),
            aube_settings::resolved::catalog_mode(ctx),
            aube_settings::resolved::save_types(ctx),
        )
    });
    // The workspace yaml is read from the root, so a value set there has to
    // reach a member; the member's own setting wins only when it turns it on.
    // The flags beat both.
    let save_types = opts.save_types.unwrap_or(save_types || root_save_types);
    let catalog_mode = if matches!(catalog_mode, CatalogMode::Manual) {
        root_catalog_mode
    } else {
        catalog_mode
    };
    // Load the workspace catalog map up front — the resolver needs it
    // later, but `catalogMode` consults the default catalog while we
    // build the specifier below. Pass the same map to the resolver to
    // avoid re-reading the workspace file.
    let workspace_catalogs = crate::commands::load_workspace_catalogs(cwd)?;
    let default_catalog = workspace_catalogs.get("default");
    let manifest_path = cwd.join("package.json");
    let mut manifest = crate::commands::load_manifest(&manifest_path)?;

    // `--save-catalog` / `--save-catalog-name` queue: each newly-added
    // package that should land in a workspace catalog records its
    // (catalog, package, range) here. Applied to the workspace yaml in
    // a single pass after the manifest loop so the file is rewritten
    // at most once per `aube add` invocation.
    let mut catalog_upserts: Vec<CatalogUpsert> = Vec::new();

    // Parse all specs and fetch packuments concurrently.
    let client =
        std::sync::Arc::new(crate::commands::make_client(cwd).with_network_mode(opts.network_mode));
    let mut parsed: Vec<_> = packages
        .iter()
        .map(|s| {
            let mut spec = parse_pkg_spec(s)?;
            // Replace the implicit default tag with the configured one
            // so that `aube add lodash` respects `tag=next` in .npmrc.
            // Only applies when the user didn't write an explicit version
            // or tag — `aube add lodash@latest` always means `latest`.
            if !spec.has_explicit_range && default_tag != "latest" {
                spec.range = default_tag.clone();
            }
            if spec.alias.is_none()
                && let Some(name) = spec
                    .local_spec
                    .as_deref()
                    .and_then(|local| local_package_name(cwd, local))
            {
                spec.name = name;
            }
            Ok::<_, miette::Report>(spec)
        })
        .collect::<miette::Result<Vec<_>>>()?;

    // `linkWorkspacePackages=true` (or the `--save-workspace-protocol`
    // flag) makes `aube add <name>` look the package up in the local
    // workspace before falling back to the registry. Build the
    // (name → version) map once for this invocation and tag any
    // matching specs so the packument-fetch loop skips them and the
    // manifest-write path branches into the workspace formatter.
    if !matches!(
        link_workspace_packages,
        aube_settings::resolved::LinkWorkspacePackages::False
    ) || matches!(opts.workspace_protocol_override, Some(true))
    {
        let workspace_versions = collect_workspace_versions(cwd);
        for spec in &mut parsed {
            if spec.linked_workspace_version.is_some() {
                continue;
            }
            // Only registry-shaped, non-aliased specs are eligible:
            // workspace/git/local/jsr/npm-aliased specs already have
            // their own routing and the user typed them on purpose.
            // Aliased specs (`my-alias@project-2`) need to skip the
            // workspace path too — `workspace:` resolves by manifest
            // key, so writing `"my-alias": "workspace:^"` would point
            // the resolver at a sibling named `my-alias` (which
            // doesn't exist) and 404 on the registry fallback.
            if aube_util::pkg::is_workspace_spec(&spec.range)
                || aube_util::pkg::is_catalog_spec(&spec.range)
                || aube_util::pkg::is_npm_spec(&spec.range)
                || aube_util::pkg::is_jsr_spec(&spec.range)
                || spec.git_spec.is_some()
                || spec.local_spec.is_some()
                || spec.jsr_name.is_some()
                || spec.alias.is_some()
            {
                continue;
            }
            let Some(version) = workspace_versions.get(&spec.name) else {
                continue;
            };
            // When the user typed an explicit range
            // (`aube add pkg@^1.2.0`), the sibling's version must
            // satisfy it — otherwise we'd silently link an
            // incompatible local copy. Fall through to the registry
            // path on a mismatch (and on unparseable ranges, where
            // the registry path's error message is more useful than
            // a workspace mismatch). Bare adds (no `@<range>`) carry
            // `range = "latest"` from the parser; the implicit
            // dist-tag never blocks a workspace match.
            if spec.has_explicit_range
                && let (Ok(parsed_version), Ok(parsed_range)) = (
                    node_semver::Version::parse(version),
                    node_semver::Range::parse(&spec.range),
                )
                && !parsed_version.satisfies(&parsed_range)
            {
                continue;
            }
            spec.linked_workspace_version = Some(version.clone());
        }
    }

    // Skip packument fetches for `workspace:*` / `workspace:^` /
    // `workspace:<range>` specs — they resolve against the local
    // workspace, not the registry. Same skip applies to git specs
    // (`kevva/is-negative`, `github:user/repo`, …) and `file:` /
    // `link:` local-path specs which the resolver dispatches via the
    // git or local branch respectively. Specs that the
    // `linkWorkspacePackages` pass tagged with a sibling version
    // also bypass the registry — the workspace is the source of
    // truth for those names. Without these guards the parallel
    // fetch below would 404 on the non-registry name.
    let mut handles = tokio::task::JoinSet::new();
    // The age gate compares against the packument `time` map, which the
    // abbreviated (corgi) payload omits — fetch full packuments when the
    // gate is active. Same needs_time split the resolver makes.
    // `registrySupportsTimeField` keeps the cheaper abbreviated path hot
    // when the registry inlines `time` in corgi payloads.
    let needs_time = minimum_release_age.is_some() && !registry_supports_time;
    let corgi_cache_dir = cache_dir.join("packuments-v1");
    let full_cache_dir = cache_dir.join("packuments-full-v1");
    let offline = opts.network_mode == aube_registry::NetworkMode::Offline;
    for spec in &parsed {
        if aube_util::pkg::is_workspace_spec(&spec.range)
            || spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.linked_workspace_version.is_some()
        {
            continue;
        }
        let client = client.clone();
        let corgi_dir = corgi_cache_dir.clone();
        let full_dir = full_cache_dir.clone();
        let name = spec.name.clone();
        handles.spawn(async move {
            let primary = if needs_time {
                client
                    .fetch_packument_with_time_cached(&name, &full_dir)
                    .await
            } else {
                client.fetch_packument_cached(&name, &corgi_dir).await
            };
            // Offline adds serve whichever cache format holds the
            // packument: the needs_time split writes online fetches to
            // one cache only, so an add cached online under the gate
            // lives in `packuments-full-v1` while an ungated one lives
            // in the corgi cache — an offline retry must not fail on a
            // package that's on disk in the other format. The full
            // payload is a superset of corgi, so that direction always
            // picks correctly.
            //
            // The corgi direction is WEAKER than it once was. A missing
            // `time` map no longer keeps versions cutoff-eligible —
            // #581 made an undeterminable publish age fail closed — so an
            // offline add falling back to a corgi-cached packument
            // resolves only what the document's `modified` timestamp
            // proves mature, and otherwise reports the age gate instead
            // of silently installing. Intended: an offline fallback must
            // not be a way around the gate. But it does mean this
            // fallback can now fail where it previously succeeded.
            let packument = match primary {
                Ok(packument) => Ok(packument),
                Err(primary_err) if offline => {
                    let fallback = if needs_time {
                        client.fetch_packument_cached(&name, &corgi_dir).await
                    } else {
                        client
                            .fetch_packument_with_time_cached(&name, &full_dir)
                            .await
                    };
                    fallback.map_err(|_| primary_err)
                }
                Err(primary_err) => Err(primary_err),
            }
            .map_err(|e| match e {
                aube_registry::Error::NotFound(missing) => {
                    package_not_found_error_for_registry(&client, missing)
                }
                error => miette!("failed to fetch {name}: {error}"),
            })?;
            Ok::<_, miette::Report>((name, packument))
        });
    }

    let mut packuments = BTreeMap::new();
    while let Some(handle) = handles.join_next().await {
        let (name, packument) = handle.into_diagnostic()??;
        packuments.insert(name, packument);
    }
    // `--save-types` candidates: (manifest key, registry name, installed version).
    let mut types_candidates: Vec<(String, String, String)> = Vec::new();

    // Resolve each package and update manifest.
    for (spec, orig) in parsed.iter().zip(packages.iter()) {
        let pkg_name_for_manifest = spec.alias.as_deref().unwrap_or(&spec.name);
        // A later spec for the same name replaces the dependency, so it also
        // replaces (or drops) that name's `@types` candidate.
        types_candidates.retain(|(key, ..)| key != pkg_name_for_manifest);

        // Workspace-protocol specs (`pkg@workspace:*`, `pkg@workspace:^`,
        // `pkg@workspace:1.2.0`) bypass the registry path entirely:
        // resolve against the local workspace, write the user's spec
        // verbatim to the manifest, and skip catalog logic (workspace
        // deps are never catalogized).
        if aube_util::pkg::is_workspace_spec(&spec.range) {
            apply_workspace_spec_to_manifest(
                cwd,
                &mut manifest,
                spec,
                pkg_name_for_manifest,
                &opts,
            )?;
            continue;
        }

        // Git specs (`kevva/is-negative`, `github:user/repo`,
        // `git+https://…#tag`) bypass the registry path: write the
        // user's verbatim spec into the manifest and let the resolver
        // dispatch the git branch on the next install. Catalog logic
        // is skipped (catalogs are for registry deps).
        if let Some(verbatim) = spec.git_spec.as_deref() {
            apply_git_spec_to_manifest(&mut manifest, pkg_name_for_manifest, verbatim, &opts);
            continue;
        }

        // `file:` / `link:` local-path specs are handled the same way
        // as git: skip the registry, write the verbatim spec, let the
        // resolver dispatch the local branch on next install.
        if let Some(verbatim) = spec.local_spec.as_deref() {
            apply_local_spec_to_manifest(&mut manifest, pkg_name_for_manifest, verbatim, &opts);
            continue;
        }

        // `linkWorkspacePackages=true` matched a sibling for this
        // spec. Write either a workspace-form specifier (rolling /
        // pinned) or a registry-form specifier per the resolved
        // `saveWorkspaceProtocol` setting and the per-invocation
        // override; the resolver picks the local copy regardless of
        // the form because it already prefers workspace siblings on
        // bare semver ranges.
        if let Some(version) = spec.linked_workspace_version.as_deref() {
            apply_linked_workspace_to_manifest(
                &mut manifest,
                pkg_name_for_manifest,
                version,
                save_workspace_protocol_setting,
                opts.workspace_protocol_override,
                &default_prefix,
                &opts,
            );
            continue;
        }

        // Every spec reaching here had a packument queued above —
        // workspace / git / local / linked-workspace specs all
        // `continue` before the fetch loop, and a failed fetch
        // short-circuits the function. A panic here means a future
        // edit added an early-continue without a matching skip.
        let packument = packuments
            .get(&spec.name)
            .expect("packument missing for non-skipped registry spec");

        crate::commands::install::control::output(
            crate::commands::install::InstallOutputLevel::Info,
            None,
            format!("Resolving {}@{}...", spec.name, spec.range),
        );

        // Resolve non-`latest` dist-tags to their tagged version: like
        // exact pins, they're a deliberate user override (strict mode
        // still refuses a gated one below). `latest` passes through
        // verbatim so `pick_version` can widen it (#681): a gated
        // `latest` steers to the newest release clearing the
        // minimumReleaseAge cutoff at or below the tag, and a mature one
        // keeps the plain dist-tag preference. A `latest` pointing at a
        // prerelease is refused rather than widened, like any other
        // channel tag.
        let effective_range = if spec.range == "latest" {
            spec.range.clone()
        } else if let Some(tagged_version) = packument.dist_tags.get(&spec.range) {
            tagged_version.clone()
        } else {
            spec.range.clone()
        };

        // Version pick, shared with the `catalogMode` rewrite below — the
        // display version must match what will actually get installed, so
        // the same pick runs when the rewrite redirects resolution to the
        // catalog's range. Delegates to the resolver's `pick_version` so
        // `add` and full resolution cannot drift: dist-tag preference,
        // `minimumReleaseAgeExclude` exemptions, and the strict/lenient
        // age-gate fallback all behave identically.
        let highest_satisfying = |range_str: &str| -> Option<String> {
            match aube_resolver::pick_version_for_add(
                packument,
                &spec.name,
                range_str,
                minimum_release_age.as_ref(),
            ) {
                aube_resolver::PickResult::Found(meta) => Some(meta.version.clone()),
                aube_resolver::PickResult::NoMatch | aube_resolver::PickResult::AgeGated(_) => None,
            }
        };
        let resolved_version = match aube_resolver::pick_version_for_add(
            packument,
            &spec.name,
            &effective_range,
            minimum_release_age.as_ref(),
        ) {
            aube_resolver::PickResult::Found(meta) => meta.version.clone(),
            // An age the registry gave us no way to check is a different
            // refusal from a release that is genuinely too new (#581), and
            // `minimumReleaseAgeStrict=false` is not offered for it: loosening
            // strictness would take the newest matching version with no age
            // evidence at all. Mirrors the resolver's split so `add` and a
            // full install report the same failure the same way.
            aube_resolver::PickResult::AgeGated(aube_resolver::AgeGateCause::Undeterminable) => {
                return Err(miette!(
                    code = aube_codes::errors::ERR_AUBE_RELEASE_AGE_MISSING_TIME,
                    help = format!(
                        "the minimumReleaseAge window is checked against the registry's `time` \
                         metadata, which omits these versions — no window would admit them\n\
                         to proceed: unset `registry-supports-time-field` if it is on (it \
                         suppresses the full-packument fetch that carries `time`), check the \
                         registry config in .npmrc, add `{}` to `minimumReleaseAgeExclude`, or \
                         set `minimumReleaseAge=0` to turn the window off for this project \
                         (`--minimum-release-age=0` for this run alone)",
                        spec.name
                    ),
                    "cannot check the publish age of {}@{effective_range} — the registry served no publish time for any matching version",
                    spec.name,
                ));
            }
            aube_resolver::PickResult::AgeGated(aube_resolver::AgeGateCause::TooNew) => {
                // Only reachable in strict mode today (the lenient pick
                // falls back instead), but derive the label anyway so a
                // future lenient AgeGated path can't print a lie.
                let (minutes, strict) = minimum_release_age
                    .as_ref()
                    .map_or((0, false), |m| (m.minutes, m.strict));
                // Same remedies the resolver's `format_age_gate_help` offers on
                // the install path — `add` used to send the user away with no
                // way out at all.
                return Err(miette!(
                    code = aube_codes::errors::ERR_AUBE_NO_MATURE_MATCHING_VERSION,
                    // `spec.name` is already the REGISTRY identity here — an
                    // aliased add carries the user-facing key in `spec.alias`
                    // and writes `npm:{spec.name}@…` to the manifest — so it is
                    // the right thing to name in an exclude remedy, unlike the
                    // resolver's `task.name` (see `AgeGateDetails::registry_name`).
                    help = format!(
                        "to bypass for this run: `--minimum-release-age=<duration>` to shorten \
                         the window (`0` turns it off), or \
                         `--minimum-release-age-exclude={0}` to exempt just this package\n\
                         to bypass persistently: shorten `minimumReleaseAge` in .npmrc (`0` \
                         turns it off), or add `{0}` to `minimumReleaseAgeExclude`",
                        spec.name
                    ),
                    "no version of {} matching {effective_range} is older than {minutes} minute(s){}",
                    spec.name,
                    if strict {
                        " (minimumReleaseAgeStrict=true)"
                    } else {
                        ""
                    },
                ));
            }
            aube_resolver::PickResult::NoMatch => {
                return Err(miette!(
                    "no version of {} matches {effective_range}",
                    spec.name
                ));
            }
        };

        // Build the specifier for package.json.
        // Dist-tags (including "latest") are written as ^version — this matches pnpm's behavior
        // where the lockfile records the resolved version, not the tag name.
        // `--save-exact` drops the `^` so the manifest pins the resolved version.
        //
        // The `npm:` protocol must survive every branch: either the user wrote
        // an alias (`foo@npm:real@range`), which produced `spec.alias`, or they
        // used the bare form (`npm:real@range`), which leaves `alias` empty but
        // keeps the prefix on `orig`. Both cases round-trip back as `npm:...`.
        // `jsr:` is handled separately below, because the manifest form omits
        // the name when the alias equals the JSR name (matching pnpm).
        let is_jsr = spec.jsr_name.is_some();
        let needs_npm_prefix = !is_jsr && (spec.alias.is_some() || orig.starts_with("npm:"));
        // A prerelease saves exact, as in pnpm: `^7.1.0-beta` also admits
        // `7.1.0-dev.20261105.1`, because `dev` sorts after `beta`, and the
        // install that follows re-resolves this range, so the caret would
        // install a nightly the user never asked for.
        let prefix = if node_semver::Version::parse(&resolved_version)
            .is_ok_and(|version| version.is_prerelease())
        {
            ""
        } else {
            default_prefix.as_str()
        };
        // npm-incumbent save-prefix convention: `npm install pkg@1.2.3` applies
        // its `save-prefix` default and writes `"^1.2.3"`, whereas pnpm/bun
        // preserve the bare version. The embedder turns this on
        // (`npm_save_prefix_on_bare_exact`) only under an npm incumbent; under
        // pnpm/bun/nub-identity it stays off and the literal is preserved.
        // Scope is exactly a BARE EXACT version the user supplied — an explicit
        // range (`^1`, `~1.2`, `>=1`) never parses as a strict `Version`, so it
        // is preserved verbatim. Dist-tags and `--save-exact` are handled by the
        // other `pin_to_resolved` clauses; this one only catches the
        // bare-exact-version add.
        let bare_exact_npm_save_prefix = aube_util::engine_context().npm_save_prefix_on_bare_exact
            && spec.has_explicit_range
            && node_semver::Version::parse(&spec.range).is_ok();
        let pin_to_resolved = spec.range == default_tag
            || packument.dist_tags.contains_key(&spec.range)
            || opts.save_exact
            || bare_exact_npm_save_prefix;
        // Dist-tags and `--save-exact` both resolve to a concrete version
        // with the configured prefix (empty when `--save-exact`). Non-dist-tag
        // explicit ranges (e.g. `lodash@^4`) are preserved as-is.
        let manual_specifier = if let Some(jsr_name) = spec.jsr_name.as_deref() {
            // jsr:<range> when the manifest key matches the JSR name (the
            // default when the user didn't supply an alias); otherwise we
            // embed the JSR name so the resolver can rebuild the npm-compat
            // name on its next read.
            let effective_range = if pin_to_resolved {
                format!("{prefix}{resolved_version}")
            } else {
                spec.range.clone()
            };
            let alias_matches_jsr_name =
                spec.alias.as_deref() == Some(jsr_name) || spec.alias.is_none();
            if alias_matches_jsr_name {
                format!("jsr:{effective_range}")
            } else {
                format!("jsr:{jsr_name}@{effective_range}")
            }
        } else if pin_to_resolved {
            if needs_npm_prefix {
                format!("npm:{}@{prefix}{resolved_version}", spec.name)
            } else {
                format!("{prefix}{resolved_version}")
            }
        } else if needs_npm_prefix {
            // Preserve npm: protocol for aliases and bare-prefix specs.
            format!("npm:{}@{}", spec.name, spec.range)
        } else {
            spec.range.clone()
        };
        // `--save-catalog` / `--save-catalog-name` short-circuits the
        // `catalogMode` decision: the user explicitly asked for the
        // dep to land in a catalog. `npm:`, `jsr:`, `workspace:`, and
        // pre-`catalog:` specs can't be re-expressed as a catalog
        // reference, so they fall back to the manual specifier and the
        // catalog yaml is left untouched (matches pnpm's `--save-catalog`
        // behavior on workspace deps).
        let exclude_from_catalog = needs_npm_prefix
            || is_jsr
            || aube_util::pkg::is_workspace_spec(&spec.range)
            || aube_util::pkg::is_catalog_spec(&spec.range);
        let (specifier, display_version) = if let Some(target) = opts.save_catalog.as_deref() {
            decide_save_catalog(
                target,
                &workspace_catalogs,
                spec,
                exclude_from_catalog,
                &manual_specifier,
                &resolved_version,
                &mut catalog_upserts,
                highest_satisfying,
            )
        } else {
            // Apply `catalogMode`. Only the default catalog participates —
            // named catalogs still require the user to write `catalog:<name>`
            // explicitly. `npm:`/alias specs can't be re-expressed as a
            // catalog reference, so they opt out regardless of mode.
            match decide_add_rewrite(
                catalog_mode,
                default_catalog,
                &spec.name,
                &spec.range,
                spec.has_explicit_range,
                &resolved_version,
                needs_npm_prefix || is_jsr,
            ) {
                CatalogRewrite::Manual => (manual_specifier, resolved_version.clone()),
                CatalogRewrite::UseDefaultCatalog => {
                    // The install will resolve against the catalog's range,
                    // not the user's original spec — so the printed version
                    // should reflect what actually lands in `node_modules`.
                    // `strict` + bare `aube add <pkg>` is the case this
                    // matters most for: the user never gave a range, so
                    // `resolved_version` comes from `latest` and can easily
                    // disagree with what the catalog entry picks. Fall back
                    // to `resolved_version` only when the catalog range
                    // can't resolve a version from the packument (shouldn't
                    // happen in practice, but we'd rather print something
                    // than fail the command on a display edge case).
                    let cat_range = default_catalog
                        .and_then(|c| c.get(&spec.name))
                        .cloned()
                        .unwrap_or_default();
                    let catalog_version = highest_satisfying(&cat_range).unwrap_or_else(|| {
                        tracing::debug!(
                            "catalog range {cat_range:?} for {} matched no packument version \
                             (or, under minimumReleaseAgeStrict, none mature enough); falling \
                             back to user-resolved version for display — the install that \
                             follows resolves the catalog range itself and fails loudly if it \
                             genuinely can't",
                            spec.name
                        );
                        resolved_version.clone()
                    });
                    ("catalog:".to_string(), catalog_version)
                }
                CatalogRewrite::StrictMismatch {
                    pkg,
                    catalog_range,
                    user_range,
                } => {
                    return Err(miette!(
                        "catalogMode=strict: {pkg}@{user_range} does not match the \
                         default catalog entry `{catalog_range}`. Update the catalog \
                         or rerun with the catalog range."
                    ));
                }
            }
        };

        crate::commands::install::control::output(
            crate::commands::install::InstallOutputLevel::Info,
            None,
            format!("  + {pkg_name_for_manifest}@{display_version} (specifier: {specifier})"),
        );

        place_dep_in_manifest(&mut manifest, pkg_name_for_manifest, specifier, &opts);
        if save_types && !is_jsr && !spec.name.starts_with("@types/") {
            types_candidates.push((
                pkg_name_for_manifest.to_string(),
                spec.name.clone(),
                display_version,
            ));
        }
    }

    // After the loop, so an `@types/*` named in this same command, or one the
    // project already lists, is kept exactly as written.
    let skipped = |types_name: &str, reason: String| {
        crate::commands::install::control::output(
            crate::commands::install::InstallOutputLevel::Info,
            None,
            format!("  skipped {types_name}: {reason}"),
        );
    };
    // A name without a scope resolves to the default registry.
    let default_registry = client.config_registry_for("");
    for (key, name, version) in types_candidates {
        // TypeScript finds an alias's types under the alias, so `renamed`
        // for `npm:real` gets `@types/renamed` pointing at `npm:@types/real`.
        let types_key = types_package_name(&key);
        let types_name = types_package_name(&name);
        if manifest.direct_dependency_range(&types_key).is_some()
            || manifest.peer_dependencies.contains_key(&types_key)
        {
            continue;
        }
        // Abbreviated packuments drop `types`/`typings`/`exports`, so read
        // the installed version from the full document.
        let full = client
            .fetch_packument_full_cached(&name, &full_cache_dir)
            .await
            .map_err(|e| miette!("failed to fetch {name}: {e}"))?;
        if full["versions"].get(&version).is_none_or(has_bundled_types) {
            continue;
        }
        // A catalog entry for the `@types` package is the project's own pin, so
        // it is saved as written and the install resolves it, aliases included.
        // An alias of the package can't be a catalog reference.
        let aliased = types_key != types_name;
        let catalog_range = match opts.save_catalog.as_deref() {
            Some(target) => workspace_catalogs.get(target),
            None if matches!(catalog_mode, CatalogMode::Manual) => None,
            None => default_catalog,
        }
        .filter(|_| !aliased)
        .and_then(|catalog| catalog.get(&types_name));
        if let Some(range) = catalog_range {
            let specifier = match opts.save_catalog.as_deref() {
                Some(target) if target != "default" => format!("catalog:{target}"),
                _ => "catalog:".to_string(),
            };
            crate::commands::install::control::output(
                crate::commands::install::InstallOutputLevel::Info,
                None,
                format!("  + {types_key}@{range} (specifier: {specifier})"),
            );
            manifest.dev_dependencies.insert(types_key, specifier);
            continue;
        }
        // A package from its scope's own registry (`@acme:registry=`) may be
        // private, so its name is never sent to a different registry for
        // `@types`. A package from the default registry is looked up wherever
        // the `@types` scope points.
        let registry = client.config_registry_for(&name);
        if registry != default_registry && registry != client.config_registry_for(&types_name) {
            skipped(
                &types_name,
                format!("{name} and @types use different registries"),
            );
            continue;
        }
        let fetched = if needs_time {
            client
                .fetch_packument_with_time_cached(&types_name, &full_cache_dir)
                .await
        } else {
            client
                .fetch_packument_cached(&types_name, &corgi_cache_dir)
                .await
        };
        // Only a 404 means "no such package"; auth and network failures
        // stop the add before package.json is written.
        let types_packument = match fetched {
            Ok(packument) => Some(packument),
            Err(aube_registry::Error::NotFound(_)) => None,
            Err(e) => return Err(miette!("failed to fetch {types_name}: {e}")),
        };
        // DefinitelyTyped versions follow the library's major.minor, so stay
        // on the major just installed rather than taking `latest`. A `^N`
        // range never admits a prerelease, so the save prefix always applies.
        let Ok(major) = node_semver::Version::parse(&version).map(|v| v.major) else {
            continue;
        };
        let picked = types_packument.as_ref().map(|packument| {
            aube_resolver::pick_version_for_add(
                packument,
                &types_name,
                &format!("^{major}"),
                minimum_release_age.as_ref(),
            )
        });
        let reason = match picked {
            Some(aube_resolver::PickResult::Found(meta)) if meta.deprecated.is_none() => {
                let manual_specifier = if aliased {
                    format!("npm:{types_name}@{default_prefix}{}", meta.version)
                } else {
                    format!("{default_prefix}{}", meta.version)
                };
                let specifier = if let Some(target) = opts.save_catalog.as_deref() {
                    decide_save_catalog(
                        target,
                        &workspace_catalogs,
                        &parse_pkg_spec(&types_name)?,
                        aliased,
                        &manual_specifier,
                        &meta.version,
                        &mut catalog_upserts,
                        |_| None,
                    )
                    .0
                } else {
                    manual_specifier
                };
                crate::commands::install::control::output(
                    crate::commands::install::InstallOutputLevel::Info,
                    None,
                    format!("  + {types_key}@{} (specifier: {specifier})", meta.version),
                );
                manifest.dev_dependencies.insert(types_key, specifier);
                continue;
            }
            None => "not found on the registry".to_string(),
            // The pick only lands on a deprecated release when every match is
            // deprecated, which on DefinitelyTyped marks a stub left behind
            // once the library started shipping its own types.
            Some(aube_resolver::PickResult::Found(_)) => "deprecated".to_string(),
            Some(aube_resolver::PickResult::NoMatch) => format!("no release for {name} {major}.x"),
            Some(aube_resolver::PickResult::AgeGated(_)) => {
                format!("no release for {name} {major}.x is old enough for minimumReleaseAge")
            }
        };
        skipped(&types_name, reason);
    }

    // Pre-flight the catalog write target BEFORE mutating the dependent
    // manifest. Under nub identity the catalog cannot live in the
    // pnpm-named workspace YAML (the next resolve refuses to read it),
    // so it must land in `workspaces.catalog` in package.json; resolving
    // + validating the target here means a `--save-catalog` that can't
    // land its entry fails before rewriting the manifest to an
    // unresolvable `catalog:` reference (issue #369).
    let catalog_target = if catalog_upserts.is_empty() {
        None
    } else {
        Some(crate::commands::catalogs::resolve_catalog_write_target(
            cwd,
        )?)
    };

    // Write the updated package.json. Under `--no-save` callers still
    // write the mutated manifest to disk for the duration of the
    // resolver + install pipeline (both re-read from disk), then
    // restore the original bytes from their snapshot before returning.
    crate::commands::write_manifest_dep_sections(&manifest_path, &manifest)?;
    if print_updated {
        crate::commands::install::control::output(
            crate::commands::install::InstallOutputLevel::Info,
            None,
            "Updated package.json",
        );
    }

    // Apply queued `--save-catalog` upserts to the pre-resolved target.
    // Lands once at the end of the per-package loop so the target is
    // rewritten at most once per command — the yaml/manifest writers
    // no-op when nothing structural changes (preserving comments/format
    // under filtered/recursive re-runs that all target the same catalog).
    if let Some(target) = &catalog_target {
        crate::commands::catalogs::apply_catalog_upserts(target, &catalog_upserts)?;
    }

    Ok(())
}

/// Resolve a `pkg@workspace:<range>` spec against the local workspace
/// and write the user's spec verbatim into the manifest. Skips the
/// registry path entirely — workspace specs only mean anything inside
/// a workspace, so we look the package up in the workspace's
/// `find_workspace_packages` set and error out clearly if it's missing.
///
/// Mirrors pnpm's `pnpm add foo@workspace:*` shape: the manifest entry
/// keeps the literal `workspace:*` (or `workspace:^`, `workspace:~`,
/// `workspace:1.2.0`, …) the user typed, which the install pipeline
/// later resolves to a `link:../foo` symlink.
fn apply_workspace_spec_to_manifest(
    cwd: &Path,
    manifest: &mut aube_manifest::PackageJson,
    spec: &ParsedPkgSpec,
    pkg_name_for_manifest: &str,
    opts: &AddManifestOptions,
) -> miette::Result<()> {
    // Walk up to the workspace root — the cwd may be a subpackage,
    // and `find_workspace_packages` is anchored at the root yaml. Fall
    // back to cwd so a single-package project with no workspace yaml
    // still surfaces a useful error from the package-lookup below.
    let workspace_root = crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let workspace_pkg_dirs = aube_workspace::find_workspace_packages(&workspace_root)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;

    // Match by the `name` field in each workspace package's manifest,
    // not by directory name — pnpm semantics. Skip dirs whose
    // package.json is unreadable.
    let mut found_version: Option<String> = None;
    for dir in &workspace_pkg_dirs {
        let pkg_manifest = match aube_manifest::PackageJson::from_path(&dir.join("package.json")) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if pkg_manifest.name.as_deref() == Some(spec.name.as_str()) {
            found_version = Some(pkg_manifest.version.unwrap_or_else(|| "0.0.0".to_string()));
            break;
        }
    }
    let Some(workspace_version) = found_version else {
        return Err(miette!(
            "no workspace package named `{}` found at or above {}; \
             `workspace:` specs only resolve against local workspace packages",
            spec.name,
            workspace_root.display()
        ));
    };

    crate::commands::install::control::output(
        crate::commands::install::InstallOutputLevel::Info,
        None,
        format!(
            "  + {pkg_name_for_manifest}@{workspace_version} (specifier: {})",
            spec.range
        ),
    );

    place_dep_in_manifest(manifest, pkg_name_for_manifest, spec.range.clone(), opts);
    Ok(())
}

/// Walk the workspace from `cwd` and build a `name → version` map of
/// every workspace package. Returns an empty map outside a workspace
/// or when discovery fails — `aube add` falls back to the registry
/// path in that case, so a partial workspace shouldn't error here.
pub(super) fn collect_workspace_versions(cwd: &Path) -> std::collections::HashMap<String, String> {
    let workspace_root = match crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
    {
        Some(root) => root,
        None => return std::collections::HashMap::new(),
    };
    let mut out = std::collections::HashMap::new();
    let dirs = match aube_workspace::find_workspace_packages(&workspace_root) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for dir in dirs {
        let Ok(pkg) = aube_manifest::PackageJson::from_path(&dir.join("package.json")) else {
            continue;
        };
        if let Some(name) = pkg.name {
            out.insert(name, pkg.version.unwrap_or_else(|| "0.0.0".to_string()));
        }
    }
    out
}

/// Write the manifest entry for a `linkWorkspacePackages` match. The
/// resolved `saveWorkspaceProtocol` and the per-invocation
/// `--save-workspace-protocol` / `--no-save-workspace-protocol`
/// override pick the form:
///
/// - rolling: `workspace:^` (or `~`/`*` per `savePrefix`)
/// - true: `workspace:<prefix><version>` (e.g. `workspace:^1.2.3`)
/// - false: `<prefix><version>` (e.g. `^1.2.3`) — the manifest looks
///   like a registry dep but the resolver still links the local copy
///   because aube prefers workspace siblings on bare semver ranges.
///
/// Mirrors the duplicate-section scrub from the registry path so a
/// follow-up `aube add` after a previous `--save-dev` add overwrites
/// the old entry rather than duplicating across sections.
#[allow(clippy::too_many_arguments)]
fn apply_linked_workspace_to_manifest(
    manifest: &mut aube_manifest::PackageJson,
    pkg_name_for_manifest: &str,
    workspace_version: &str,
    save_workspace_protocol: aube_settings::resolved::SaveWorkspaceProtocol,
    workspace_protocol_override: Option<bool>,
    save_prefix: &str,
    opts: &AddManifestOptions,
) {
    use aube_settings::resolved::SaveWorkspaceProtocol;
    // `--no-save-workspace-protocol` forces registry-style; explicit
    // `--save-workspace-protocol` keeps the configured workspace form
    // (defaulting to `rolling` when nothing else picks); otherwise
    // defer to the resolved setting.
    let effective = match workspace_protocol_override {
        Some(false) => SaveWorkspaceProtocol::False,
        Some(true) if matches!(save_workspace_protocol, SaveWorkspaceProtocol::False) => {
            SaveWorkspaceProtocol::Rolling
        }
        _ => save_workspace_protocol,
    };
    let specifier = match effective {
        SaveWorkspaceProtocol::Rolling => {
            // Rolling form drops the version: `workspace:^`. Empty
            // `savePrefix` (`--save-exact`) collapses to
            // `workspace:*` so the rolling sigil still resolves the
            // sibling regardless of its version.
            let sigil = if save_prefix.is_empty() {
                "*"
            } else {
                save_prefix
            };
            format!("workspace:{sigil}")
        }
        SaveWorkspaceProtocol::True => {
            format!("workspace:{save_prefix}{workspace_version}")
        }
        SaveWorkspaceProtocol::False => {
            format!("{save_prefix}{workspace_version}")
        }
    };

    crate::commands::install::control::output(
        crate::commands::install::InstallOutputLevel::Info,
        None,
        format!("  + {pkg_name_for_manifest}@{workspace_version} (specifier: {specifier})"),
    );

    place_dep_in_manifest(manifest, pkg_name_for_manifest, specifier, opts);
}

/// Write a git-form spec verbatim into the manifest. Mirrors the
/// duplicate-section scrub of the registry path so re-running
/// `aube add <git-spec>` after a previous registry add overwrites the
/// old entry instead of duplicating it across `dependencies` and
/// `devDependencies`.
///
/// The manifest carries the literal user-typed string
/// (`kevva/is-negative`, `github:user/repo`, …) — preserving the
/// verbatim form keeps the manifest readable, and aube's resolver
/// handles every form `parse_git_spec` recognizes equivalently.
///
/// Limitation: when the user didn't supply an alias, the manifest key
/// is the repo segment of the clone URL (e.g. `is-negative` for
/// `kevva/is-negative`). If the upstream package's `package.json`
/// `name` differs from the repo segment, pass an alias:
/// `aube add my-pkg@kevva/is-negative`.
fn apply_git_spec_to_manifest(
    manifest: &mut aube_manifest::PackageJson,
    pkg_name_for_manifest: &str,
    verbatim_spec: &str,
    opts: &AddManifestOptions,
) {
    crate::commands::install::control::output(
        crate::commands::install::InstallOutputLevel::Info,
        None,
        format!("  + {pkg_name_for_manifest} (specifier: {verbatim_spec})"),
    );

    place_dep_in_manifest(
        manifest,
        pkg_name_for_manifest,
        verbatim_spec.to_string(),
        opts,
    );
}

/// Write a `file:` / `link:` spec verbatim into the manifest. Same
/// section-scrub semantics as [`apply_git_spec_to_manifest`] — the
/// only difference is the manifest-key derivation (URL repo segment
/// vs path basename) which lives in the parser.
///
/// The manifest carries the literal user-typed string
/// (`file:./pkg`, `link:../sibling`, …) — preserving the verbatim
/// form keeps the manifest readable, and aube's resolver handles
/// every form `LocalSource::parse` recognizes equivalently.
///
/// Limitation: when the user didn't supply an alias, the manifest
/// key is the basename of the path (e.g. `foo` for
/// `file:./packages/foo`, `bundle` for `file:./bundle.tgz`). Pass an
/// alias when the upstream `package.json` `name` differs from the
/// basename: `aube add my-pkg@file:./packages/foo`.
fn apply_local_spec_to_manifest(
    manifest: &mut aube_manifest::PackageJson,
    pkg_name_for_manifest: &str,
    verbatim_spec: &str,
    opts: &AddManifestOptions,
) {
    apply_git_spec_to_manifest(manifest, pkg_name_for_manifest, verbatim_spec, opts);
}

/// Decide what `aube add --save-catalog[=<name>]` should write to the
/// manifest for one package, and queue any catalog-yaml mutation. Pulls
/// the per-package logic out of `update_manifest_for_add` so the main
/// loop stays readable.
///
/// Returns `(manifest_specifier, display_version)`. The display_version
/// is what gets printed on the `+ pkg@<version>` line and reflects what
/// will actually land in `node_modules` after install — not necessarily
/// the version the user originally typed.
#[allow(clippy::too_many_arguments)]
fn decide_save_catalog(
    target: &str,
    workspace_catalogs: &crate::commands::CatalogMap,
    spec: &ParsedPkgSpec,
    exclude_from_catalog: bool,
    manual_specifier: &str,
    resolved_version: &str,
    upserts: &mut Vec<CatalogUpsert>,
    highest_satisfying: impl Fn(&str) -> Option<String>,
) -> (String, String) {
    if exclude_from_catalog {
        return (manual_specifier.to_string(), resolved_version.to_string());
    }
    // Manifest specifier. `default` writes plain `catalog:`, named
    // catalogs use `catalog:<name>` (matches pnpm).
    let manifest_specifier = if target == "default" {
        "catalog:".to_string()
    } else {
        format!("catalog:{target}")
    };
    let target_catalog = workspace_catalogs.get(target);
    if let Some(existing_range) = target_catalog.and_then(|c| c.get(&spec.name)) {
        // Already in target catalog — never overwrite.
        let compatible = range_compatible(
            &spec.range,
            spec.has_explicit_range,
            existing_range,
            resolved_version,
        );
        if compatible {
            // Catalog entry covers the user's range — manifest can use
            // `catalog:` and the install will resolve through the
            // existing entry. Display the catalog's resolved version
            // for the same reason `decide_add_rewrite` does.
            let catalog_version = highest_satisfying(existing_range).unwrap_or_else(|| {
                tracing::debug!(
                    "catalog range {existing_range:?} for {} matched no packument version \
                     (or, under minimumReleaseAgeStrict, none mature enough); falling back \
                     to user-resolved version for display — the install that follows \
                     resolves the catalog range itself and fails loudly if it genuinely can't",
                    spec.name
                );
                resolved_version.to_string()
            });
            return (manifest_specifier, catalog_version);
        }
        // Incompatible — preserve the existing catalog entry and fall
        // back to writing the user's spec into the manifest. Matches
        // pnpm/test/saveCatalog.ts:488 (the "never overwrites existing
        // catalogs" test).
        return (manual_specifier.to_string(), resolved_version.to_string());
    }
    // Not in target catalog — queue the addition. The catalog entry
    // mirrors what we'd otherwise write to `package.json`: `manual_specifier`
    // already encodes the right shape — explicit semver ranges pass through,
    // dist-tags resolve to `<save-prefix><resolved-version>`, bare `aube
    // add <pkg>` defaults to the same prefix+resolved form. The npm:/jsr:
    // cases are unreachable here because they hit the `exclude_from_catalog`
    // early return above.
    upserts.push(CatalogUpsert {
        catalog: target.to_string(),
        package: spec.name.clone(),
        range: manual_specifier.to_string(),
    });
    (manifest_specifier, resolved_version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(save_dev: bool, save_optional: bool, save_peer: bool) -> AddManifestOptions {
        AddManifestOptions {
            save_dev,
            setting_overrides: Vec::new(),
            save_exact: false,
            save_optional,
            save_peer,
            save_types: Some(false),
            network_mode: aube_registry::NetworkMode::Online,
            save_catalog: None,
            workspace_protocol_override: None,
        }
    }

    fn place(opts: &AddManifestOptions) -> aube_manifest::PackageJson {
        let mut m = aube_manifest::PackageJson::default();
        place_dep_in_manifest(&mut m, "is-odd", "^3.0.1".to_string(), opts);
        m
    }

    #[test]
    fn save_peer_writes_both_peer_and_dev() {
        // pnpm's `getSaveType` maps `savePeer` to `devDependencies` and writes
        // the peer entry on top, so `--save-peer` (even WITHOUT `--save-dev`)
        // lands the package in BOTH sections. Verified against pnpm 10.15.
        let m = place(&opts(false, false, true));
        assert_eq!(
            m.peer_dependencies.get("is-odd").map(String::as_str),
            Some("^3.0.1")
        );
        assert_eq!(
            m.dev_dependencies.get("is-odd").map(String::as_str),
            Some("^3.0.1")
        );
        assert!(m.dependencies.is_empty());
        assert!(m.optional_dependencies.is_empty());
    }

    #[test]
    fn save_peer_with_save_dev_still_writes_both() {
        let m = place(&opts(true, false, true));
        assert!(m.peer_dependencies.contains_key("is-odd"));
        assert!(m.dev_dependencies.contains_key("is-odd"));
    }

    #[test]
    fn plain_add_writes_only_dependencies() {
        let m = place(&opts(false, false, false));
        assert_eq!(
            m.dependencies.get("is-odd").map(String::as_str),
            Some("^3.0.1")
        );
        assert!(m.dev_dependencies.is_empty());
        assert!(m.peer_dependencies.is_empty());
    }

    #[test]
    fn save_dev_and_save_optional_route_to_their_own_section() {
        assert!(
            place(&opts(true, false, false))
                .dev_dependencies
                .contains_key("is-odd")
        );
        assert!(
            place(&opts(false, true, false))
                .optional_dependencies
                .contains_key("is-odd")
        );
    }

    #[test]
    fn scrub_clears_stale_section_on_resave() {
        // A package previously in `dependencies` moves cleanly to peer+dev
        // (no duplicate left behind in `dependencies`).
        let mut m = aube_manifest::PackageJson::default();
        m.dependencies
            .insert("is-odd".to_string(), "^1.0.0".to_string());
        place_dep_in_manifest(
            &mut m,
            "is-odd",
            "^3.0.1".to_string(),
            &opts(false, false, true),
        );
        assert!(m.dependencies.is_empty());
        assert!(m.peer_dependencies.contains_key("is-odd"));
        assert!(m.dev_dependencies.contains_key("is-odd"));
    }

    #[test]
    fn missing_package_hints_use_only_the_registry_the_name_routes_through() {
        let corpus = Some("react\nlodash\nexpress\n");
        let public = aube_registry::client::RegistryClient::new("https://registry.npmjs.org");
        let private = aube_registry::client::RegistryClient::new("https://packages.example.test");

        let public_error = package_not_found_error_for_registry_with_corpus(
            &public,
            "lodaszh".to_string(),
            corpus,
        );
        assert_eq!(
            public_error.to_string(),
            "package not found: lodaszh; did you mean lodash?"
        );

        let private_error = package_not_found_error_for_registry_with_corpus(
            &private,
            "lodaszh".to_string(),
            corpus,
        );
        assert_eq!(private_error.to_string(), "package not found: lodaszh");
    }

    #[tokio::test]
    async fn offline_add_resolves_from_packument_cache() {
        let project = tempfile::tempdir().unwrap();
        let cache_root = project.path().join("cache");
        std::fs::write(project.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            project.path().join(".npmrc"),
            format!("cache-dir={}\n", cache_root.display()),
        )
        .unwrap();

        let packument: aube_registry::Packument = serde_json::from_value(serde_json::json!({
            "name": "cached-only",
            "dist-tags": { "latest": "1.0.0" },
            "versions": {
                "1.0.0": {
                    "name": "cached-only",
                    "version": "1.0.0"
                }
            }
        }))
        .unwrap();
        let cache_dir = crate::commands::packument_cache_dir_for_cwd(project.path());
        crate::commands::make_client(project.path()).seed_packument_cache(
            "cached-only",
            &cache_dir,
            &packument,
            None,
            None,
            true,
        );

        update_manifest_for_add(
            project.path(),
            &["cached-only".to_string()],
            AddManifestOptions {
                save_dev: false,
                save_exact: false,
                save_optional: false,
                save_peer: false,
                save_types: Some(false),
                network_mode: aube_registry::NetworkMode::Offline,
                save_catalog: None,
                workspace_protocol_override: None,
                setting_overrides: Vec::new(),
            },
            false,
        )
        .await
        .unwrap();

        let manifest = std::fs::read_to_string(project.path().join("package.json")).unwrap();
        assert!(manifest.contains(r#""cached-only": "^1.0.0""#));
    }

    #[tokio::test]
    async fn prerelease_dist_tag_saves_exact_so_a_later_prerelease_cannot_replace_it() {
        let project = tempfile::tempdir().unwrap();
        let cache_root = project.path().join("cache");
        std::fs::write(project.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            project.path().join(".npmrc"),
            format!("cache-dir={}\n", cache_root.display()),
        )
        .unwrap();

        // `dev` sorts after `beta`, so `^7.1.0-beta` would admit the nightly.
        let packument: aube_registry::Packument = serde_json::from_value(serde_json::json!({
            "name": "tagged",
            "dist-tags": { "latest": "7.0.2", "beta": "7.1.0-beta" },
            "versions": {
                "7.0.2": { "name": "tagged", "version": "7.0.2" },
                "7.1.0-beta": { "name": "tagged", "version": "7.1.0-beta" },
                "7.1.0-dev.20261105.1": { "name": "tagged", "version": "7.1.0-dev.20261105.1" }
            }
        }))
        .unwrap();
        let cache_dir = crate::commands::packument_cache_dir_for_cwd(project.path());
        crate::commands::make_client(project.path())
            .seed_packument_cache("tagged", &cache_dir, &packument, None, None, true);

        let add = |spec: &'static str| {
            let dir = project.path().to_path_buf();
            async move {
                update_manifest_for_add(
                    &dir,
                    &[spec.to_string()],
                    AddManifestOptions {
                        save_dev: false,
                        save_exact: false,
                        save_optional: false,
                        save_peer: false,
                        save_types: Some(false),
                        network_mode: aube_registry::NetworkMode::Offline,
                        save_catalog: None,
                        workspace_protocol_override: None,
                        setting_overrides: Vec::new(),
                    },
                    false,
                )
                .await
                .unwrap();
                std::fs::read_to_string(dir.join("package.json")).unwrap()
            }
        };

        assert!(
            add("tagged@beta")
                .await
                .contains(r#""tagged": "7.1.0-beta""#)
        );
        // A stable tag keeps the configured prefix.
        assert!(add("tagged@latest").await.contains(r#""tagged": "^7.0.2""#));
    }

    #[test]
    fn bundled_types_come_from_types_typings_or_the_root_export_types_condition() {
        use serde_json::json;
        for declared in [
            json!({ "types": "index.d.ts" }),
            json!({ "typings": "index.d.ts" }),
            json!({ "exports": { ".": { "import": { "types": "./index.d.mts" } } } }),
            json!({ "exports": { "types": "./index.d.ts", "default": "./index.js" } }),
            json!({ "exports": [{ "types@>=5": "./ts5.d.ts" }, "./index.js"] }),
        ] {
            assert!(has_bundled_types(&declared), "{declared}");
        }
        for untyped in [
            json!({}),
            json!({ "types": "" }),
            json!({ "exports": { ".": { "types": null, "default": "./index.js" } } }),
            // A typed subpath doesn't type `import "pkg"`.
            json!({ "exports": { ".": "./index.js", "./feature": { "types": "./f.d.ts" } } }),
        ] {
            assert!(!has_bundled_types(&untyped), "{untyped}");
        }
        assert_eq!(types_package_name("express"), "@types/express");
        assert_eq!(types_package_name("@babel/core"), "@types/babel__core");
    }

    /// A mock-registry document for `name`, every version carrying `fields`.
    fn registry_doc(
        name: &str,
        versions: &[&str],
        fields: serde_json::Value,
    ) -> (String, u16, String) {
        let docs: serde_json::Map<_, _> = versions
            .iter()
            .map(|version| {
                let mut doc = fields.clone();
                doc["name"] = name.into();
                doc["version"] = (*version).into();
                (version.to_string(), doc)
            })
            .collect();
        let body = serde_json::json!({
            "name": name,
            "dist-tags": { "latest": versions.last() },
            "versions": docs,
        });
        (name.to_string(), 200, body.to_string())
    }

    /// Serves `routes` as a registry: `@types/*` only under `/types/`, and
    /// `/private/` as a second registry for `@private`.
    async fn serve_registry(
        routes: Vec<(String, u16, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let path = path.trim_start_matches("/private");
                let (status, body) = routes
                    .iter()
                    .find(|(name, ..)| {
                        let prefix = if name.starts_with("@types/") {
                            "/types"
                        } else {
                            ""
                        };
                        path == format!("{prefix}/{}", name.replace('/', "%2F"))
                    })
                    .map_or((404, ""), |(_, status, body)| (*status, body.as_str()));
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (registry, server)
    }

    #[tokio::test]
    async fn save_types_adds_types_only_for_untyped_packages_and_fails_on_a_lookup_error() {
        use serde_json::json;

        let routes = vec![
            registry_doc("untyped", &["1.2.0"], json!({})),
            registry_doc("@types/untyped", &["1.0.5", "2.0.0"], json!({})),
            registry_doc("@scope/untyped", &["3.1.0"], json!({})),
            registry_doc("@types/scope__untyped", &["3.0.1"], json!({})),
            registry_doc("typed", &["2.0.0"], json!({ "types": "index.d.ts" })),
            registry_doc("@types/typed", &["2.0.0"], json!({})),
            registry_doc("no-dt", &["1.0.0"], json!({})),
            registry_doc("kept", &["1.0.0"], json!({})),
            registry_doc("@types/kept", &["1.0.0"], json!({})),
            registry_doc("@private/lib", &["1.0.0"], json!({})),
            registry_doc("@types/private__lib", &["1.0.0"], json!({})),
            registry_doc("denied", &["1.0.0"], json!({})),
            registry_doc("twice", &["4.0.0", "5.0.0"], json!({})),
            registry_doc("@types/twice", &["4.0.0", "5.0.0"], json!({})),
            ("@types/denied".to_string(), 401, String::new()),
            registry_doc("stubbed", &["1.0.0"], json!({})),
            registry_doc(
                "@types/stubbed",
                &["1.0.0"],
                json!({ "deprecated": "stub types" }),
            ),
        ];
        let (registry, server) = serve_registry(routes).await;
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".npmrc"),
            format!(
                "registry={registry}\n@private:registry={registry}private/\n\
                 @types:registry={registry}types/\n\
                 cache-dir={}\nminimumReleaseAge=0\n",
                project.path().join("cache").display()
            ),
        )
        .unwrap();
        let manifest_path = project.path().join("package.json");
        std::fs::write(
            &manifest_path,
            r#"{ "dependencies": { "@types/kept": "~0.1.0" } }"#,
        )
        .unwrap();
        let save_types = || AddManifestOptions {
            save_types: Some(true),
            ..opts(false, false, false)
        };

        #[derive(Default)]
        struct RecordingReporter(std::sync::Mutex<Vec<String>>);
        impl crate::commands::install::InstallReporter for RecordingReporter {
            fn report(&self, event: crate::commands::install::InstallEvent) {
                if let crate::commands::install::InstallEvent::Output { message, .. } = event {
                    self.0.lock().unwrap().push(message);
                }
            }
        }
        let reporter = std::sync::Arc::new(RecordingReporter::default());
        crate::commands::install::control::scope(
            crate::commands::install::InstallControl::events(reporter.clone()),
            update_manifest_for_add(
                project.path(),
                &[
                    "untyped",
                    "@scope/untyped",
                    "typed",
                    "no-dt",
                    "kept",
                    "@private/lib",
                    "twice@4.0.0",
                    "twice@5.0.0",
                    "stubbed",
                ]
                .map(String::from),
                save_types(),
                false,
            ),
        )
        .await
        .unwrap();
        let manifest = aube_manifest::PackageJson::from_path(&manifest_path).unwrap();
        // `@types/untyped` stays on its library's major, not on `latest`, and
        // comes from the `@types` registry although `untyped` doesn't.
        // `@types/private__lib` is never asked of a registry `@private`
        // doesn't use, and the add says so. The last spec for a name picks both
        // the dependency and its `@types`.
        assert_eq!(
            manifest.dev_dependencies,
            BTreeMap::from([
                ("@types/scope__untyped".to_string(), "^3.0.1".to_string()),
                ("@types/twice".to_string(), "^5.0.0".to_string()),
                ("@types/untyped".to_string(), "^1.0.5".to_string()),
            ]),
        );
        assert_eq!(manifest.dependencies["twice"], "5.0.0");
        assert_eq!(manifest.dependencies["@types/kept"], "~0.1.0");
        let output = reporter.0.lock().unwrap().clone();
        let skip =
            "  skipped @types/private__lib: @private/lib and @types use different registries";
        assert!(output.iter().any(|line| line == skip), "{output:#?}");
        let skip = "  skipped @types/stubbed: deprecated";
        assert!(output.iter().any(|line| line == skip), "{output:#?}");

        // Only a 404 is skipped: a 401 fails the add before package.json is written.
        let written = std::fs::read_to_string(&manifest_path).unwrap();
        let error =
            update_manifest_for_add(project.path(), &["denied".to_string()], save_types(), false)
                .await
                .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("@types/denied"), "{error}");
        assert_eq!(std::fs::read_to_string(&manifest_path).unwrap(), written);
    }

    #[tokio::test]
    async fn save_types_goes_through_the_workspace_catalog() {
        use serde_json::json;

        let (registry, server) = serve_registry(vec![
            registry_doc("listed", &["2.0.0"], json!({})),
            registry_doc("unlisted", &["1.0.0"], json!({})),
            registry_doc("@types/unlisted", &["1.0.3"], json!({})),
        ])
        .await;
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".npmrc"),
            format!(
                "registry={registry}\n@types:registry={registry}types/\n\
                 cache-dir={}\nminimumReleaseAge=0\n",
                project.path().join("cache").display()
            ),
        )
        .unwrap();
        std::fs::write(
            project.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\ncatalogMode: strict\ncatalog:\n  '@types/listed': npm:@acme/listed-types@^2\n",
        )
        .unwrap();
        std::fs::write(project.path().join("package.json"), "{}").unwrap();
        let member = project.path().join("packages/member");
        std::fs::create_dir_all(&member).unwrap();
        let manifest_path = member.join("package.json");
        std::fs::write(&manifest_path, "{}").unwrap();
        let add = |name: &str, save_catalog: Option<&str>| {
            let dir = member.as_path();
            let packages = [name.to_string()];
            let options = AddManifestOptions {
                save_types: Some(true),
                save_catalog: save_catalog.map(String::from),
                ..opts(false, false, false)
            };
            async move { update_manifest_for_add(dir, &packages, options, false).await }
        };

        // The entry is saved as written: nothing serves `@types/listed`.
        add("listed", None).await.unwrap();
        add("unlisted", Some("default")).await.unwrap();
        server.abort();
        let manifest = aube_manifest::PackageJson::from_path(&manifest_path).unwrap();
        assert_eq!(manifest.dev_dependencies["@types/listed"], "catalog:");
        assert_eq!(manifest.dev_dependencies["@types/unlisted"], "catalog:");
        let catalogs = crate::commands::load_workspace_catalogs(&member).unwrap();
        assert_eq!(catalogs["default"]["@types/unlisted"], "^1.0.3");
    }

    #[tokio::test]
    async fn save_types_setting_applies_unless_the_flag_turns_it_off() {
        use serde_json::json;

        let (registry, server) = serve_registry(vec![
            registry_doc("one", &["1.0.0"], json!({})),
            registry_doc("@types/one", &["1.0.0"], json!({})),
            registry_doc("two", &["1.0.0"], json!({})),
            registry_doc("@types/two", &["1.0.0"], json!({})),
        ])
        .await;
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".npmrc"),
            format!(
                "registry={registry}\n@types:registry={registry}types/\n\
                 cache-dir={}\nminimumReleaseAge=0\n",
                project.path().join("cache").display()
            ),
        )
        .unwrap();
        // Set only in the root yaml, so a member has to inherit it.
        std::fs::write(
            project.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\nsaveTypes: true\n",
        )
        .unwrap();
        std::fs::write(project.path().join("package.json"), "{}").unwrap();
        let member = project.path().join("packages/member");
        std::fs::create_dir_all(&member).unwrap();
        let manifest_path = member.join("package.json");
        std::fs::write(&manifest_path, "{}").unwrap();
        for (name, save_types) in [("one", None), ("two", Some(false))] {
            let options = AddManifestOptions {
                save_types,
                ..opts(false, false, false)
            };
            update_manifest_for_add(&member, &[name.to_string()], options, false)
                .await
                .unwrap();
        }
        server.abort();
        let manifest = aube_manifest::PackageJson::from_path(&manifest_path).unwrap();
        assert_eq!(manifest.dev_dependencies["@types/one"], "^1.0.0");
        assert!(!manifest.dev_dependencies.contains_key("@types/two"));
    }

    #[tokio::test]
    async fn local_specs_are_saved_under_the_package_s_own_name() {
        let project = tempfile::tempdir().unwrap();
        for (dir, manifest) in [
            ("named", r#"{ "name": "@acme/real-name" }"#),
            ("unnamed", "{}"),
        ] {
            std::fs::create_dir(project.path().join(dir)).unwrap();
            std::fs::write(project.path().join(dir).join("package.json"), manifest).unwrap();
        }
        let manifest_path = project.path().join("package.json");
        std::fs::write(&manifest_path, "{}").unwrap();
        update_manifest_for_add(
            project.path(),
            &["./named", "./unnamed", "picked@./named"].map(String::from),
            opts(false, false, false),
            false,
        )
        .await
        .unwrap();
        let manifest = aube_manifest::PackageJson::from_path(&manifest_path).unwrap();
        assert_eq!(manifest.dependencies["@acme/real-name"], "link:./named");
        assert_eq!(manifest.dependencies["unnamed"], "link:./unnamed");
        assert_eq!(manifest.dependencies["picked"], "link:./named");
    }

    #[tokio::test]
    async fn offline_add_falls_back_to_full_packument_cache() {
        // Online adds under the age gate cache packuments in
        // `packuments-full-v1` only. If the gate is later disabled, the
        // primary offline lookup moves to the corgi cache — but the add
        // must still serve the full packument already on disk instead of
        // failing on the empty corgi cache.
        let project = tempfile::tempdir().unwrap();
        let cache_root = project.path().join("cache");
        std::fs::write(project.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            project.path().join(".npmrc"),
            format!("cache-dir={}\nminimumReleaseAge=0\n", cache_root.display()),
        )
        .unwrap();

        let packument: aube_registry::Packument = serde_json::from_value(serde_json::json!({
            "name": "full-cached-only",
            "dist-tags": { "latest": "1.0.0" },
            "versions": {
                "1.0.0": {
                    "name": "full-cached-only",
                    "version": "1.0.0"
                }
            }
        }))
        .unwrap();
        let full_cache_dir = crate::commands::packument_full_cache_dir_for_cwd(project.path());
        crate::commands::make_client(project.path()).seed_full_packument_cache(
            "full-cached-only",
            &full_cache_dir,
            &packument,
            None,
            None,
            true,
        );

        update_manifest_for_add(
            project.path(),
            &["full-cached-only".to_string()],
            AddManifestOptions {
                save_dev: false,
                save_exact: false,
                save_optional: false,
                save_peer: false,
                save_types: Some(false),
                network_mode: aube_registry::NetworkMode::Offline,
                save_catalog: None,
                workspace_protocol_override: None,
                setting_overrides: Vec::new(),
            },
            false,
        )
        .await
        .unwrap();

        let manifest = std::fs::read_to_string(project.path().join("package.json")).unwrap();
        assert!(manifest.contains(r#""full-cached-only": "^1.0.0""#));
    }
}
