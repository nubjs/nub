//! `aube ignored-builds` — print packages whose lifecycle scripts were
//! skipped by the `pnpm.allowBuilds` allowlist.
//!
//! Walks the lockfile and reports any package that declares a
//! `preinstall` / `install` / `postinstall` script but isn't explicitly
//! allowed by the current `BuildPolicy`. Registry packages are classified
//! from their stored `package.json` in the global store; source-backed
//! (`file:`/tarball/git/…) packages, which have no store index, are taken
//! from the install-recorded unreviewed set instead (see [`collect_ignored`]).
//! Shared with `approve-builds`, which re-uses `collect_ignored` to drive
//! its interactive picker.
//!
//! A pure read of project state — no network, no project-file writes, no
//! project lock (the install-state read may lazily migrate its own
//! on-disk format, an internal, idempotent housekeeping write).

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
    println!("{indent}{}", entry.display_spec());
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
/// `version` is the resolved version from the lockfile. `approval_key` is
/// the `allowBuilds` entry that authorizes the build: the bare package
/// name for registry packages, but the source key (`name@file:…`,
/// `name@<git-url>#<sha>`, …) for source-backed packages, which a bare
/// name must never approve. `suspicions` is the result of running the
/// content-sniff against the stored manifest's lifecycle script bodies —
/// empty when the scripts look clean, populated when one or more
/// dangerous-shape heuristics fired. Used by the `approve-builds` picker
/// to flag suspicious entries so the user has more than `name@version` to
/// judge by.
///
/// Field order matters: derived `Ord` compares by declaration
/// order, so `(name, version)` orders identically to the prior
/// manual impl. `collect_ignored` already deduplicates on
/// `(name, version)`, so the tiebreak fields are unreachable
/// in practice — keeping the derived shape avoids the
/// `Eq`/`Ord` inconsistency that an explicit Ord-on-prefix impl
/// would introduce.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct IgnoredEntry {
    pub name: String,
    pub version: String,
    pub approval_key: String,
    pub suspicions: Vec<aube_scripts::Suspicion>,
}

impl IgnoredEntry {
    /// Whether this entry must be approved by its source key rather than
    /// its bare name — true for `file:`/tarball/git/… packages, where
    /// `approval_key` carries the source identity instead of the name.
    pub(super) fn is_source_backed(&self) -> bool {
        self.approval_key != self.name
    }

    /// The `name@…` spec to show the user. Registry packages read as
    /// `name@version`; source-backed packages read as their source key
    /// (`name@file:./dep`), matching the install-time warning and pnpm's
    /// `ignored-builds` output so the displayed identity is exactly what
    /// gets written to `allowBuilds`.
    pub(super) fn display_spec(&self) -> String {
        if self.is_source_backed() {
            self.approval_key.clone()
        } else {
            format!("{}@{}", self.name, self.version)
        }
    }
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
    // Resolve a no-integrity package's index by its URL-keyed
    // computed-sha512 binding (matching the install warm read), so a
    // v1/legacy-lock package with a suspicious lifecycle script still
    // surfaces here — keying by `None` would miss now that the
    // content-free root index is no longer written.
    let no_integrity_index =
        crate::state::read_no_integrity_index_for(project_dir, graph.packages.values());

    // Source-backed (`file:`/tarball/git/…) packages have no
    // `(name, version)`-keyed store index — their content is imported by
    // dep_path, not a registry integrity — so the store read below always
    // misses for them and they'd be silently dropped (the `approve-builds`
    // dead-end this closes). The install already decided which have
    // unreviewed builds and recorded their source approval keys in install
    // state; this is the same set the `WARN_..._IGNORED_BUILD_SCRIPTS`
    // warning enumerates via `unreviewed_dep_builds`, so keying off it here
    // makes the warning and this command agree by construction.
    let recorded_source_builds: BTreeSet<String> =
        crate::state::read_state_unreviewed_builds(project_dir)
            .into_iter()
            .collect();

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<IgnoredEntry> = Vec::new();

    for pkg in graph.packages.values() {
        // Match on registry_name, not pkg.name. Allowlist pins the
        // real pkg name. npm: alias would sneak past otherwise. Same
        // fix as every other policy.decide callsite.
        if super::install::package_build_is_allowed(&policy, pkg) {
            continue;
        }
        if !seen.insert((pkg.name.clone(), pkg.version.clone())) {
            continue;
        }
        // A source-backed dep is authorized by its source key, not its bare
        // name, so surface it under that key. Suspicions stay empty: the
        // content-sniff ran at install time and the warning already
        // surfaced any findings; re-deriving them would mean re-reading each
        // dep's materialized manifest, which this pure lockfile read avoids.
        // A stale recorded set is self-correcting — the policy check above
        // drops an already-approved dep before it reaches here.
        let (approval_key, suspicions) = match pkg.source_approval_key() {
            Some(source_key) if recorded_source_builds.contains(&source_key) => {
                (source_key, Vec::new())
            }
            Some(_) => continue,
            None => {
                let read_key = pkg.integrity.as_deref().or_else(|| {
                    no_integrity_index
                        .get(&format!("{}@{}", pkg.registry_name(), pkg.version))
                        .map(String::as_str)
                });
                let Some(suspicions) =
                    lifecycle_scripts_with_suspicions(&store, &pkg.name, &pkg.version, read_key)
                else {
                    continue;
                };
                (pkg.name.clone(), suspicions)
            }
        };
        out.push(IgnoredEntry {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            approval_key,
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
    // `integrity` is the read key the caller resolved: the lockfile SRI,
    // or — for a no-integrity package — the per-project computed-sha512
    // binding, so a v1/legacy-lock package stays classifiable now that
    // the content-free root index is no longer written. A package with
    // no binding yet simply isn't classified (conservative miss).
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
