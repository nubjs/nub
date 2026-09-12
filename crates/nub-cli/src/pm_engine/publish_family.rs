//! Publish family — registry writes, packaging, and auth through the
//! embedded aube engine: `publish`, `pack`, `version`, `deprecate`,
//! `undeprecate`, `dist-tag` (+`dist-tags`), `unpublish`, `login`
//! (+`adduser`), `logout`, and the native account/registry verbs
//! `whoami`, `owner` (+`owners`), and `token`.
//!
//! Wiring shape (shared by every wired engine verb; the parse plumbing
//! lives here and is borrowed by the other three families — hoist it into
//! `mod.rs` once it grows a fourth consumer): stamp one concrete
//! `usage_rs::Cli` root per verb with [`verb_cli`], parse the verb's argv
//! against it — help text and usage errors are routed through
//! [`present::rewrite_help`] so `--help` can't leak engine branding — then
//! build an [`super::engine_session`] (embedder preflight: env families,
//! user agent, nub setting defaults) and run the engine command on the
//! session runtime. Failures route through [`present::emit_report`] (brand
//! rewrite + the engine's own exit table); success output is the engine's
//! own (stdout = data, stderr = progress/notices — audited: no engine
//! branding or doc URLs in this family's success prints).
//!
//! # Why one root per verb
//!
//! usage-rs's derive rejects generic structs, so the former
//! `VerbArgs<A: clap::Args>` cannot exist: each verb gets its own concrete
//! root, stamped by [`verb_cli`]. The command NAME is still the spelling the
//! user typed (`nub i`, `nub ls`), because usage evaluates a computed
//! `#[usage(name = …)]` expression on every render — [`display_name`] is
//! that expression, published by [`set_display_name`] immediately before the
//! parse. `name_spec` carries the portable literal for the emitted spec.
//!
//! Help and failures are rendered with `render_help` / `render_failure`
//! rather than `usage_rs::embedded::outcome`: those two are the PLAIN
//! renderers (ANSI styling splitting a word would defeat the brand rewrite)
//! and they apply the computed name, which `outcome` does not.
//!
//! Family notes:
//! - `publish` is the family's one workspace-aware verb: it takes the
//!   selector flags (`--filter`/`-F`, `-r`/`--recursive`, `--filter-prod`,
//!   `--fail-if-no-match`, `--include-workspace-root`) at the verb level,
//!   mirroring aube's global flags + its `compute_effective_filter`.
//! - `login`/`logout` mutate the user's `~/.npmrc` (auth tokens / scoped
//!   registries); registries and tokens always come from `.npmrc`, never
//!   hardcoded. Upstream's `$AUBE_AUTH_TOKEN` escape hatch reads through
//!   the env-families seam, so it is invisible under nub (brand boundary:
//!   nub doesn't honor another tool's branded env vars) — non-interactive
//!   token entry is piped stdin (`echo "$TOKEN" | nub login`).
//! - account/registry verbs (`whoami`, `owner`, `token`): native engine
//!   implementations that hit the registry directly with the same
//!   `.npmrc` auth resolution as the publish writes (`whoami` →
//!   `/-/whoami`; `owner` → collaborators GET + maintainers PUT; `token`
//!   → `/-/npm/v1/tokens` CRUD). `stage` is dropped (not a real npm/pnpm
//!   command) — it falls through to the unknown-command path.
//! - `dist-tag`, `owner` and `token` carry an engine SUBCOMMAND enum, and
//!   usage refuses to flatten a group that declares subcommands (the
//!   parent's tables have nowhere to put them). Their roots therefore
//!   re-declare the engine's own enum plus its sibling flags and rebuild the
//!   engine args struct in `into_engine` — the types are all `pub`, so the
//!   flag surface still comes from upstream rather than a hand mirror.

use std::cell::Cell;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use aube_workspace::selector::EffectiveFilter;

use super::{VerbSpec, present, stub_error};

// ───────────────────────── shared parse plumbing ──────────────────────────
//
// Declared ahead of every consumer in this file: a `macro_rules!` is in scope
// only after its own definition, and `run_verb` below invokes `run_wired!`.

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

/// Print a rendered help page to stdout through the help-grade rewrite.
///
/// Help/usage text describes nub's configured contract (workspace-yaml list,
/// config namespaces, …) rather than runtime facts, which is why it takes
/// [`present::rewrite_help`] and not the plain report rewrite.
pub(super) fn print_page(page: Option<String>) {
    println!(
        "{}",
        present::rewrite_help(page.unwrap_or_default().trim_end())
    );
}

/// [`print_page`] on stderr, for a usage failure and for the automatic help
/// `arg_required_else_help` raises (which usage models as a failure, exit 2).
pub(super) fn eprint_page(page: Option<String>) {
    eprintln!(
        "{}",
        present::rewrite_help(page.unwrap_or_default().trim_end())
    );
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
        // fields that exist to be documented. Upstream aube's own root carries
        // the same allow for the same reason.
        #[allow(dead_code)]
        #[usage(
            name = crate::pm_engine::publish_family::display_name(),
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
            ) -> crate::pm_engine::publish_family::Parsed<Self> {
                use crate::pm_engine::publish_family::{
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
                        Parsed::Exit(aube_codes::exit::EXIT_CLI_USAGE)
                    }
                    Err(err) => {
                        eprint_page(Some(Self::render_failure(&argv, &err)));
                        Parsed::Exit(aube_codes::exit::EXIT_CLI_USAGE)
                    }
                }
            }

            /// The rewritten long help page under the display name `bin` —
            /// the nub-rendered help paths (`dlx`, `create`) and the
            /// brand-cleanliness sweeps.
            #[allow(dead_code)]
            fn long_help(bin: &str) -> String {
                crate::pm_engine::publish_family::set_display_name(bin);
                crate::pm_engine::present::rewrite_help(
                    Self::render_help(Self::command(), true).unwrap_or_default(),
                )
            }
        }
    };
}
pub(super) use verb_cli;

/// [`verb_cli`] for the plain shape: aube's own args type and nothing else.
macro_rules! plain_verb_cli {
    ($name:ident, $spec:tt, $engine:ty) => {
        crate::pm_engine::publish_family::verb_cli! {
            $name, $spec, { #[usage(flatten)] args: $engine }
        }
        impl $name {
            fn into_engine(self) -> $engine {
                self.args
            }
        }
    };
}
pub(super) use plain_verb_cli;

/// Parse `$root` under `nub <typed>` and run `$run` on the shared
/// global-scope session, or return the settled exit code.
macro_rules! run_wired {
    ($root:ident, $typed:expr, $args:expr, $run:path) => {
        match $root::parse_argv(&format!("nub {}", $typed), $args) {
            crate::pm_engine::publish_family::Parsed::Ok(cli) => {
                crate::pm_engine::publish_family::run_engine(cli.into_engine(), $run)
            }
            crate::pm_engine::publish_family::Parsed::Exit(code) => Ok(code),
        }
    };
}
pub(super) use run_wired;

/// Every flag spelling on `cmd` that consumes the FOLLOWING argv token.
///
/// Read off the parse tables themselves, so a value-taking field added to an
/// engine args type — or to a flattened nub group — is picked up with no
/// second list to keep in sync. `require_equals` flags stay out on purpose:
/// their next token is a positional (or a parse error), never their value.
pub(super) fn separate_value_flags(cmd: &usage_rs::Command<'_>) -> Vec<String> {
    let mut spellings: Vec<String> = cmd
        .flags
        .iter()
        .filter(|flag| flag.takes_value && !flag.require_equals)
        .flat_map(|flag| {
            flag.shorts
                .iter()
                .map(|short| format!("-{}", *short as char))
                .chain(flag.longs.iter().map(|long| format!("--{long}")))
                .collect::<Vec<_>>()
        })
        .collect();
    spellings.sort();
    spellings.dedup();
    spellings
}

/// The standard wired-verb epilogue: build the engine session, run the verb's
/// `async fn run(A)` on the session runtime, route failures through the
/// presentation layer.
pub(super) fn run_engine<A, F, Fut>(args: A, run: F) -> Result<i32>
where
    F: FnOnce(A) -> Fut,
    Fut: Future<Output = miette::Result<()>>,
{
    // The shared wired-verb shape backs only GLOBAL-SCOPE verbs — store/cache
    // forensics, package.json edits (`pkg`/`set-script`), and the registry/auth
    // surface (`pack`/`version`/`deprecate`/`token`/…). None read or write the
    // project lockfile, so identity resolution is lenient (see
    // `engine_session_global`): a multi-lockfile project must not block them.
    let session = super::engine_session_global(None)?;
    match session.runtime.block_on(run(args)) {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

// ───────────────────────── per-verb roots ──────────────────────────

/// Dispatcher for the family's verbs. `typed` is the spelling the user
/// wrote (alias-aware: drives `--help`/usage rendering); matching is on
/// the canonical spelling.
pub(crate) fn run_verb(
    spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    use aube::commands as cmd;
    match spec.canonical {
        "publish" => run_publish(typed, args),
        "pack" => run_wired!(PackCli, typed, args, cmd::pack::run),
        "version" => run_wired!(VersionCli, typed, args, cmd::version::run),
        "deprecate" => run_wired!(DeprecateCli, typed, args, cmd::deprecate::run),
        "undeprecate" => run_wired!(UndeprecateCli, typed, args, cmd::undeprecate::run),
        "dist-tag" => run_wired!(DistTagCli, typed, args, cmd::dist_tag::run),
        "unpublish" => run_wired!(UnpublishCli, typed, args, cmd::unpublish::run),
        "login" => run_wired!(LoginCli, typed, args, cmd::login::run),
        "logout" => run_wired!(LogoutCli, typed, args, cmd::logout::run),
        "whoami" => run_wired!(WhoamiCli, typed, args, cmd::whoami::run),
        "owner" => run_wired!(OwnerCli, typed, args, cmd::owner::run),
        "token" => run_wired!(TokenCli, typed, args, cmd::token::run),
        // Unreachable while the registry and this match agree; kept so a
        // future registry addition degrades to the stub instead of panicking.
        _ => Err(stub_error(typed, args, pm_hint)),
    }
}

plain_verb_cli!(PackCli, "nub pack", aube::commands::pack::PackArgs);
plain_verb_cli!(
    VersionCli,
    "nub version",
    aube::commands::version::VersionArgs
);
plain_verb_cli!(
    DeprecateCli,
    "nub deprecate",
    aube::commands::deprecate::DeprecateArgs
);
plain_verb_cli!(
    UndeprecateCli,
    "nub undeprecate",
    aube::commands::undeprecate::UndeprecateArgs
);
plain_verb_cli!(
    UnpublishCli,
    "nub unpublish",
    aube::commands::unpublish::UnpublishArgs
);
plain_verb_cli!(LoginCli, "nub login", aube::commands::login::LoginArgs);
plain_verb_cli!(LogoutCli, "nub logout", aube::commands::logout::LogoutArgs);
plain_verb_cli!(WhoamiCli, "nub whoami", aube::commands::whoami::WhoamiArgs);

// The three subcommand-bearing roots. usage refuses to flatten a group that
// declares subcommands, so each re-declares the engine's own enum beside its
// sibling flags and reassembles the engine args in `into_engine`.
verb_cli! {
    DistTagCli, "nub dist-tag", {
        #[usage(subcommand)]
        command: aube::commands::dist_tag::DistTagCommand,
        #[usage(flatten)]
        network: aube::cli_args::NetworkArgs,
    }
}

impl DistTagCli {
    fn into_engine(self) -> aube::commands::dist_tag::DistTagArgs {
        aube::commands::dist_tag::DistTagArgs {
            command: self.command,
            network: self.network,
        }
    }
}

verb_cli! {
    OwnerCli, "nub owner", {
        #[usage(subcommand)]
        command: aube::commands::owner::OwnerCommand,
        /// One-time password from a 2FA authenticator (for add/rm).
        #[usage(long, value_name = "CODE", global)]
        otp: Option<String>,
        #[usage(flatten)]
        network: aube::cli_args::NetworkArgs,
    }
}

impl OwnerCli {
    fn into_engine(self) -> aube::commands::owner::OwnerArgs {
        aube::commands::owner::OwnerArgs {
            command: self.command,
            otp: self.otp,
            network: self.network,
        }
    }
}

verb_cli! {
    TokenCli, "nub token", {
        #[usage(subcommand)]
        command: aube::commands::token::TokenCommand,
        #[usage(flatten)]
        network: aube::cli_args::NetworkArgs,
    }
}

impl TokenCli {
    fn into_engine(self) -> aube::commands::token::TokenArgs {
        aube::commands::token::TokenArgs {
            command: self.command,
            network: self.network,
        }
    }
}

// `nub publish`: aube's `PublishArgs` plus the workspace selector flags
// (global flags upstream; verb-level here because engine verbs bypass
// nub's own top-level parser).
verb_cli! {
    PublishCli, "nub publish", {
        #[usage(flatten)]
        args: aube::commands::publish::PublishArgs,
        #[usage(flatten)]
        filter: FilterFlags,
    }
}

// The workspace selector surface, mirroring the spellings of aube's
// global flags (`vendor/aube/crates/aube/src/lib.rs::Cli`). `//` comment —
// a doc comment here would surface as the `nub publish --help` about-line.
#[derive(Debug, Default, usage_rs::Args)]
struct FilterFlags {
    /// Restrict the command to workspace packages matching the pattern.
    ///
    /// Supports exact names, globs (`@scope/*`), paths (`./packages/api`),
    /// graph selectors (`pkg...`, `...pkg`), git-ref selectors
    /// (`[origin/main]`), and exclusions (`!pkg`). Repeatable.
    #[usage(short = 'F', long, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run across every workspace package (equivalent to `--filter=*`;
    /// an explicit `--filter` wins).
    #[usage(short = 'r', long)]
    recursive: bool,

    /// Production-only variant of `--filter`: graph walks skip
    /// `devDependencies`. Repeatable; combines with `--filter`.
    #[usage(long, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Error when a workspace selector matches no packages.
    #[usage(long)]
    fail_if_no_match: bool,

    /// Include the workspace root in recursive workspace operations.
    #[usage(long, hide)]
    include_workspace_root: bool,
}

impl FilterFlags {
    /// Mirror of aube's `compute_effective_filter`: `-r` is sugar for
    /// `--filter=*` and a no-op when an explicit selector is present.
    fn effective(self) -> EffectiveFilter {
        let mut filters = self.filter;
        if self.recursive && filters.is_empty() && self.filter_prod.is_empty() {
            filters.push("*".to_string());
        }
        EffectiveFilter {
            filters,
            filter_prods: self.filter_prod,
            fail_if_no_match: self.fail_if_no_match,
            include_workspace_root: self.include_workspace_root,
        }
    }
}

fn run_publish(typed: &str, args: &[String]) -> Result<i32> {
    let wrap = match PublishCli::parse_argv(&format!("nub {typed}"), args) {
        Parsed::Ok(wrap) => wrap,
        Parsed::Exit(code) => return Ok(code),
    };
    let filter = wrap.filter.effective();
    // `publish` reads package.json + workspace catalogs and uploads to the
    // registry; it never reads or writes the project lockfile, so identity
    // resolution is lenient (see `engine_session_global`).
    let session = super::engine_session_global(None)?;
    match session
        .runtime
        .block_on(aube::commands::publish::run(wrap.args, filter))
    {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// `search` is dispatched by [`super::info_family`] (it is a read-only
/// registry query) but rides this family's plain wired shape.
pub(super) fn run_search(typed: &str, args: &[String]) -> Result<i32> {
    run_wired!(SearchCli, typed, args, aube::commands::search::run)
}

plain_verb_cli!(SearchCli, "nub search", aube::commands::search::SearchArgs);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_is_filter_star_unless_an_explicit_selector_wins() {
        // Mirrors aube's compute_effective_filter contract.
        let bare_r = FilterFlags {
            recursive: true,
            ..Default::default()
        };
        assert_eq!(bare_r.effective().filters, vec!["*".to_string()]);

        let explicit = FilterFlags {
            recursive: true,
            filter: vec!["@scope/*".to_string()],
            ..Default::default()
        };
        assert_eq!(explicit.effective().filters, vec!["@scope/*".to_string()]);

        let prod_only = FilterFlags {
            recursive: true,
            filter_prod: vec!["api".to_string()],
            ..Default::default()
        };
        let eff = prod_only.effective();
        assert!(
            eff.filters.is_empty(),
            "-r must not add * beside --filter-prod"
        );
        assert_eq!(eff.filter_prods, vec!["api".to_string()]);
    }

    #[test]
    fn parse_argv_resolves_help_to_exit_0_and_usage_errors_to_exit_2() {
        // `--help` is handled at parse time (exit 0, text already brand-
        // rewritten by parse_argv), a bad flag is a usage error (exit 2 —
        // the engine exit table's cli-usage code).
        let help = PackCli::parse_argv("nub pack", &["--help".to_string()]);
        assert!(matches!(help, Parsed::Exit(0)), "--help must exit 0");

        let bad = PackCli::parse_argv("nub pack", &["--definitely-not-a-flag".to_string()]);
        assert!(
            matches!(bad, Parsed::Exit(code) if code == aube_codes::exit::EXIT_CLI_USAGE),
            "usage errors must exit with the engine's cli-usage code"
        );

        let ok = PackCli::parse_argv("nub pack", &["--dry-run".to_string()]);
        assert!(matches!(ok, Parsed::Ok(w) if w.args.dry_run));
    }

    /// The command name a wrapper renders is the spelling the user typed, not
    /// the portable `name_spec` literal — `nub i --help` must not say
    /// `nub install`.
    #[test]
    fn rendered_help_carries_the_typed_spelling() {
        let help = PackCli::long_help("nub pack-alias");
        assert!(
            help.contains("nub pack-alias"),
            "usage line must carry the typed spelling: {help}"
        );
        assert!(
            !help.to_lowercase().contains("aube"),
            "help must be brand-clean: {help}"
        );
    }
}
