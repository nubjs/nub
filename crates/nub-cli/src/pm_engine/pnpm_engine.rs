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
const NUB: Embedder = Embedder { lockfile_basename: "nub.lock", virtual_store_dirname: ".store" };

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

/// Run the engine on the process argv and return its exit status.
pub(crate) fn run_process_argv() -> Result<i32> {
    let embedder = selected_profile().unwrap_or(Embedder::PNPM);
    match pnpm_cli::run(std::env::args_os().collect(), embedder) {
        Ok(()) => Ok(0),
        Err(report) => {
            eprintln!("{report:?}");
            Ok(1)
        }
    }
}
