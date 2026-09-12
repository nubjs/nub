//! The pnpm 12 engine, embedded in-process (feature `pm-pnpm`).
//!
//! Selected by `NUB_PM_ENGINE=pnpm` while aube and pnpm coexist, so the
//! default binary and every existing path stay untouched. The engine parses
//! the process argv itself — nub's PM grammar is pnpm's, so `nub install …`
//! is `pnpm install …` to it — and runs exactly as the standalone `pnpm`
//! binary would. Identity (nub.lock, the Nub store, the rebrand) arrives with
//! the embedder profile on the `nubjs/pnpm` fork; until then this is the
//! proof that the git-dependency build produces a working engine.

use std::process::ExitCode;

use anyhow::Result;

/// Whether the pnpm engine is selected for this invocation.
pub(crate) fn selected() -> bool {
    std::env::var_os("NUB_PM_ENGINE").is_some_and(|v| v == "pnpm")
}

/// Run the engine on the process argv and return its exit status.
///
/// `pnpm_cli::main` reads `std::env::args_os()` directly, and nub's argv for
/// a PM verb is `nub <verb> [args]`, which pnpm's parser reads as its own
/// `<verb> [args]`. `ExitCode` exposes no accessor, so the status is
/// recovered by comparison.
pub(crate) fn run_process_argv() -> Result<i32> {
    let code = pnpm_cli::main();
    Ok(if code == ExitCode::SUCCESS { 0 } else { 1 })
}
