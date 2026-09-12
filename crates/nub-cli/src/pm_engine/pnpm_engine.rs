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

/// Rebrand the engine's diagnostic codes for nub's users.
///
/// The engine declares ~800 `ERR_PNPM_*` codes as compile-time attributes,
/// and two of its own code paths compare those strings literally, so the
/// rename happens here, on the rendered report, rather than where the codes
/// are constructed.
fn rebrand_codes(rendered: &str) -> String {
    rendered.replace("ERR_PNPM_", "ERR_NUB_").replace("WARN_PNPM_", "WARN_NUB_")
}

/// Run the engine on the process argv and return its exit status.
pub(crate) fn run_process_argv() -> Result<i32> {
    let embedder = selected_profile().unwrap_or(Embedder::PNPM);
    match pnpm_cli::run(std::env::args_os().collect(), embedder) {
        Ok(()) => Ok(0),
        Err(report) => {
            let rendered = format!("{report:?}");
            // A pnpm-incumbent project must see pnpm's own codes verbatim.
            if embedder.program_name == Embedder::PNPM.program_name {
                eprintln!("{rendered}");
            } else {
                eprintln!("{}", rebrand_codes(&rendered));
            }
            Ok(1)
        }
    }
}
