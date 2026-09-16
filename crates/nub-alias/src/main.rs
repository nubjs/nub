//! nub-alias — the file behind `nubx` and `nubr` where a symlink cannot go.
//!
//! nub is one binary that reads its verb from argv[0] (`Argv0::detect` in
//! crates/nub-cli/src/cli.rs). The Unix release archives carry `bin/nubx` and
//! `bin/nubr` as symlinks to `bin/nub`; the Windows zips carry this stub under
//! both names instead. Invoked, it takes the verb from its own file stem, finds
//! the `nub` binary beside itself, and runs it with the verb in `__NUB_ARGV0` —
//! the channel the npm launcher already uses, which nub reads once and erases so
//! no grandchild inherits it. Arguments, stdio and the exit status pass straight
//! through.
//!
//! On Unix the stub execs nub in place with argv[0] set to the verb, so it also
//! serves as a local check of the dispatch without a Windows machine.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

const NUB_EXE: &str = if cfg!(windows) { "nub.exe" } else { "nub" };

fn main() -> ExitCode {
    let me = match env::current_exe() {
        Ok(p) => p,
        Err(e) => return fatal(format!("could not locate this executable: {e}")),
    };
    let verb = me
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nub: PathBuf = me.with_file_name(NUB_EXE);
    if !nub.is_file() {
        return fatal(format!(
            "{verb}: the nub binary is not beside this alias at {}; reinstall nub",
            nub.display()
        ));
    }
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    run(&nub, &verb, &args)
}

#[cfg(unix)]
fn run(nub: &PathBuf, verb: &str, args: &[OsString]) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let err = Command::new(nub).arg0(verb).args(args).exec();
    fatal(format!("{verb}: failed to exec {}: {err}", nub.display()))
}

#[cfg(windows)]
fn run(nub: &PathBuf, verb: &str, args: &[OsString]) -> ExitCode {
    // Ctrl+C reaches every process on the console. nub handles it and exits with
    // its own status; the stub only has to stay alive long enough to report that
    // status, so it stops reacting to the signal itself.
    // SAFETY: a null handler with `add = TRUE` only toggles this process's own
    // Ctrl+C handling; there is no memory involved.
    unsafe {
        SetConsoleCtrlHandler(None, 1);
    }
    match Command::new(nub)
        .args(args)
        .env("__NUB_ARGV0", verb)
        .status()
    {
        Ok(status) => match status.code() {
            Some(code) => std::process::exit(code),
            None => ExitCode::FAILURE,
        },
        Err(e) => fatal(format!("{verb}: failed to run {}: {e}", nub.display())),
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(ctrl_type: u32) -> i32>,
        add: i32,
    ) -> i32;
}

fn fatal(msg: String) -> ExitCode {
    eprintln!("nub-alias: {msg}");
    ExitCode::FAILURE
}
