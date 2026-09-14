//! The parse plumbing every wired engine verb shares.
//!
//! Four families stamp one `usage_rs::Cli` root per verb, parse that verb's
//! argv against it, and route help and usage errors through
//! [`super::present`] so neither can leak the engine's branding. None of that
//! depends on which engine runs afterwards; it lived in the family that
//! needed it first, beside the engine half being removed.
//!
//! [`super::publish_family`]'s `run_wired` deliberately stayed behind: it
//! expands to the session-and-run epilogue, which is engine-specific in
//! exactly the way the rest of this is not.
//!
//! Order matters inside this file: a `macro_rules!` is in scope only after
//! its own definition, and the families invoke these.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use super::present;

/// What a usage error exits with. pnpm and the vendored engine agree on 2,
/// which is usage's and clap's convention too, so this is the shared answer
/// rather than either engine's choice.
pub(super) const EXIT_CLI_USAGE: i32 = 2;
// Spelled absolutely at both macro uses: the expansion lands in the calling
// module, where a bare name resolves to nothing.

/// Outcome of parsing a verb's args: either the parsed value or "already
/// handled" (help/version printed, or a usage error reported) with the
/// process exit code to return.
pub(super) enum Parsed<P> {
    Ok(P),
    Exit(i32),
}

thread_local! {
    /// The command name the wrapper about to render will print — the spelling
    /// the user typed, so `nub i --help` says `nub i`.
    static DISPLAY_NAME: Cell<&'static str> = const { Cell::new("nub") };
}

/// The runtime `#[usage(name = …)]` expression every stamped root carries.
///
/// usage requires a computed identity to be `&'static str` and re-evaluates
/// it on each render, so this is a plain read of the cell
/// [`set_display_name`] just wrote.
pub(super) fn display_name() -> &'static str {
    DISPLAY_NAME.with(Cell::get)
}

/// Publish the display name for the parse or render that follows.
///
/// The `&'static str` requirement means the name has to be interned; one verb
/// runs per process, so outside the test binary the set holds a single entry.
pub(super) fn set_display_name(name: &str) {
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let interned = match names.get(name) {
        Some(found) => *found,
        None => {
            let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
            names.insert(leaked);
            leaked
        }
    };
    drop(names);
    DISPLAY_NAME.with(|cell| cell.set(interned));
}

/// Print a rendered help page to stdout through [`present::rewrite`].
pub(super) fn print_page(page: Option<String>) {
    println!("{}", present::rewrite(page.unwrap_or_default().trim_end()));
}

/// [`print_page`] on stderr, for a usage failure and for the automatic help
/// `arg_required_else_help` raises (which usage models as a failure, exit 2).
pub(super) fn eprint_page(page: Option<String>) {
    eprintln!("{}", present::rewrite(page.unwrap_or_default().trim_end()));
}

/// Stamp one `usage_rs::Cli` root for an engine verb, plus the shared parse
/// epilogue.
///
/// `$spec` is the portable `name_spec` literal (the emitted spec cannot hold
/// the runtime expression) — captured as `tt`, not `literal`: a `literal`
/// fragment reaches the derive wrapped in an opaque group, which its
/// attribute parser rejects as "expected a string". The name a user sees
/// comes from
/// [`display_name`]. `unknown_flags = "error"` restores clap's rejection of a
/// typo'd flag — usage's default is permissive and would bind `--dry-rn` as a
/// positional value. The usage-error exit code is the engine's own
/// `EXIT_CLI_USAGE` (2), which is also clap's, so every family agrees.
///
/// A `///` doc comment on the stamped struct would become the command's
/// about-line and clobber the engine verb's own, so the roots below carry
/// none.
macro_rules! verb_cli {
    ($name:ident, $spec:tt, { $($body:tt)* }) => {
        #[derive(usage_rs::Cli)]
        // The parser WRITES every field; `dead_code` only counts reads, so a
        // root whose surface is help-only (`DlxHelpCli`) would be flagged for
        // fields that exist to be documented. The engine's own root carries
        // the same allow for the same reason.
        #[allow(dead_code)]
        #[usage(
            name = crate::pm_engine::verb_parse::display_name(),
            name_spec = $spec,
            unknown_flags = "error"
        )]
        struct $name { $($body)* }

        impl $name {
            /// Parse this verb's argv under the display name `bin`.
            #[allow(dead_code)]
            fn parse_argv(
                bin: &str,
                args: &[String],
            ) -> crate::pm_engine::verb_parse::Parsed<Self> {
                use crate::pm_engine::verb_parse::{
                    Parsed, eprint_page, print_page, set_display_name,
                };
                let owned: Vec<::std::ffi::OsString> =
                    args.iter().map(::std::ffi::OsString::from).collect();
                let argv: Vec<&::std::ffi::OsStr> =
                    owned.iter().map(::std::ffi::OsString::as_os_str).collect();
                set_display_name(bin);
                match Self::parse_from(&argv) {
                    Ok(parsed) => Parsed::Ok(parsed),
                    Err(usage_rs::Error::Help { cmd, long }) => {
                        print_page(Self::render_help(cmd, long));
                        Parsed::Exit(0)
                    }
                    Err(usage_rs::Error::HelpAll { cmd }) => {
                        print_page(Self::render_help(cmd, true));
                        Parsed::Exit(0)
                    }
                    // `arg_required_else_help`: a usage failure in clap's
                    // terminal contract, so stderr + the usage code.
                    Err(usage_rs::Error::MissingArgsHelp { cmd }) => {
                        eprint_page(Self::render_help(cmd, false));
                        Parsed::Exit(crate::pm_engine::verb_parse::EXIT_CLI_USAGE)
                    }
                    Err(err) => {
                        eprint_page(Some(Self::render_failure(&argv, &err)));
                        Parsed::Exit(crate::pm_engine::verb_parse::EXIT_CLI_USAGE)
                    }
                }
            }

            /// The rewritten long help page under the display name `bin` —
            /// the nub-rendered help paths (`dlx`, `create`) and the
            /// brand-cleanliness sweeps.
            #[allow(dead_code)]
            fn long_help(bin: &str) -> String {
                crate::pm_engine::verb_parse::set_display_name(bin);
                crate::pm_engine::present::rewrite(
                    &Self::render_help(Self::command(), true).unwrap_or_default(),
                )
            }
        }
    };
}
pub(super) use verb_cli;

// `plain_verb_cli!` stood beside `verb_cli!` for the shape that flattens the
// engine's own args type and nothing else. Every verb that used it was a
// publish-family verb, and they are all the engine's now, so it went with the
// family. `verb_cli!` above survives because `store_config_family` still
// stamps roots with it.
