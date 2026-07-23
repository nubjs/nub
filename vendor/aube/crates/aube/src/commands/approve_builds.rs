//! `aube approve-builds` — flip packages to `true` in the workspace
//! yaml's `allowBuilds` map and run their install scripts in the same
//! invocation. Writes to `aube-workspace.yaml` by default, or mutates
//! an existing `pnpm-workspace.yaml` in place.
//!
//! Walks the lockfile via `ignored_builds::collect_ignored`, presents an
//! interactive multi-select picker (or approves everything under
//! `--all`), merges the selections into the workspace yaml's
//! `allowBuilds` map, then runs a rebuild scoped to the approved names
//! (dep scripts only; root lifecycle hooks stay untouched). Matches
//! pnpm v11, which collapsed the old allow/deny list keys into one
//! review map and builds approved packages as part of `approve-builds`
//! itself. Entries are added as bare package names so a future
//! resolution of the same dep under a different version keeps working
//! without re-prompting.
//!
//! The global path stays write-only: pnpm 11 removed `approve-builds
//! --global` outright, so there is no reference behavior to mirror,
//! and each global install dir would need its own retargeted rebuild —
//! the printed hint keeps that flow explicit instead.

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
    // The lock covers only the config write. It is dropped before the
    // build step: rebuild does not take the project lock, and holding a
    // write lock across arbitrarily long lifecycle scripts would block
    // every other engine command on the project for the whole build.
    let approved = {
        let _lock = super::take_project_lock(&cwd)?;
        run_project(&cwd, args.all, args.packages)?
    };
    if approved.is_empty() {
        return Ok(());
    }
    // pnpm parity: the same invocation runs the just-approved packages'
    // scripts. The scoped form of rebuild is exactly the right shape —
    // it runs only the named deps' scripts, skips the root lifecycle
    // hooks, and treats the explicit names as the policy opt-in.
    super::rebuild::run(
        super::rebuild::RebuildArgs { packages: approved },
        aube_workspace::selector::EffectiveFilter::default(),
    )
    .await
}

/// Approve builds for the current project and return the approved
/// names (empty when nothing was ignored, nothing was selected, or the
/// interactive confirmation was declined — the caller skips the build
/// step in all three cases).
fn run_project(cwd: &Path, all: bool, packages: Vec<String>) -> miette::Result<Vec<String>> {
    let ignored = super::ignored_builds::collect_ignored(cwd)?;
    if ignored.is_empty() {
        println!("No ignored builds to approve.");
        return Ok(Vec::new());
    }

    let interactive = !all && packages.is_empty();
    let selected = select_project(&ignored, all, packages)?;

    if selected.is_empty() {
        println!("No packages selected.");
        return Ok(Vec::new());
    }

    // A source-backed package (`file:`/tarball/git/…) is authorized by its
    // source key, never its bare name, so the `allowBuilds` write must use
    // `approval_key`. The follow-up rebuild instead matches deps by graph
    // `name` (pnpm's `rebuild <name>` contract), so the two lists diverge
    // for source-backed deps and must be threaded separately.
    let entries = selected_entries(&ignored, &selected);
    let approval_keys = dedupe(entries.iter().map(|e| e.approval_key.clone()).collect());
    let display = entries
        .iter()
        .map(|e| e.display_spec())
        .collect::<Vec<_>>()
        .join(", ");

    // The picker only toggles names; scripts running is a separate
    // consent. pnpm gates the interactive path behind the same
    // default-No confirmation (`--all` and positional approvals are
    // themselves the explicit consent, so they build straight away).
    if interactive {
        let confirmed = demand::Confirm::new(format!(
            "The next packages will now be built: {display}. Do you approve?"
        ))
        .selected(false)
        .run()
        .into_diagnostic()
        .wrap_err("failed to read approve-builds confirmation")?;
        if !confirmed {
            return Ok(Vec::new());
        }
    }

    let written = aube_manifest::workspace::add_to_allow_builds(cwd, &approval_keys)
        .into_diagnostic()
        .wrap_err("failed to update workspace yaml")?;

    let rel = written
        .strip_prefix(cwd)
        .unwrap_or(written.as_path())
        .display();
    println!("Approved {} package(s) in {rel}:", approval_keys.len());
    for entry in &entries {
        println!("  {}", entry.display_spec());
    }
    Ok(selected)
}

/// The `IgnoredEntry`s whose bare `name` the caller selected, preserving
/// `ignored`'s sorted order. Bridges the name-keyed selection surface
/// (picker values, positional args, `--all`) to the entries' richer
/// identity — `approval_key` for the `allowBuilds` write and
/// `display_spec` for user-facing output.
fn selected_entries<'a>(
    ignored: &'a [super::ignored_builds::IgnoredEntry],
    selected: &[String],
) -> Vec<&'a super::ignored_builds::IgnoredEntry> {
    let want: HashSet<&str> = selected.iter().map(String::as_str).collect();
    ignored
        .iter()
        .filter(|e| want.contains(e.name.as_str()))
        .collect()
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
                    entry
                        .ignored
                        .iter()
                        .map(|i| i.approval_key.clone())
                        .collect(),
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
    for (install_dir, approval_keys) in selected {
        let written = aube_manifest::workspace::add_to_allow_builds(&install_dir, &approval_keys)
            .into_diagnostic()
            .wrap_err("failed to update global install workspace yaml")?;
        written_dirs += 1;
        approved += approval_keys.len();
        println!(
            "Approved {} package(s) in {}:",
            approval_keys.len(),
            written.display()
        );
        for key in &approval_keys {
            println!("  {key}");
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
        // Match on the user-typed bare `name`, but record the
        // `approval_key` — the source key authorizes a source-backed
        // build; the bare name would be silently ignored by the policy.
        let keys: Vec<String> = entry
            .ignored
            .iter()
            .filter(|ignored| wanted.contains(ignored.name.as_str()))
            .map(|ignored| ignored.approval_key.clone())
            .collect();
        if !keys.is_empty() {
            selected.insert(entry.install_dir.clone(), keys);
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
        let label = format_picker_label(&entry.display_spec(), &entry.suspicions);
        picker = picker.option(demand::DemandOption::new(entry.name.clone()).label(&label));
    }
    picker
        .run()
        .into_diagnostic()
        .wrap_err("failed to read approve-builds selection")
}

/// The entry's display spec plus a compact suspicious-shape tag when the
/// content-sniff fired against any of the package's lifecycle
/// scripts. One picker row is narrow, so only the first match's
/// category gets a tag; `+N more` follows when more than one
/// matched. The full breakdown lives in `print_suspicion_summary`.
fn format_picker_label(spec: &str, suspicions: &[aube_scripts::Suspicion]) -> String {
    if suspicions.is_empty() {
        return spec.to_string();
    }
    let first = suspicions[0].kind.category();
    let extra = suspicions.len() - 1;
    if extra == 0 {
        format!("{spec}  ⚠ suspicious: {first}")
    } else {
        format!("{spec}  ⚠ suspicious: {first} +{extra} more")
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
            // `split_once(':')` below splits on the FIRST colon, so `idx`
            // (digits only) is cleanly separated even when the payload
            // `approval_key` itself carries colons (a `name@file:…`
            // source key). Recording the approval_key — not the bare
            // name — is what lets a source-backed global build be
            // authorized rather than silently ignored by the policy.
            let value = format!("{idx}:{}", ignored.approval_key);
            let base = format_picker_label(&ignored.display_spec(), &ignored.suspicions);
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
        let Some((idx, approval_key)) = item.split_once(':') else {
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
            .push(approval_key.to_string());
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::format_picker_label;
    use aube_scripts::{Suspicion, SuspicionKind};

    #[test]
    fn label_for_clean_package_is_bare_spec() {
        assert_eq!(format_picker_label("esbuild@0.20.2", &[]), "esbuild@0.20.2");
    }

    #[test]
    fn label_for_single_suspicion_shows_category() {
        let s = vec![Suspicion {
            kind: SuspicionKind::ShellPipe,
            hook: "postinstall",
        }];
        assert_eq!(
            format_picker_label("lodash@1.0.0", &s),
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
            format_picker_label("evil-pkg@9.9.9", &s),
            "evil-pkg@9.9.9  ⚠ suspicious: curl|sh +2 more"
        );
    }

    #[test]
    fn label_for_source_backed_package_uses_source_key() {
        assert_eq!(format_picker_label("dep@file:./dep", &[]), "dep@file:./dep");
    }
}
