//! `aube why <pkg>` — print reverse dependency chains.
//!
//! For each importer, walks its direct deps forward through the lockfile
//! graph, recording every chain that ends at a package matching the query.
//! Each chain is a path `root → ... → target`, which answers the question
//! "why is this package in my node_modules?".
//!
//! This is a pure read — no network, no filesystem mutation, no project lock.

use aube_lockfile::{DepType, LockfileGraph, dep_type_label};
use clap::Args;
use miette::{Context, miette};
use std::collections::{BTreeSet, HashSet};

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube why debug
  my-app@1.0.0 /home/user/project

  dependencies:
  express 4.19.2
  └── debug 2.6.9
  body-parser 1.20.2
  └── debug 2.6.9

  # Only follow chains starting at a devDependency
  $ aube why --dev typescript

  # Include each node's store path
  $ aube why --long debug

  # Tab-separated, one chain per line (pipe-friendly)
  $ aube why --parseable debug

  # JSON: an array of chain objects
  $ aube why --json debug
";

#[derive(Debug, Args)]
pub struct WhyArgs {
    /// Package name to search for (exact match against package names)
    pub package: String,

    /// Only follow chains that start at a devDependency
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Only follow chains that start at a production (or optional) dependency
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,

    /// Output as JSON — an array of chain objects
    #[arg(long, conflicts_with = "parseable")]
    pub json: bool,

    /// Append each node's `.aube/<dep_path>` store path to the tree output
    #[arg(long)]
    pub long: bool,

    /// Tab-separated output: one line per chain, `importer\tdep_type\tname@ver\t...`
    #[arg(long)]
    pub parseable: bool,
}

pub async fn run(
    args: WhyArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    // Workspace root wins over the nearest project root so `aube why`
    // run from inside `packages/foo/` reads the lockfile + manifest at
    // the workspace root instead of the subpackage (which has no
    // lockfile of its own).
    let cwd = crate::dirs::workspace_or_project_root()?;

    if !filter.is_empty() {
        return run_filtered(&cwd, &args, &filter);
    }

    // Yaml-only workspace roots have no `package.json`; fall back to
    // a default manifest so the lockfile parser sees the same shape.
    let manifest = super::load_manifest_or_default(&cwd)?;

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(());
        }
        Err(e) => {
            return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
        }
    };

    let filter = DepFilter::from_flags(args.prod, args.dev);
    let chains = collect_chains(&graph, &args.package, filter, None);
    let vstore_max_len = super::resolve_virtual_store_dir_max_length_for_cwd(&cwd);
    // Honor `virtualStoreDir` so `--long` prints the actual on-disk
    // location when the user has relocated the virtual store.
    let vstore_prefix = super::format_virtual_store_display_prefix(
        &super::resolve_virtual_store_dir_for_cwd(&cwd),
        &cwd,
    );
    print_result(
        &manifest,
        &cwd,
        &args,
        &chains,
        vstore_max_len,
        &vstore_prefix,
    );
    Ok(())
}

fn run_filtered(
    cwd: &std::path::Path,
    args: &WhyArgs,
    workspace_filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let workspace_root = crate::dirs::find_workspace_root(cwd).ok_or_else(|| {
        miette!(
            "{}: --filter requires a workspace root ({}, or package.json with a `workspaces` field) at or above {}",
            aube_util::cmd("why"),
            aube_util::workspace_markers(),
            cwd.display()
        )
    })?;

    let manifest = super::load_manifest_or_default(&workspace_root)?;

    let graph = match aube_lockfile::parse_lockfile(&workspace_root, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(());
        }
        Err(e) => {
            return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
        }
    };

    let workspace_pkgs = aube_workspace::find_workspace_packages(&workspace_root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "{}: --filter requires a workspace root ({}, or package.json with a `workspaces` field) at {}",
            aube_util::cmd("why"),
            aube_util::workspace_markers(),
            workspace_root.display()
        ));
    }

    let selected = aube_workspace::selector::select_workspace_packages(
        &workspace_root,
        &workspace_pkgs,
        workspace_filter,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if selected.is_empty() {
        return Err(miette!(
            "{}: filter {workspace_filter:?} did not match any workspace package",
            aube_util::cmd("why")
        ));
    }

    let mut importers = BTreeSet::new();
    for pkg in &selected {
        importers.insert(super::workspace_importer_path(&workspace_root, &pkg.dir)?);
    }

    let dep_filter = DepFilter::from_flags(args.prod, args.dev);
    let chains = collect_chains(&graph, &args.package, dep_filter, Some(&importers));
    let vstore_max_len = super::resolve_virtual_store_dir_max_length_for_cwd(&workspace_root);
    let vstore_prefix = super::format_virtual_store_display_prefix(
        &super::resolve_virtual_store_dir_for_cwd(&workspace_root),
        &workspace_root,
    );
    print_result(
        &manifest,
        &workspace_root,
        args,
        &chains,
        vstore_max_len,
        &vstore_prefix,
    );
    Ok(())
}

fn print_result(
    manifest: &aube_manifest::PackageJson,
    cwd: &std::path::Path,
    args: &WhyArgs,
    chains: &[Chain],
    vstore_max_len: usize,
    vstore_prefix: &str,
) {
    if chains.is_empty() {
        // Machine-readable formats must stay machine-readable when empty:
        // JSON emits `[]`, parseable emits nothing. Only the default tree
        // output gets the human-readable explanation.
        if args.json {
            println!("[]");
        } else if !args.parseable {
            println!(
                "Package \"{}\" is not in the dependency graph.",
                args.package
            );
        }
        return;
    }

    if args.json {
        print_json(chains, args.long);
    } else if args.parseable {
        print_parseable(chains, args.long);
    } else {
        print_tree(
            manifest,
            cwd,
            chains,
            args.long,
            vstore_max_len,
            vstore_prefix,
        );
    }
}

use super::DepFilter;

/// One reverse-dependency chain from a root direct dep down to a matching package.
#[derive(Debug, Clone)]
struct Chain {
    importer: String,
    dep_type: DepType,
    /// Full path from the root direct dep to the matched package.
    frames: Vec<Frame>,
}

#[derive(Debug, Clone)]
struct Frame {
    name: String,
    version: String,
    dep_path: String,
}

/// Walk every importer's direct deps, DFS into the lockfile graph, collect
/// every path whose tail package matches `target`.
fn collect_chains(
    graph: &LockfileGraph,
    target: &str,
    filter: DepFilter,
    importer_filter: Option<&BTreeSet<String>>,
) -> Vec<Chain> {
    let mut chains: Vec<Chain> = Vec::new();
    for (importer, roots) in &graph.importers {
        if let Some(importers) = importer_filter
            && !importers.contains(importer)
        {
            continue;
        }
        for root in roots {
            if !filter.keeps(root.dep_type) {
                continue;
            }
            let mut stack: Vec<Frame> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            walk(
                graph,
                &root.dep_path,
                target,
                importer,
                root.dep_type,
                &mut stack,
                &mut seen,
                &mut chains,
            );
        }
    }
    chains
}

#[allow(clippy::too_many_arguments)]
fn walk(
    graph: &LockfileGraph,
    dep_path: &str,
    target: &str,
    importer: &str,
    dep_type: DepType,
    stack: &mut Vec<Frame>,
    seen: &mut HashSet<String>,
    out: &mut Vec<Chain>,
) {
    let Some(pkg) = graph.get_package(dep_path) else {
        return;
    };
    if !seen.insert(dep_path.to_string()) {
        return;
    }
    stack.push(Frame {
        name: pkg.name.clone(),
        version: pkg.version.clone(),
        dep_path: dep_path.to_string(),
    });

    if pkg.name == target {
        out.push(Chain {
            importer: importer.to_string(),
            dep_type,
            frames: stack.clone(),
        });
    } else {
        // `pkg.dependencies` stores (name → version); the dep_path is
        // `{name}@{version}` (see resolver::dep_path_for, list::render_json).
        for (child_name, child_version) in &pkg.dependencies {
            let child_dep_path = format!("{child_name}@{child_version}");
            walk(
                graph,
                &child_dep_path,
                target,
                importer,
                dep_type,
                stack,
                seen,
                out,
            );
        }
    }

    stack.pop();
    seen.remove(dep_path);
}

// ------- rendering -------

fn print_tree(
    manifest: &aube_manifest::PackageJson,
    cwd: &std::path::Path,
    chains: &[Chain],
    long: bool,
    vstore_max_len: usize,
    vstore_prefix: &str,
) {
    let name = manifest.name.as_deref().unwrap_or("(unnamed)");
    let version = manifest.version.as_deref().unwrap_or("");
    println!("{name}@{version} {}", cwd.display());
    println!();

    // Group chains by (importer, dep_type) for section headers, matching
    // `aube list`'s layout. Preserve input order within each group so output
    // is stable when the lockfile is stable.
    let mut groups: std::collections::BTreeMap<(String, &'static str), Vec<&Chain>> =
        std::collections::BTreeMap::new();
    for chain in chains {
        let key = (chain.importer.clone(), dep_type_label(chain.dep_type));
        groups.entry(key).or_default().push(chain);
    }

    let mut first = true;
    for ((importer, label), group) in &groups {
        if !first {
            println!();
        }
        first = false;
        if importer == "." {
            println!("{label}:");
        } else {
            println!("{importer}: {label}:");
        }

        for chain in group.iter() {
            render_chain(chain, long, vstore_max_len, vstore_prefix);
        }
    }
}

/// Render one chain as a line of stems:
///   foo 1.2.0
///   └── bar 2.0.0
///       └── target 3.1.4
fn render_chain(chain: &Chain, long: bool, vstore_max_len: usize, vstore_prefix: &str) {
    for (i, frame) in chain.frames.iter().enumerate() {
        let extra = if long {
            format!(
                "  ({vstore_prefix}{})",
                aube_lockfile::dep_path_filename::dep_path_to_filename(
                    &frame.dep_path,
                    vstore_max_len,
                )
            )
        } else {
            String::new()
        };
        if i == 0 {
            println!("{} {}{}", frame.name, frame.version, extra);
        } else {
            let indent = "    ".repeat(i - 1);
            println!("{indent}└── {} {}{}", frame.name, frame.version, extra);
        }
    }
}

/// One tab-separated line per chain:
///   importer\tdepType\tname@ver\tname@ver\t...
/// With `--long`, each `name@ver` cell is suffixed with `|<dep_path>` so
/// the store path survives a tab-split without needing a separate column.
fn print_parseable(chains: &[Chain], long: bool) {
    for chain in chains {
        let mut parts: Vec<String> = Vec::with_capacity(chain.frames.len() + 2);
        parts.push(chain.importer.clone());
        parts.push(dep_type_label(chain.dep_type).to_string());
        for frame in &chain.frames {
            if long {
                parts.push(format!(
                    "{}@{}|{}",
                    frame.name, frame.version, frame.dep_path
                ));
            } else {
                parts.push(format!("{}@{}", frame.name, frame.version));
            }
        }
        println!("{}", parts.join("\t"));
    }
}

fn print_json(chains: &[Chain], long: bool) {
    // Build a minimal JSON-compatible representation via serde_json::Value.
    // Not using a #[derive(Serialize)] struct because the shape is small and
    // the command currently has no other serde surface.
    let arr: Vec<serde_json::Value> = chains
        .iter()
        .map(|chain| {
            let frames: Vec<serde_json::Value> = chain
                .frames
                .iter()
                .map(|frame| {
                    let mut obj = serde_json::Map::new();
                    obj.insert("name".into(), frame.name.clone().into());
                    obj.insert("version".into(), frame.version.clone().into());
                    if long {
                        obj.insert("depPath".into(), frame.dep_path.clone().into());
                    }
                    serde_json::Value::Object(obj)
                })
                .collect();
            let mut obj = serde_json::Map::new();
            obj.insert("importer".into(), chain.importer.clone().into());
            obj.insert("depType".into(), dep_type_label(chain.dep_type).into());
            obj.insert("chain".into(), frames.into());
            serde_json::Value::Object(obj)
        })
        .collect();
    let json = serde_json::to_string_pretty(&serde_json::Value::Array(arr))
        .unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DirectDep, LockedPackage, LockfileGraph};
    use std::collections::BTreeMap;

    fn make_graph() -> LockfileGraph {
        // root → foo@1.0.0 → bar@2.0.0 → target@3.0.0
        //      → baz@1.0.0 → target@3.0.0 (diamond)
        //
        // Use `..Default::default()` to avoid churning this helper every
        // time LockedPackage gains a field (e.g. peer_dependencies from #40).
        fn pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> LockedPackage {
            let mut dependencies = BTreeMap::new();
            for (n, v) in deps {
                dependencies.insert((*n).to_string(), (*v).to_string());
            }
            LockedPackage {
                name: name.to_string(),
                version: version.to_string(),
                integrity: None,
                dependencies,
                dep_path: format!("{name}@{version}"),
                ..Default::default()
            }
        }

        let mut packages = BTreeMap::new();
        packages.insert(
            "foo@1.0.0".to_string(),
            pkg("foo", "1.0.0", &[("bar", "2.0.0")]),
        );
        packages.insert(
            "bar@2.0.0".to_string(),
            pkg("bar", "2.0.0", &[("target", "3.0.0")]),
        );
        packages.insert(
            "baz@1.0.0".to_string(),
            pkg("baz", "1.0.0", &[("target", "3.0.0")]),
        );
        packages.insert("target@3.0.0".to_string(), pkg("target", "3.0.0", &[]));

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".into(),
                    dep_path: "foo@1.0.0".into(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".into()),
                },
                DirectDep {
                    name: "baz".into(),
                    dep_path: "baz@1.0.0".into(),
                    dep_type: DepType::Dev,
                    specifier: Some("^1.0.0".into()),
                },
            ],
        );

        LockfileGraph {
            importers,
            packages,
            ..Default::default()
        }
    }

    #[test]
    fn finds_both_diamond_paths() {
        let graph = make_graph();
        let chains = collect_chains(&graph, "target", DepFilter::All, None);
        assert_eq!(chains.len(), 2);
        // One chain via foo → bar → target, one via baz → target
        let names: Vec<Vec<&str>> = chains
            .iter()
            .map(|c| c.frames.iter().map(|f| f.name.as_str()).collect())
            .collect();
        assert!(names.contains(&vec!["foo", "bar", "target"]));
        assert!(names.contains(&vec!["baz", "target"]));
    }

    #[test]
    fn prod_filter_excludes_dev_roots() {
        let graph = make_graph();
        let chains = collect_chains(&graph, "target", DepFilter::ProdOnly, None);
        // Should only get foo → bar → target (baz is Dev)
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].frames[0].name, "foo");
    }

    #[test]
    fn dev_filter_keeps_only_dev_roots() {
        let graph = make_graph();
        let chains = collect_chains(&graph, "target", DepFilter::DevOnly, None);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].frames[0].name, "baz");
        assert_eq!(chains[0].dep_type, DepType::Dev);
    }

    #[test]
    fn nonexistent_package_returns_empty() {
        let graph = make_graph();
        let chains = collect_chains(&graph, "nope", DepFilter::All, None);
        assert!(chains.is_empty());
    }

    #[test]
    fn direct_dep_match_produces_single_frame() {
        let graph = make_graph();
        let chains = collect_chains(&graph, "foo", DepFilter::All, None);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].frames.len(), 1);
        assert_eq!(chains[0].frames[0].name, "foo");
    }
}
