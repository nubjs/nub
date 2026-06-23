use super::AddArgs;
use super::spec::{ParsedPkgSpec, parse_pkg_spec};
use crate::commands::catalogs::{
    CatalogRewrite, CatalogUpsert, decide_add_rewrite, range_compatible,
};
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone)]
pub(super) struct AddManifestOptions {
    pub(super) save_dev: bool,
    pub(super) save_exact: bool,
    pub(super) save_optional: bool,
    pub(super) save_peer: bool,
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
}

impl AddManifestOptions {
    pub(super) fn from_args(args: &AddArgs) -> Self {
        Self {
            save_dev: args.save_dev,
            save_exact: args.save_exact,
            save_optional: args.save_optional,
            save_peer: args.save_peer,
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
        }
    }
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
    let (default_tag, default_prefix, catalog_mode) =
        crate::commands::with_settings_ctx(cwd, |ctx| {
            let tag = aube_settings::resolved::tag(ctx);
            let prefix = if opts.save_exact {
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
            (tag, prefix, catalog_mode)
        });
    let workspace_settings_cwd = crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let (link_workspace_packages, save_workspace_protocol_setting) =
        crate::commands::with_settings_ctx(&workspace_settings_cwd, |ctx| {
            (
                aube_settings::resolved::link_workspace_packages(ctx),
                aube_settings::resolved::save_workspace_protocol(ctx),
            )
        });
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
    let client = std::sync::Arc::new(crate::commands::make_client(cwd));
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
    let mut handles = Vec::new();
    for spec in &parsed {
        if aube_util::pkg::is_workspace_spec(&spec.range)
            || spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.linked_workspace_version.is_some()
        {
            continue;
        }
        let client = client.clone();
        let name = spec.name.clone();
        let handle = tokio::spawn(async move {
            let packument = client
                .fetch_packument(&name)
                .await
                .map_err(|e| miette!("failed to fetch {name}: {e}"))?;
            Ok::<_, miette::Report>((name, packument))
        });
        handles.push(handle);
    }

    let mut packuments = BTreeMap::new();
    for handle in handles {
        let (name, packument) = handle.await.into_diagnostic()??;
        packuments.insert(name, packument);
    }

    // Resolve each package and update manifest.
    for (spec, orig) in parsed.iter().zip(packages.iter()) {
        let pkg_name_for_manifest = spec.alias.as_deref().unwrap_or(&spec.name);

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

        eprintln!("Resolving {}@{}...", spec.name, spec.range);

        // Resolve "latest" and other dist-tags to a version range.
        let effective_range = if let Some(tagged_version) = packument.dist_tags.get(&spec.range) {
            tagged_version.clone()
        } else {
            spec.range.clone()
        };

        // Find highest matching version. Reused below when a
        // `catalogMode` rewrite redirects resolution to the catalog's
        // range — the display version should match what will actually
        // get installed, not what the user's original range resolved
        // to, so we call this twice when the rewrite fires.
        //
        // Parse every candidate version once (skipping invalid ones
        // entirely) and sort the parsed pairs. Comparator-only parsing
        // burned ~2N parses per add; pre-parse turns it into N + log N
        // and lets the satisfies-scan reuse the parsed `Version`.
        let mut parsed_versions: Vec<(&String, node_semver::Version)> = packument
            .versions
            .keys()
            .filter_map(|v| node_semver::Version::parse(v).ok().map(|p| (v, p)))
            .collect();
        parsed_versions.sort_by(|a, b| b.1.cmp(&a.1));
        let highest_satisfying = |range_str: &str| -> Option<String> {
            let range = node_semver::Range::parse(range_str).ok()?;
            // Mirror `aube_resolver::pick_version`: prefer the
            // `dist-tags.latest` version when it satisfies the range.
            // npm and pnpm both pin toward the publisher's tagged
            // build over the strictly-highest matching version, and
            // the display line here must agree with what the
            // resolver actually installs.
            if let Some(latest) = packument.dist_tags.get("latest")
                && let Ok(parsed_latest) = node_semver::Version::parse(latest)
                && parsed_latest.satisfies(&range)
                && packument.versions.contains_key(latest)
            {
                return Some(latest.clone());
            }
            parsed_versions
                .iter()
                .find(|(_, parsed)| parsed.satisfies(&range))
                .map(|(raw, _)| (*raw).clone())
        };
        let resolved_version = highest_satisfying(&effective_range)
            .ok_or_else(|| miette!("no version of {} matches {effective_range}", spec.name))?;

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
        let prefix = &default_prefix;
        let pin_to_resolved = spec.range == default_tag
            || packument.dist_tags.contains_key(&spec.range)
            || opts.save_exact;
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
                            "catalog range {cat_range:?} for {} did not match any packument version; \
                             falling back to user-resolved version for display",
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

        eprintln!("  + {pkg_name_for_manifest}@{display_version} (specifier: {specifier})");

        place_dep_in_manifest(&mut manifest, pkg_name_for_manifest, specifier, &opts);
    }

    // Write the updated package.json. Under `--no-save` callers still
    // write the mutated manifest to disk for the duration of the
    // resolver + install pipeline (both re-read from disk), then
    // restore the original bytes from their snapshot before returning.
    crate::commands::write_manifest_dep_sections(&manifest_path, &manifest)?;
    if print_updated {
        eprintln!("Updated package.json");
    }

    // Apply queued `--save-catalog` upserts. Lands once at the end of
    // the per-package loop so the workspace yaml is rewritten at most
    // once per command — `edit_workspace_yaml` no-ops when nothing
    // structural changes (preserving comments under filtered/recursive
    // re-runs that all target the same catalog).
    if !catalog_upserts.is_empty() {
        let yaml_root = crate::dirs::find_workspace_yaml_root(cwd)
            .or_else(|| crate::dirs::find_workspace_root(cwd))
            .unwrap_or_else(|| cwd.to_path_buf());
        let yaml_path = aube_manifest::workspace::workspace_yaml_target(&yaml_root);
        crate::commands::catalogs::upsert_catalog_entries(&yaml_path, &catalog_upserts)?;
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

    eprintln!(
        "  + {pkg_name_for_manifest}@{workspace_version} (specifier: {})",
        spec.range
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

    eprintln!("  + {pkg_name_for_manifest}@{workspace_version} (specifier: {specifier})");

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
    eprintln!("  + {pkg_name_for_manifest} (specifier: {verbatim_spec})");

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
                    "catalog range {existing_range:?} for {} did not match any \
                     packument version; falling back to user-resolved version for display",
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
            save_exact: false,
            save_optional,
            save_peer,
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
}
