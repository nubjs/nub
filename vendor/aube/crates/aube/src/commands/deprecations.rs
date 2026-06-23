//! `aube deprecations` — report deprecated packages in the resolved graph.
//!
//! Reads the project's lockfile, re-queries each resolved package's
//! packument from the registry (hitting the on-disk packument cache with
//! ETag revalidation — no extra cost if the install just ran), and prints
//! anything the registry currently flags as `deprecated`. Direct deps are
//! shown by default; `--transitive` includes the rest. `allowedDeprecatedVersions`
//! in `package.json` (plus its `pnpm.` / `aube.` nested variants) mutes
//! matching ranges.

use super::{make_client, packument_cache_dir};
use crate::deprecations::{DeprecationRecord, classify};
use aube_lockfile::{LockfileGraph, dep_type_label};
use aube_registry::Packument;
use aube_resolver::is_deprecation_allowed;
use clap::Args;
use clx::style;
use miette::{Context, IntoDiagnostic};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

#[derive(Debug, Args)]
pub struct DeprecationsArgs {
    /// Exit with a non-zero status if any deprecations are found.
    #[arg(long)]
    pub exit_code: bool,

    /// Emit JSON instead of the default text layout.
    #[arg(long)]
    pub json: bool,

    /// Include transitive dependencies as well as direct ones.
    #[arg(long)]
    pub transitive: bool,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Serialize)]
struct JsonEntry {
    name: String,
    version: String,
    dep_path: String,
    direct: bool,
    message: String,
}

pub async fn run(args: DeprecationsArgs) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    let cwd = crate::dirs::project_root()?;

    let manifest = super::load_manifest(&cwd.join("package.json"))?;

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(Some(0));
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    let allowed = manifest.allowed_deprecated_versions();

    // Packages we need packuments for. Keyed by *registry name* — the
    // real name behind an npm-alias, since `"my-lodash": "npm:lodash@^4"`
    // would 404 a packument fetch under `my-lodash`. In direct-only
    // mode we narrow to the names actually listed by importers (via
    // their dep_path → LockedPackage lookup), which already correctly
    // picks up aliased direct deps.
    let target_registry_names: BTreeSet<String> = if args.transitive {
        graph
            .packages
            .values()
            .filter(|p| p.local_source.is_none())
            .map(|p| p.registry_name().to_string())
            .collect()
    } else {
        graph
            .importers
            .values()
            .flat_map(|deps| deps.iter())
            .filter_map(|d| graph.packages.get(&d.dep_path))
            .filter(|p| p.local_source.is_none())
            .map(|p| p.registry_name().to_string())
            .collect()
    };

    if target_registry_names.is_empty() {
        return emit_empty(args.json, args.exit_code, args.transitive);
    }

    let client = Arc::new(make_client(&cwd));
    let cache_dir = packument_cache_dir();

    // Parallel packument fetch. Failures are per-name — one unreachable
    // package shouldn't sink the whole report.
    let mut set = tokio::task::JoinSet::new();
    for name in &target_registry_names {
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        let name = name.clone();
        set.spawn(async move {
            let result = client.fetch_packument_cached(&name, &cache_dir).await;
            (name, result)
        });
    }
    let mut packuments: HashMap<String, Packument> =
        HashMap::with_capacity(target_registry_names.len());
    while let Some(res) = set.join_next().await {
        let (name, result) = res.into_diagnostic().wrap_err("packument fetch panicked")?;
        match result {
            Ok(p) => {
                packuments.insert(name, p);
            }
            Err(e) => eprintln!("warn: failed to fetch packument for {name}: {e}"),
        }
    }

    // Walk the graph (rather than the packument map) so the report
    // preserves lockfile order and honors dedupe on (name, version).
    // `registry_name()` is what keys the packument map; `pkg.name` is
    // what the user sees and what `allowedDeprecatedVersions` matches.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut records: Vec<DeprecationRecord> = Vec::new();
    for pkg in graph.packages.values() {
        if pkg.local_source.is_some() {
            continue;
        }
        if !seen.insert((pkg.name.clone(), pkg.version.clone())) {
            continue;
        }
        let Some(packument) = packuments.get(pkg.registry_name()) else {
            continue;
        };
        let Some(version_meta) = packument.versions.get(&pkg.version) else {
            continue;
        };
        let Some(msg) = version_meta.deprecated.as_deref() else {
            continue;
        };
        if is_deprecation_allowed(&pkg.name, &pkg.version, &allowed) {
            continue;
        }
        records.push(DeprecationRecord {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            dep_path: pkg.dep_path.clone(),
            message: Arc::<str>::from(msg),
        });
    }

    let (direct, transitive) = classify(&records, &graph);
    // In direct-only mode, "nothing to report" means zero *direct*
    // records — a deprecated transitive that happens to share a name
    // with a fetched direct packument would otherwise render as an
    // empty header followed by the trailing "not checked" hint, which
    // reads as if nothing was found.
    let scope_empty = if args.transitive {
        records.is_empty()
    } else {
        direct.is_empty()
    };
    if scope_empty {
        return emit_empty(args.json, args.exit_code, args.transitive);
    }

    if args.json {
        render_json(&direct, &transitive, args.transitive)?;
    } else {
        render_text(&direct, &transitive, args.transitive, &graph);
    }

    if args.exit_code {
        return Ok(Some(1));
    }
    Ok(None)
}

fn emit_empty(
    json: bool,
    exit_code: bool,
    include_transitive: bool,
) -> miette::Result<Option<i32>> {
    if json {
        println!("[]");
    } else if include_transitive {
        eprintln!("No deprecated packages.");
    } else {
        // Direct-only mode: be explicit that transitive deps weren't
        // queried — the install-time summary hints at `aube deprecations`
        // for the full view, and silently claiming "no deprecated
        // packages" here would undermine that.
        eprintln!(
            "No deprecated direct dependencies. Run with --transitive to include transitive deps."
        );
    }
    if exit_code {
        // Empty means "nothing to flag" — exit 0 even with --exit-code.
        return Ok(Some(0));
    }
    Ok(None)
}

fn render_text(
    direct: &[&DeprecationRecord],
    transitive: &[&DeprecationRecord],
    include_transitive: bool,
    graph: &LockfileGraph,
) {
    for r in direct {
        let origin = describe_direct_origin(r, graph);
        println!(
            "{} {}@{}{}",
            style::eyellow("deprecated").bold(),
            r.name,
            r.version,
            match origin {
                Some(o) => format!(" ({o})"),
                None => String::new(),
            }
        );
        println!("  {}", r.message);
        println!();
    }
    if include_transitive {
        for r in transitive {
            println!(
                "{} {}@{} ({})",
                style::eyellow("deprecated").bold(),
                r.name,
                r.version,
                style::edim("transitive"),
            );
            println!("  {}", r.message);
            println!();
        }
        return;
    }
    // Direct-only mode. We didn't fetch transitive packuments, so we
    // can't print a count — but we still want to surface that the
    // report isn't exhaustive, mirroring the install-time hint.
    eprintln!(
        "{}",
        style::edim("Transitive deps weren't checked. Run with --transitive for the full view.")
    );
}

fn render_json(
    direct: &[&DeprecationRecord],
    transitive: &[&DeprecationRecord],
    include_transitive: bool,
) -> miette::Result<()> {
    let mut entries: Vec<JsonEntry> = Vec::new();
    for r in direct {
        entries.push(JsonEntry {
            name: r.name.clone(),
            version: r.version.clone(),
            dep_path: r.dep_path.clone(),
            direct: true,
            message: r.message.to_string(),
        });
    }
    if include_transitive {
        for r in transitive {
            entries.push(JsonEntry {
                name: r.name.clone(),
                version: r.version.clone(),
                dep_path: r.dep_path.clone(),
                direct: false,
                message: r.message.to_string(),
            });
        }
    }
    let json = serde_json::to_string_pretty(&entries)
        .into_diagnostic()
        .wrap_err("failed to serialize JSON output")?;
    println!("{json}");
    Ok(())
}

fn describe_direct_origin(r: &DeprecationRecord, graph: &LockfileGraph) -> Option<String> {
    // Summarize which importer(s) declare this direct dep. Typical
    // monorepo: multiple importers list the same name; show the first
    // one plus a `(+N more)` tail when the fanout is wider. Match on
    // the resolved `LockedPackage` (via `d.dep_path`) rather than
    // `d.name == r.name` so aliased direct deps — where `DirectDep.name`
    // differs from the record's name on some lockfile formats — still
    // light up.
    let mut importers: Vec<(&String, &aube_lockfile::DirectDep)> = Vec::new();
    for (imp, deps) in &graph.importers {
        if let Some(d) = deps.iter().find(|d| {
            graph
                .packages
                .get(&d.dep_path)
                .is_some_and(|pkg| pkg.name == r.name && pkg.version == r.version)
        }) {
            importers.push((imp, d));
        }
    }
    if importers.is_empty() {
        return None;
    }
    let (first_imp, first_dep) = importers[0];
    let dep_type = dep_type_label(first_dep.dep_type);
    let rest = importers.len().saturating_sub(1);
    // `.` is the root importer; reading "via . > devDependencies" is
    // ugly, so collapse the root to a clearer phrase.
    let imp_display: BTreeMap<&str, &str> = BTreeMap::from([(".", "package.json")]);
    let imp_label = imp_display
        .get(first_imp.as_str())
        .copied()
        .unwrap_or(first_imp.as_str());
    let suffix = if rest > 0 {
        format!(" +{rest} more")
    } else {
        String::new()
    };
    Some(format!("via {imp_label} > {dep_type}{suffix}"))
}
