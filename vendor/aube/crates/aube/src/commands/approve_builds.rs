//! `aube approve-builds` — flip packages to `true` in the workspace
//! yaml's `allowBuilds` map so their install scripts run on the next
//! `aube install`. Writes to `aube-workspace.yaml` by default, or
//! mutates an existing `pnpm-workspace.yaml` in place.
//!
//! Walks the lockfile via `ignored_builds::collect_ignored`, presents an
//! interactive multi-select picker (or approves everything under
//! `--all`), then merges the selections into the workspace yaml's
//! `allowBuilds` map. Matches pnpm v11, which collapsed the old
//! allow/deny list keys into one review map. Entries are added as bare
//! package names so a future resolution of the same dep under a
//! different version keeps working without re-prompting.

use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{IsTerminal, Write};
use std::path::Path;

const INTERACTIVE_TTY_ERROR: &str = "approve-builds needs stdin and stderr to be TTYs for the interactive picker; pass `--all` or name packages positionally to approve non-interactively";

#[derive(Debug, Args)]
pub struct ApproveBuildsArgs {
    /// Approve every pending ignored build without prompting.
    #[arg(long)]
    pub all: bool,

    /// Operate on globally-installed packages instead of the current project.
    #[arg(short = 'g', long)]
    pub global: bool,

    /// Packages to approve directly, skipping the picker.
    ///
    /// Each name must match a currently-ignored build. Unknown names
    /// are rejected so a typo cannot silently no-op.
    #[arg(value_name = "PKG")]
    pub packages: Vec<String>,
}

pub async fn run(args: ApproveBuildsArgs) -> miette::Result<()> {
    if args.global {
        return run_global(args);
    }

    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;
    run_project(&cwd, args.all, args.packages)
}

fn run_project(cwd: &Path, all: bool, packages: Vec<String>) -> miette::Result<()> {
    let ignored = super::ignored_builds::collect_ignored(cwd)?;
    if ignored.is_empty() {
        println!("No ignored builds to approve.");
        return Ok(());
    }

    let selected = select_project(&ignored, all, packages)?;

    if selected.is_empty() {
        println!("No packages selected.");
        return Ok(());
    }

    let written = aube_manifest::workspace::add_to_allow_builds(cwd, &selected)
        .into_diagnostic()
        .wrap_err("failed to update workspace yaml")?;

    let rel = written
        .strip_prefix(cwd)
        .unwrap_or(written.as_path())
        .display();
    println!("Approved {} package(s) in {rel}:", selected.len());
    for name in &selected {
        println!("  {name}");
    }
    println!(
        "Run `{}` (or `{}`) to execute their scripts.",
        aube_util::cmd("install"),
        aube_util::cmd("rebuild")
    );
    Ok(())
}

fn run_global(args: ApproveBuildsArgs) -> miette::Result<()> {
    let global_ignored = collect_global_ignored()?;
    if global_ignored.is_empty() {
        println!("No ignored builds to approve.");
        return Ok(());
    }

    let selected = if args.all {
        if !args.packages.is_empty() {
            return Err(miette!(
                "`--all` and positional package names are mutually exclusive"
            ));
        }
        global_ignored
            .iter()
            .map(|entry| {
                (
                    entry.install_dir.clone(),
                    entry.ignored.iter().map(|i| i.name.clone()).collect(),
                )
            })
            .collect()
    } else if !args.packages.is_empty() {
        select_global_packages(&global_ignored, args.packages)?
    } else {
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return Err(miette!(INTERACTIVE_TTY_ERROR));
        }
        pick_global_interactively(&global_ignored)?
    };

    if selected.is_empty() {
        println!("No packages selected.");
        return Ok(());
    }

    let mut approved = 0usize;
    let mut written_dirs = 0usize;
    for (install_dir, names) in selected {
        let written = aube_manifest::workspace::add_to_allow_builds(&install_dir, &names)
            .into_diagnostic()
            .wrap_err("failed to update global install workspace yaml")?;
        written_dirs += 1;
        approved += names.len();
        println!(
            "Approved {} package(s) in {}:",
            names.len(),
            written.display()
        );
        for name in &names {
            println!("  {name}");
        }
    }

    println!("Approved {approved} package(s) across {written_dirs} global install(s).");
    println!(
        "Run `{} -C <global-install-dir> install` (or `{} -C <global-install-dir> rebuild`) to execute their scripts.",
        aube_util::prog(),
        aube_util::prog()
    );
    Ok(())
}

fn select_project(
    ignored: &[super::ignored_builds::IgnoredEntry],
    all: bool,
    packages: Vec<String>,
) -> miette::Result<Vec<String>> {
    if all {
        if !packages.is_empty() {
            return Err(miette!(
                "`--all` and positional package names are mutually exclusive"
            ));
        }
        return Ok(ignored.iter().map(|e| e.name.clone()).collect());
    }
    if !packages.is_empty() {
        let known: HashSet<&str> = ignored.iter().map(|e| e.name.as_str()).collect();
        let unknown: Vec<&str> = packages
            .iter()
            .filter(|p| !known.contains(p.as_str()))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(miette!(
                "not in the ignored-builds set: {}. Run `{}` to see candidates.",
                unknown.join(", "),
                aube_util::cmd("ignored-builds")
            ));
        }
        return Ok(dedupe(packages));
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(miette!(INTERACTIVE_TTY_ERROR));
    }
    pick_interactively(ignored)
}

#[derive(Debug)]
struct GlobalIgnored {
    install_dir: std::path::PathBuf,
    aliases: Vec<String>,
    ignored: Vec<super::ignored_builds::IgnoredEntry>,
}

fn collect_global_ignored() -> miette::Result<Vec<GlobalIgnored>> {
    let layout = super::global::GlobalLayout::resolve()?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for info in super::global::scan_packages(&layout.pkg_dir) {
        if !seen.insert(info.install_dir.clone()) {
            continue;
        }
        let ignored = super::ignored_builds::collect_ignored(&info.install_dir)?;
        if ignored.is_empty() {
            continue;
        }
        out.push(GlobalIgnored {
            install_dir: info.install_dir,
            aliases: info.aliases,
            ignored,
        });
    }
    out.sort_by(|a, b| a.install_dir.cmp(&b.install_dir));
    Ok(out)
}

fn select_global_packages(
    global_ignored: &[GlobalIgnored],
    packages: Vec<String>,
) -> miette::Result<BTreeMap<std::path::PathBuf, Vec<String>>> {
    let wanted = dedupe(packages);
    let known: HashSet<&str> = global_ignored
        .iter()
        .flat_map(|entry| entry.ignored.iter().map(|ignored| ignored.name.as_str()))
        .collect();
    let unknown: Vec<&str> = wanted
        .iter()
        .filter(|name| !known.contains(name.as_str()))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(miette!(
            "not in the ignored-builds set: {}. Run `{} -g` to see candidates.",
            unknown.join(", "),
            aube_util::cmd("ignored-builds")
        ));
    }

    let wanted: HashSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut selected = BTreeMap::new();
    for entry in global_ignored {
        let names: Vec<String> = entry
            .ignored
            .iter()
            .filter(|ignored| wanted.contains(ignored.name.as_str()))
            .map(|ignored| ignored.name.clone())
            .collect();
        if !names.is_empty() {
            selected.insert(entry.install_dir.clone(), names);
        }
    }
    Ok(selected)
}

fn dedupe(packages: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    packages
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Show a `demand::MultiSelect` picker seeded with every ignored package
/// and return the names the user accepted. Using bare names (not
/// `name@version`) keeps the written allowBuilds entry broad, so the
/// next resolution with a patch-level bump doesn't silently drop back
/// into the ignored set.
///
/// When any entry carries content-sniff suspicions, a one-shot summary
/// is printed to stderr before the picker opens so the user sees the
/// full list of flagged signals (the picker label only has room for
/// a short tag). The picker entry itself is annotated with `⚠
/// suspicious: <category>` so flagged rows stand out while scrolling.
fn pick_interactively(
    ignored: &[super::ignored_builds::IgnoredEntry],
) -> miette::Result<Vec<String>> {
    print_suspicion_summary(ignored);
    let mut picker = demand::MultiSelect::new("Choose which packages to allow building")
        .description("Space to toggle, Enter to confirm")
        .min(1);
    for entry in ignored {
        let label = format_picker_label(&entry.name, &entry.version, &entry.suspicions);
        picker = picker.option(demand::DemandOption::new(entry.name.clone()).label(&label));
    }
    picker
        .run()
        .into_diagnostic()
        .wrap_err("failed to read approve-builds selection")
}

/// `name@version` plus a compact suspicious-shape tag when the
/// content-sniff fired against any of the package's lifecycle
/// scripts. One picker row is narrow, so only the first match's
/// category gets a tag; `+N more` follows when more than one
/// matched. The full breakdown lives in `print_suspicion_summary`.
fn format_picker_label(
    name: &str,
    version: &str,
    suspicions: &[aube_scripts::Suspicion],
) -> String {
    if suspicions.is_empty() {
        return format!("{name}@{version}");
    }
    let first = suspicions[0].kind.category();
    let extra = suspicions.len() - 1;
    if extra == 0 {
        format!("{name}@{version}  ⚠ suspicious: {first}")
    } else {
        format!("{name}@{version}  ⚠ suspicious: {first} +{extra} more")
    }
}

/// Print every flagged package's full suspicion list to stderr before
/// the picker takes over the screen. No-op when nothing flagged so
/// the clean case stays terse.
fn print_suspicion_summary(ignored: &[super::ignored_builds::IgnoredEntry]) {
    let flagged: Vec<&super::ignored_builds::IgnoredEntry> = ignored
        .iter()
        .filter(|e| !e.suspicions.is_empty())
        .collect();
    if flagged.is_empty() {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "⚠ {} package(s) have lifecycle scripts that matched dangerous-shape heuristics:",
        flagged.len()
    );
    for entry in flagged {
        let _ = writeln!(stderr, "  {}@{}", entry.name, entry.version);
        for sus in &entry.suspicions {
            let _ = writeln!(stderr, "    • {} — {}", sus.hook, sus.kind.description());
        }
    }
    let _ = writeln!(
        stderr,
        "  Inspect each script in `node_modules/.aube/<dep_path>/node_modules/<name>/package.json` before approving."
    );
}

fn pick_global_interactively(
    global_ignored: &[GlobalIgnored],
) -> miette::Result<BTreeMap<std::path::PathBuf, Vec<String>>> {
    for entry in global_ignored {
        print_suspicion_summary(&entry.ignored);
    }
    let mut picker = demand::MultiSelect::new("Choose which global packages to allow building")
        .description("Space to toggle, Enter to confirm")
        .min(1);
    for (idx, entry) in global_ignored.iter().enumerate() {
        let aliases = entry.aliases.join(", ");
        for ignored in &entry.ignored {
            // split_once below keeps the full package name even if a
            // private registry allows ':' inside it.
            let value = format!("{idx}:{}", ignored.name);
            let base = format_picker_label(&ignored.name, &ignored.version, &ignored.suspicions);
            let label = format!("{aliases}: {base}");
            picker = picker.option(demand::DemandOption::new(value).label(&label));
        }
    }

    let picked: Vec<String> = picker
        .run()
        .into_diagnostic()
        .wrap_err("failed to read approve-builds selection")?;
    let mut selected: BTreeMap<std::path::PathBuf, Vec<String>> = BTreeMap::new();
    for item in picked {
        let Some((idx, name)) = item.split_once(':') else {
            continue;
        };
        let Ok(idx) = idx.parse::<usize>() else {
            continue;
        };
        let Some(entry) = global_ignored.get(idx) else {
            continue;
        };
        selected
            .entry(entry.install_dir.clone())
            .or_default()
            .push(name.to_string());
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::format_picker_label;
    use aube_scripts::{Suspicion, SuspicionKind};

    #[test]
    fn label_for_clean_package_is_bare_spec() {
        assert_eq!(
            format_picker_label("esbuild", "0.20.2", &[]),
            "esbuild@0.20.2"
        );
    }

    #[test]
    fn label_for_single_suspicion_shows_category() {
        let s = vec![Suspicion {
            kind: SuspicionKind::ShellPipe,
            hook: "postinstall",
        }];
        assert_eq!(
            format_picker_label("lodash", "1.0.0", &s),
            "lodash@1.0.0  ⚠ suspicious: curl|sh"
        );
    }

    #[test]
    fn label_for_multiple_suspicions_shows_first_plus_count() {
        let s = vec![
            Suspicion {
                kind: SuspicionKind::ShellPipe,
                hook: "postinstall",
            },
            Suspicion {
                kind: SuspicionKind::SecretEnvRead,
                hook: "postinstall",
            },
            Suspicion {
                kind: SuspicionKind::ExfilEndpoint,
                hook: "postinstall",
            },
        ];
        assert_eq!(
            format_picker_label("evil-pkg", "9.9.9", &s),
            "evil-pkg@9.9.9  ⚠ suspicious: curl|sh +2 more"
        );
    }
}
