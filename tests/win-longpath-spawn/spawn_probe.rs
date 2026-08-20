//! Does a stock Rust binary fail to spawn an over-long image where Git Bash
//! succeeds? Round one showed bash executing a 280-character path fine, which
//! disproves "Windows cannot CreateProcess past MAX_PATH" as a blanket claim —
//! so the variable is the SPAWNER, not the length. `nub compile` is Rust, and
//! that is the process whose spawn actually failed with os error 3.
//!
//! Built with plain `rustc`, no cargo: this asks about `std::process::Command`
//! and nothing else.

use std::path::PathBuf;
use std::process::Command;

fn attempt(label: &str, program: &str) {
    match Command::new(program).arg("--version").output() {
        Ok(out) => println!(
            "{label:<14} PASS ({})",
            String::from_utf8_lossy(&out.stdout).trim()
        ),
        Err(error) => println!(
            "{label:<14} FAIL (kind={:?} raw={:?}) {error}",
            error.kind(),
            error.raw_os_error()
        ),
    }
}

fn main() {
    let long = PathBuf::from(std::env::args().nth(1).expect("usage: probe <long-exe>"));
    let short = PathBuf::from(std::env::args().nth(2).expect("usage: probe <long> <short>"));
    println!("long path is {} chars", long.as_os_str().len());

    // The exact failing case: the path as-is, through std::process::Command.
    attempt("direct", &long.display().to_string());
    // The prefix the docs say does not help process creation — tested from Rust
    // this time, so bash's own command resolution cannot confound it.
    attempt("verbatim", &format!(r"\\?\{}", long.display()));
    // The shape the fix uses.
    attempt("short-copy", &short.display().to_string());
}
