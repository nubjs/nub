//! `aube licenses` / `aube licenses ls` — report dependency licenses.
//!
//! Walks the lockfile, reads each installed package's `license` field from
//! its virtual-store `package.json`, and prints a table grouped by license
//! (or a JSON array with `--json`). Pure read — no network, no writes,
//! no project lock.

use super::DepFilter;
use aube_lockfile::LockfileGraph;
use clap::Args;
use miette::{Context, IntoDiagnostic};
use serde::Serialize;
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

#[derive(Debug, Args)]
pub struct LicensesArgs {
    /// pnpm-compat subcommand marker.
    ///
    /// `aube licenses ls [flags...]` is accepted as a synonym for
    /// bare `aube licenses [flags...]` so scripts written for pnpm
    /// keep working. Modeled as an optional positional instead of a
    /// clap subcommand so flags can appear on either side of `ls`
    /// (subcommands swallow the parent's flags).
    #[arg(value_parser = ["ls"], hide = true)]
    pub subcommand: Option<String>,

    /// Show only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Emit a JSON array keyed by package instead of the default table
    #[arg(long)]
    pub json: bool,

    /// Include the resolved path on disk for each package
    #[arg(long)]
    pub long: bool,

    /// Show only production dependencies (skip devDependencies)
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
    #[command(flatten)]
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

    let aube_dir = super::resolve_virtual_store_dir_for_cwd(&cwd);
    let rows = collect_rows(&aube_dir, &filtered, args.long);

    if args.json {
        render_json(&rows)?;
    } else {
        render_grouped(&rows, args.long);
    }

    Ok(())
}

/// Walk every package in the filtered graph and read its license from the
/// virtual-store manifest. Packages whose manifest can't be read (e.g.,
/// `node_modules` not materialized yet) fall back to "UNKNOWN" so one
/// missing file doesn't sink the whole report.
fn collect_rows(aube_dir: &Path, graph: &LockfileGraph, long: bool) -> Vec<Row> {
    // Deduplicate by (name, version) so peer-context duplicates
    // (`react@18.2.0` vs `react@18.2.0(prop-types@15.8.1)`) only show once.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut rows: Vec<Row> = Vec::new();

    for pkg in graph.packages.values() {
        if !seen.insert((pkg.name.clone(), pkg.version.clone())) {
            continue;
        }
        let pkg_dir = virtual_store_pkg_dir(aube_dir, &pkg.dep_path, &pkg.name);
        let license = read_license(&pkg_dir);
        rows.push(Row {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            license: license.unwrap_or_else(|| "UNKNOWN".to_string()),
            path: if long {
                Some(pkg_dir.display().to_string())
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
fn virtual_store_pkg_dir(aube_dir: &Path, dep_path: &str, name: &str) -> PathBuf {
    use aube_lockfile::dep_path_filename::{
        DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
    };
    aube_dir
        .join(dep_path_to_filename(
            dep_path,
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        ))
        .join("node_modules")
        .join(name)
}

/// Read the `license` field from a package's `package.json`.
///
/// Accepts every shape real packages use in the wild:
/// - `"license": "MIT"` — SPDX string
/// - `"license": { "type": "MIT" }` — legacy object form still found on npm
/// - `"licenses": [ { "type": "MIT" }, ... ]` — legacy array, pick the first
///
/// Returns `None` when the manifest is unreadable or the field is missing.
fn read_license(pkg_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(pkg_dir.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value.get("license").and_then(extract_license).or_else(|| {
        value
            .get("licenses")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(extract_license)
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
}
