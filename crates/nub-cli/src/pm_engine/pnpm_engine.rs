//! The pnpm 12 engine, embedded in-process (feature `pm-pnpm`).
//!
//! Selected by `NUB_PM_ENGINE` while aube and pnpm coexist, so the default
//! binary and every existing path stay untouched. nub's PM grammar is pnpm's,
//! so `nub install …` is `pnpm install …` to the engine's parser.
//!
//! `NUB_PM_ENGINE=auto` lets the project's identity choose, which is what
//! project routing grows into. The two forced values stay beside it because a
//! differential needs them: `pnpm` runs the engine under pnpm's own naming on
//! a fixture nub would claim, and `pnpm-nub` under nub's (`nub.lock`,
//! `node_modules/.store`) on one pnpm would.

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
};

/// How this invocation decides which naming the engine runs under.
enum Selection {
    /// The project's own identity decides — what project routing becomes
    /// once the engine owns the PM verbs outright.
    Auto,
    /// One profile, regardless of the project. This is what makes a
    /// differential against real pnpm possible on a fixture that nub would
    /// otherwise claim, and vice versa.
    Forced(Embedder),
}

/// The selection this invocation asked for, if the engine is selected at all.
fn selection() -> Option<Selection> {
    match std::env::var_os("NUB_PM_ENGINE")?.to_str()? {
        "pnpm" => Some(Selection::Forced(Embedder::PNPM)),
        "pnpm-nub" => Some(Selection::Forced(NUB)),
        "auto" => Some(Selection::Auto),
        _ => None,
    }
}

/// Whether the pnpm engine is selected for this invocation.
pub(crate) fn selected() -> bool {
    selection().is_some()
}

/// The profile the project's own identity asks for.
///
/// This is also where a configuration that cannot be honoured is refused,
/// because it is the first point at which both the identity and nub's own
/// config file are in hand.
fn profile_from_identity() -> Result<Embedder> {
    let cwd = std::env::current_dir()?;
    let identity = project_identity::detect(&cwd);
    if let Some(loaded) = crate::project_config::load_project_config(&cwd)?
        && let Some(path) = loaded.source.path.as_deref()
    {
        project_identity::check_install_block(identity, path, &loaded.values.install)?;
    }
    Ok(match identity {
        ProjectIdentity::Pnpm => Embedder::PNPM,
        ProjectIdentity::Nub => NUB,
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
/// Deliberately narrow: only the usage prefix and the code prefixes are
/// rewritten. The engine also suggests commands as `` `pnpm <verb>` ``, and
/// those are left alone — several name verbs nub either does not have or
/// spells differently, so substituting the program name would turn a brand
/// leak into wrong advice.
fn rebrand(rendered: &str, embedder: Embedder) -> String {
    rendered
        .replace("Usage: pnpm ", &format!("Usage: {} ", embedder.program_name))
        .replace("ERR_PNPM_", "ERR_NUB_")
        .replace("WARN_PNPM_", "WARN_NUB_")
}

/// Run the engine on the process argv and return its exit status.
pub(crate) fn run_process_argv() -> Result<i32> {
    let embedder = match selection() {
        Some(Selection::Forced(embedder)) => embedder,
        Some(Selection::Auto) | None => profile_from_identity()?,
    };
    match pnpm_cli::run(std::env::args_os().collect(), embedder) {
        Ok(()) => Ok(0),
        Err(report) => {
            let rendered = format!("{report:?}");
            // A pnpm-incumbent project must see pnpm's own output verbatim.
            if embedder.program_name == Embedder::PNPM.program_name {
                eprintln!("{rendered}");
            } else {
                eprintln!("{}", rebrand(&rendered, embedder));
            }
            Ok(1)
        }
    }
}
