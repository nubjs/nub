//! `aube outdated` — compare installed versions against the registry.
//!
//! Reads the root importer's direct deps from the lockfile, fetches each
//! package's packument (via the disk-backed cache), and prints the ones
//! whose current resolved version lags behind the `latest` dist-tag or
//! behind the highest version that still satisfies the range in
//! `package.json`. Mirrors `pnpm outdated`'s default table layout.
//!
//! Pure read: no state changes, no `node_modules/` writes, no project lock.

use super::{DepFilter, make_client, packument_cache_dir};
use aube_lockfile::{DepType, DirectDep, dep_type_label};
use aube_registry::Packument;
use clap::Args;
use miette::{Context, IntoDiagnostic};
use serde::Serialize;
use std::collections::HashMap;
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
    // Whether the packument carried a `latest` dist-tag. When false,
    // `latest` is the human-facing "(unknown)" sentinel and the drift
    // check ignores it so a missing tag doesn't flip exit code 1.
    #[serde(skip)]
    latest_known: bool,
    #[serde(skip)]
    specifier: Option<String>,
    #[serde(skip)]
    importer: Option<String>,
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
        // Discussion #602: separate per-importer tables with a blank
        // line so the headers don't pile up against each other when
        // every workspace package has drift. JSON output is suppressed
        // here because it's a single object per call.
        if printed_table && !args.json {
            println!();
        }
        let drifted = run_graph(
            &root,
            args.clone_for_fanout(),
            &graph,
            roots,
            Some(importer),
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
        let (mut package_rows, matched) =
            collect_rows(&info.install_dir, collect_args, &graph, roots).await?;
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

    run_graph(cwd, args, &graph, graph.root_deps(), importer).await
}

async fn run_graph(
    cwd: &Path,
    args: OutdatedArgs,
    graph: &aube_lockfile::LockfileGraph,
    roots: &[DirectDep],
    importer: Option<String>,
) -> miette::Result<bool> {
    let (mut rows, matched_any) = collect_rows(cwd, args.clone_for_fanout(), graph, roots).await?;
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

    // Fetch every packument in parallel via a JoinSet. Failures are surfaced
    // per-row so a single missing package doesn't sink the whole report.
    let mut set = tokio::task::JoinSet::new();
    for dep in &roots {
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        let name = dep.name.clone();
        set.spawn(async move {
            let result = client.fetch_packument_cached(&name, &cache_dir).await;
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
    for dep in &roots {
        let packument = packuments.remove(&dep.name);
        let current = match graph.get_package(&dep.dep_path) {
            Some(p) => p.version.clone(),
            None => "(missing)".to_string(),
        };
        let packument = match packument {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                eprintln!("warn: failed to fetch packument for {}: {e}", dep.name);
                continue;
            }
            None => continue,
        };
        // `latest` is optional so a registry that never publishes a
        // `latest` dist-tag (common on private registries) doesn't get
        // silently flagged as outdated. Drift detection treats an
        // unknown latest the same as "matches current".
        let latest: Option<String> = packument.dist_tags.get("latest").cloned();

        // Wanted = highest version in the packument that still satisfies the
        // manifest range. Fall back to `current` when the range is unparseable
        // (workspace:/file: specifiers, git URLs, etc.) so we don't lie.
        let wanted = dep
            .specifier
            .as_deref()
            .and_then(|spec| super::max_satisfying_version(&packument, spec))
            .unwrap_or_else(|| current.clone());

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
/// changed relative to `current` colored. Mirrors pnpm's
/// `@pnpm/colorize-semver-diff` palette: red for major bumps, cyan
/// for minor, green for patch, magenta for prerelease changes.
/// Falls back to the plain string when either side fails to parse
/// as semver or `target == current`.
///
/// The padding is added on the *raw* string before color codes so
/// downstream column alignment isn't thrown off by invisible escapes.
fn colorize_diff(current: &str, target: &str, width: usize) -> String {
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
    // looking at a TTY on stdout. clx 1.3 only ships `nred`/`ncyan`
    // directly, so green and magenta come from `nstyle(...).green()`
    // / `.magenta()` — same effect, just the chain is explicit.
    let painted = match head_color {
        SemverDiff::Major => style::nstyle(tail).red().to_string(),
        SemverDiff::Minor => style::nstyle(tail).cyan().to_string(),
        SemverDiff::Patch => style::nstyle(tail).green().to_string(),
        SemverDiff::Prerelease => style::nstyle(tail).magenta().to_string(),
    };
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
            let wanted = colorize_diff(&r.current, &r.wanted, want_w);
            let latest = if r.latest_known {
                colorize_diff(&r.current, &r.latest, latest_w)
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
        let painted = colorize_diff("1.2.3", "1.2.3", 6);
        assert_eq!(strip_ansi(&painted).trim_end(), "1.2.3");
    }

    #[test]
    fn major_bump_renders_target_string() {
        // ANSI escapes only appear when clx's color detection picks a
        // colored output mode (TTY/`CLICOLOR_FORCE`); the test runs
        // headless so we assert on visible content only.
        let painted = colorize_diff("1.2.3", "2.0.0", 6);
        assert_eq!(strip_ansi(&painted).trim_end(), "2.0.0");
    }

    #[test]
    fn patch_bump_keeps_unchanged_head_plain() {
        // The leading `1.2.` prefix matches the current version and
        // must always render plain — only the trailing component is
        // a candidate for color when a colored terminal is in play.
        let painted = colorize_diff("1.2.3", "1.2.4", 6);
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
        let painted = colorize_diff("1.2.3-rc.1", "1.2.3", 6);
        assert_eq!(strip_ansi(&painted).trim_end(), "1.2.3");
    }

    #[test]
    fn unparseable_versions_fall_back_to_plain() {
        // dist-tags ("latest") and other non-semver strings should
        // skip colorization rather than panic. Width still applies.
        let painted = colorize_diff("1.2.3", "latest", 8);
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
