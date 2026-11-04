//! `aube licenses` / `aube licenses ls` — report dependency licenses.
//!
//! Walks the lockfile, reads each installed package's `license` field from
//! its virtual-store `package.json`, and prints a table grouped by license
//! (or a JSON array with `--json`). Pure read — no network, no writes,
//! no project lock.

use super::DepFilter;
use aube_lockfile::LockfileGraph;
use miette::{Context, IntoDiagnostic};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube licenses
  ├─ Apache-2.0
  │  └─ typescript@5.4.5
  ├─ ISC
  │  └─ semver@7.6.0
  └─ MIT
     ├─ express@4.19.2
     ├─ lodash@4.17.21
     └─ zod@3.23.8

  # Only production deps
  $ aube licenses --prod

  # Include each package's store path
  $ aube licenses --long

  # JSON array, one object per package
  $ aube licenses --json
";

#[derive(Debug, usage_rs::Args)]
pub struct LicensesArgs {
    /// pnpm-compat subcommand marker.
    ///
    /// `aube licenses ls [flags...]` is accepted as a synonym for
    /// bare `aube licenses [flags...]` so scripts written for pnpm
    /// keep working. Modeled as an optional positional instead of a
    /// subcommand so flags can appear on either side of `ls`
    /// (subcommands swallow the parent's flags).
    #[usage(arg, choices("ls"), hide)]
    pub subcommand: Option<String>,

    /// Show only devDependencies
    #[usage(short = 'D', long, conflicts = "--prod")]
    pub dev: bool,

    /// Emit a JSON array keyed by package instead of the default table
    #[usage(long)]
    pub json: bool,

    /// Include the resolved path on disk for each package
    #[usage(long)]
    pub long: bool,

    /// Show only production dependencies (skip devDependencies)
    #[usage(short = 'P', long, long = "production", conflicts = "--dev")]
    pub prod: bool,
    #[usage(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Serialize)]
struct Row {
    name: String,
    version: String,
    license: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

pub(super) struct InstalledPackageMetadata {
    pub license: Option<String>,
    pub path: PathBuf,
}

pub async fn run(args: LicensesArgs) -> miette::Result<()> {
    args.network.install_overrides();
    // `licenses ls` is pnpm-compat; it behaves identically to bare `licenses`.
    let _ = args.subcommand;

    let cwd = crate::dirs::project_root()?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(());
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    let filter = DepFilter::from_flags(args.prod, args.dev);
    let filtered = graph.filter_deps(|d| filter.keeps(d.dep_type));
    let installed_metadata =
        collect_installed_metadata(&cwd, &graph, filtered.packages.keys().map(String::as_str))?;
    let rows = collect_rows(&filtered, &installed_metadata, args.long);

    if args.json {
        render_json(&rows)?;
    } else {
        render_grouped(&rows, args.long);
    }

    Ok(())
}

pub(super) fn collect_installed_metadata<'a>(
    cwd: &Path,
    graph: &LockfileGraph,
    dep_paths: impl IntoIterator<Item = &'a str>,
) -> miette::Result<BTreeMap<String, InstalledPackageMetadata>> {
    let aube_dir = super::resolve_virtual_store_dir_for_cwd(cwd);
    // Prefer the recorded layout over current config: the install may have
    // used a one-shot `--node-linker=hoisted` override.
    let installed_layout = crate::state::read_state_layout(cwd)
        .or_else(|| crate::state::read_default_state_layout(cwd));
    let installed_licenses = crate::state::read_state_package_licenses(cwd);
    let virtual_store_dir_max_length = installed_layout
        .as_ref()
        .and_then(|layout| layout.virtual_store_dir_max_length)
        .unwrap_or_else(|| super::resolve_virtual_store_dir_max_length_for_cwd(cwd));
    let installed_hoisted = installed_layout
        .as_ref()
        .map(|layout| matches!(layout.linker, crate::state::InstallLayoutMode::Hoisted))
        .unwrap_or_else(|| {
            super::with_settings_ctx(cwd, |ctx| {
                matches!(
                    aube_settings::resolved::node_linker(ctx),
                    aube_settings::resolved::NodeLinker::Hoisted
                )
            })
        });
    let hoisted_placements = if installed_hoisted {
        let modules_dir_name = installed_layout
            .as_ref()
            .map(|layout| layout.modules_dir_name.as_str())
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                installed_layout
                    .as_ref()
                    .and_then(|layout| infer_legacy_modules_dir_name(layout, graph))
            })
            // A dependency-free legacy snapshot has no direct entry from
            // which to infer the directory. The value is immaterial because
            // there are no placements, so use the historical default.
            .unwrap_or_else(|| "node_modules".to_string());
        let recorded_hoisting_limits = installed_layout
            .as_ref()
            .and_then(|layout| layout.hoisting_limits)
            .map(|limits| match limits {
                crate::state::InstallHoistingLimits::None => aube_linker::HoistingLimits::None,
                crate::state::InstallHoistingLimits::Workspaces => {
                    aube_linker::HoistingLimits::Workspaces
                }
                crate::state::InstallHoistingLimits::Dependencies => {
                    aube_linker::HoistingLimits::Dependencies
                }
            });
        // Prefer the exact linker-produced map. Replanning the full graph can
        // change which conflicting version owns a root placement after a
        // filtered install.
        Some(match crate::state::read_hoisted_placements(cwd) {
            Some(placements) => placements,
            None => match recorded_hoisting_limits {
                Some(limits) => aube_linker::HoistedPlacements::from_graph(
                    cwd,
                    graph,
                    &modules_dir_name,
                    limits,
                )?,
                None => legacy_hoisted_placements(cwd, graph, &modules_dir_name)?,
            },
        })
    } else {
        None
    };

    let mut metadata = BTreeMap::new();
    for dep_path in dep_paths {
        let Some(pkg) = graph.get_package(dep_path) else {
            continue;
        };
        let recorded_link_dir = installed_layout
            .as_ref()
            .and_then(|layout| layout.packages.get(dep_path))
            .filter(|package| package.link)
            .and_then(|package| {
                cwd.join(&package.package_json_path)
                    .parent()
                    .map(Path::to_path_buf)
            });
        let linked = recorded_link_dir.is_some()
            || matches!(
                pkg.local_source.as_ref(),
                Some(aube_lockfile::LocalSource::Link(_))
            );
        let path = match (pkg.local_source.as_ref(), recorded_link_dir) {
            (Some(aube_lockfile::LocalSource::Link(path)), _) => cwd.join(path),
            (None, Some(path)) => path,
            _ => hoisted_placements
                .as_ref()
                .and_then(|placements| placements.package_dir(dep_path))
                .map(Path::to_path_buf)
                .unwrap_or_else(|| {
                    virtual_store_pkg_dir(
                        &aube_dir,
                        dep_path,
                        &pkg.name,
                        virtual_store_dir_max_length,
                    )
                }),
        };
        let recorded_license = installed_licenses.licenses.get(dep_path).cloned();
        metadata.insert(
            dep_path.to_string(),
            InstalledPackageMetadata {
                license: if linked {
                    read_license(&path).or(recorded_license)
                } else {
                    recorded_license.or_else(|| read_license(&path))
                }
                .or_else(|| pkg.license.clone()),
                path,
            },
        );
    }
    Ok(metadata)
}

pub(super) fn collect_installed_licenses<'a>(
    cwd: &Path,
    graph: &LockfileGraph,
    dep_paths: impl IntoIterator<Item = &'a str>,
) -> miette::Result<BTreeMap<String, InstalledPackageMetadata>> {
    let installed = crate::state::read_state_package_licenses(cwd);
    let mut metadata = BTreeMap::new();
    let mut fallback_paths = Vec::new();
    for dep_path in dep_paths {
        let Some(pkg) = graph.get_package(dep_path) else {
            continue;
        };
        let cached = installed.licenses.get(dep_path).cloned();
        let license = if let Some(path) = installed.linked_package_dirs.get(dep_path) {
            read_license(&cwd.join(path)).or(cached)
        } else if cached.is_some() {
            cached
        } else {
            fallback_paths.push(dep_path);
            continue;
        }
        .or_else(|| pkg.license.clone());
        metadata.insert(
            dep_path.to_string(),
            InstalledPackageMetadata {
                license,
                path: PathBuf::new(),
            },
        );
    }
    if !fallback_paths.is_empty() {
        metadata.extend(collect_installed_metadata(cwd, graph, fallback_paths)?);
    }
    Ok(metadata)
}

/// Infer `modulesDir` from the root importer's recorded direct entries in
/// state written before the field was persisted explicitly.
fn infer_legacy_modules_dir_name(
    layout: &crate::state::InstallLayoutState,
    graph: &LockfileGraph,
) -> Option<String> {
    let entries = layout.direct_entries.get(".")?;
    let deps = graph.importers.get(".")?;
    entries.iter().zip(deps).find_map(|(entry, dep)| {
        let mut modules_dir = PathBuf::from(entry);
        for _ in Path::new(&dep.name).components() {
            if !modules_dir.pop() {
                return None;
            }
        }
        Some(modules_dir.to_string_lossy().into_owned())
    })
}

/// Legacy layout state did not record `hoistingLimits`. Reconstruct every
/// possible plan and choose the one that matches the most package directories
/// on disk, instead of consulting mutable current settings.
fn legacy_hoisted_placements(
    cwd: &Path,
    graph: &LockfileGraph,
    modules_dir_name: &str,
) -> Result<aube_linker::HoistedPlacements, aube_linker::Error> {
    let mut best = None;
    for limits in [
        aube_linker::HoistingLimits::None,
        aube_linker::HoistingLimits::Workspaces,
        aube_linker::HoistingLimits::Dependencies,
    ] {
        let placements =
            aube_linker::HoistedPlacements::from_graph(cwd, graph, modules_dir_name, limits)?;
        let matches = graph
            .packages
            .keys()
            .filter(|dep_path| placements.package_dir(dep_path).is_some())
            .count();
        if best
            .as_ref()
            .is_none_or(|(best_matches, _)| matches > *best_matches)
        {
            best = Some((matches, placements));
        }
    }
    Ok(best.map_or_else(
        aube_linker::HoistedPlacements::default,
        |(_, placements)| placements,
    ))
}

/// Walk every package in the filtered graph and render its collected metadata.
/// Packages whose manifest couldn't be read fall back to "UNKNOWN" so one
/// missing file doesn't sink the whole report.
fn collect_rows(
    graph: &LockfileGraph,
    installed_metadata: &BTreeMap<String, InstalledPackageMetadata>,
    long: bool,
) -> Vec<Row> {
    // Deduplicate by (name, version) so peer-context duplicates
    // (`react@18.2.0` vs `react@18.2.0(prop-types@15.8.1)`) only show once.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut rows: Vec<Row> = Vec::new();

    for pkg in graph.packages.values() {
        if !seen.insert((pkg.name.clone(), pkg.version.clone())) {
            continue;
        }
        let metadata = installed_metadata.get(&pkg.dep_path);
        rows.push(Row {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            license: metadata
                .and_then(|metadata| metadata.license.clone())
                .unwrap_or_else(|| "UNKNOWN".to_string()),
            path: if long {
                metadata.map(|metadata| metadata.path.display().to_string())
            } else {
                None
            },
        });
    }

    rows.sort_by(|a, b| {
        a.license
            .cmp(&b.license)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.version.cmp(&b.version))
    });
    rows
}

/// Resolve the on-disk virtual-store directory for a single package.
///
/// Mirrors the linker's naming rules: every dep_path (including
/// scoped ones) is run through `dep_path_to_filename` to produce a
/// single flat entry name under the per-project virtual store, then
/// we walk into its `node_modules/<name>` for the materialized
/// package. Scoped names survive as `@scope+name@version` in the
/// entry name and still as `@scope/name` inside that entry's nested
/// `node_modules/`.
///
/// `aube_dir` is the resolved `virtualStoreDir` — the caller threads
/// it in via `commands::resolve_virtual_store_dir_for_cwd` so a
/// custom override lands on the same path the linker wrote to.
fn virtual_store_pkg_dir(
    aube_dir: &Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
) -> PathBuf {
    use aube_lockfile::dep_path_filename::dep_path_to_filename;
    aube_dir
        .join(dep_path_to_filename(dep_path, virtual_store_dir_max_length))
        .join("node_modules")
        .join(name)
}

/// Read the `license` field from a package's `package.json`.
///
/// Accepts every shape real packages use in the wild:
/// - `"license": "MIT"` — SPDX string
/// - `"license": { "type": "MIT" }` — legacy object form still found on npm
/// - `"licenses": [ { "type": "MIT" }, ... ]` — legacy array
///
/// Returns `None` when the manifest is unreadable or the field is missing.
pub(crate) fn read_license(pkg_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(pkg_dir.join("package.json")).ok()?;
    let manifest: ManifestLicenseFields = serde_json::from_slice(&bytes).ok()?;
    license_from_values(manifest.license.as_ref(), manifest.licenses.as_ref())
}

#[derive(Deserialize)]
struct ManifestLicenseFields {
    license: Option<serde_json::Value>,
    licenses: Option<serde_json::Value>,
}

pub(super) fn license_from_values(
    license: Option<&serde_json::Value>,
    licenses: Option<&serde_json::Value>,
) -> Option<String> {
    license.and_then(extract_license).or_else(|| {
        licenses
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(extract_license).collect::<Vec<_>>())
            .filter(|licenses| !licenses.is_empty())
            .map(|licenses| licenses.join(" OR "))
    })
}

fn extract_license(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(obj) => {
            obj.get("type").and_then(|t| t.as_str()).map(String::from)
        }
        _ => None,
    }
}

/// Default output: group by license, list packages underneath. Mirrors the
/// shape of `pnpm licenses` closely enough for casual inspection and for
/// regex-based screen scrapers.
fn render_grouped(rows: &[Row], long: bool) {
    if rows.is_empty() {
        println!("(no dependencies)");
        return;
    }

    let mut by_license: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for row in rows {
        by_license
            .entry(row.license.as_str())
            .or_default()
            .push(row);
    }

    let last_idx = by_license.len().saturating_sub(1);
    for (i, (license, entries)) in by_license.iter().enumerate() {
        let license_connector = if i == last_idx { "└─" } else { "├─" };
        println!("{license_connector} {license}");
        let inner_prefix = if i == last_idx { "   " } else { "│  " };
        let last_entry = entries.len().saturating_sub(1);
        for (j, row) in entries.iter().enumerate() {
            let entry_connector = if j == last_entry { "└─" } else { "├─" };
            println!(
                "{inner_prefix}{entry_connector} {}@{}",
                row.name, row.version
            );
            if long && let Some(path) = &row.path {
                let tail_prefix = if j == last_entry { "   " } else { "│  " };
                println!("{inner_prefix}{tail_prefix} {path}");
            }
        }
    }
}

fn render_json(rows: &[Row]) -> miette::Result<()> {
    let out = serde_json::to_string_pretty(rows)
        .into_diagnostic()
        .wrap_err("failed to serialize licenses output")?;
    println!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_license_string() {
        let v = serde_json::json!("MIT");
        assert_eq!(extract_license(&v).as_deref(), Some("MIT"));
    }

    #[test]
    fn extract_license_object() {
        let v = serde_json::json!({ "type": "Apache-2.0", "url": "..." });
        assert_eq!(extract_license(&v).as_deref(), Some("Apache-2.0"));
    }

    #[test]
    fn extract_license_missing_type() {
        let v = serde_json::json!({ "url": "..." });
        assert!(extract_license(&v).is_none());
    }

    #[test]
    fn manifest_license_fields_skip_unrelated_manifest_data() {
        let manifest: ManifestLicenseFields = serde_json::from_value(serde_json::json!({
            "name": "example",
            "dependencies": {"dep": "1.0.0"},
            "scripts": {"postinstall": "node build.js"},
            "license": {"type": "Apache-2.0", "url": "https://example.com/license"}
        }))
        .unwrap();
        assert_eq!(
            license_from_values(manifest.license.as_ref(), manifest.licenses.as_ref()),
            Some("Apache-2.0".to_string())
        );
    }

    #[test]
    fn manifest_license_fields_keep_valid_primary_when_legacy_field_is_malformed() {
        let manifest: ManifestLicenseFields = serde_json::from_value(serde_json::json!({
            "license": "MIT",
            "licenses": "not-an-array"
        }))
        .unwrap();
        assert_eq!(
            license_from_values(manifest.license.as_ref(), manifest.licenses.as_ref()),
            Some("MIT".to_string())
        );
    }

    #[test]
    fn legacy_license_array_preserves_every_license() {
        let value = serde_json::json!([
            {"type": "MIT"},
            {"type": "Apache-2.0"},
            {"url": "ignored"}
        ]);
        assert_eq!(
            license_from_values(None, Some(&value)),
            Some("MIT OR Apache-2.0".to_string())
        );
    }
}
