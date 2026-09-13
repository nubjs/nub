//! The pnpm 12 engine, embedded in-process (feature `pm-pnpm`).
//!
//! Selected by `NUB_PM_ENGINE` while aube and pnpm coexist, so the default
//! binary and every existing path stay untouched. nub's PM grammar is pnpm's,
//! so `nub install …` is `pnpm install …` to the engine's parser.
//!
//! `NUB_PM_ENGINE=auto` lets the project's identity choose, which is what
//! project routing grows into. The two forced values stay beside it because a
//! differential needs them: `pnpm` runs the engine under pnpm's own rules on a
//! fixture nub would claim, and `pnpm-nub` under nub's (`nub.lock`,
//! `node_modules/.store`, and the settings nub resolves) on one pnpm would.

use super::host_settings;
use super::project_identity::{self, ProjectIdentity};
use anyhow::Result;
use pnpm_config::Embedder;

/// nub's naming for the files and directories the engine owns.
const NUB: Embedder = Embedder {
    program_name: "nub",
    program_version: env!("CARGO_PKG_VERSION"),
    // nub provisions Node and pins its own version; the engine must not act
    // on a packageManager pin or a devEngines.runtime entry on its behalf.
    manage_package_manager_versions: false,
    manage_runtimes: false,
    // nub-incumbent projects declare their members in `package.json`, the
    // neutral spelling every package manager reads; nub writes no
    // `pnpm-workspace.yaml`.
    workspaces_from_package_manifest: true,
    lockfile_basename: "nub.lock",
    virtual_store_dirname: ".store",
    // A nub project's configuration is `nub.jsonc`, `package.json` and
    // `.npmrc`, never pnpm's files; `profile` supplies what nub resolved.
    reads_pnpm_config: false,
    workspace_settings: None,
    compat_package_extensions: None,
};

/// nub's own compatibility rules, in the shape the engine's database takes.
///
/// The engine already carries Yarn's rules and pnpm's three additions; these
/// are the machine-derived ones `@nubjs/extensions` adds on top, which the
/// profile layers beneath them. A rule the engine cannot read is dropped
/// rather than failing the install, matching how `compat_db` treats a
/// database it cannot parse at all.
fn host_compat_rules() -> &'static indexmap::IndexMap<String, pnpm_config::PackageExtension> {
    static RULES: std::sync::OnceLock<indexmap::IndexMap<String, pnpm_config::PackageExtension>> =
        std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        super::compat_db::bundled_package_extensions()
            .iter()
            .filter_map(|(selector, body)| {
                serde_json::from_value(body.clone())
                    .ok()
                    .map(|extension| (selector.clone(), extension))
            })
            .collect()
    })
}

/// How this invocation decides which rules the engine runs under.
enum Selection {
    /// The project's own identity decides — what project routing becomes
    /// once the engine owns the PM verbs outright.
    Auto,
    /// One identity, regardless of the project. This is what makes a
    /// differential against real pnpm possible on a fixture that nub would
    /// otherwise claim, and vice versa.
    Forced(ProjectIdentity),
}

/// The selection this invocation asked for, if the engine is selected at all.
fn selection() -> Option<Selection> {
    match std::env::var_os("NUB_PM_ENGINE")?.to_str()? {
        "pnpm" => Some(Selection::Forced(ProjectIdentity::Pnpm)),
        "pnpm-nub" => Some(Selection::Forced(ProjectIdentity::Nub)),
        "auto" => Some(Selection::Auto),
        _ => None,
    }
}

/// Whether the pnpm engine is selected for this invocation.
pub(crate) fn selected() -> bool {
    selection().is_some()
}

/// The profile this invocation runs the engine under.
///
/// This is also where a configuration that cannot be honoured is refused,
/// because it is the first point at which both the identity and nub's own
/// config file are in hand.
fn profile(selection: Selection) -> Result<Embedder> {
    let cwd = std::env::current_dir()?;
    let identity = match selection {
        Selection::Forced(identity) => identity,
        Selection::Auto => project_identity::detect(&cwd),
    };
    let loaded = crate::project_config::load_project_config(&cwd)?;
    if let Some(loaded) = &loaded
        && let Some(path) = loaded.source.path.as_deref()
    {
        project_identity::check_install_block(identity, path, &loaded.values.install)?;
    }
    Ok(match identity {
        ProjectIdentity::Pnpm => Embedder::PNPM,
        ProjectIdentity::Nub => {
            let install = loaded
                .map(|loaded| loaded.values.install)
                .unwrap_or_default();
            // The engine keeps its configuration for the whole run, so the
            // settings live as long as the process does.
            let settings = Box::leak(Box::new(host_settings::resolve(&cwd, &install)?));
            Embedder {
                workspace_settings: Some(settings),
                compat_package_extensions: Some(host_compat_rules()),
                ..NUB
            }
        }
    })
}

/// Rebrand a rendered engine report for nub's users.
///
/// Two families of name survive the profile and reach here as text. The
/// engine declares ~800 `ERR_PNPM_*` codes as compile-time attributes, and
/// two of its own code paths compare those strings literally, so renaming
/// them at construction would change engine behavior. A handful of misuse
/// errors likewise bake `Usage: pnpm <verb>` into a `#[display]` attribute,
/// which the runtime program name the profile sets cannot reach.
///
/// The commands a report tells the user to RUN are rewritten too, by
/// [`rewrite_suggestions`].
fn rebrand(rendered: &str, embedder: Embedder) -> String {
    rewrite_suggestions(rendered, embedder.program_name)
        .replace(
            "Usage: pnpm ",
            &format!("Usage: {} ", embedder.program_name),
        )
        .replace("ERR_PNPM_", "ERR_NUB_")
        .replace("WARN_PNPM_", "WARN_NUB_")
}

/// Verbs whose advice nub has to respell rather than just rename.
///
/// The engine's `self-update` replaces the pnpm it runs as. nub's
/// self-update is `upgrade`, and nub's `update` — which is what the engine
/// reads `upgrade` as — updates dependencies, so swapping the program name
/// alone here would point the user at a command that does something else.
const SUGGESTION_RENAMES: [(&str, &str); 1] = [("self-update", "upgrade")];

/// Rewrite the commands a rendered report tells the user to run.
///
/// The engine writes them `pnpm <verb>`, always inside quotes or
/// backticks, and nub serves nearly every verb it names under the same
/// spelling — so the program name is substituted and the verb kept, except
/// for the few [`SUGGESTION_RENAMES`] respells. The quoting is what keeps
/// this off ordinary prose, where `pnpm` is the subject of a sentence
/// rather than a command to type.
fn rewrite_suggestions(rendered: &str, program: &str) -> String {
    let mut out = String::with_capacity(rendered.len());
    let mut rest = rendered;
    while let Some(at) = rest.find("pnpm ") {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        rest = &from["pnpm ".len()..];
        if !before.ends_with(['"', '`']) {
            out.push_str("pnpm ");
            continue;
        }
        out.push_str(program);
        out.push(' ');
        let verb = rest
            .split(|c: char| !c.is_ascii_lowercase() && c != '-')
            .next()
            .unwrap_or_default();
        if let Some((_, respelled)) = SUGGESTION_RENAMES.iter().find(|(named, _)| *named == verb) {
            out.push_str(respelled);
            rest = &rest[verb.len()..];
        }
    }
    out.push_str(rest);
    out
}

/// Run the engine on `argv` and return its exit status.
///
/// `argv` is the whole command line, program name first, as the host
/// resolved it — not the process argv. nub reads a few flags of its own
/// before the verb and acts on them itself, and the engine's grammar has
/// no spelling for those, so what it runs on is what nub left.
pub(crate) fn run(argv: Vec<std::ffi::OsString>) -> Result<i32> {
    let embedder = profile(selection().unwrap_or(Selection::Auto))?;
    // The engine's own entry point installs this before it can print. It
    // drops each cause the level above already states in full, so a host
    // that leaves miette at its default renders chains the engine collapses
    // — a divergence invisible on a one-level diagnostic and plain on a
    // deep one.
    pnpm_diagnostics::install_report_handler();
    match pnpm_cli::run(argv, embedder) {
        Ok(()) => Ok(0),
        Err(report) => {
            // The engine skips its own render for a command that has
            // already printed its report, and answers for which those are,
            // so nothing here has to track the list across a pin move.
            if !pnpm_cli::is_reported_error(&report) {
                // The `Error: ` prefix is the engine's own, not decoration:
                // without it a pnpm-incumbent project's stderr differs from
                // real pnpm's on every failure.
                let rendered = format!("Error: {report:?}");
                // A pnpm-incumbent project must see pnpm's own output verbatim.
                if embedder.program_name == Embedder::PNPM.program_name {
                    eprintln!("{rendered}");
                } else {
                    eprintln!("{}", rebrand(&rendered, embedder));
                }
            }
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{host_compat_rules, rewrite_suggestions};

    /// Only a quoted command is a command to type. The engine's prose uses
    /// its own name as a subject too, and rewriting that would say nub does
    /// things the sentence is not about.
    #[test]
    fn only_a_quoted_command_is_rewritten() {
        assert_eq!(
            rewrite_suggestions(r#"Run "pnpm approve-builds" to pick."#, "nub"),
            r#"Run "nub approve-builds" to pick."#
        );
        assert_eq!(
            rewrite_suggestions("run `pnpm clean --lockfile` and `pnpm install`.", "nub"),
            "run `nub clean --lockfile` and `nub install`."
        );
        assert_eq!(
            rewrite_suggestions("pnpm requires one wheel per package", "nub"),
            "pnpm requires one wheel per package"
        );
    }

    /// A verb nub spells differently is respelled, not merely renamed:
    /// `nub self-update` does not exist and `nub update` updates
    /// dependencies, so the program name alone would be wrong advice.
    #[test]
    fn a_verb_nub_spells_differently_is_respelled() {
        assert_eq!(
            rewrite_suggestions(r#"run "pnpm self-update latest" to downgrade."#, "nub"),
            r#"run "nub upgrade latest" to downgrade."#
        );
    }

    /// The conversion drops a rule the engine cannot parse rather than failing
    /// an install, so a shape the engine stops accepting after a pin move would
    /// otherwise shrink nub's database with no error anywhere.
    #[test]
    fn every_bundled_compatibility_rule_reaches_the_engine() {
        let bundled = super::super::compat_db::bundled_package_extensions();
        assert!(
            !bundled.is_empty(),
            "the bundled database parsed to nothing"
        );
        let dropped: Vec<&String> = bundled
            .iter()
            .filter(|(_, body)| {
                serde_json::from_value::<pnpm_config::PackageExtension>((*body).clone()).is_err()
            })
            .map(|(selector, _)| selector)
            .collect();
        assert!(
            dropped.is_empty(),
            "rules the engine cannot read: {dropped:?}"
        );
        assert_eq!(host_compat_rules().len(), bundled.len());
    }
}
