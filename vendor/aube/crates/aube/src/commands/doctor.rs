//! `aube doctor` — broad install-health diagnostic.
//!
//! Prints a grouped snapshot of aube's environment (version, directories,
//! project layout, registries, node version) followed by any warnings and
//! errors we can detect statically. Mirrors the shape of `mise doctor`:
//! info sections first, then an accumulated list of warnings, then a list
//! of errors. Exits non-zero when the error list is non-empty.
//!
//! Individual checks (`check_virtual_store_links`, `check_install_state`,
//! `check_foreign_package_manager_dirs`) are kept as free functions so
//! they can be reused from more focused commands in the future without
//! round-tripping through the formatter.
//!
//! The diagnostic itself is read-only: reads config, package.json,
//! `.aube-state`, and walks `node_modules/.aube/` — never writes the
//! project. After the report renders, fires the async update notifier
//! (network-bound on a cold cache, cached for 24 h thereafter), which
//! also writes `<cacheDir>/update-check.json` on a successful fetch.

use clap::Args;
use miette::Context;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube doctor
  version: 1.0.0-beta.4
  node: v22.11.0
  ...
  No problems found

  $ aube doctor --json | jq .warnings
";

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Emit a machine-readable JSON report instead of the grouped text.
    #[arg(long, short = 'J')]
    pub json: bool,
}

pub async fn run(args: DoctorArgs) -> miette::Result<Option<i32>> {
    let cwd = crate::dirs::cwd()?;
    let project_root = crate::dirs::find_project_root(&cwd);
    let anchor = project_root.clone().unwrap_or_else(|| cwd.clone());

    // Resolve the project's runtime so the report shows what scripts
    // would actually run on. A resolution failure is a finding, not a
    // crash — doctor exists to surface exactly these problems.
    let runtime_error = crate::runtime::ensure_for_cwd(&cwd).await.err();

    let mut report = build_report(&anchor, project_root.is_some())?;
    if let Some(e) = runtime_error {
        report
            .errors
            .push(format!("node runtime resolution failed: {e}"));
    }

    if args.json {
        print_json(&report);
    } else {
        print_human(&report);
    }

    crate::update_check::check_and_notify(&anchor).await;

    if !report.errors.is_empty() {
        // Exit 1 when the doctor found errors. Return the code for the
        // binary's single `std::process::exit` rather than terminating
        // here, keeping the command embed-safe.
        return Ok(Some(1));
    }
    Ok(None)
}

#[derive(Debug, Default)]
struct Report {
    sections: Vec<Section>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug)]
struct Section {
    title: &'static str,
    items: Vec<(String, String)>,
}

impl Section {
    fn new(title: &'static str) -> Self {
        Self {
            title,
            items: Vec::new(),
        }
    }

    fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.items.push((key.into(), value.into()));
    }
}

fn build_report(anchor: &Path, in_project: bool) -> miette::Result<Report> {
    let mut report = Report::default();

    report.sections.push(version_section());
    report.sections.push(runtime_section(&mut report.warnings));
    report.sections.push(dirs_section(anchor));

    if in_project {
        let project = project_section(anchor, &mut report);
        report.sections.push(project);
        check_install_state(anchor, &mut report);
        check_foreign_package_manager_dirs(anchor, &mut report);
        check_virtual_store_links(anchor, &mut report)?;
    } else {
        let mut s = Section::new("project");
        s.push(
            "status",
            "no package.json at or above the current directory",
        );
        report.sections.push(s);
    }

    report.sections.push(registry_section(anchor));

    Ok(report)
}

fn version_section() -> Section {
    let mut s = Section::new("version");
    let id = aube_util::embedder();
    s.push(id.name, id.version);
    s.push(
        "build-profile",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    s.push("target", std::env::consts::OS);
    s.push("arch", std::env::consts::ARCH);
    s
}

fn runtime_section(warnings: &mut Vec<String>) -> Section {
    let mut s = Section::new("runtime");
    match crate::engines::effective_node_version(None) {
        Some(v) => s.push("node", format!("v{v}")),
        None => {
            s.push("node", "(not found)");
            warnings.push(format!(
                "`node` is not available on PATH — lifecycle scripts and `{}` will fail",
                aube_util::cmd("run")
            ));
        }
    }
    if let Some(ctx) = crate::runtime::current() {
        s.push("node-source", ctx.source.label());
        if let Some(requested) = &ctx.requested {
            s.push("node-requested", requested.clone());
            s.push("node-provenance", ctx.provenance.label());
            if ctx.version.is_none() {
                warnings.push(format!(
                    "the project requests node {requested} (via {}) but the requirement is unsatisfied",
                    ctx.source.label()
                ));
            }
        }
        if let Some(bin) = &ctx.node_bin {
            s.push("node-bin", display_path_owned(bin.clone()));
        }
    }
    if let Some(shell) = std::env::var_os("SHELL")
        && let Some(text) = shell.to_str()
    {
        s.push("shell", text);
    }
    s
}

fn dirs_section(anchor: &Path) -> Section {
    let mut s = Section::new("dirs");
    let store_root = aube_store::dirs::store_dir()
        .map(display_path_owned)
        .unwrap_or_else(|| "(unresolved)".into());
    s.push("store", store_root);
    s.push(
        "cache",
        display_path_owned(super::resolved_cache_dir(anchor)),
    );
    s.push(
        "packument-cache",
        display_path_owned(super::resolved_cache_dir(anchor).join("packuments-v1")),
    );
    s.push(
        "virtual-store",
        display_path_owned(super::resolve_virtual_store_dir_for_cwd(anchor)),
    );
    s
}

fn project_section(anchor: &Path, report: &mut Report) -> Section {
    let mut s = Section::new("project");
    s.push("root", display_path_owned(anchor));

    match aube_manifest::PackageJson::from_path(&anchor.join("package.json")) {
        Ok(manifest) => {
            let label = match (&manifest.name, &manifest.version) {
                (Some(n), Some(v)) => format!("{n}@{v}"),
                (Some(n), None) => n.clone(),
                _ => "(unnamed)".into(),
            };
            s.push("package", label);
            // Self-version pin (packageManager / devEngines.packageManager).
            // Tool name from the active embedder (standalone aube → "aube").
            let name = aube_util::embedder().name;
            let self_pin = manifest
                .dev_engines
                .as_ref()
                .and_then(|d| d.aube_package_manager())
                .and_then(|e| e.version.clone())
                .map(|v| (v, "devEngines.packageManager"))
                .or_else(|| {
                    manifest
                        .extra
                        .get("packageManager")
                        .and_then(|v| v.as_str())
                        .and_then(|raw| raw.strip_prefix(&format!("{name}@")))
                        .map(|v| (v.to_string(), "packageManager"))
                });
            if let Some((pin, source)) = self_pin {
                s.push(format!("{name}-pin"), format!("{pin} (via {source})"));
            }
            if let Some(range) = manifest.engines.get("node")
                && let Some(node) = crate::engines::effective_node_version(None)
            {
                match (
                    node_semver::Version::parse(&node),
                    node_semver::Range::parse(range),
                ) {
                    (Ok(v), Ok(r)) if !v.satisfies(&r) => {
                        report.errors.push(format!(
                            "root package requires node {range}, but this is v{node}"
                        ));
                    }
                    _ => {}
                }
            }
            if let Some(pm) = manifest
                .extra
                .get("packageManager")
                .and_then(|v| v.as_str())
            {
                s.push("package-manager", pm);
            }
        }
        Err(err) => {
            report.errors.push(format!(
                "failed to parse package.json at {}: {err}",
                display_path_owned(anchor.join("package.json"))
            ));
        }
    }

    let lockfile = aube_lockfile::detect_existing_lockfile_kind(anchor);
    s.push(
        "lockfile",
        lockfile
            .map(|k| k.filename().to_string())
            .unwrap_or_else(|| {
                format!(
                    "(none — first install will create {})",
                    aube_util::lockfile_basename()
                )
            }),
    );

    s
}

fn registry_section(anchor: &Path) -> Section {
    let mut s = Section::new("registry");
    let config = super::load_npm_config(anchor);
    s.push("default", &config.registry);
    if !config.scoped_registries.is_empty() {
        let scoped = config
            .scoped_registries
            .iter()
            .map(|(k, v)| format!("{k} -> {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        s.push("scoped", scoped);
    }
    let client = super::make_client(anchor);
    let auth_state = if client.has_resolved_auth_for(&config.registry) {
        "configured"
    } else {
        "(none)"
    };
    s.push("auth", auth_state);
    s
}

fn check_install_state(anchor: &Path, report: &mut Report) {
    if let Some(reason) = crate::state::check_needs_install(anchor) {
        // "install state not found" on a project that has no
        // `node_modules/` yet is expected — it's informational, not an
        // error. Any *other* reason (lockfile/manifest hash drift,
        // missing modules dir on a state file that otherwise exists)
        // is a real staleness signal.
        let modules_dir = super::project_modules_dir(anchor);
        if modules_dir.exists() {
            report.warnings.push(format!(
                "node_modules is stale: {reason}. Run `{}`.",
                aube_util::cmd("install")
            ));
        }
    }
}

/// Scan for artifacts other package managers leave behind. aube
/// deliberately co-exists with whatever's already on disk — we never
/// reach into `.pnpm/` / `.yarn/` — but a user running two PMs against
/// the same project is a recipe for drift, so surface any leftovers as
/// warnings. We cover every layout that writes a marker at a known
/// path:
///
/// - `node_modules/.pnpm/`  → pnpm's isolated virtual store
/// - `<project>/.yarn/`     → yarn berry (zero-install cache / PnP support files)
/// - `<project>/.pnp.cjs`
///   or `<project>/.pnp.loader.mjs` → yarn PnP (no `node_modules` at all)
///
/// Yarn-classic and npm leave no distinctive marker — both write plain
/// flat `node_modules/` trees — so we can't detect them here. Users
/// running either against an aube project will still notice at
/// `aube install` time when the lockfile is rewritten.
fn check_foreign_package_manager_dirs(anchor: &Path, report: &mut Report) {
    let modules = super::project_modules_dir(anchor);
    if modules.join(".pnpm").is_dir() {
        report.warnings.push(
            "node_modules/.pnpm/ exists alongside aube's tree — this project was last installed with pnpm. aube and pnpm can co-exist, but expect both trees to drift unless you pick one."
                .to_string(),
        );
    }
    if anchor.join(".yarn").is_dir() {
        report.warnings.push(
            ".yarn/ exists at the project root — this project was last touched with yarn (berry or the zero-install flow). Safe to delete if you've committed to aube.".to_string(),
        );
    }
    if anchor.join(".pnp.cjs").is_file() || anchor.join(".pnp.loader.mjs").is_file() {
        report.warnings.push(
            "yarn PnP loader files (.pnp.cjs / .pnp.loader.mjs) are present — Node may still run from PnP instead of aube's node_modules/ until they're removed.".to_string(),
        );
    }
}

fn check_virtual_store_links(anchor: &Path, report: &mut Report) -> miette::Result<()> {
    let links = super::check::run_report(anchor).wrap_err("failed to walk the virtual store")?;
    if !links.issues.is_empty() {
        // The virtual-store leaf is `.{embedder().name}` (see
        // `aube-linker`'s `aube_dir`), so route the reported path through
        // the profile. Standalone aube → `node_modules/.aube/`.
        report.errors.push(format!(
            "{} broken {} in node_modules/.{}/ (run `{}` for details)",
            links.issues.len(),
            if links.issues.len() == 1 {
                "dependency link"
            } else {
                "dependency links"
            },
            aube_util::embedder().name,
            aube_util::cmd("check"),
        ));
    }
    Ok(())
}

fn display_path_owned(p: impl AsRef<Path>) -> String {
    let p = p.as_ref();
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(rest) = p.strip_prefix(PathBuf::from(&home))
    {
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

fn print_human(report: &Report) {
    for section in &report.sections {
        if section.items.is_empty() {
            continue;
        }
        println!("{}:", section.title);
        let max = section
            .items
            .iter()
            .map(|(k, _)| k.len())
            .max()
            .unwrap_or(0);
        for (k, v) in &section.items {
            println!("  {:<width$}  {}", k, v, width = max);
        }
        println!();
    }

    if !report.warnings.is_empty() {
        let label = if report.warnings.len() == 1 {
            "warning"
        } else {
            "warnings"
        };
        println!("{} {label}:", report.warnings.len());
        for (i, w) in report.warnings.iter().enumerate() {
            println!("  {}. {w}", i + 1);
        }
        println!();
    }

    if report.errors.is_empty() {
        println!("No problems found");
    } else {
        let label = if report.errors.len() == 1 {
            "problem"
        } else {
            "problems"
        };
        println!("{} {label} found:", report.errors.len());
        for (i, e) in report.errors.iter().enumerate() {
            println!("  {}. {e}", i + 1);
        }
    }
}

fn print_json(report: &Report) {
    let mut root = Map::new();
    let mut sections = Map::new();
    for section in &report.sections {
        let mut items = Map::new();
        for (k, v) in &section.items {
            items.insert(k.clone(), Value::String(v.clone()));
        }
        sections.insert(section.title.to_string(), Value::Object(items));
    }
    root.insert("sections".into(), Value::Object(sections));
    root.insert(
        "warnings".into(),
        Value::Array(report.warnings.iter().cloned().map(Value::String).collect()),
    );
    root.insert(
        "errors".into(),
        Value::Array(report.errors.iter().cloned().map(Value::String).collect()),
    );
    let out =
        serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_else(|_| "{}".to_string());
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_runs_outside_a_project() {
        let tmp = tempfile::tempdir().unwrap();
        let report = build_report(tmp.path(), false).unwrap();
        assert!(
            !report.sections.is_empty(),
            "expected at least version + dirs sections"
        );
    }

    #[test]
    fn doctor_runs_inside_a_minimal_project() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"name":"demo","version":"0.1.0"}"#,
        )
        .unwrap();
        let report = build_report(cwd, true).unwrap();
        assert!(report.sections.iter().any(|s| s.title == "project"));
    }
}
