//! A bound on how many times nub can start itself in one chain of processes.
//!
//! Nub resolves `node` (and `npm`, `pnpm`, …) by looking past its own shims, and
//! every new shape of "the binary it found is really nub" has recursed without
//! end: a symlink to the shim in a test's bin dir, a version manager falling back
//! to the next `node` on PATH, a `.bin/node` that execs itself, a wrapper script.
//! Each fix recognised one more shape. This module stops the chain without
//! recognising anything, in two ways that share one variable:
//!
//! - **A counter.** Every nub process reads [`NESTING_ENV`], refuses to run once
//!   it reaches [`MAX_DEPTH`], and hands `depth + 1` to everything it starts. Any
//!   loop that carries the environment through its links (a symlink, a copy, a
//!   wrapper with or without `exec`, a spawning shim, Windows) now ends with one
//!   error instead of filling the process table. rustup's `RUST_RECURSION_COUNT`
//!   and uv's `UV_RUN_RECURSION_DEPTH` are the same idea.
//! - **A probe at the bound.** A probe of a candidate (`--version`, the identity
//!   and flag probes) runs with the counter already at [`MAX_DEPTH`], so a
//!   candidate that leads back to nub, however indirectly, makes nub refuse on the
//!   first re-entry. Discovery reads that refusal in the probe's stderr
//!   ([`probe_reentered_nub`]) and skips the candidate. rustup PR #5136 probes PATH
//!   candidates the same way.
//!
//! **A real Node between two nubs is nesting, not a loop, so launching one clears
//! the count** ([`clear`]; discovery decides what counts as real). What a loop
//! never does is reach a Node, so the count measures exactly that. Without this
//! reset, deep scripts would spend the budget, and worse, a daemon that restarts
//! itself through `node` would inherit one more every generation and refuse to
//! start weeks later, on its 64th restart. rustup and uv have no such reset and
//! accept the first cost; they are not on every `node` a machine runs. A Node
//! reached through a version manager's shim is not proven real, so nesting through
//! one still counts, which is what the generous bound is for.
//!
//! A time window was the other way to forget a stale count. It was rejected: it
//! puts a value that changes on every run into the environment of every child,
//! which defeats tools that cache on the environment.
//!
//! A chain whose links scrub the environment (`env -i`, an allowlist that drops
//! `__NUB_*`) is not bounded here; nub's own build jail carries the variable. Nor is
//! a chain of nub builds older than this module, which neither count nor refuse.

// @lat: [[research/node-version-discovery#Node version discovery and pin-file resolution#9. Failure modes worth naming]]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// `<depth>` or `<depth>:<candidate>`: how many nub processes stand above this
/// one, and the program the nearest of them started as Node, for the error.
pub const NESTING_ENV: &str = "__NUB_NESTING";

/// The deepest a chain of nub processes may go.
pub const MAX_DEPTH: u32 = 64;

/// The error code a refusing nub prints, which a probing nub reads back.
pub const LOOP_CODE: &str = "ERR_NUB_LAUNCH_LOOP";

/// This process's own depth, set once by [`enter`].
static DEPTH: OnceLock<u32> = OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
struct Inherited {
    depth: u32,
    candidate: Option<PathBuf>,
}

/// An unset or unreadable depth counts as 0: nub never writes one it cannot
/// read, and refusing to run over a stray variable would be worse than missing
/// one loop. The depth is read from the raw bytes, because the candidate after it
/// is a path and need not be UTF-8; it only names the loop, so it is shown lossily.
fn parse(value: Option<&OsStr>) -> Inherited {
    let bytes = value.map(OsStr::as_encoded_bytes).unwrap_or_default();
    let (depth, candidate) = match bytes.iter().position(|&byte| byte == b':') {
        Some(colon) => (&bytes[..colon], Some(&bytes[colon + 1..])),
        None => (bytes, None),
    };
    Inherited {
        depth: std::str::from_utf8(depth)
            .ok()
            .and_then(|depth| depth.trim().parse().ok())
            .unwrap_or(0),
        candidate: candidate
            .filter(|candidate| !candidate.is_empty())
            .map(|candidate| PathBuf::from(String::from_utf8_lossy(candidate).into_owned())),
    }
}

/// The refusal: this process would be one nub too many.
#[derive(Debug, thiserror::Error)]
#[error(
    "{LOOP_CODE}: nub has started itself {MAX_DEPTH} times in a row without reaching a \
     confirmed Node binary, so it stopped instead of starting again.\n\
     \x20\x20The last one was started through `{started_as}` and is `{exe}`. A `node` that leads \
     back to nub does this: a link or copy of nub, a wrapper script that runs it, or a version \
     manager that falls back to the next `node` on PATH. Put the real binary ahead of it on \
     PATH, or pin a Node with `nub node pin <version>`."
)]
pub struct LaunchLoop {
    started_as: String,
    exe: String,
}

/// Count this process into the chain: refuse at the bound, else record the depth
/// and export it to every child. Internal re-entries (the parent-death watcher,
/// the node-gyp bootstrap) are mechanism, not links in a chain, and skip this.
///
/// # Safety
///
/// Mutates the process environment, so it must run before any other thread
/// exists. `main` calls it first.
pub unsafe fn enter() -> Result<(), LaunchLoop> {
    let inherited = parse(std::env::var_os(NESTING_ENV).as_deref());
    if inherited.depth >= MAX_DEPTH {
        // Name what the parent started, when it said; else how this process was
        // invoked. A probe passes the candidate's path as argv[0], but a launch
        // sets argv[0] to plain `node`, so the parent's record is the useful one.
        let started_as = inherited
            .candidate
            .or_else(|| std::env::args_os().next().map(PathBuf::from))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "node".to_string());
        let exe = std::env::current_exe()
            .map(|exe| exe.display().to_string())
            .unwrap_or_else(|_| "nub".to_string());
        return Err(LaunchLoop { started_as, exe });
    }
    let depth = inherited.depth + 1;
    let _ = DEPTH.set(depth);
    // SAFETY: upheld by this function's caller.
    unsafe { std::env::set_var(NESTING_ENV, depth.to_string()) };
    Ok(())
}

fn with_candidate(depth: u32, candidate: &Path) -> std::ffi::OsString {
    let mut value = std::ffi::OsString::from(format!("{depth}:"));
    value.push(candidate.as_os_str());
    value
}

/// Name the program a child is about to run as Node, so a nub at the bound can say
/// which one led back to it. The depth is this process's own, which the child
/// inherits anyway; nothing changes when [`enter`] never ran.
pub fn mark_launch(cmd: &mut Command, candidate: &Path) {
    if let Some(depth) = DEPTH.get() {
        cmd.env(NESTING_ENV, with_candidate(*depth, candidate));
    }
}

/// End the chain at a child known to be a real Node: its children start from zero.
pub fn clear(cmd: &mut Command) {
    cmd.env_remove(NESTING_ENV);
}

/// Run a probe of `candidate` at the bound: a real Node ignores the variable, and
/// any nub the probe reaches refuses at once with [`LOOP_CODE`].
pub fn mark_probe(cmd: &mut Command, candidate: &Path) {
    cmd.env(NESTING_ENV, with_candidate(MAX_DEPTH, candidate));
}

/// Whether a failed probe's stderr is nub refusing, i.e. the candidate leads back
/// to nub.
pub fn probe_reentered_nub(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr).contains(LOOP_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_depth_and_the_candidate_that_follows_the_first_colon() {
        assert_eq!(
            parse(None),
            Inherited {
                depth: 0,
                candidate: None
            }
        );
        assert_eq!(parse(Some(OsStr::new("7"))).depth, 7);
        // A Windows path carries its own colon; only the first one separates.
        let windows = parse(Some(OsStr::new(r"64:C:\tools\node.exe")));
        assert_eq!(windows.depth, 64);
        assert_eq!(windows.candidate, Some(PathBuf::from(r"C:\tools\node.exe")));
        // Nothing nub writes, so it counts as no chain rather than refusing.
        assert_eq!(parse(Some(OsStr::new("garbage"))).depth, 0);
        assert_eq!(parse(Some(OsStr::new("3:"))).candidate, None);
    }

    #[test]
    fn a_probe_is_marked_at_the_bound_and_a_refusal_is_recognised() {
        let mut probe = Command::new("node");
        mark_probe(&mut probe, Path::new("/opt/bin/node"));
        let value = probe
            .get_envs()
            .find(|(key, _)| *key == NESTING_ENV)
            .and_then(|(_, value)| value)
            .unwrap();
        let inherited = parse(Some(value));
        assert_eq!(inherited.depth, MAX_DEPTH);
        assert_eq!(inherited.candidate, Some(PathBuf::from("/opt/bin/node")));

        // A candidate path that is not UTF-8 must not cost the depth, or a probe
        // through it would re-enter nub at depth 1 every time.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let mut probe = Command::new("node");
            mark_probe(
                &mut probe,
                Path::new(OsStr::from_bytes(b"/tmp/tools-\xff/node")),
            );
            let value = probe
                .get_envs()
                .find(|(key, _)| *key == NESTING_ENV)
                .and_then(|(_, value)| value)
                .unwrap();
            assert_eq!(parse(Some(value)).depth, MAX_DEPTH);
        }

        let refusal = LaunchLoop {
            started_as: "/opt/bin/node".into(),
            exe: "/opt/nub".into(),
        }
        .to_string();
        assert!(probe_reentered_nub(
            format!("Error: {refusal}\n").as_bytes()
        ));
        assert!(!probe_reentered_nub(b"node: bad option: --frobnicate\n"));
    }
}
