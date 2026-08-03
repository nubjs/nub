//! Output-verbosity flags for the install family, forwarded to the embedded
//! engine's existing text-mode renderers.
//!
//! nub dispatches the engine's command impls directly and never runs aube's
//! `async_main`, so the reporter/verbosity setup `async_main` performs (force
//! the progress UI to text, retune the log level, install the silent-stderr
//! redirect) does not happen for nub — leaving no way to quiet `nub install`.
//! This module mirrors that setup for the spellings real pnpm accepts:
//! `--reporter <default|append-only|silent>`, `--silent`/`-s`, and
//! `--loglevel <level>`. Each maps onto the engine's own switch — there are no
//! nub-specific output knobs.
//!
//! Mapping (mirrors `vendor/aube/crates/aube/src/startup.rs`):
//! - silent (`--silent`/`-s`, `--reporter=silent`, `--loglevel silent`) →
//!   progress to text + engine logs off + [`aube::silence_own_output`] (skips
//!   the install summary; redirects fd 2 on Unix). Matches `pnpm --silent`:
//!   nothing on stderr but fatal errors.
//! - `--reporter=append-only` → progress to text (the engine drops its
//!   progress object; the dependency summary still prints).
//! - `--loglevel <error|warn|info|debug>` → retune the engine log level
//!   (`error` hides warnings; `info`/`debug` surface more). `debug` also
//!   forces text so logs don't collide with the progress display.

use std::sync::atomic::{AtomicU8, Ordering};

use super::log;

// ───────────────────────── process-global defaults ──────────────────────────
//
// nub accepts the output flags in TWO positions: after the verb (`nub install
// --silent`, parsed by the per-verb `OutputFlags` below) and BEFORE it (`nub
// --silent install`, `nub --reporter=silent add foo`). The pre-verb position is
// parsed by the hand-rolled scan in `cli::dispatch`, never by clap — the PM
// verbs are dispatched through their own clap `Command` (install/ci) or a
// separately-built one (the registry verbs), neither of which sees the
// top-level globals. So the pre-verb values are recorded here as process
// defaults and merged UNDER the per-verb flags (per-verb always wins). This
// mirrors the existing `cli::SILENT` atomic that already carries `--silent` to
// `nub run`'s preamble — same seam, extended to the PM reporter/loglevel.
//
// `0` is "unset" for each enum; the encodings are private to this module.
// Pre-verb `--silent`/`-s` folds into `PROC_REPORTER = Silent` (it's the
// documented `--reporter=silent` alias) so all three pre-verb spellings merge
// through the SAME `.or` path and a per-verb `--reporter` can override any of
// them uniformly — there is no separate silent cell.
static PROC_REPORTER: AtomicU8 = AtomicU8::new(0);
static PROC_LOGLEVEL: AtomicU8 = AtomicU8::new(0);

fn reporter_to_u8(r: Reporter) -> u8 {
    match r {
        Reporter::Default => 1,
        Reporter::AppendOnly => 2,
        Reporter::Silent => 3,
    }
}
fn u8_to_reporter(v: u8) -> Option<Reporter> {
    match v {
        1 => Some(Reporter::Default),
        2 => Some(Reporter::AppendOnly),
        3 => Some(Reporter::Silent),
        _ => None,
    }
}
fn loglevel_to_u8(l: LogLevel) -> u8 {
    match l {
        LogLevel::Silent => 1,
        LogLevel::Error => 2,
        LogLevel::Warn => 3,
        LogLevel::Info => 4,
        LogLevel::Debug => 5,
    }
}
fn u8_to_loglevel(v: u8) -> Option<LogLevel> {
    match v {
        1 => Some(LogLevel::Silent),
        2 => Some(LogLevel::Error),
        3 => Some(LogLevel::Warn),
        4 => Some(LogLevel::Info),
        5 => Some(LogLevel::Debug),
        _ => None,
    }
}

/// Record `nub --silent <verb>` as a process default — the `--reporter=silent`
/// alias, so it merges through the same `.or` path as a pre-verb `--reporter`.
/// Called by `cli::dispatch` when the global `--silent`/`-s` precedes a verb.
pub fn set_global_silent() {
    PROC_REPORTER.store(reporter_to_u8(Reporter::Silent), Ordering::Relaxed);
}

/// Parse + record a pre-verb `--reporter <value>` as a process default. Returns
/// the invalid value (for a clean usage error) when the spelling isn't one of
/// `default`/`append-only`/`silent`. Reuses clap's `ValueEnum` parse so the
/// accepted set can't drift from the per-verb flag — `false` = case-sensitive,
/// matching the per-verb clap surface (which has no `ignore_case`).
pub fn set_global_reporter_str(value: &str) -> Result<(), String> {
    use clap::ValueEnum as _;
    let r = Reporter::from_str(value, false).map_err(|_| value.to_string())?;
    PROC_REPORTER.store(reporter_to_u8(r), Ordering::Relaxed);
    Ok(())
}

/// Parse + record a pre-verb `--loglevel <value>` as a process default. Returns
/// the invalid value when the spelling isn't one of
/// `silent`/`error`/`warn`/`info`/`debug`. Case-sensitive, matching per-verb.
pub fn set_global_loglevel_str(value: &str) -> Result<(), String> {
    use clap::ValueEnum as _;
    let l = LogLevel::from_str(value, false).map_err(|_| value.to_string())?;
    PROC_LOGLEVEL.store(loglevel_to_u8(l), Ordering::Relaxed);
    Ok(())
}

fn proc_reporter() -> Option<Reporter> {
    u8_to_reporter(PROC_REPORTER.load(Ordering::Relaxed))
}
fn proc_loglevel() -> Option<LogLevel> {
    u8_to_loglevel(PROC_LOGLEVEL.load(Ordering::Relaxed))
}

/// `--reporter` values nub accepts, mirroring pnpm's. `ndjson` is deliberately
/// absent: a machine-readable event stream is a separate feature from quieting
/// and is not yet wired through the embedder — see the issue thread.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum Reporter {
    /// The default progress display.
    Default,
    /// Plain, append-only output (no progress object).
    AppendOnly,
    /// Suppress all non-error output.
    Silent,
}

/// `--loglevel` values nub accepts, mirroring pnpm's documented set (`debug`,
/// `info`, `warn`, `error`) plus `silent` (what `--silent` resolves to).
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum LogLevel {
    Silent,
    Error,
    Warn,
    Info,
    Debug,
}

// The forwarded output flags, flattened into the install/ci clap surfaces and
// the engine-verb globals (and embedded in the install/ci flag structs).
// Default = no override (the engine's normal output). Spellings mirror pnpm's;
// each field's `///` doc is its `--help` text. (Plain `//` on the struct: a
// rustdoc comment here would clobber the flattened command's about-text.)
#[derive(Debug, Default, Clone, Copy, clap::Args)]
pub struct OutputFlags {
    /// Output format: `default`, `append-only`, or `silent`.
    #[arg(long, value_name = "NAME", value_enum)]
    pub reporter: Option<Reporter>,

    /// Suppress all output except errors (alias for `--reporter=silent`).
    #[arg(short = 's', long)]
    pub silent: bool,

    /// Log level: logs at or above this level are shown. One of `debug`,
    /// `info`, `warn`, `error`, `silent`.
    #[arg(long, value_name = "LEVEL", value_enum)]
    pub loglevel: Option<LogLevel>,
}

impl OutputFlags {
    /// The reporter in effect: the per-verb flag, else the pre-verb process
    /// default (`nub --reporter=… <verb>`). Per-verb always wins.
    fn eff_reporter(&self) -> Option<Reporter> {
        self.reporter.or_else(proc_reporter)
    }

    /// The loglevel in effect: per-verb flag, else the process default.
    fn eff_loglevel(&self) -> Option<LogLevel> {
        self.loglevel.or_else(proc_loglevel)
    }

    /// True when any spelling resolves to full silence (`pnpm --silent`),
    /// including the pre-verb global forms (`nub --silent <verb>`,
    /// `nub --reporter=silent <verb>`). Public so command impls that print
    /// their own (non-engine) summary — e.g. `import` — can suppress it.
    pub fn is_silent(&self) -> bool {
        self.silent
            || self.eff_reporter() == Some(Reporter::Silent)
            || self.eff_loglevel() == Some(LogLevel::Silent)
    }

    /// True at `--loglevel debug`, in either position. nub's own install report
    /// reads this to expand its materialization digest into per-package detail;
    /// the engine's tracing filter is the separate concern `engine_level`
    /// handles.
    pub fn is_debug(&self) -> bool {
        !self.is_silent() && self.eff_loglevel() == Some(LogLevel::Debug)
    }

    /// True when the progress UI must drop to plain text — silent, the
    /// append-only reporter, or a debug log level (whose lines would
    /// otherwise collide with the progress display). Mirrors the engine's
    /// `force_text`.
    fn force_text(&self) -> bool {
        self.is_silent()
            || self.eff_reporter() == Some(Reporter::AppendOnly)
            || self.eff_loglevel() == Some(LogLevel::Debug)
    }

    /// The engine log level to apply, as a tracing token, or `None` to leave
    /// the default. Silent turns logging off entirely.
    fn engine_level(&self) -> Option<&'static str> {
        if self.is_silent() {
            return Some("off");
        }
        match self.eff_loglevel() {
            Some(LogLevel::Error) => Some("error"),
            Some(LogLevel::Info) => Some("info"),
            Some(LogLevel::Debug) => Some("debug"),
            // `warn` is already the default filter, so no reload is needed.
            // Silent is handled above; `None` leaves the default intact.
            Some(LogLevel::Warn | LogLevel::Silent) | None => None,
        }
    }

    /// Apply the resolved output mode for the rest of this command. Returns a
    /// guard ([`OutputGuard`] is itself `#[must_use]`) that MUST be held across
    /// the engine run — when silent, its `Drop` restores stderr (so a final
    /// error report still prints). Idempotent and cheap when no flag is set (the
    /// common path): it does nothing.
    pub fn apply(&self) -> OutputGuard {
        if self.force_text() {
            clx::progress::set_output(clx::progress::ProgressOutput::Text);
        }
        if let Some(level) = self.engine_level() {
            log::set_engine_loglevel(level);
        }
        let silencer = self.is_silent().then(aube::silence_own_output);
        OutputGuard {
            _silencer: silencer,
        }
    }
}

/// Holds the engine's silent-output guard for the duration of a command. Drop
/// restores stderr. Inert (no guard) when the command isn't silent.
#[must_use]
pub struct OutputGuard {
    _silencer: Option<aube::OwnOutputSilencer>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(reporter: Option<Reporter>, silent: bool, loglevel: Option<LogLevel>) -> OutputFlags {
        OutputFlags {
            reporter,
            silent,
            loglevel,
        }
    }

    #[test]
    fn silent_spellings_all_resolve_to_silence() {
        assert!(flags(None, true, None).is_silent());
        assert!(flags(Some(Reporter::Silent), false, None).is_silent());
        assert!(flags(None, false, Some(LogLevel::Silent)).is_silent());
        // append-only is text, not silence.
        assert!(!flags(Some(Reporter::AppendOnly), false, None).is_silent());
        assert!(!flags(None, false, Some(LogLevel::Error)).is_silent());
    }

    #[test]
    fn force_text_covers_silent_append_only_and_debug() {
        assert!(flags(None, true, None).force_text());
        assert!(flags(Some(Reporter::AppendOnly), false, None).force_text());
        assert!(flags(None, false, Some(LogLevel::Debug)).force_text());
        // default reporter / info level keep the rich display.
        assert!(!flags(Some(Reporter::Default), false, None).force_text());
        assert!(!flags(None, false, Some(LogLevel::Info)).force_text());
        assert!(!flags(None, false, None).force_text());
    }

    #[test]
    fn engine_level_maps_each_level_and_silence_to_off() {
        assert_eq!(flags(None, true, None).engine_level(), Some("off"));
        assert_eq!(
            flags(None, false, Some(LogLevel::Error)).engine_level(),
            Some("error")
        );
        assert_eq!(
            flags(None, false, Some(LogLevel::Debug)).engine_level(),
            Some("debug")
        );
        // No level flag — and an explicit `warn`, which equals the default —
        // both leave the filter untouched (no redundant reload).
        assert_eq!(flags(None, false, None).engine_level(), None);
        assert_eq!(
            flags(None, false, Some(LogLevel::Warn)).engine_level(),
            None
        );
        assert_eq!(
            flags(Some(Reporter::AppendOnly), false, None).engine_level(),
            None
        );
    }
}
