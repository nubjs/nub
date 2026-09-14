//! Output-verbosity state for the install family: the spellings real pnpm
//! accepts — `--reporter <default|append-only|silent>`, `--silent`/`-s`,
//! `--loglevel <level>` — resolved across nub's two flag positions. There are
//! no nub-specific output knobs.
//!
//! Applying the mode is the ENGINE's job: it parses the family's command lines
//! at the CLI front door and runs its own reporter/verbosity startup. What nub
//! still needs from these flags is [`OutputFlags::is_silent`], which decides
//! whether nub's own non-engine output — the resolved-layout report — prints.

use std::sync::atomic::{AtomicU8, Ordering};

// ───────────────────────── process-global defaults ──────────────────────────
//
// nub accepts the output flags in TWO positions: after the verb (`nub install
// --silent`, parsed by the per-verb `OutputFlags` below) and BEFORE it (`nub
// --silent install`, `nub --reporter=silent add foo`). The pre-verb position is
// parsed by the hand-rolled scan in `cli::dispatch`, never by the parser — the
// PM verbs are dispatched through their own usage `Cli` (install/ci) or a
// separately-derived one (the registry verbs), neither of which sees the
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
/// `default`/`append-only`/`silent`. Reuses the derived `ValueEnum` words so the
/// accepted set can't drift from the per-verb flag — case-sensitive, matching
/// the per-verb surface (the enum declares no `ignore_case`).
pub fn set_global_reporter_str(value: &str) -> Result<(), String> {
    use usage_rs::spec::ValueEnum as _;
    let r = Reporter::from_choice(value).ok_or_else(|| value.to_string())?;
    PROC_REPORTER.store(reporter_to_u8(r), Ordering::Relaxed);
    Ok(())
}

/// Parse + record a pre-verb `--loglevel <value>` as a process default. Returns
/// the invalid value when the spelling isn't one of
/// `silent`/`error`/`warn`/`info`/`debug`. Case-sensitive, matching per-verb.
pub fn set_global_loglevel_str(value: &str) -> Result<(), String> {
    use usage_rs::spec::ValueEnum as _;
    let l = LogLevel::from_choice(value).ok_or_else(|| value.to_string())?;
    PROC_LOGLEVEL.store(loglevel_to_u8(l), Ordering::Relaxed);
    Ok(())
}

fn proc_reporter() -> Option<Reporter> {
    u8_to_reporter(PROC_REPORTER.load(Ordering::Relaxed))
}
fn proc_loglevel() -> Option<LogLevel> {
    u8_to_loglevel(PROC_LOGLEVEL.load(Ordering::Relaxed))
}

/// `--reporter` values nub's own commands accept, mirroring pnpm's. `ndjson` is
/// absent: nub's own commands have no event stream. A command line the engine
/// takes keeps it, because the engine judges its own `--reporter`.
// Variant names kebab-case into the accepted words, which is what makes
// `AppendOnly` spell `append-only` without a rename attribute.
#[derive(Copy, Clone, Debug, PartialEq, Eq, usage_rs::ValueEnum)]
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
#[derive(Copy, Clone, Debug, PartialEq, Eq, usage_rs::ValueEnum)]
pub enum LogLevel {
    Silent,
    Error,
    Warn,
    Info,
    Debug,
}

// The resolved output flags. Default = no override (the engine's normal
// output). Spellings mirror pnpm's; each field's `///` doc is its `--help`
// text. Nothing flattens this into a nub-parsed command any more — the family's
// verbs are parsed by the engine — so the values arrive from
// `pnpm_engine::output_flags`, which scans the same three spellings off the
// argv the engine is about to receive. (Plain `//` on the struct: a rustdoc
// comment here would clobber a flattened command's about-text.)
#[derive(Debug, Default, Clone, Copy, usage_rs::Args)]
pub struct OutputFlags {
    /// Output format: `default`, `append-only`, or `silent`.
    #[usage(long, value_name = "NAME", value_enum)]
    pub reporter: Option<Reporter>,

    /// Suppress all output except errors (alias for `--reporter=silent`).
    #[usage(short = 's', long)]
    pub silent: bool,

    /// Log level: logs at or above this level are shown. One of `debug`,
    /// `info`, `warn`, `error`, `silent`.
    #[usage(long, value_name = "LEVEL", value_enum)]
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
    /// `nub --reporter=silent <verb>`). Public so the nub-side output that the
    /// engine does not own — the resolved-layout report — can suppress itself.
    pub fn is_silent(&self) -> bool {
        self.silent
            || self.eff_reporter() == Some(Reporter::Silent)
            || self.eff_loglevel() == Some(LogLevel::Silent)
    }
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
}
