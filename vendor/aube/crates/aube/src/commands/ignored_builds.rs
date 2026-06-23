//! `aube ignored-builds` — print packages whose lifecycle scripts were
//! skipped by the `pnpm.allowBuilds` allowlist.
//!
//! Walks the lockfile, reads each dep's stored `package.json` from the
//! global store, and reports any package that declares a
//! `preinstall` / `install` / `postinstall` script but isn't explicitly
//! allowed by the current `BuildPolicy`. Shared with `approve-builds`,
//! which re-uses [`collect_ignored`] to drive its interactive picker.
//!
//! Pure read — no network, no writes, no project lock.

use clap::Args;
use miette::{Context, IntoDiagnostic};
use std::collections::BTreeSet;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube ignored-builds
  The following builds were ignored during install:
    esbuild@0.20.2
    puppeteer@22.8.0

  # When nothing was skipped
  $ aube ignored-builds
  No ignored builds.

  # Approve them for this project
  $ aube approve-builds
";

#[derive(Debug, Args)]
pub struct IgnoredBuildsArgs {
    /// Operate on globally-installed packages instead of the current project.
    #[arg(short = 'g', long)]
    pub global: bool,
}

pub async fn run(args: IgnoredBuildsArgs) -> miette::Result<()> {
    if args.global {
        return run_global();
    }

    let cwd = crate::dirs::project_root()?;
    let ignored = collect_ignored(&cwd)?;

    if ignored.is_empty() {
        println!("No ignored builds.");
        return Ok(());
    }

    println!("The following builds were ignored during install:");
    for entry in &ignored {
        print_entry_line("  ", entry);
    }
    Ok(())
}

/// Render one `IgnoredEntry` to stdout: `<indent><name>@<version>`,
/// followed by `<indent>  ⚠ <hook>: <description>` lines for each
/// content-sniff match against the package's lifecycle scripts.
fn print_entry_line(indent: &str, entry: &IgnoredEntry) {
    println!("{indent}{}@{}", entry.name, entry.version);
    for sus in &entry.suspicions {
        println!("{indent}  ⚠ {} — {}", sus.hook, sus.kind.description());
    }
}

fn run_global() -> miette::Result<()> {
    let layout = super::global::GlobalLayout::resolve()?;
    let mut installs = super::global::scan_packages(&layout.pkg_dir);
    installs.sort_by(|a, b| a.install_dir.cmp(&b.install_dir));

    let mut printed = false;
    let mut seen = std::collections::BTreeSet::new();
    for info in installs {
        if !seen.insert(info.install_dir.clone()) {
            continue;
        }
        let ignored = collect_ignored(&info.install_dir)?;
        if ignored.is_empty() {
            continue;
        }
        if !printed {
            println!("The following global builds were ignored during install:");
            printed = true;
        }
        println!(
            "  {} ({})",
            info.aliases.join(", "),
            info.install_dir.display()
        );
        for entry in &ignored {
            print_entry_line("    ", entry);
        }
    }

    if !printed {
        println!("No ignored builds.");
    }
    Ok(())
}

/// One package whose lifecycle scripts were skipped because it was not
/// allowed by the current `BuildPolicy`. `name` is the pnpm package name,
/// `version` is the resolved version from the lockfile. `suspicions`
/// is the result of running the content-sniff against the stored
/// manifest's lifecycle script bodies — empty when the scripts look
/// clean, populated when one or more dangerous-shape heuristics
/// fired. Used by the `approve-builds` picker to flag suspicious
/// entries so the user has more than `name@version` to judge by.
///
/// Field order matters: derived `Ord` compares by declaration
/// order, so `(name, version)` orders identically to the prior
/// manual impl. `collect_ignored` already deduplicates on
/// `(name, version)`, so the `suspicions` tiebreak is unreachable
/// in practice — keeping the derived shape avoids the
/// `Eq`/`Ord` inconsistency that an explicit Ord-on-prefix impl
/// would introduce.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct IgnoredEntry {
    pub name: String,
    pub version: String,
    pub suspicions: Vec<aube_scripts::Suspicion>,
}

/// Load the lockfile and build policy for `project_dir`, then return the
/// sorted, deduplicated list of `(name, version)` pairs that declare a
/// dep-lifecycle hook and are not allowed by the policy.
///
/// Returns an empty list (not an error) if there is no lockfile yet —
/// callers print their own "nothing to do" message.
pub(super) fn collect_ignored(project_dir: &std::path::Path) -> miette::Result<Vec<IgnoredEntry>> {
    let manifest = super::load_manifest(&project_dir.join("package.json"))?;

    let graph = match aube_lockfile::parse_lockfile(project_dir, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    let workspace = aube_manifest::WorkspaceConfig::load(project_dir)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let (policy, _warnings) =
        super::install::build_policy_from_sources(&manifest, &workspace, false);

    let store = super::open_store(project_dir)?;

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<IgnoredEntry> = Vec::new();

    for pkg in graph.packages.values() {
        if !seen.insert((pkg.name.clone(), pkg.version.clone())) {
            continue;
        }
        // Match on registry_name, not pkg.name. Allowlist pins the
        // real pkg name. npm: alias would sneak past otherwise. Same
        // fix as every other policy.decide callsite.
        let source_key = pkg.source_approval_key();
        if matches!(
            policy.decide_package(pkg.registry_name(), &pkg.version, source_key.as_deref()),
            aube_scripts::AllowDecision::Allow
        ) {
            continue;
        }
        let Some(suspicions) = lifecycle_scripts_with_suspicions(
            &store,
            &pkg.name,
            &pkg.version,
            pkg.integrity.as_deref(),
        ) else {
            continue;
        };
        out.push(IgnoredEntry {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            suspicions,
        });
    }

    out.sort();
    Ok(out)
}

/// Read `<name>@<version>`'s stored `package.json` from the global store
/// and decide whether the install pipeline would have run dep
/// lifecycle scripts for it. Returns `Some(suspicions)` when scripts
/// (or the implicit `node-gyp rebuild` fallback) would have fired;
/// `None` when nothing to do. Suspicions are the content-sniff
/// matches against the declared script bodies — empty in the common
/// case, populated when one or more dangerous-shape heuristics fired.
///
/// Missing / unreadable manifests conservatively return `None` — the
/// package might have scripts we can't see, but reporting them as
/// "ignored" would be noise since the install pipeline also skipped
/// them for the same reason.
fn lifecycle_scripts_with_suspicions(
    store: &aube_store::Store,
    name: &str,
    version: &str,
    integrity: Option<&str>,
) -> Option<Vec<aube_scripts::Suspicion>> {
    // Cache lookup is integrity-keyed when available (prevents
    // same-(name, version) entries from different sources colliding)
    // and falls back to the plain (name, version) key when integrity
    // is absent so proxy-served packages without `dist.integrity` are
    // still classifiable.
    let index = store.load_index(name, version, integrity)?;
    let stored = index.get("package.json")?;
    let content = std::fs::read_to_string(&stored.store_path).ok()?;
    let manifest = serde_json::from_str::<aube_manifest::PackageJson>(&content).ok()?;
    let has_declared = aube_scripts::DEP_LIFECYCLE_HOOKS
        .iter()
        .any(|h| manifest.scripts.contains_key(h.script_name()));
    // Delegate the implicit-rebuild gate to `aube-scripts` so this
    // stays in lockstep with what the install pipeline actually runs.
    // Presence comes from the store index here (the package isn't
    // materialized yet at this point in the command), but the
    // condition itself lives in exactly one place.
    let has_implicit =
        aube_scripts::implicit_install_script(&manifest, index.contains_key("binding.gyp"))
            .is_some();
    if !has_declared && !has_implicit {
        return None;
    }
    Some(aube_scripts::sniff_lifecycle(&manifest))
}
