//! `aube outdated` — compare installed versions against the registry.
//!
//! Reads the root importer's direct deps from the lockfile, fetches each
//! package's packument, and prints the ones whose current resolved version
//! lags behind what an install would land on. Mirrors `pnpm outdated`'s
//! default table layout.
//!
//! Both version columns run through the resolver's own picker, so a
//! `minimumReleaseAge` window moves them exactly as it moves an install and
//! the report cannot offer an upgrade `nub update` would refuse (#722). A
//! window in effect also moves the fetch from the abbreviated packument to the
//! full one, the only source of the per-version publish times the window is
//! checked against. Both tiers are disk-cached.
//!
//! Pure read: no state changes, no `node_modules/` writes, no project lock.

use super::{DepFilter, make_client, packument_cache_dir, packument_full_cache_dir};
use aube_lockfile::{DepType, DirectDep, dep_type_label};
use aube_registry::Packument;
use clap::Args;
use miette::{Context, IntoDiagnostic};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube outdated
  Package     Current  Wanted   Latest
  lodash      4.17.20  4.17.21  4.17.21
  typescript  5.3.3    5.3.3    5.4.5
  zod         3.22.4   3.22.4   3.23.8

  # Also print the package.json specifier and dep type
  $ aube outdated --long
  Package     Current  Wanted   Latest
  lodash      4.17.20  4.17.21  4.17.21
  typescript  5.3.3    5.3.3    5.4.5

    lodash (dependencies): ^4.17.20
    typescript (devDependencies): ^5.3.0

  # Filter by prefix
  $ aube outdated '@babel/*'

  # Machine-readable (pnpm-compatible shape)
  $ aube outdated --json
  {
    \"lodash\": {
      \"current\": \"4.17.20\",
      \"wanted\": \"4.17.21\",
      \"latest\": \"4.17.21\"
    }
  }

  # Nothing to report exits 0
  $ aube outdated
  All dependencies up to date.
";

#[derive(Debug, Args)]
pub struct OutdatedArgs {
    /// Optional package name (prefix match) to filter the report
    pub pattern: Option<String>,

    /// Show only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Check globally-installed packages instead of the current project.
    #[arg(short = 'g', long, conflicts_with = "workspace_root")]
    pub global: bool,

    /// Emit a JSON object keyed by package name instead of the default table
    #[arg(long)]
    pub json: bool,

    /// Also show deps whose `wanted` version matches the installed version
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
    /// Operate on the workspace root regardless of cwd.
    ///
    /// Mirrors pnpm's `-w/--workspace-root`: from a sub-package,
    /// `aube outdated -w` reports the root manifest's deps instead
    /// of the sub-package's. No-op when paired with `-r` / `--filter`
    /// (those already drive workspace selection from the root).
    #[arg(short = 'w', long = "workspace-root", visible_alias = "workspace")]
    pub workspace_root: bool,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
}

#[derive(Debug, Serialize)]
struct Row {
    // Skipped on serialize — the outer `render_json` map is keyed by
    // name, so duplicating it inside each entry would diverge from
    // pnpm's `{ "<name>": { ... } }` shape.
    #[serde(skip)]
    name: String,
    current: String,
    wanted: String,
    latest: String,
    #[serde(rename = "dependencyType", serialize_with = "serialize_dep_type")]
    dep_type: DepType,
    // Whether a `latest` is being reported at all. False when the packument
    // carried no `latest` dist-tag, and also when the window admits no version
    // for that column — in both cases `latest` is the human-facing "(unknown)"
    // sentinel (visible only under `--long`, since a row with no drift is
    // otherwise not printed) and the drift check ignores it, so neither case
    // flips exit code 1.
    #[serde(skip)]
    latest_known: bool,
    #[serde(skip)]
    specifier: Option<String>,
    #[serde(skip)]
    importer: Option<String>,
}

/// Resolve the project's effective `minimumReleaseAge` configuration.
///
/// `outdated` is a pure read and builds no resolver, so it goes to the same
/// settings accessor `install`/`add` use rather than standing up a
/// [`aube_resolver::Resolver`] it would never resolve with. `None` means no
/// window is in effect and every pick below stays on today's ungated path.
fn age_gate_for(cwd: &Path) -> Option<aube_resolver::MinimumReleaseAge> {
    let files = super::FileSources::load(cwd);
    let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
    let ctx = files.ctx(&raw_workspace, aube_settings::values::process_env(), &[]);
    super::install::resolve_minimum_release_age(&ctx, None)
}

/// The `Latest` column: the newest version the window admits, bounded by the
/// `latest` dist-tag.
///
/// A packument with no `latest` tag yields `None` and the column stays
/// unknown, which keeps it out of the drift decision exactly as it was before
/// any window existed. That guard is load-bearing rather than defensive:
/// `pick_version` answers a literal `latest` range it cannot resolve by falling
/// back to `highest_stable_version`, which reads version keys and never
/// consults a dist-tag — so passing an absent tag through would SYNTHESIZE one
/// and start flipping the exit code for registries that publish no `latest`.
/// Nub pins the window on for every project, so that would be the default path.
fn latest_pick(
    packument: &Packument,
    registry_name: &str,
    gate: Option<&aube_resolver::MinimumReleaseAge>,
    current: &str,
) -> Option<String> {
    let tagged = packument.dist_tags.get("latest").cloned();
    tagged.as_ref()?;
    // Ranged on the tag rather than on `*`: `pick_version` bounds a gated
    // `latest` at the tagged version (#681), so the fallback can never surface
    // a higher major the publisher had already untagged.
    // The undeterminable flag is deliberately dropped: it describes the
    // `latest` tag's own candidate set, which no message keys on. See the
    // warning's rationale in `collect_rows`.
    let picked = gated_pick(packument, registry_name, "latest", gate, tagged).0?;

    // Never offer a DOWNGRADE. That same widening walks DOWNWARD looking for a
    // release old enough to clear the window, so a window wider than the
    // installed version's own age lands below `current` — and the column then
    // advertises an older version as the one to move to, counts as drift, and
    // pins the exit code at 1 with nothing worth installing on offer. That is
    // the dead end #722 was about, reached by a different route.
    //
    // "worth installing" rather than "installable": the lower release IS
    // reachable — `update --latest` passes the literal `latest` range through
    // the same widening (`update.rs:634`) and resolves to it.
    //
    // A pick at or below `current` reports `current` instead of dropping to
    // unknown: there genuinely is no upgrade, and saying so is both truer and
    // what lets the command exit 0.
    //
    // Accepted cost: a publisher who ROLLS BACK the tag — ships 3.0.0, retracts
    // it, re-tags 2.9.1 — no longer surfaces here to someone already on 3.0.0.
    // That signal was never dependable in this column, since it appears only
    // when the installed version happens to sit above the tag, and a retraction
    // reaches the user through deprecation metadata instead.
    let (Ok(new), Ok(cur)) = (
        node_semver::Version::parse(&picked),
        node_semver::Version::parse(current),
    ) else {
        // Either side unparseable means the two are not ordered at all, so leave
        // the pick alone rather than guess a direction. Two known ways `current`
        // gets there: the `(missing)` sentinel for a dep absent from the graph,
        // and a git dep read from an npm v1 lockfile, whose `version` the legacy
        // lifter carries through verbatim (`aube-lockfile/src/npm/read.rs`).
        // Not exhaustive — nub reads six lockfile formats and only pnpm's is
        // known to normalize a local dep to a parseable `0.0.0`.
        return Some(picked);
    };
    Some(if new < cur { current.to_string() } else { picked })
}

/// The version a column should show: what an install would actually land on.
///
/// `ungated` is what the column shows with no window in effect. The gated pick
/// runs through [`aube_resolver::pick_version_for_add`] — the exact entry point
/// `add` uses — so a column can never advertise a version `install`/`update`
/// would then decline (#722).
///
/// The window is applied SILENTLY, with no marker and no note. It is the
/// project's own configured policy and `install`/`update` honor it without
/// remark; a report that editorialized about it on every run would be noise,
/// and under nub's 24-hour default that is most runs. A version held back is
/// simply not offered.
///
/// Returns the version to show, plus whether the window's refusal was
/// `Undeterminable` — the two `AgeGated` causes are NOT interchangeable here.
///
/// `TooNew` is the policy working: the version ages out within the window and
/// the report stays silent, so nothing is offered and no row appears.
///
/// `Undeterminable` is a metadata failure, not a policy outcome. The registry
/// served no publish time, so the gate fails closed and `install`/`update`
/// hard-error with a DIFFERENT error and disjoint remedies (`Error::
/// ReleaseAgeMissingTime`, #581). Staying silent there would print `All
/// dependencies up to date.` for a project where every install refuses — the
/// report disagreeing with the installer, which is the whole of #722. The
/// `wanted` caller warns instead — and only that one, since only the manifest
/// range predicts plain `update` — on stderr beside the existing
/// packument-fetch warning; stdout stays data.
fn gated_pick(
    packument: &Packument,
    registry_name: &str,
    range: &str,
    gate: Option<&aube_resolver::MinimumReleaseAge>,
    ungated: Option<String>,
) -> (Option<String>, bool) {
    let Some(gate) = gate else {
        return (ungated, false);
    };
    match aube_resolver::pick_version_for_add(packument, registry_name, range, Some(gate)) {
        aube_resolver::PickResult::Found(meta) => (Some(meta.version.clone()), false),
        aube_resolver::PickResult::AgeGated(aube_resolver::AgeGateCause::Undeterminable) => {
            (None, true)
        }
        aube_resolver::PickResult::AgeGated(_) => (None, false),
        // The range itself matches nothing (`workspace:`/`file:`, a git URL).
        // Not an age verdict; leave today's fallback in place.
        aube_resolver::PickResult::NoMatch => (ungated, false),
    }
}

/// Serialize `DepType` using pnpm's `package.json` field names so
/// `outdated --json` is a drop-in match for `pnpm outdated --json`.
fn serialize_dep_type<S: serde::Serializer>(dt: &DepType, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(dep_type_label(*dt))
}

pub async fn run(
    args: OutdatedArgs,
    mut filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    args.network.install_overrides();
    if args.global {
        if !filter.is_empty() {
            return Err(miette::miette!(
                "{}: --global cannot be used with --recursive or --filter",
                aube_util::cmd("outdated")
            ));
        }
        return run_global(args).await;
    }

    let mut cwd = crate::dirs::project_root()?;
    if !filter.is_empty() {
        // Discussion #602: include the workspace root in `outdated -r`
        // by default. pnpm parity here is strict (root is opt-in via
        // `include-workspace-root: true`), but for read-only audits
        // the surprise of "where are my root deps?" outweighs the
        // parity concern.
        filter.include_workspace_root = true;
        return run_filtered(&cwd, args, &filter).await;
    }
    // `-w/--workspace-root`: retarget the report at the workspace
    // root manifest, regardless of which sub-package the user ran
    // from. Mirrors `pnpm -w outdated`. No-op when no workspace root
    // exists above cwd (single-project install) so the flag is safe
    // to leave in shell aliases.
    if args.workspace_root
        && let Some(root) = crate::dirs::find_workspace_root(&cwd)
    {
        cwd = root;
    }
    // Match pnpm: exit 1 when any dependency is outdated so CI patterns
    // like `aube outdated || exit 1` behave the same. The code is
    // returned for the binary's single `std::process::exit` rather than
    // exited in place, keeping the command embed-safe.
    if run_one(&cwd, args, None).await? {
        Ok(Some(1))
    } else {
        Ok(None)
    }
}

async fn run_filtered(
    cwd: &Path,
    args: OutdatedArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<Option<i32>> {
    let (root, matched) = super::select_workspace_packages(cwd, filter, "outdated")?;
    let manifest = super::load_manifest(&root.join("package.json"))?;
    let graph = match aube_lockfile::parse_lockfile(&root, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(None);
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };
    let mut any_drift = false;
    let mut printed_table = false;
    let root_files = super::FileSources::load(&root);
    let raw_workspace = aube_manifest::workspace::load_raw(&root).unwrap_or_else(|error| {
        tracing::debug!(
            %error,
            workspace_root = %root.display(),
            "ignoring invalid workspace config while resolving outdated settings"
        );
        BTreeMap::new()
    });
    let env = aube_settings::values::process_env();
    for pkg in matched {
        let importer = pkg
            .name
            .clone()
            .unwrap_or_else(|| pkg.dir.display().to_string());
        let importer_path = super::workspace_importer_path(&root, &pkg.dir)?;
        let roots = graph
            .importers
            .get(&importer_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut files = root_files.clone();
        if pkg.dir != root {
            files.extend_project_sources(&pkg.dir);
        }
        let ctx = files.ctx(&raw_workspace, env, &[]);
        let ignored = super::update::ignored_update_dependencies_from_ctx(&ctx, &pkg.manifest);
        let selected_roots: Vec<DirectDep> = roots
            .iter()
            .filter(|dep| !ignored.contains(&dep.name))
            .cloned()
            .collect();
        // Discussion #602: separate per-importer tables with a blank
        // line so the headers don't pile up against each other when
        // every workspace package has drift. JSON output is suppressed
        // here because it's a single object per call.
        if printed_table && !args.json {
            println!();
        }
        // Per-importer, matching the settings context built above: a
        // workspace package may carry its own `.npmrc` window.
        let gate = super::install::resolve_minimum_release_age(&ctx, None);
        let drifted = run_graph(
            &root,
            args.clone_for_fanout(),
            &graph,
            &selected_roots,
            Some(importer),
            gate.as_ref(),
        )
        .await?;
        printed_table = true;
        if drifted {
            any_drift = true;
        }
    }
    if any_drift {
        // Return the code for the binary's single `std::process::exit`
        // rather than exiting in place, keeping the command embed-safe.
        return Ok(Some(1));
    }
    Ok(None)
}

async fn run_global(args: OutdatedArgs) -> miette::Result<Option<i32>> {
    let layout = super::global::GlobalLayout::resolve()?;
    let mut packages = super::global::scan_packages(&layout.pkg_dir);
    packages.sort_by(|a, b| a.aliases.first().cmp(&b.aliases.first()));

    if packages.is_empty() {
        if args.json {
            println!("{{}}");
        } else {
            println!("(no global packages installed)");
        }
        return Ok(None);
    }

    let mut rows = Vec::new();
    let mut matched_any = false;
    let mut matched_install = false;
    let mut parsed_install = false;
    let mut skipped_lockfile = false;
    for info in packages {
        let matched_aliases: Option<Vec<&str>> = args.pattern.as_deref().map(|pattern| {
            info.aliases
                .iter()
                .filter_map(|alias| alias.starts_with(pattern).then_some(alias.as_str()))
                .collect()
        });
        if matched_aliases.as_ref().is_some_and(Vec::is_empty) {
            continue;
        }
        matched_install = true;

        let manifest = super::load_manifest(&info.install_dir.join("package.json"))?;
        let graph = match aube_lockfile::parse_lockfile(&info.install_dir, &manifest) {
            Ok(g) => g,
            Err(aube_lockfile::Error::NotFound(_)) => {
                skipped_lockfile = true;
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_GLOBAL_OUTDATED_NO_LOCKFILE,
                    "global install at {} has no lockfile; skipping outdated check",
                    info.install_dir.display()
                );
                continue;
            }
            Err(e) => {
                return Err(miette::Report::new(e)).wrap_err_with(|| {
                    format!(
                        "failed to parse global lockfile in {}",
                        info.install_dir.display()
                    )
                });
            }
        };
        parsed_install = true;
        let mut collect_args = args.clone_for_fanout();
        collect_args.pattern = None;
        let selected_roots;
        let roots = if let Some(aliases) = matched_aliases {
            selected_roots = graph
                .root_deps()
                .iter()
                .filter(|dep| aliases.iter().any(|alias| dep.name == *alias))
                .cloned()
                .collect::<Vec<_>>();
            selected_roots.as_slice()
        } else {
            graph.root_deps()
        };
        let gate = age_gate_for(&info.install_dir);
        let (mut package_rows, matched) = collect_rows(
            &info.install_dir,
            collect_args,
            &graph,
            roots,
            gate.as_ref(),
        )
        .await?;
        if matched {
            matched_any = true;
        }
        rows.append(&mut package_rows);
    }

    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let has_drift = has_drift(&rows);
    let no_checkable_global_dependencies =
        rows.is_empty() && matched_install && !parsed_install && skipped_lockfile;
    if args.json {
        if no_checkable_global_dependencies {
            render_no_checkable_global_json()?;
        } else {
            render_json(&rows)?;
        }
    } else if no_checkable_global_dependencies {
        println!("(no checkable global dependencies)");
    } else if rows.is_empty() && !matched_any {
        println!("(no matching dependencies)");
    } else {
        render_table(&rows, args.long);
    }

    if has_drift { Ok(Some(1)) } else { Ok(None) }
}

async fn run_one(cwd: &Path, args: OutdatedArgs, importer: Option<String>) -> miette::Result<bool> {
    let manifest = super::load_manifest(&cwd.join("package.json"))?;
    let ignored = super::update::ignored_update_dependencies(cwd, &manifest)?;

    let graph = match aube_lockfile::parse_lockfile(cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!(
                "No lockfile found. Run `{}` first.",
                aube_util::cmd("install")
            );
            return Ok(false);
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    let roots: Vec<DirectDep> = graph
        .root_deps()
        .iter()
        .filter(|dep| !ignored.contains(&dep.name))
        .cloned()
        .collect();
    let gate = age_gate_for(cwd);
    run_graph(cwd, args, &graph, &roots, importer, gate.as_ref()).await
}

async fn run_graph(
    cwd: &Path,
    args: OutdatedArgs,
    graph: &aube_lockfile::LockfileGraph,
    roots: &[DirectDep],
    importer: Option<String>,
    gate: Option<&aube_resolver::MinimumReleaseAge>,
) -> miette::Result<bool> {
    let (mut rows, matched_any) =
        collect_rows(cwd, args.clone_for_fanout(), graph, roots, gate).await?;
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let has_drift = has_drift(&rows);
    for row in &mut rows {
        row.importer.clone_from(&importer);
    }

    if args.json {
        render_json(&rows)?;
    } else if rows.is_empty() && !matched_any {
        println!("(no matching dependencies)");
    } else {
        render_table(&rows, args.long);
    }

    // Return the drift flag to the caller. The single-project caller (`run`)
    // maps `true` to exit code 1 (pnpm parity: `aube outdated || exit 1`),
    // and the recursive caller (`run_filtered`) aggregates drift across
    // importers — the exit decision lives at the top so the command layer
    // stays embed-safe (no in-place `std::process::exit`).
    Ok(has_drift)
}

async fn collect_rows(
    cwd: &Path,
    args: OutdatedArgs,
    graph: &aube_lockfile::LockfileGraph,
    roots: &[DirectDep],
    gate: Option<&aube_resolver::MinimumReleaseAge>,
) -> miette::Result<(Vec<Row>, bool)> {
    let filter = DepFilter::from_flags(args.prod, args.dev);
    let roots: Vec<&DirectDep> = roots
        .iter()
        .filter(|d| filter.keeps(d.dep_type))
        .filter(|d| match args.pattern.as_deref() {
            None => true,
            Some(p) => d.name.starts_with(p),
        })
        .collect();

    if roots.is_empty() {
        return Ok((Vec::new(), false));
    }
    let roots: Vec<&DirectDep> = roots
        .into_iter()
        .filter(|d| {
            !d.specifier
                .as_deref()
                .is_some_and(aube_util::pkg::is_workspace_spec)
        })
        .collect();
    if roots.is_empty() {
        return Ok((Vec::new(), false));
    }

    let client = std::sync::Arc::new(make_client(cwd));
    let cache_dir = packument_cache_dir();
    // The age gate needs per-version publish times, and npmjs's abbreviated
    // (corgi) packument carries none — its document-level `modified` only
    // proves maturity for a package that has published NOTHING recently,
    // which is never true of the packages this report is about. So a window
    // in effect switches the fetch to the full document. Measured cost is
    // +17-30% on the wire, not the ~2x the uncompressed sizes suggest — the
    // `time` map is repetitive ISO text and gzips hard.
    //
    // Cached, unlike `build_resolver`'s `cache_full_packuments: false`. That
    // opt-out is for the MUTATING verbs, which write a lockfile off the pick
    // and so must see a dist-tag bump the instant it lands; a report is not
    // load-bearing that way, and nub pins the window on for every project, so
    // an uncached fetch here would mean a full uncached GET per direct
    // dependency on the default path of a read-only command. The cache's TTL
    // and ETag revalidation are the same freshness terms the ungated path
    // already runs on.
    let needs_time = gate.is_some();
    let full_cache_dir = packument_full_cache_dir();

    // An `npm:` alias carries the alias as `DirectDep.name`, which the
    // registry has never heard of — fetching by it produced a bogus
    // "failed to fetch packument for <alias>" warning and then reported
    // the dep as up to date forever. The real name lives on the resolved
    // package, so key every fetch on `registry_name()` while rows keep
    // displaying the alias the user actually wrote in package.json.
    let registry_name_for = |dep: &DirectDep| -> String {
        graph
            .get_package(&dep.dep_path)
            .map(|p| p.registry_name().to_string())
            .unwrap_or_else(|| dep.name.clone())
    };

    // Fetch every packument in parallel via a JoinSet. Failures are surfaced
    // per-row so a single missing package doesn't sink the whole report.
    // Deduplicated by registry name so two aliases of one package (or an
    // alias alongside the real dep) don't fetch it twice.
    let mut set = tokio::task::JoinSet::new();
    let mut fetching: HashSet<String> = HashSet::new();
    for dep in &roots {
        let name = registry_name_for(dep);
        if !fetching.insert(name.clone()) {
            continue;
        }
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        let full_cache_dir = full_cache_dir.clone();
        set.spawn(async move {
            let result = if needs_time {
                client
                    .fetch_packument_with_time_cached(&name, &full_cache_dir)
                    .await
            } else {
                client.fetch_packument_cached(&name, &cache_dir).await
            };
            (name, result)
        });
    }
    let mut packuments: HashMap<String, Result<Packument, aube_registry::Error>> =
        HashMap::with_capacity(roots.len());
    while let Some(res) = set.join_next().await {
        let (name, result) = res.into_diagnostic().wrap_err("packument fetch panicked")?;
        packuments.insert(name, result);
    }

    let mut rows: Vec<Row> = Vec::new();
    // Several deps can share one registry name, and the lookup below reads
    // the shared entry rather than consuming it, so a failed fetch would
    // otherwise warn once per dep.
    let mut warned: HashSet<String> = HashSet::new();
    for dep in &roots {
        let registry_name = registry_name_for(dep);
        // `get`, not `remove`: several deps can share one registry name.
        let packument = packuments.get(&registry_name);
        let current = match graph.get_package(&dep.dep_path) {
            Some(p) => p.version.clone(),
            None => "(missing)".to_string(),
        };
        let packument = match packument {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                if warned.insert(registry_name.clone()) {
                    eprintln!("warn: failed to fetch packument for {registry_name}: {e}");
                }
                continue;
            }
            None => continue,
        };
        // Both columns run through the resolver's own picker so the report
        // cannot advertise a version `install`/`update` would then decline —
        // the promise `wanted_version`'s doc comment already makes, which
        // went unkept for `latest` and for any window in effect (#722).
        //
        // `latest` is optional so a registry that never publishes a
        // `latest` dist-tag (common on private registries) doesn't get
        // silently flagged as outdated. Drift detection treats an
        // unknown latest the same as "matches current".
        let latest = latest_pick(packument, &registry_name, gate, &current);

        // Wanted = highest version in the packument that still satisfies the
        // manifest range. Fall back to `current` when the range is unparseable
        // (workspace:/file: specifiers, git URLs, etc.) so we don't lie.
        let spec = dep.specifier.as_deref();
        let (wanted, wanted_undated) = match spec {
            Some(spec) => gated_pick(
                packument,
                &registry_name,
                spec,
                gate,
                super::wanted_version(packument, spec),
            ),
            None => (None, false),
        };
        let wanted = wanted.unwrap_or_else(|| current.clone());

        // The registry dated no version in the MANIFEST's range, so the gate
        // admits none of them and an install of this package hard-errors.
        // Reporting it as up to date would put this command at odds with the
        // installer, which is the disagreement #722 is about. Warn once per
        // package, on stderr beside the fetch warning above, so stdout stays
        // data.
        //
        // Keyed on the `wanted` column ALONE, which is why `latest_pick`
        // discards its own verdict rather than offering it here. The `latest`
        // column answers a different question: it resolves the literal
        // `latest` range, which a gated pick widens to `<=dist-tags.latest` —
        // a candidate set bounded by the tag and disjoint from the manifest
        // range. Plain `update` resolves the manifest range, so a refusal in
        // the `latest` column is no evidence about it. A stale or rolled-back
        // `latest` tag reaches that state routinely, and folding it in here
        // told the user an update would fail on a package where it succeeds.
        if wanted_undated && warned.insert(registry_name.clone()) {
            eprintln!(
                "warn: {registry_name} has no registry publish times, so \
                 minimumReleaseAge cannot admit any version; \
                 `{}` will fail for it",
                aube_util::cmd("update")
            );
        }

        let latest_known = latest.is_some();
        let latest_drift = latest.as_deref().is_some_and(|l| l != current);
        let wanted_drift = current != wanted;
        let changed = latest_drift || wanted_drift;
        if changed || args.long {
            rows.push(Row {
                name: dep.name.clone(),
                current,
                wanted,
                latest: latest.unwrap_or_else(|| "(unknown)".to_string()),
                dep_type: dep.dep_type,
                latest_known,
                specifier: dep.specifier.clone(),
                importer: None,
            });
        }
    }

    Ok((rows, true))
}

fn has_drift(rows: &[Row]) -> bool {
    // Hide "up-to-date but only because --long" rows from the non-empty check
    // so `--long` alone doesn't cause a pnpm CI pipeline to flip to exit 1.
    // A row only counts as drift when its latest is known AND differs from
    // current, or its wanted version diverges from current — a missing
    // `latest` dist-tag must never flip the exit code.
    //
    // A window that admits nothing needs no special case here: `gated_pick`
    // returns `None`, `wanted` falls back to `current` and `latest` stays
    // unknown, so the row reports no drift on its own (#722).
    rows.iter()
        .any(|r| (r.latest_known && r.current != r.latest) || r.current != r.wanted)
}

impl OutdatedArgs {
    fn clone_for_fanout(&self) -> Self {
        Self {
            pattern: self.pattern.clone(),
            dev: self.dev,
            global: self.global,
            json: self.json,
            long: self.long,
            prod: self.prod,
            workspace_root: self.workspace_root,
            network: self.network.clone(),
        }
    }
}

/// Render `target` left-padded to `width`, with the portion that
/// changed relative to `current` colored on the traffic-light ramp:
/// red for major bumps, yellow for minor, green for patch — a natural
/// severity progression within the portable ANSI-16 palette (and the
/// same ramp pnpm's own interactive updater uses) — plus magenta for
/// prerelease-only changes, deliberately outside the ramp. Falls back
/// to the plain string when either side fails to parse as semver or
/// `target == current`.
///
/// The padding is added on the *raw* string before color codes so
/// downstream column alignment isn't thrown off by invisible escapes.
///
/// `on_stderr` selects the stream whose TTY state gates color emission:
/// `outdated`'s table prints on stdout (`false`), while the interactive
/// update picker renders on stderr (`true`).
pub(crate) fn colorize_diff(current: &str, target: &str, width: usize, on_stderr: bool) -> String {
    use clx::style;
    let plain = format!("{target:<width$}");
    if current == target {
        return plain;
    }
    let Ok(cur) = node_semver::Version::parse(current) else {
        return plain;
    };
    let Ok(new) = node_semver::Version::parse(target) else {
        return plain;
    };
    let trailing_pad = " ".repeat(width.saturating_sub(target.len()));
    // Identify the leftmost differing component. Once we hit one,
    // every component to the right is also "new" and gets the same
    // color so a `1.2.3 → 2.0.0` major bump highlights the whole
    // tail, not just the leading `2`.
    let head_color = if cur.major != new.major {
        SemverDiff::Major
    } else if cur.minor != new.minor {
        SemverDiff::Minor
    } else if cur.patch != new.patch {
        SemverDiff::Patch
    } else {
        SemverDiff::Prerelease
    };
    let core = format!("{}.{}.{}", new.major, new.minor, new.patch);
    let prerelease = if !new.pre_release.is_empty() {
        let parts: Vec<String> = new.pre_release.iter().map(|p| p.to_string()).collect();
        format!("-{}", parts.join("."))
    } else {
        String::new()
    };
    // Split the rendered version into (unchanged head, changed tail)
    // so only the differing slice carries color. Major bumps keep
    // the whole string painted; prerelease-only differences leave
    // `MAJOR.MINOR.PATCH` plain and color the `-rc.x` tail.
    let split_at = match head_color {
        SemverDiff::Major => 0,
        SemverDiff::Minor => format!("{}.", new.major).len(),
        SemverDiff::Patch => format!("{}.{}.", new.major, new.minor).len(),
        SemverDiff::Prerelease => core.len(),
    };
    let (head, tail_in_core) = core.split_at(split_at.min(core.len()));
    let tail = format!("{tail_in_core}{prerelease}");
    // Prerelease-promoted-to-stable case (e.g. `1.2.3-rc.1` → `1.2.3`):
    // every numeric component matches and the new version has no
    // prerelease, so the computed tail collapses to an empty string
    // and nothing would carry color. The version genuinely changed,
    // so paint the whole `core` instead so the row reads as updated.
    let (head, tail) = if tail.is_empty() && cur != new {
        ("", core.as_str())
    } else {
        (head, tail.as_str())
    };
    // `render_table` writes via `println!` (stdout), so the styled
    // tail must use the stdout-aware color helpers (`nstyle`).
    // The `e*` family in clx checks stderr's TTY state and would
    // either inject ANSI escapes into a piped report file or
    // suppress color when stderr is redirected but the user is
    // looking at a TTY on stdout. The colors chain off the base
    // (`nstyle(...).red()` etc.) so the stream choice stays explicit.
    // The picker path renders on stderr and passes `on_stderr: true` for
    // the symmetric `estyle` gating.
    let base = if on_stderr {
        style::estyle(tail)
    } else {
        style::nstyle(tail)
    };
    let painted = match head_color {
        SemverDiff::Major => base.red(),
        SemverDiff::Minor => base.yellow(),
        SemverDiff::Patch => base.green(),
        SemverDiff::Prerelease => base.magenta(),
    }
    .to_string();
    format!("{head}{painted}{trailing_pad}")
}

#[derive(Clone, Copy)]
enum SemverDiff {
    Major,
    Minor,
    Patch,
    Prerelease,
}

fn render_table(rows: &[Row], long: bool) {
    if rows.is_empty() {
        println!("All dependencies up to date.");
        return;
    }

    // Compute column widths.
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(7).max(7);
    let cur_w = rows
        .iter()
        .map(|r| r.current.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let want_w = rows
        .iter()
        .map(|r| r.wanted.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let latest_w = rows
        .iter()
        .map(|r| r.latest.len())
        .max()
        .unwrap_or(6)
        .max(6);

    // Per-row pre-colored cells. Width math above uses the raw
    // strings so ANSI escapes don't throw off `<`-padding.
    let painted: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            let wanted = colorize_diff(&r.current, &r.wanted, want_w, false);
            let latest = if r.latest_known {
                colorize_diff(&r.current, &r.latest, latest_w, false)
            } else {
                format!("{:<latest_w$}", r.latest)
            };
            (wanted, latest)
        })
        .collect();

    if rows.iter().any(|r| r.importer.is_some()) {
        let importer_w = rows
            .iter()
            .filter_map(|r| r.importer.as_ref())
            .map(|s| s.len())
            .max()
            .unwrap_or(8)
            .max(8);
        println!(
            "{:<importer_w$}  {:<name_w$}  {:<cur_w$}  {:<want_w$}  {:<latest_w$}",
            "Importer", "Package", "Current", "Wanted", "Latest",
        );
        for (row, (wanted, latest)) in rows.iter().zip(&painted) {
            println!(
                "{:<importer_w$}  {:<name_w$}  {:<cur_w$}  {wanted}  {latest}",
                row.importer.as_deref().unwrap_or(""),
                row.name,
                row.current,
            );
        }
    } else {
        println!(
            "{:<name_w$}  {:<cur_w$}  {:<want_w$}  {:<latest_w$}",
            "Package", "Current", "Wanted", "Latest",
        );
        for (row, (wanted, latest)) in rows.iter().zip(&painted) {
            println!(
                "{:<name_w$}  {:<cur_w$}  {wanted}  {latest}",
                row.name, row.current,
            );
        }
    }

    if long {
        println!();
        for row in rows {
            if let Some(spec) = &row.specifier {
                let dep_label = dep_type_label(row.dep_type);
                println!("  {} ({dep_label}): {spec}", row.name);
            }
        }
    }
}

fn render_json(rows: &[Row]) -> miette::Result<()> {
    // Emit a pnpm-compatible shape: `{ "<name>": { current, wanted, latest } }`.
    // If malformed global state presents duplicate root names, keep every
    // row by promoting that one key to an array instead of overwriting.
    use serde_json::{Map, Value};
    let mut map: Map<String, Value> = Map::new();
    for row in rows {
        let v = serde_json::to_value(row).into_diagnostic()?;
        match map.remove(&row.name) {
            None => {
                map.insert(row.name.clone(), v);
            }
            Some(Value::Array(mut values)) => {
                values.push(v);
                map.insert(row.name.clone(), Value::Array(values));
            }
            Some(existing) => {
                map.insert(row.name.clone(), Value::Array(vec![existing, v]));
            }
        }
    }
    let out = serde_json::to_string_pretty(&Value::Object(map)).into_diagnostic()?;
    println!("{out}");
    Ok(())
}

fn render_no_checkable_global_json() -> miette::Result<()> {
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "checked": false,
        "code": aube_codes::warnings::WARN_AUBE_GLOBAL_OUTDATED_NO_LOCKFILE,
        "message": "no checkable global dependencies"
    }))
    .into_diagnostic()?;
    println!("{out}");
    Ok(())
}

#[cfg(test)]
mod colorize_tests {
    use super::{Row, colorize_diff};
    use aube_lockfile::DepType;

    fn strip_ansi(s: &str) -> String {
        // Strip CSI sequences for assertion purposes — the renderer
        // itself emits them, but tests assert on the visible glyphs.
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn equal_versions_render_plain() {
        let painted = colorize_diff("1.2.3", "1.2.3", 6, false);
        assert_eq!(strip_ansi(&painted).trim_end(), "1.2.3");
    }

    #[test]
    fn major_bump_renders_target_string() {
        // ANSI escapes only appear when clx's color detection picks a
        // colored output mode (TTY/`CLICOLOR_FORCE`); the test runs
        // headless so we assert on visible content only.
        let painted = colorize_diff("1.2.3", "2.0.0", 6, false);
        assert_eq!(strip_ansi(&painted).trim_end(), "2.0.0");
    }

    #[test]
    fn patch_bump_keeps_unchanged_head_plain() {
        // The leading `1.2.` prefix matches the current version and
        // must always render plain — only the trailing component is
        // a candidate for color when a colored terminal is in play.
        let painted = colorize_diff("1.2.3", "1.2.4", 6, false);
        let visible = strip_ansi(&painted);
        assert_eq!(visible.trim_end(), "1.2.4");
        assert!(painted.starts_with("1.2."), "head should render plain");
    }

    #[test]
    fn prerelease_promoted_to_stable_renders_changed_version() {
        // Regression: 1.2.3-rc.1 → 1.2.3 has matching MAJOR.MINOR.PATCH
        // with the new version carrying no prerelease, so the
        // computed tail collapsed to "" and a colored terminal
        // would render the row fully plain even though the version
        // genuinely changed. The fallback paints the whole core
        // when tail would be empty; here we just assert the visible
        // version is correct (color presence depends on the
        // ambient TTY mode, which is off in unit tests).
        let painted = colorize_diff("1.2.3-rc.1", "1.2.3", 6, false);
        assert_eq!(strip_ansi(&painted).trim_end(), "1.2.3");
    }

    #[test]
    fn unparseable_versions_fall_back_to_plain() {
        // dist-tags ("latest") and other non-semver strings should
        // skip colorization rather than panic. Width still applies.
        let painted = colorize_diff("1.2.3", "latest", 8, false);
        assert_eq!(painted, "latest  ");
    }

    #[test]
    fn json_duplicate_names_promote_to_array() {
        let rows = vec![
            Row {
                name: "same".to_string(),
                current: "1.0.0".to_string(),
                wanted: "1.0.1".to_string(),
                latest: "1.0.1".to_string(),
                dep_type: DepType::Production,
                latest_known: true,
                specifier: Some("^1.0.0".to_string()),
                importer: None,
            },
            Row {
                name: "same".to_string(),
                current: "2.0.0".to_string(),
                wanted: "2.0.1".to_string(),
                latest: "2.0.1".to_string(),
                dep_type: DepType::Production,
                latest_known: true,
                specifier: Some("^2.0.0".to_string()),
                importer: None,
            },
        ];
        let mut map = serde_json::Map::new();
        for row in rows {
            let value = serde_json::to_value(&row).unwrap();
            match map.remove(&row.name) {
                None => {
                    map.insert(row.name, value);
                }
                Some(serde_json::Value::Array(mut values)) => {
                    values.push(value);
                    map.insert(row.name, serde_json::Value::Array(values));
                }
                Some(existing) => {
                    map.insert(row.name, serde_json::Value::Array(vec![existing, value]));
                }
            }
        }

        assert_eq!(map["same"].as_array().unwrap().len(), 2);
    }
}

/// The release-age window's effect on the report (#722).
///
/// The window is applied silently — no marker, no note. These pin the two
/// things that must hold for that silence to be honest: the columns name what
/// an install would land on, and a window that admits nothing produces no
/// actionable row rather than an unreachable upgrade.
#[cfg(test)]
mod age_gate_tests {
    use super::*;
    use aube_lockfile::DepType;

    /// `2.0.1` published inside any recent window; `2.0.0` in 2020, so it
    /// clears every window a test would set.
    fn packument() -> Packument {
        serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "dist-tags": { "latest": "2.0.1" },
            "versions": {
                "2.0.0": { "name": "pkg", "version": "2.0.0" },
                "2.0.1": { "name": "pkg", "version": "2.0.1" },
            },
            "time": {
                "2.0.0": "2020-01-01T00:00:00.000Z",
                "2.0.1": "2099-01-01T00:00:00.000Z",
            },
        }))
        .expect("test packument parses")
    }

    fn gate(strict: bool) -> aube_resolver::MinimumReleaseAge {
        aube_resolver::MinimumReleaseAge {
            minutes: 1440,
            exclude: aube_resolver::PackageVersionPolicy::empty(),
            strict,
        }
    }

    fn row(current: &str, wanted: &str, latest: Option<&str>) -> Row {
        Row {
            name: "pkg".to_string(),
            current: current.to_string(),
            wanted: wanted.to_string(),
            latest: latest.unwrap_or("(unknown)").to_string(),
            dep_type: DepType::Production,
            latest_known: latest.is_some(),
            specifier: Some("^2.0.0".to_string()),
            importer: None,
        }
    }

    #[test]
    fn no_window_leaves_both_columns_on_the_ungated_pick() {
        let p = packument();
        assert_eq!(
            gated_pick(&p, "pkg", "^2.0.0", None, Some("2.0.1".into()))
                .0
                .as_deref(),
            Some("2.0.1")
        );
        assert_eq!(latest_pick(&p, "pkg", None, "2.0.0").as_deref(), Some("2.0.1"));
    }

    #[test]
    fn the_window_lowers_the_pick_to_what_an_install_would_land_on() {
        // The whole point of #722: the column must not name 2.0.1, which the
        // resolver would decline.
        let p = packument();
        let g = gate(true);
        assert_eq!(
            gated_pick(&p, "pkg", "^2.0.0", Some(&g), Some("2.0.1".into()))
                .0
                .as_deref(),
            Some("2.0.0")
        );
        assert_eq!(latest_pick(&p, "pkg", Some(&g), "2.0.0").as_deref(), Some("2.0.0"));
    }

    #[test]
    fn exclude_exempts_a_package_from_the_window() {
        let mut g = gate(true);
        g.exclude = aube_resolver::PackageVersionPolicy::parse_lossy(vec!["pkg".to_string()]).0;
        assert_eq!(
            gated_pick(
                &packument(),
                "pkg",
                "^2.0.0",
                Some(&g),
                Some("2.0.1".into())
            )
            .0
            .as_deref(),
            Some("2.0.1"),
            "minimumReleaseAgeExclude must reach the report, not just the install"
        );
    }

    #[test]
    fn a_window_that_admits_nothing_yields_no_version_rather_than_an_error() {
        // `install` fails closed here; a report has no such duty. `None` makes
        // the caller fall back to the locked version, so the row reports no
        // drift and never appears — there is genuinely nothing to act on.
        let p = packument();
        let (picked, undated) =
            gated_pick(&p, "pkg", "2.0.1", Some(&gate(true)), Some("2.0.1".into()));
        assert_eq!(picked, None);
        assert!(
            !undated,
            "2.0.1 IS dated — this refusal is TooNew, which stays silent"
        );
        let r = row("2.0.0", "2.0.0", None);
        assert!(
            !has_drift(std::slice::from_ref(&r)),
            "an upgrade the window refuses must not flip the exit code"
        );
    }

    /// A registry that publishes no `latest` dist-tag (common on private
    /// registries) stays exempt from the drift check. `pick_version` answers a
    /// literal `latest` range with `highest_stable_version`, which reads
    /// version keys and never looks at a dist-tag — so feeding it an absent tag
    /// invents one and starts failing the exit code for those registries, on
    /// the default path, since nub pins the window on.
    #[test]
    fn a_registry_without_a_latest_tag_keeps_latest_unknown() {
        let p: Packument = serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "dist-tags": {},
            "versions": { "2.0.0": { "name": "pkg", "version": "2.0.0" } },
            "time": { "2.0.0": "2020-01-01T00:00:00.000Z" },
        }))
        .unwrap();
        assert_eq!(latest_pick(&p, "pkg", Some(&gate(true)), "1.0.0"), None);
        assert_eq!(latest_pick(&p, "pkg", None, "1.0.0"), None);
        // Guard the mechanism, so a resolver change cannot quietly reintroduce
        // the synthesis the call-site guard exists to stop.
        assert!(
            matches!(
                aube_resolver::pick_version_for_add(&p, "pkg", "latest", None),
                aube_resolver::PickResult::Found(_)
            ),
            "the picker still synthesizes a tag; the guard is what stops it"
        );
    }

    #[test]
    fn a_real_upgrade_still_counts_as_drift() {
        // The window must not mask an upgrade that IS installable.
        assert!(has_drift(&[row("2.0.0", "2.0.0", Some("2.1.0"))]));
        assert!(has_drift(&[row("2.0.0", "2.1.0", Some("2.1.0"))]));
    }

    /// A registry that dates no version cannot be gated at all, and
    /// `install`/`update` hard-error on it with a distinct error (#581). The
    /// report must not silently call that "up to date" — the two refusals are
    /// not interchangeable.
    #[test]
    fn an_undatable_registry_is_reported_as_such_not_as_silence() {
        let p: Packument = serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "dist-tags": { "latest": "2.0.0" },
            "modified": "2099-01-01T00:00:00.000Z",
            "versions": {
                "1.0.0": { "name": "pkg", "version": "1.0.0" },
                "2.0.0": { "name": "pkg", "version": "2.0.0" },
            },
        }))
        .unwrap();
        let (picked, undated) =
            gated_pick(&p, "pkg", "^1.0.0", Some(&gate(true)), Some("2.0.0".into()));
        assert_eq!(picked, None, "nothing is installable");
        assert!(
            undated,
            "and the caller must be told WHY, so it warns instead of printing \
             `All dependencies up to date.`"
        );
    }

    /// A stale or rolled-back `latest` tag routinely leaves the tag undated
    /// while the manifest's own range resolves fine. The warning names plain
    /// `update`, which resolves the MANIFEST range, so it must key on that
    /// column alone — keying on `latest` too claimed a failure that does not
    /// happen.
    #[test]
    fn an_undated_latest_tag_does_not_predict_a_failure_of_the_manifest_range() {
        let p: Packument = serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "dist-tags": { "latest": "2.0.0" },
            "versions": {
                "2.0.0": { "name": "pkg", "version": "2.0.0" },
                "3.0.0": { "name": "pkg", "version": "3.0.0" },
            },
            // 2.0.0 — the tagged latest — is undated; 3.0.0 is dated and old.
            "time": { "3.0.0": "2020-01-01T00:00:00.000Z" },
        }))
        .unwrap();
        let g = gate(true);
        assert_eq!(
            latest_pick(&p, "pkg", Some(&g), "3.0.0"),
            None,
            "the `latest` column genuinely admits nothing here"
        );
        // Guard the premise: `None` alone cannot tell an undated refusal from
        // a too-new one, and undated-ness is what this case is about.
        assert!(matches!(
            aube_resolver::pick_version_for_add(&p, "pkg", "latest", Some(&g)),
            aube_resolver::PickResult::AgeGated(aube_resolver::AgeGateCause::Undeterminable)
        ));
        let (picked, undated) = gated_pick(&p, "pkg", "^3.0.0", Some(&g), Some("3.0.0".into()));
        assert_eq!(
            picked.as_deref(),
            Some("3.0.0"),
            "the manifest range resolves"
        );
        assert!(
            !undated,
            "so the warning must NOT fire — `nub update` succeeds on this package"
        );
    }

    /// The `Latest` column must never point BACKWARDS.
    ///
    /// A window wider than the installed version's own age sends the widened
    /// `<=dist-tags.latest` scan down PAST `current` to the newest release old
    /// enough to clear. Reporting that advertises a DOWNGRADE, counts as drift,
    /// and holds the exit code at 1 with nothing installable — #722's dead end
    /// reached by another route.
    #[test]
    fn a_window_wider_than_the_installed_version_offers_no_downgrade() {
        let p: Packument = serde_json::from_value(serde_json::json!({
            "name": "pkg",
            "dist-tags": { "latest": "2.0.0" },
            "versions": {
                "1.0.0": { "name": "pkg", "version": "1.0.0" },
                "2.0.0": { "name": "pkg", "version": "2.0.0" },
            },
            // 2.0.0 is what is installed, and it is too new for the window;
            // 1.0.0 is the newest release the window does admit.
            "time": {
                "1.0.0": "2020-01-01T00:00:00.000Z",
                "2.0.0": "2099-01-01T00:00:00.000Z",
            },
        }))
        .unwrap();
        let g = gate(true);
        // Guard the premise: without the clamp the column would say 1.0.0, so
        // this case genuinely exercises the backwards pick rather than a refusal.
        assert!(matches!(
            aube_resolver::pick_version_for_add(&p, "pkg", "latest", Some(&g)),
            aube_resolver::PickResult::Found(m) if m.version == "1.0.0"
        ));
        assert_eq!(
            latest_pick(&p, "pkg", Some(&g), "2.0.0").as_deref(),
            Some("2.0.0"),
            "the newest admitted release is OLDER than what is installed, so \
             there is no upgrade and the column reports current"
        );
        let r = row("2.0.0", "2.0.0", Some("2.0.0"));
        assert!(
            !has_drift(std::slice::from_ref(&r)),
            "and with no upgrade on offer the command must exit 0"
        );
    }
}
