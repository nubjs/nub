//! `aube fetch` — populate the global store from a lockfile without linking.
//!
//! Mirrors pnpm's `pnpm fetch`: reads `pnpm-lock.yaml` (or any other
//! supported lockfile) and downloads every tarball into the global
//! content-addressable store. It does **not** create `node_modules/`,
//! and it does **not** require `package.json` to exist. The primary use
//! case is Docker builds that want to cache dependencies in an early
//! layer keyed only on the lockfile.

use super::{install, make_client};
use aube_lockfile::{DepType, LockedPackage};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Args)]
pub struct FetchArgs {
    /// Only fetch devDependencies
    #[arg(long, short = 'D', conflicts_with = "prod")]
    pub dev: bool,

    /// Only fetch production + optional dependencies (skip devDependencies)
    #[arg(long, short = 'P', conflicts_with = "dev")]
    pub prod: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(args: FetchArgs) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    let cwd = crate::dirs::project_root_or_cwd()?;
    let _lock = super::take_project_lock(&cwd)?;
    let start = std::time::Instant::now();

    // package.json is optional — aube fetch is meant to run in a directory
    // that only has a lockfile (Docker cache-warming layer). When present,
    // it's needed so yarn.lock can classify direct deps; for pnpm-lock.yaml
    // the manifest is effectively unused by the parser.
    let manifest_path = cwd.join("package.json");
    let manifest = if manifest_path.exists() {
        super::load_manifest(&manifest_path)?
    } else {
        // Minimal empty manifest so parse_lockfile_with_kind's signature is happy.
        serde_json::from_str::<aube_manifest::PackageJson>("{}").into_diagnostic()?
    };

    let (graph, kind) = match aube_lockfile::parse_lockfile_with_kind(&cwd, &manifest) {
        Ok(pair) => pair,
        Err(aube_lockfile::Error::NotFound(_)) => {
            return Err(miette!(
                "no lockfile found — {} requires a lockfile \
                 (pnpm-lock.yaml, yarn.lock, package-lock.json, npm-shrinkwrap.json, or bun.lock)",
                aube_util::cmd("fetch")
            ));
        }
        Err(e) => {
            return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
        }
    };

    // yarn.lock and bun.lock don't record dep type on their own entries —
    // classification is derived from package.json at parse time. Without a
    // manifest, `--prod`/`--dev` would silently fetch zero packages because
    // `root_deps()` is empty. Error loudly instead.
    if (args.prod || args.dev)
        && !manifest_path.exists()
        && matches!(
            kind,
            aube_lockfile::LockfileKind::Yarn
                | aube_lockfile::LockfileKind::YarnBerry
                | aube_lockfile::LockfileKind::Bun
        )
    {
        return Err(miette!(
            "--prod/--dev filtering requires package.json when reading {}\n\
             help: add package.json (even an empty `{{}}` with dependencies), \
             or omit the flag to fetch all packages",
            kind.filename()
        ));
    }

    let total_packages = graph.packages.len();

    // Pick which packages to fetch based on --prod / --dev.
    // Default: everything in the lockfile.
    let filtered: BTreeMap<String, LockedPackage> = if args.prod || args.dev {
        let seed_types: &[DepType] = if args.prod {
            &[DepType::Production, DepType::Optional]
        } else {
            &[DepType::Dev]
        };
        let closure = dep_closure(&graph, seed_types);
        graph
            .packages
            .iter()
            .filter(|(k, _)| closure.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    } else {
        graph.packages.clone()
    };

    eprintln!(
        "Fetching {} of {} from {}",
        filtered.len(),
        pluralizer::pluralize("package", total_packages as isize, true),
        kind.filename()
    );

    let store = std::sync::Arc::new(super::open_store(&cwd)?);
    let client = std::sync::Arc::new(make_client(&cwd));

    // `aube fetch` is a prefetch-only operation: the user is asking
    // us to populate the global store without running any scripts
    // against their tree. Pass `ignore_scripts=true` so git deps
    // with a `prepare` script get imported as-is instead of
    // triggering a nested install inside the clone.
    //
    // `git_shallow_hosts` is resolved from the project's `.npmrc` /
    // `pnpm-workspace.yaml` here before we hand the list off to
    // `fetch_packages`, so callers that invoke `aube fetch` from
    // inside a project pick up that project's configuration without
    // any extra CLI plumbing.
    let files = crate::commands::FileSources::load(&cwd);
    let raw_workspace = aube_manifest::workspace::load_both(&cwd)
        .map(|(_, raw)| raw)
        .unwrap_or_default();
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    let git_shallow_hosts = aube_settings::resolved::git_shallow_hosts(&ctx);
    let (_indices, cached, fetched) = install::fetch_packages(
        &filtered,
        &store,
        client,
        None,
        /*ignore_scripts=*/ true,
        /*git_prepare_depth=*/ 0,
        git_shallow_hosts,
    )
    .await?;

    eprintln!(
        "Fetched {} package{} ({cached} cached, {fetched} downloaded) in {:.0?}",
        filtered.len(),
        if filtered.len() == 1 { "" } else { "s" },
        start.elapsed()
    );

    Ok(())
}

/// BFS the lockfile graph from the set of root direct deps whose `DepType`
/// is in `seed_types`, walking through `LockedPackage.dependencies` to
/// pull in transitives. Returns the set of dep_paths reachable.
fn dep_closure(
    graph: &aube_lockfile::LockfileGraph,
    seed_types: &[DepType],
) -> std::collections::HashSet<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for dep in graph.root_deps() {
        if seed_types.contains(&dep.dep_type) && seen.insert(dep.dep_path.clone()) {
            queue.push_back(dep.dep_path.clone());
        }
    }

    while let Some(dep_path) = queue.pop_front() {
        let Some(pkg) = graph.packages.get(&dep_path) else {
            continue;
        };
        // `LockedPackage.dependencies` values are dep_path *tails* —
        // the string that follows `<name>@` in the child's dep_path.
        // Recombine with the child name to rebuild the key we'll look
        // up in `graph.packages`. Skipping this recombine would search
        // the graph by the tail alone (e.g. `"6.0.0"`), miss every
        // transitive, and `fetch --prod` would underreport its count.
        for (child_name, child_tail) in &pkg.dependencies {
            let child_path = format!("{child_name}@{child_tail}");
            if seen.insert(child_path.clone()) {
                queue.push_back(child_path);
            }
        }
    }

    seen
}
