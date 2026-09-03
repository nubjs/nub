//! `--hide-console`: the launcher's half of hiding a Windows console.
//!
//! The compiler gives the artifact the GUI subsystem, which is where Bun's and
//! Deno's equivalent flags stop — they ARE the process they hide, so a subsystem
//! bit is the whole fix. This launcher is not: it spawns Node, and Windows
//! allocates a console for a console-subsystem child whose parent has none. Left
//! alone, the flash simply moves from the launcher to Node, and a first run moves
//! it again to `curl`, `icacls`, and the `node --version` probe.
//!
//! So the suppression is process-wide rather than threaded through each call, and
//! it is recorded ONCE from the payload manifest before anything spawns.
//! `creation_flags` is the only channel — `Stdio` says nothing about consoles, and
//! `CREATE_NO_WINDOW` is per-`CreateProcess`, so every site has to opt in.
//!
//! CONDITIONAL ON THERE BEING NO CONSOLE ALREADY, which is the part that keeps the
//! flag usable. A GUI process started from `cmd.exe` inherits that console, and
//! suppressing the child's would throw the user's own output away for no gain;
//! started from Explorer there is no console to inherit and nothing to lose. Bun
//! gets the same split for free by being one process, and this reproduces it.

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
static SUPPRESS: AtomicBool = AtomicBool::new(false);

/// Whether the payload asked to be hidden, kept apart from whether anything was
/// actually suppressed. The two differ exactly when a console is already
/// attached, and only that distinction can tell a CI runner that owns a console
/// (where this feature has nothing to do) from a build that lost its flag
/// somewhere between the compiler and the payload.
#[cfg(windows)]
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Record whether every child this launcher spawns must be console-free. Called
/// once, from the payload manifest, before the first spawn.
pub fn arm(hide_console: bool) {
    #[cfg(windows)]
    {
        REQUESTED.store(hide_console, Ordering::Relaxed);
        SUPPRESS.store(
            hide_console && !nub_core::node::spawn::process_has_console(),
            Ordering::Relaxed,
        );
    }
    #[cfg(not(windows))]
    let _ = hide_console;
}

/// What [`arm`] decided, for the timing trace.
///
/// A CI leg cannot see a window, so "no console appeared" is not observable there
/// — only whether this launcher took the suppressing path. Without that the probe
/// would pass identically on a runner that happens to own a console, where nothing
/// is suppressed and every assertion still holds.
pub fn state() -> &'static str {
    #[cfg(windows)]
    {
        match (
            REQUESTED.load(Ordering::Relaxed),
            SUPPRESS.load(Ordering::Relaxed),
        ) {
            (_, true) => "suppressing child consoles",
            // Not a defect, and the reason the two flags are tracked separately:
            // this is the deliberate terminal case, where the user is watching.
            (true, false) => "requested, but a console is already attached",
            (false, false) => "off (not a hidden build)",
        }
    }
    #[cfg(not(windows))]
    "not applicable off Windows"
}

/// Start `cmd` without a console window, on a hidden launch. A no-op everywhere
/// else, including every Unix host — the concept does not exist there.
pub fn apply(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    if SUPPRESS.load(Ordering::Relaxed) {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`: run a console application with no console window,
        /// and do not give it the parent's console either.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
