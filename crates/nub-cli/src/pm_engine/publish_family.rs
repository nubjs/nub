//! Publish family — registry writes, packaging, and auth through the
//! embedded aube engine: `publish`, `pack`, `version`, `deprecate`,
//! `undeprecate`, `dist-tag` (+`dist-tags`), `unpublish`, `login`
//! (+`adduser`), `logout`, and the npm-fallback verbs `whoami`, `owner`,
//! `token`, `stage`.
//!
//! Wiring shape (shared by every wired engine verb; the generic helpers
//! live here and are borrowed by `store_config_family` — hoist them into
//! `mod.rs` once a third family wants them): parse the verb's aube args
//! type at the nub layer with [`parse_verb`] — clap output (help text,
//! usage errors) is routed through [`present::rewrite`] so `--help` can't
//! leak engine branding — then build an [`super::engine_session`]
//! (embedder preflight: env families, user agent, nub setting defaults)
//! and run the engine command on the session runtime. Failures route
//! through [`present::emit_report`] (brand rewrite + the engine's own
//! exit table); success output is the engine's own (stdout = data,
//! stderr = progress/notices — audited: no engine branding or doc URLs
//! in this family's success prints).
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
//! - npm-fallback verbs (`whoami`, `owner`, `token`, `stage`): upstream
//!   doesn't implement these; nub mirrors its behavior — delegate to `npm`
//!   when the `npmPath` setting is configured, otherwise fail with the
//!   npm-only-command diagnostic (rendered as `ERR_NUB_NPM_ONLY_COMMAND`).

use std::future::Future;

use anyhow::Result;
use aube_workspace::selector::EffectiveFilter;

use super::{VerbSpec, present, stub_error};

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
        "pack" => run_async::<cmd::pack::PackArgs, _, _>(typed, args, cmd::pack::run),
        "version" => run_async::<cmd::version::VersionArgs, _, _>(typed, args, cmd::version::run),
        "deprecate" => {
            run_async::<cmd::deprecate::DeprecateArgs, _, _>(typed, args, cmd::deprecate::run)
        }
        "undeprecate" => {
            run_async::<cmd::undeprecate::UndeprecateArgs, _, _>(typed, args, cmd::undeprecate::run)
        }
        "dist-tag" => {
            run_async::<cmd::dist_tag::DistTagArgs, _, _>(typed, args, cmd::dist_tag::run)
        }
        "unpublish" => {
            run_async::<cmd::unpublish::UnpublishArgs, _, _>(typed, args, cmd::unpublish::run)
        }
        "login" => run_async::<cmd::login::LoginArgs, _, _>(typed, args, cmd::login::run),
        "logout" => run_async::<cmd::logout::LogoutArgs, _, _>(typed, args, cmd::logout::run),
        "whoami" | "owner" | "token" | "stage" => run_npm_fallback(spec.canonical, typed, args),
        // Unreachable while the registry and this match agree; kept so a
        // future registry addition degrades to the stub instead of panicking.
        _ => Err(stub_error(typed, args, pm_hint)),
    }
}

/// Outcome of parsing a verb's args: either the parsed value or "already
/// handled" (help/version printed, or a usage error reported) with the
/// process exit code to return.
pub(super) enum Parsed<P> {
    Ok(P),
    Exit(i32),
}

/// Parse `args` as `P` under the display name `bin` (e.g. `nub pack`, so
/// usage lines read as nub's). Help and usage output is rendered through
/// the brand rewrite — the aube args types carry engine-flavored doc
/// comments that must not reach the terminal verbatim. Exit codes follow
/// the engine/clap convention: 0 for `--help`/`--version`, 2 for usage
/// errors (the engine exit table's cli-usage code).
pub(super) fn parse_verb<P: clap::Parser>(bin: &str, args: &[String]) -> Parsed<P> {
    let argv = std::iter::once(bin.to_string()).chain(args.iter().cloned());
    match P::try_parse_from(argv) {
        Ok(parsed) => Parsed::Ok(parsed),
        Err(err) => {
            let rendered = present::rewrite(&err.render().to_string());
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                print!("{rendered}");
                Parsed::Exit(0)
            } else {
                eprint!("{rendered}");
                Parsed::Exit(2)
            }
        }
    }
}

// Generic wrapper so any bare `clap::Args` struct (the aube per-verb args
// types) can be parsed as a top-level command. (A `//` comment, not `///`:
// clap derives the help about-line from doc comments, and this one must
// not surface in `nub <verb> --help`.)
#[derive(clap::Parser)]
pub(super) struct VerbArgs<A: clap::Args> {
    #[command(flatten)]
    pub(super) args: A,
}

/// The standard wired-verb shape: parse `A`, build the engine session, run
/// the verb's `async fn run(A)` on the session runtime, route failures
/// through the presentation layer.
pub(super) fn run_async<A, F, Fut>(typed: &str, args: &[String], run: F) -> Result<i32>
where
    A: clap::Args,
    F: FnOnce(A) -> Fut,
    Fut: Future<Output = miette::Result<()>>,
{
    let parsed = match parse_verb::<VerbArgs<A>>(&format!("nub {typed}"), args) {
        Parsed::Ok(wrap) => wrap.args,
        Parsed::Exit(code) => return Ok(code),
    };
    let session = super::engine_session(None)?;
    match session.runtime.block_on(run(parsed)) {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// npm-fallback verbs. The engine entry is synchronous, but it resolves
/// the `npmPath` setting through the env/settings seams, so the session's
/// embedder preflight must still run first (the runtime it builds idles).
pub(super) fn run_npm_fallback(
    canonical: &'static str,
    typed: &str,
    args: &[String],
) -> Result<i32> {
    let parsed = match parse_verb::<VerbArgs<aube::commands::npm_fallback::FallbackArgs>>(
        &format!("nub {typed}"),
        args,
    ) {
        Parsed::Ok(wrap) => wrap.args,
        Parsed::Exit(code) => return Ok(code),
    };
    let _session = super::engine_session(None)?;
    match aube::commands::npm_fallback::run(canonical, &parsed) {
        Ok(code) => Ok(code),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

// `nub publish`: aube's `PublishArgs` plus the workspace selector flags
// (global flags upstream; verb-level here because engine verbs bypass
// nub's clap surface). `//` comment — doc comments become the help
// about-line.
#[derive(clap::Parser)]
struct PublishWrap {
    #[command(flatten)]
    args: aube::commands::publish::PublishArgs,
    #[command(flatten)]
    filter: FilterFlags,
}

// The workspace selector surface, mirroring the spellings of aube's
// global flags (`vendor/aube/crates/aube/src/lib.rs::Cli`). `//` comment —
// a doc comment here would surface as the `nub publish --help` about-line.
#[derive(Debug, Default, clap::Args)]
struct FilterFlags {
    /// Restrict the command to workspace packages matching the pattern.
    ///
    /// Supports exact names, globs (`@scope/*`), paths (`./packages/api`),
    /// graph selectors (`pkg...`, `...pkg`), git-ref selectors
    /// (`[origin/main]`), and exclusions (`!pkg`). Repeatable.
    #[arg(short = 'F', long, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run across every workspace package (equivalent to `--filter=*`;
    /// an explicit `--filter` wins).
    #[arg(short = 'r', long)]
    recursive: bool,

    /// Production-only variant of `--filter`: graph walks skip
    /// `devDependencies`. Repeatable; combines with `--filter`.
    #[arg(long, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Error when a workspace selector matches no packages.
    #[arg(long)]
    fail_if_no_match: bool,

    /// Include the workspace root in recursive workspace operations.
    #[arg(long, hide = true)]
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
    let wrap = match parse_verb::<PublishWrap>(&format!("nub {typed}"), args) {
        Parsed::Ok(wrap) => wrap,
        Parsed::Exit(code) => return Ok(code),
    };
    let filter = wrap.filter.effective();
    let session = super::engine_session(None)?;
    match session
        .runtime
        .block_on(aube::commands::publish::run(wrap.args, filter))
    {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

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
    fn parse_verb_resolves_help_to_exit_0_and_usage_errors_to_exit_2() {
        // `--help` is handled at parse time (exit 0, text already brand-
        // rewritten by parse_verb), a bad flag is a usage error (exit 2 —
        // the engine exit table's cli-usage code).
        let help = parse_verb::<VerbArgs<aube::commands::pack::PackArgs>>(
            "nub pack",
            &["--help".to_string()],
        );
        assert!(matches!(help, Parsed::Exit(0)), "--help must exit 0");

        let bad = parse_verb::<VerbArgs<aube::commands::pack::PackArgs>>(
            "nub pack",
            &["--definitely-not-a-flag".to_string()],
        );
        assert!(matches!(bad, Parsed::Exit(2)), "usage errors must exit 2");

        let ok = parse_verb::<VerbArgs<aube::commands::pack::PackArgs>>(
            "nub pack",
            &["--dry-run".to_string()],
        );
        assert!(matches!(ok, Parsed::Ok(w) if w.args.dry_run));
    }
}
