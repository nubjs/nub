//! The pnpm 12 engine, embedded in-process (feature `pm-pnpm`).
//!
//! Selected by `NUB_PM_ENGINE` while aube and pnpm coexist, so the default
//! binary and every existing path stay untouched. nub's PM grammar is pnpm's,
//! so `nub install …` is `pnpm install …` to the engine's parser.
//!
//! `NUB_PM_ENGINE=pnpm` runs the engine under pnpm's own naming, and
//! `NUB_PM_ENGINE=pnpm-nub` under nub's (`nub.lock`, `node_modules/.store`).
//! Both are internal switches that project identity replaces once it routes
//! the PM verbs.

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

/// The engine profile selected for this invocation, if any.
fn selected_profile() -> Option<Embedder> {
    match std::env::var_os("NUB_PM_ENGINE")?.to_str()? {
        "pnpm" => Some(Embedder::PNPM),
        "pnpm-nub" => Some(NUB),
        _ => None,
    }
}

/// Whether the pnpm engine is selected for this invocation.
pub(crate) fn selected() -> bool {
    selected_profile().is_some()
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
    let embedder = selected_profile().unwrap_or(Embedder::PNPM);
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
