//! Store/config family — global-store and cache forensics plus settings
//! through the embedded aube engine: `store` (add/path/prune/status),
//! `cache` (list/view/delete/prune/list-registries), `cat-file`,
//! `cat-index`, `find-hash`, `config` (+`c`) with the hidden top-level
//! `get`/`set` shorthands, and the native package.json editors `pkg`,
//! `set-script` (engine-implemented, not an npm shell-out).
//!
//! The wiring helpers (`parse_verb`, `run_async`) are
//! shared with [`super::publish_family`] — see its module doc for the
//! common shape (brand-rewritten help/usage, engine session preflight,
//! failures through [`present::emit_report`]).
//!
//! Family notes:
//! - `store path` prints the *resolved* store-version dir on stdout — under
//!   nub's embedder defaults that is `$XDG_DATA_HOME/nub/store/v1` (data,
//!   not a diagnostic; already nub-named via the `storeDir` default).
//! - `cache` operates on the engine's packument cache under the RESOLVED
//!   `cacheDir` — `<XDG_CACHE_HOME>/nub/pm/packuments-*` by default (the
//!   identity's `cache_namespace`), or wherever `NUB_CACHE_DIR` /
//!   `npm_config_cache_dir` / `.npmrc cache-dir` points it. That makes
//!   `cache list` the cheapest read-side proof of which cache directory the
//!   engine resolved, which is how `pm_env_matrix` pins it. Paths printed by
//!   `cache view --json` / `cache delete` are real on-disk paths, which the
//!   rewrite policy deliberately preserves.
//! - `config` write routing is pnpm-VERSION-AWARE (decision 2026-06-20,
//!   supersedes the earlier "npmrc-first" routing; **no `config.toml`, ever**).
//!   The config home for non-layout SCALAR settings is pnpm-version-dependent —
//!   there is no single file that round-trips on every pnpm — so the router
//!   gates on the incumbent pnpm version (see [`config_model`] +
//!   [`project_scalar_home`]).
//!   npm-shared keys (`registry`, proxies, per-host auth templates,
//!   `@scope:registry`, bare auth scalars, …) → `.npmrc` (engine writer), so
//!   npm/yarn/pnpm of every version see the same value (unchanged). Non-shared
//!   non-layout scalars under a pnpm-v11+ incumbent → `pnpm-workspace.yaml` (created if
//!   absent), because v11 reads scalars SOLELY from the workspace yaml
//!   (`isIniConfigKey` keeps only auth/network in `.npmrc`) so a `.npmrc` scalar
//!   would no-op. Layout scalars always go to `.npmrc`: Nub does not read layout
//!   from branded YAML, and the paired settings allowlist keeps their neutral
//!   aliases readable under pnpm 11. Non-shared scalars under a pnpm-v10/v9 incumbent, the
//!   UNKNOWN-pnpm-version default, and nub identity / npm / yarn / bun → the
//!   *project* `.npmrc` (the neutral home): v10/v9 read scalars from `.npmrc`,
//!   and the unknown default picks `.npmrc` as the safest target for the
//!   dominant v9/v10 base (a v11-shaped yaml written into a v10 project silently
//!   no-ops). Never a pnpm-branded file for these, never `config.toml`;
//!   `--global`/`--local` selectors do not change that project target. READS are
//!   version-AGNOSTIC and need no gate for non-layout settings: the resolver
//!   reads those from both `pnpm-workspace.yaml` and `.npmrc`, while layout reads
//!   only from `.npmrc`.
//!   Workspace *map* settings (`allowBuilds.<pkg>`, `overrides.<pkg>`, bare
//!   `allowBuilds`, …) are refused with a pnpm-workspace.yaml pointer at any
//!   incumbency/version (upstream's fallback would write a
//!   `package.json#aube.<map>` field, and `.npmrc` lines for map entries are
//!   unread). A free-form unknown key has no workspace-yaml schema, so it goes
//!   to `.npmrc` verbatim even under a pnpm-v11 incumbent.
//! - **GLOBAL config reads follow identity; writes stay neutral:**
//!     - **Reads:** the neutral user `~/.npmrc` is always eligible. Pnpm's
//!       branded global `config.yaml` and `auth.ini` are eligible only under a
//!       provable pnpm-v11+ incumbent, through the separate
//!       `read_pnpm_global_config` posture. A pnpm ≤10/unknown-major, Nub, npm,
//!       Yarn, or Bun project never imports them.
//!     - **Writes** (`config set --global`, equivalently
//!       `global config set`): NEVER a PM-branded
//!       global file. In global mode there is no project → no incumbent PM → nub
//!       can't know which PM's global file is meant, so writes go NEUTRAL:
//!       npm-shared/auth keys → `~/.npmrc` (every tool reads it); every other
//!       scalar → nub's neutral global home (also `~/.npmrc`). Never pnpm's
//!       `config.yaml`/`auth.ini`, never `config.toml`. Non-secret settings
//!       default to PROJECT scope (the incumbency split above). Protected npm
//!       credential keys default to the user `~/.npmrc` so an unqualified set
//!       cannot put a token in a commonly tracked project file; explicit
//!       `--local` still selects the project `.npmrc`.
//! - `config list`/`get` delegate to the engine unchanged, with one carve-out:
//!   `config get registry` at the default merged view
//!   substitutes the engine's effective default
//!   (`https://registry.npmjs.org/`) when no config file sets one — the
//!   engine only reads config files and prints `undefined` for the unset
//!   key, where pnpm reports the default it would actually install from.
//!   Other unset keys still print `undefined` (engine behavior; a general
//!   fix belongs upstream — the settings metadata defaults are display
//!   strings, several of which name engine paths nub's embedder tier
//!   replaces, so substituting them wholesale here would lie). On non-unix
//!   the substitution is inert (it rides the fd capture, a documented
//!   no-op there) and `undefined` still prints. Note the scope asymmetry
//!   Delete follows set's scope contract, including the protected-credential
//!   user default, so an unqualified delete reaches the file an unqualified set
//!   populated.
//! - A key naming a `nub.jsonc` field (`nodeCompat`, `install.linker`,
//!   `dlx.consent`, …) is claimed by [`try_nub_field`] before any of the
//!   `.npmrc` routing above and handled by [`crate::config_fields`]. The two
//!   surfaces share one command and one set of scope flags but never one file:
//!   the field table is exact-match, so a key nub does not own reaches the
//!   engine unchanged.
//! - `config explain` / `config find` / `config tui` stay unwired: they
//!   print engine reference docs straight to stdout, bypassing the brand
//!   rewrite. They are hidden from `--help` by [`config_command`], which is
//!   also where nub's own `init` and `path` subcommands are added — help is rendered from
//!   the same `Command` that parses, so the two cannot disagree.

use anyhow::Result;
use aube::commands::config::{ConfigArgs, ConfigCommand};

use super::publish_family::{Parsed, VerbArgs, run_async};
use super::{VerbSpec, present, stub_error};

/// Dispatcher for the family's verbs (see [`super::publish_family::run_verb`]
/// for the shape).
pub(crate) fn run_verb(
    spec: &'static VerbSpec,
    typed: &str,
    args: &[String],
    pm_hint: &str,
) -> Result<i32> {
    use aube::commands as cmd;
    match spec.canonical {
        "store" => run_async::<cmd::store::StoreArgs, _, _>(typed, args, cmd::store::run),
        "cache" => run_async::<cmd::cache::CacheArgs, _, _>(typed, args, cmd::cache::run),
        "cat-file" => {
            run_async::<cmd::cat_file::CatFileArgs, _, _>(typed, args, cmd::cat_file::run)
        }
        "cat-index" => {
            run_async::<cmd::cat_index::CatIndexArgs, _, _>(typed, args, cmd::cat_index::run)
        }
        "find-hash" => {
            run_async::<cmd::find_hash::FindHashArgs, _, _>(typed, args, cmd::find_hash::run)
        }
        "config" | "get" | "set" => run_config(spec.canonical, typed, args),
        "pkg" => run_async::<cmd::pkg::PkgArgs, _, _>(typed, args, cmd::pkg::run),
        "set-script" => {
            run_async::<cmd::set_script::SetScriptArgs, _, _>(typed, args, cmd::set_script::run)
        }
        // Unreachable while the registry and this match agree; kept so a
        // future registry addition degrades to the stub instead of panicking.
        _ => Err(stub_error(typed, args, pm_hint)),
    }
}

/// Parse + dispatch the three config spellings. Top-level `get`/`set` are
/// aube's hidden shorthands for `config get` / `config set`; the
/// subcommand name is spliced into the argv so all three flow through one
/// `ConfigArgs` parse (and usage errors render as `nub get …` / `nub set …`).
fn run_config(canonical: &str, typed: &str, args: &[String]) -> Result<i32> {
    // `init` and `path` are nub's OWN subcommands, absent from the engine's
    // `ConfigCommand`, so they must be claimed ahead of the parse that would
    // reject them — the same interception shape `try_nub_config` uses for
    // nub-namespaced keys. Only the `config`/`c` spelling: under the hidden
    // `get`/`set` shorthands these words are setting keys, not subcommands.
    if canonical == "config"
        && let [subcommand, rest @ ..] = args
        && matches!(subcommand.as_str(), "init" | "path")
        // `--help` is left to the parse below, which renders the subcommand's
        // own help from the augmented command rather than manual validation.
        && !rest.iter().any(|arg| arg == "-h" || arg == "--help")
    {
        return match subcommand.as_str() {
            "init" => run_config_init(typed, rest),
            "path" => run_config_path(typed, rest),
            _ => unreachable!(),
        };
    }
    let (bin, argv): (String, Vec<String>) = match canonical {
        "config" => (format!("nub {typed}"), args.to_vec()),
        shorthand => (
            "nub".to_string(),
            std::iter::once(shorthand.to_string())
                .chain(args.iter().cloned())
                .collect(),
        ),
    };
    let mut parsed = match parse_config_args(&bin, &argv) {
        Parsed::Ok(args) => args,
        Parsed::Exit(code) => return Ok(code),
    };
    inherit_parent_scope(&mut parsed);
    protect_default_auth_scope(&mut parsed);
    dispatch_config(parsed)
}

/// Scope flags are accepted on either side of a config subcommand. The engine
/// flattens the bare-list flags into the parent command, so copy a parent scope
/// into key subcommands before Nub intercepts their keys, then clear the parent
/// copy so the engine does not reject it as a stray list flag. An explicit
/// subcommand scope wins, matching [`aube::commands::config`]'s list behavior.
fn inherit_parent_scope(parsed: &mut ConfigArgs) {
    let parent_global = parsed.list.global;
    let parent_local = parsed.list.local;
    let child_scope = match &mut parsed.command {
        Some(ConfigCommand::Get(args)) => Some((&mut args.global, &mut args.local)),
        Some(ConfigCommand::Set(args)) => Some((&mut args.global, &mut args.local)),
        Some(ConfigCommand::Delete(args)) => Some((&mut args.global, &mut args.local)),
        _ => None,
    };
    if let Some((global, local)) = child_scope {
        if !*global && !*local {
            *global = parent_global;
            *local = parent_local;
        }
        parsed.list.global = false;
        parsed.list.local = false;
    }
}

/// Keep credentials out of a commonly tracked project `.npmrc` unless the
/// user explicitly asks for project scope. Deletion follows the same default
/// as writing so an unqualified command operates on the file an unqualified
/// set populated. Parent-position selectors have already been inherited.
fn protect_default_auth_scope(parsed: &mut ConfigArgs) {
    let args = match &mut parsed.command {
        Some(ConfigCommand::Set(args)) => Some((&args.key, &mut args.global, &args.local)),
        Some(ConfigCommand::Delete(args)) => Some((&args.key, &mut args.global, &args.local)),
        _ => None,
    };
    if let Some((key, global, local)) = args
        && !*global
        && !*local
        && aube::commands::config::is_protected_key(key)
    {
        *global = true;
    }
}

/// The `config` command as NUB wires it: the engine's derived `ConfigArgs`
/// (still the source of truth for every flag and the subcommands it owns), plus
/// nub's own `init` and `path`, minus the three nub refuses.
///
/// Help is rendered from the same `Command` that parses, so `--help` cannot
/// advertise a surface that does not run — the failure this exists to fix, where
/// `path` worked but was invisible while `explain`/`find`/`tui` were listed and
/// errored.
fn config_command(bin: &str) -> clap::Command {
    use clap::CommandFactory as _;

    let mut cmd = VerbArgs::<ConfigArgs>::command().name(bin.to_string());
    for unwired in ["explain", "find", "tui"] {
        cmd = cmd.mut_subcommand(unwired, |sub| sub.hide(true));
    }
    // Each `about` names both homes, because one key space spans them.
    cmd.mut_subcommand("get", |sub| {
        sub.about("Print the effective value of a setting key or `nub.jsonc` field")
    })
    .mut_subcommand("set", |sub| {
        sub.about(
            "Write a setting key to `.npmrc`, or a field to `nub.jsonc`. Protected credentials use the user `.npmrc` unless `--local` is explicit",
        )
    })
    .mut_subcommand("delete", |sub| {
        sub.about(
            "Remove a setting key from `.npmrc`, or a field from `nub.jsonc`. Protected credentials use the user `.npmrc` unless `--local` is explicit",
        )
    })
    .subcommand(config_init_command())
    .subcommand(clap::Command::new("path").about("Print the path of the global `nub.jsonc`"))
}

/// The independently parsed Nub-owned `init` surface. Keeping one command
/// builder for both top-level help and execution makes an advertised flag a
/// runnable flag by construction.
fn config_init_command() -> clap::Command {
    clap::Command::new("init")
        .about("Create a commented `nub.jsonc` without changing any defaults")
        .arg(
            clap::Arg::new("global")
                .short('g')
                .long("global")
                .help("Create the user configuration instead of the project configuration")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            clap::Arg::new("local")
                .long("local")
                .help("Create the project file (the default)")
                .action(clap::ArgAction::SetTrue)
                .conflicts_with("global"),
        )
}

/// Parse against [`config_command`], routing help and usage output through the
/// same brand rewrite [`parse_verb`] applies.
fn parse_config_args(bin: &str, args: &[String]) -> Parsed<ConfigArgs> {
    use clap::FromArgMatches as _;

    let argv = std::iter::once(bin.to_string()).chain(args.iter().cloned());
    let parsed = config_command(bin)
        .try_get_matches_from(argv)
        .and_then(|matches| VerbArgs::<ConfigArgs>::from_arg_matches(&matches));
    match parsed {
        Ok(wrap) => Parsed::Ok(wrap.args),
        Err(err) => {
            let rendered = present::rewrite_help(err.render().to_string());
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

/// Create a behavior-neutral project or user-global `nub.jsonc`. Every setting
/// is commented out; the active schema URL gives editors the exhaustive field
/// descriptions and completions. Existing files are never merged or replaced.
fn run_config_init(typed: &str, rest: &[String]) -> Result<i32> {
    let bin = format!("nub {typed} init");
    let argv = std::iter::once(bin.clone()).chain(rest.iter().cloned());
    let matches = match config_init_command().name(bin).try_get_matches_from(argv) {
        Ok(matches) => matches,
        Err(error) => {
            let rendered = present::rewrite_help(error.render().to_string());
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                print!("{rendered}");
                return Ok(0);
            }
            eprint!("{rendered}");
            return Ok(2);
        }
    };

    let global = matches.get_flag("global");
    let (path, scope) = if global {
        let path = crate::config::config_path().ok_or_else(|| {
            anyhow::anyhow!(
                "nub {typed} init: no config directory resolves\n\
                 \x20\x20set XDG_CONFIG_HOME or HOME to a writable directory"
            )
        })?;
        (path, crate::config::InitScope::Global)
    } else {
        (
            crate::config_fields::project_file(),
            crate::config::InitScope::Project,
        )
    };

    match crate::config::init_file(&path, scope) {
        Ok(()) => {
            println!("Created {}", path.display());
            Ok(0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!(
                "nub: {} already exists\n\x20\x20nothing was written — edit it directly or use `nub config set`",
                path.display()
            );
            Ok(1)
        }
        Err(error) => Err(anyhow::anyhow!(
            "nub {typed} init: could not create {}: {error}",
            path.display()
        )),
    }
}

/// Print the global settings file's path — `~/.config/nub/nub.jsonc` and its
/// `XDG_CONFIG_HOME`/`%APPDATA%` variants, resolved by [`crate::config::config_path`]
/// so the precedence lives in exactly one place. Prints whether or not the file
/// exists and never creates it: the point is `$EDITOR "$(nub config path)"` on a
/// machine that has no settings yet.
fn run_config_path(typed: &str, rest: &[String]) -> Result<i32> {
    if !rest.is_empty() && rest != ["--global"] && rest != ["-g"] {
        eprintln!("nub {typed} path: takes no arguments\n\x20\x20usage: nub {typed} path");
        return Ok(2);
    }
    let path = crate::config::config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "nub {typed} path: no config directory resolves\n\
             \x20\x20set XDG_CONFIG_HOME or HOME to a writable directory"
        )
    })?;
    println!("{}", path.display());
    Ok(0)
}

fn config_is_global(parsed: &ConfigArgs) -> bool {
    match &parsed.command {
        Some(ConfigCommand::Get(get)) => get.global,
        Some(ConfigCommand::Set(set)) => set.global,
        Some(ConfigCommand::Delete(delete)) => delete.global,
        Some(ConfigCommand::List(list)) => list.global,
        _ => parsed.list.global,
    }
}

/// Per-(package-manager, major-version) config-home registry.
///
/// nub's compat targets are tracked PER MAJOR VERSION of each package manager
/// (the AGENTS.md core design position): a PM's config home can move between
/// majors, so the WRITE target for a scalar setting is a small LOOKUP keyed by
/// `(pm, major)` rather than a hardcoded compare. This keeps it cheap to slot in
/// a new major (or a new PM) when one materially diverges — it is the
/// architecture, NOT a mandate to populate every PM. Today only pnpm's config
/// home actually moves across the majors people run (v10 vs v11), so pnpm is the
/// only populated row; npm / yarn-classic vs yarn-berry / bun are EXTENSION
/// POINTS (their scalar settings live in `.npmrc` for the versions nub targets,
/// so they take the neutral default — add a row if a future major moves a home).
///
/// This governs ONLY the project WRITE target for a non-auth SCALAR setting.
/// Auth/registry keys always go to `.npmrc`; map settings are refused; global
/// writes are neutral (`~/.npmrc`); READS are version-agnostic (the resolver
/// reads scalars from both `.npmrc` and the workspace yaml at once).
mod config_model {
    /// Where a non-auth scalar setting must be WRITTEN so the incumbent PM
    /// reads it back — the home that round-trips, per PM+major.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum ScalarHome {
        /// Project `.npmrc` (INI). The lowest-common-denominator home: read by
        /// npm/yarn/bun and by pnpm v9/v10 (and by pnpm v11 for AUTH, though not
        /// for scalars). nub's neutral default + the pnpm-unknown default.
        Npmrc,
        /// Project `pnpm-workspace.yaml` (YAML). pnpm v11+ reads scalar settings
        /// SOLELY from here (`isIniConfigKey` reserves `.npmrc` for auth), so a
        /// `.npmrc` scalar no-ops on v11.
        PnpmWorkspaceYaml,
    }

    /// The scalar config home for a pnpm incumbent at `major` (`None` = pnpm
    /// declared but version unknown).
    ///
    /// pnpm table (verified against pnpm source at the v9.15.9 / v10.15.1 /
    /// v11.3.0 tags):
    /// - **pnpm ≤ 10, and UNKNOWN version → `.npmrc`.** v9 is INI-only; v10
    ///   reads scalars from both `.npmrc` and yaml, so `.npmrc` round-trips and
    ///   avoids emitting a pnpm-branded file. Unknown defaults here too: the
    ///   dominant + most-compatible target (v9/v10 read `.npmrc`; v11 still
    ///   reads AUTH from `.npmrc`; only a v11 SCALAR is missed, which is
    ///   recoverable, whereas a v11-shaped yaml written into a v10 project
    ///   silently no-ops).
    /// - **pnpm ≥ 11 → `pnpm-workspace.yaml`.** v11 reads scalars SOLELY from
    ///   yaml; a `.npmrc` scalar no-ops.
    pub(super) fn pnpm_scalar_home(major: Option<u64>) -> ScalarHome {
        match major {
            Some(m) if m >= 11 => ScalarHome::PnpmWorkspaceYaml,
            // pnpm 9, 10, anything earlier, and unknown → .npmrc.
            _ => ScalarHome::Npmrc,
        }
    }
}

/// Resolve the project's scalar config-WRITE home. Detection: the declared
/// `packageManager` / `devEngines` pin (`declared_pm_raw`, packageManager
/// first) gives the pnpm major. The installed-PM `--version` probe and
/// lockfile-version signal from the agreed detection chain are intentionally
/// NOT consulted: both only matter to move an UNKNOWN version off its default,
/// and the pnpm-unknown default is already the dominant/most-compatible home
/// (`.npmrc`) — so a brittle subprocess probe buys nothing. `pnpm_incumbent`
/// (the resolved config surface) gates whether a pnpm-branded yaml may be
/// written at all (brand boundary): when false (non-pnpm / nub identity) the
/// home is always the neutral `.npmrc`.
fn project_scalar_home(pnpm_incumbent: bool) -> config_model::ScalarHome {
    if !pnpm_incumbent {
        // Non-pnpm incumbent or nub identity: never a pnpm-branded file.
        return config_model::ScalarHome::Npmrc;
    }
    // The version may only select the pnpm yaml/version-gated route when the
    // declared name is LITERALLY "pnpm". `resolve_config_surface` maps an
    // UNKNOWN declared tool name (e.g. `deno`, `vlt`) to `PnpmOrFresh` too
    // (conservative — keeps the full pnpm-compat surface live), so without this
    // name-gate a `packageManager: "deno@11.0.0"` would feed major 11 into the
    // pnpm gate and leak a `pnpm-workspace.yaml` (brand boundary). Gating the
    // version on `name == "pnpm"` means any non-pnpm / unknown declared name —
    // and a genuine fresh / lockfile-only pnpm project, which has NO declaration
    // (name `None`) — resolves to major `None` → the `.npmrc` model.
    let major = std::env::current_dir()
        .ok()
        .and_then(|cwd| nub_core::pm::resolve::declared_pm_raw(&cwd))
        .and_then(|(name, version)| (name == "pnpm").then_some(version).flatten())
        .and_then(|v| super::parse_major_minor(&v).0);
    config_model::pnpm_scalar_home(major)
}

/// nub's OWN settings live in `nub.jsonc`, NOT `.npmrc` (which npm will soon
/// ERROR on for unrecognized keys) — so a key naming a `nub.jsonc` field is
/// intercepted BEFORE the engine's `.npmrc`/pnpm-yaml routing. Two tables claim
/// keys here, both exact-match so a key nub does not own falls through
/// unchanged: [`crate::config_fields::FIELDS`], the nub CLI address table; and
/// the legacy `exec.implicitDlx` spelling below, which predates that schema and
/// keeps its own storage location and its `prompt` default on an unset read.
///
/// The kebab spelling `exec.implicit-dlx` is accepted as a read/route ALIAS
/// (stale muscle memory from the pre-migration TOML key) but always writes the
/// canonical `exec.implicitDlx`.
///
/// `set`/`get`/`delete` (and the `unset`/`rm` aliases, which parse to `Delete`)
/// all route so a field can be cleared back to its default too — without the
/// delete arm, clearing a nub field would silently no-op against an `.npmrc`
/// that never held it. Returns `Some(exit)` when the key was ours, `None` to
/// fall through to the engine's `.npmrc`-class handling.
fn try_nub_config(parsed: &ConfigArgs, global: bool) -> Option<i32> {
    use crate::config::ImplicitDlx;
    if let Some(code) = try_nub_field(parsed, global) {
        return Some(code);
    }
    const KEY: &str = "exec.implicitDlx";
    const KEY_ALIAS: &str = "exec.implicit-dlx";
    let is_key = |k: &str| k == KEY || k == KEY_ALIAS;
    match &parsed.command {
        Some(ConfigCommand::Set(set)) if is_key(&set.key) => {
            let Some(value) = ImplicitDlx::parse(&set.value) else {
                eprintln!(
                    "nub: `{}` is not a valid value for {KEY} (use `prompt` or `never`).",
                    set.value
                );
                return Some(1);
            };
            match crate::config::set_implicit_dlx(value) {
                Ok(()) => {
                    println!("{KEY} = {}", value.as_str());
                    Some(0)
                }
                Err(e) => {
                    eprintln!("nub: could not write nub.jsonc: {e}");
                    Some(1)
                }
            }
        }
        Some(ConfigCommand::Get(get)) if is_key(&get.key) => {
            println!("{}", crate::config::implicit_dlx().as_str());
            Some(0)
        }
        // `delete`/`unset`/`rm` all parse to `Delete` — clear it in nub.jsonc
        // rather than no-op'ing on `.npmrc` (which never held it).
        Some(ConfigCommand::Delete(del)) if is_key(&del.key) => {
            match crate::config::unset_implicit_dlx() {
                Ok(()) => Some(0),
                Err(e) => {
                    eprintln!("nub: could not write nub.jsonc: {e}");
                    Some(1)
                }
            }
        }
        _ => None,
    }
}

/// Route a `nub.jsonc` field to [`crate::config_fields`], or `None` when the key
/// names no field.
///
/// Scope comes from Nub's public `--global` / `--local` selectors, so
/// `nub.jsonc` and `.npmrc` settings share one grammar. An unflagged field write
/// stays `Auto` rather than collapsing to `Project`: global-only fields such as
/// `dlx.consent` have no project home and must still reach the global file.
fn try_nub_field(parsed: &ConfigArgs, global: bool) -> Option<i32> {
    use crate::config_fields::{self, Scope};

    let write_scope = |local: bool| {
        if global {
            Scope::Global
        } else if local {
            Scope::Project
        } else {
            Scope::Auto
        }
    };
    let outcome = match &parsed.command {
        Some(ConfigCommand::Get(get)) => {
            let field = config_fields::field(&get.key)?;
            let scope = if global {
                Scope::Global
            } else if get.local {
                Scope::Project
            } else {
                Scope::Auto
            };
            config_fields::get(field, scope, get.json)
        }
        Some(ConfigCommand::Set(set)) => {
            let field = config_fields::field(&set.key)?;
            config_fields::set(field, &set.value, write_scope(set.local))
        }
        Some(ConfigCommand::Delete(del)) => {
            let field = config_fields::field(&del.key)?;
            config_fields::delete(field, write_scope(del.local))
        }
        _ => return None,
    };
    Some(outcome.unwrap_or_else(|e| {
        eprintln!("nub: {e}");
        1
    }))
}

fn dispatch_config(parsed: ConfigArgs) -> Result<i32> {
    let global = config_is_global(&parsed);
    if let Some(code) = try_nub_config(&parsed, global) {
        return Ok(code);
    }
    match &parsed.command {
        // Write routing (module doc). GLOBAL writes (`--global`)
        // are NEUTRAL-ONLY — nub never writes a PM-branded global file: in
        // global mode there is no project, hence no incumbent PM, so nub can't
        // know which PM's global file the user means. npm-shared/auth keys go
        // to `~/.npmrc` (every tool reads it); every other scalar goes to
        // nub's neutral global home (also `~/.npmrc` — the resolver reads each
        // setting's `.npmrc` alias from the user file). PROJECT writes mirror
        // pnpm v11's `getConfigFileInfo`: npm-shared → `.npmrc`; non-shared
        // scalar → `pnpm-workspace.yaml` under a pnpm incumbent (parity) else
        // the neutral project `.npmrc`; maps refused. The pnpm-incumbent
        // signal is the resolved config surface (project scope only).
        Some(ConfigCommand::Set(set)) => {
            super::engine_brand_preflight();
            if global {
                // Global scope has no router, so it repeats the refusals it
                // needs — the same shape as the map refusal below. A setting
                // nub does not consume is refused in BOTH scopes; `--global`
                // would otherwise be an open door straight to `~/.npmrc`.
                if let Some(err) = npmrc_first::unsupported_setting_refusal(&set.key) {
                    return Err(err);
                }
                // Neutral global write. npm-shared/auth keys FIRST (a key like
                // `registry` is auth, not the `registries` map — the shared
                // check must win before the map refusal below).
                if npmrc_first::is_npm_shared_key(&set.key) {
                    // Auth/registry → engine's `~/.npmrc` writer at user scope.
                    // Fall through to the engine's user-scoped writer.
                } else if let Some(meta) = npmrc_first::map_setting_meta(&set.key) {
                    // A bare map setting can't be a single scalar; the neutral
                    // home is `.npmrc`, which can't hold a map either.
                    return Err(npmrc_first::map_setting_error_for(meta));
                } else {
                    return npmrc_first::set_user_npmrc(&set.key, &set.value);
                }
            } else {
                // A non-shared scalar lands in `pnpm-workspace.yaml` ONLY under
                // a pnpm-v11+ incumbent (v11 reads scalars solely from YAML); a
                // pnpm-v10/v9 incumbent — and the unknown-version default — keep
                // scalars in the neutral project `.npmrc` (v9/v10 read them from
                // there, and v11 still reads auth from there). Non-pnpm and
                // nub-identity surfaces also keep `.npmrc` (read_branded off).
                // `nub.jsonc` outranks `.npmrc` for the settings it supplies,
                // so ask what THIS project sets before choosing a file. The
                // answer is a refusal rather than a different destination: the
                // two surfaces do not share a value grammar, and moving the
                // write would desynchronize `get` from `set` (module doc on
                // the duplicate_home module docs).
                let (supplied, _native) =
                    super::project_supplied_settings(&std::env::current_dir()?);
                if let Some(field) = super::duplicate_home::shadowing_field(&set.key, &supplied) {
                    return Err(super::duplicate_home::shadowed_error(&set.key, field));
                }
                let pnpm_incumbent = aube_util::engine_context().read_branded_pnpm_config;
                let scalar_to_yaml = project_scalar_home(pnpm_incumbent)
                    == config_model::ScalarHome::PnpmWorkspaceYaml;
                match npmrc_first::classify_set(&set.key, scalar_to_yaml) {
                    npmrc_first::SetRoute::Engine => {} // fall through to delegate
                    npmrc_first::SetRoute::ProjectWorkspaceYaml => {
                        return npmrc_first::set_project_workspace_yaml(&set.key, &set.value);
                    }
                    npmrc_first::SetRoute::ProjectNpmrc => {
                        return npmrc_first::set_project_npmrc(&set.key, &set.value);
                    }
                    npmrc_first::SetRoute::Refuse(err) => return Err(err),
                }
            }
        }
        // Unset `registry` at the merged view: substitute the engine's
        // effective default for its `undefined` (module doc).
        Some(ConfigCommand::Get(get)) if get.key == "registry" && !get.local && !get.global => {
            let json = get.json;
            return run_config_get_registry(parsed, json);
        }
        Some(ConfigCommand::Explain(_)) => return Err(unwired_config_sub("explain")),
        Some(ConfigCommand::Find(_)) => return Err(unwired_config_sub("find")),
        Some(ConfigCommand::Tui) => return Err(unwired_config_sub("tui")),
        // `get` / `list` / `delete` / bare `config` delegate unchanged.
        _ => {}
    }
    // `config` reads/writes `.npmrc`-class settings; it never reads the project
    // lockfile, so identity resolution is lenient (see `engine_session_global`):
    // a multi-lockfile project must not block a `config get`/`set`/`list`.
    let session = super::engine_session_global(None)?;
    match session
        .runtime
        .block_on(aube::commands::config::run(parsed))
    {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// `config get registry` with no value in any config file: the engine's
/// lookup prints `undefined`; pnpm prints the default registry it would
/// actually install from. Run the engine's own lookup with stdout captured
/// and substitute only that exact outcome — any configured value passes
/// through byte-identical. The default literal mirrors the engine's
/// `NpmConfig` default (vendor/aube/crates/aube-registry/src/config/load.rs).
fn run_config_get_registry(parsed: ConfigArgs, json: bool) -> Result<i32> {
    const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";
    // Global-scope config read (see the sibling `config` dispatch): lenient.
    let session = super::engine_session_global(None)?;
    let (result, captured) = super::with_fd_captured(1, || {
        session
            .runtime
            .block_on(aube::commands::config::run(parsed))
    });
    let code = match result {
        Ok(()) => 0,
        Err(report) => present::emit_report(&report),
    };
    if code == 0 && captured.trim() == "undefined" {
        if json {
            println!("{}", serde_json::Value::String(DEFAULT_REGISTRY.into()));
        } else {
            println!("{DEFAULT_REGISTRY}");
        }
    } else {
        print!("{captured}");
    }
    Ok(code)
}

/// The engine prints reference docs for these straight to stdout (no
/// rewrite seam), so they stay unwired rather than risk a brand leak.
fn unwired_config_sub(sub: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nub config {sub}: not supported yet\n\
         \x20\x20settings reference: https://pnpm.io/settings (nub reads the same `.npmrc` keys)"
    )
}

#[cfg(test)]
mod help_tests {
    /// Nub's public config help exposes the project-default / `--global`
    /// grammar and does not retain the engine's location selector as a hidden
    /// compatibility surface.
    #[test]
    fn config_help_exposes_global_without_location() {
        let mut cmd = super::config_command("nub config");
        let help = crate::pm_engine::present::rewrite_help(cmd.render_long_help().to_string());
        let set_help = crate::pm_engine::present::rewrite_help(
            cmd.find_subcommand_mut("set")
                .expect("config has a set subcommand")
                .render_long_help()
                .to_string(),
        );
        let delete_help = crate::pm_engine::present::rewrite_help(
            cmd.find_subcommand_mut("delete")
                .expect("config has a delete subcommand")
                .render_long_help()
                .to_string(),
        );
        for (name, text) in [
            ("config", &help),
            ("config set", &set_help),
            ("config delete", &delete_help),
        ] {
            assert!(
                !text.to_lowercase().contains("aube") && !text.contains("config.toml"),
                "nub {name} help must be brand-clean and config.toml-free: {text}"
            );
            assert!(text.contains("--global"), "nub {name}: {text}");
            assert!(!text.contains("--location"), "nub {name}: {text}");
        }
        for text in [&set_help, &delete_help] {
            assert!(
                text.contains(
                    "Protected credentials use the user `.npmrc` unless `--local` is explicit"
                ),
                "{text}"
            );
        }
    }
}

/// The npmrc-first write routing for non-shared keys. See the module doc
/// for the policy; this module owns the predicate (mirrored from the
/// engine's `is_npm_shared_key`, which is crate-private at the pinned
/// API), the route classification, and the project-`.npmrc` writer.
mod npmrc_first {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, anyhow};
    use aube_settings::meta::{self, SettingMeta};

    use crate::pm_engine::present;

    pub(super) enum SetRoute {
        /// npm-shared key → the engine's own `.npmrc` writer (location
        /// honored, stale `config.toml` shadows swept by upstream). This is
        /// nub's `isIniConfigKey` equivalent: registry/auth/`@scope:`/`//host`
        /// keys npm + yarn + pnpm all read from `.npmrc`.
        Engine,
        /// Non-layout, non-shared scalar under a pnpm-**v11+** incumbent →
        /// `pnpm-workspace.yaml`. v11 reads scalar settings SOLELY from the
        /// workspace yaml (`isIniConfigKey` keeps only auth/network in
        /// `.npmrc`), so a scalar written to `.npmrc` would no-op; nub mirrors
        /// v11 and creates the yaml so a subsequent `pnpm config get` / install
        /// reads it back. Only fires under a provable pnpm-v11+ incumbent — a
        /// pnpm-named file is never written for v10/v9 (they read `.npmrc`), nor
        /// for non-pnpm / nub identity (brand boundary).
        ProjectWorkspaceYaml,
        /// Layout scalar under every incumbent, or another non-shared scalar
        /// outside pnpm v11 → the project `.npmrc` (the
        /// neutral home: every tool reads it, no pnpm-branded file emitted,
        /// never `config.toml`). Covers pnpm v10/v9 (they read scalars from
        /// `.npmrc`), the unknown-pnpm-version default (safest for the dominant
        /// v9/v10 base), and nub identity / npm / yarn / bun. Matches what
        /// `nub pm use nub` migration writes for a nub-identity project.
        ProjectNpmrc,
        /// Workspace map settings and scalar-nested misuses.
        Refuse(anyhow::Error),
    }

    /// Classify a `config set` key. Pure (no fs) so the routing table is
    /// unit-testable; the write itself happens in [`set_project_npmrc`] /
    /// [`set_project_workspace_yaml`].
    ///
    /// `scalar_to_yaml` is true ONLY for a pnpm-**v11+** incumbent — the one
    /// version whose config home for non-layout scalar settings is
    /// `pnpm-workspace.yaml` (see `pnpm_uses_yaml_scalar_home`). It decides ONLY
    /// where a non-shared, non-layout scalar lands: `pnpm-workspace.yaml` under v11, the neutral project
    /// `.npmrc` for pnpm v10/v9, the unknown-version default, and every
    /// non-pnpm / nub-identity surface. npm-shared keys (`.npmrc`) and map
    /// refusals are independent of this signal.
    pub(super) fn classify_set(key: &str, scalar_to_yaml: bool) -> SetRoute {
        // A setting nub's embedder profile declares it does not consume. First,
        // and its own arm rather than a case of the `setting_for_key` match
        // below — that lookup is embedder-FILTERED, so an unsupported setting
        // reads as unknown and falls to the free-form `ProjectNpmrc` route,
        // writing the key verbatim into the user's `.npmrc`. That is how
        // `aubeNoAutoInstall` used to land there: inert, unreadable by anything,
        // and carrying the engine's brand into a file nub wrote.
        if let Some(err) = unsupported_setting_refusal(key) {
            return SetRoute::Refuse(err);
        }
        if is_npm_shared_key(key) {
            return SetRoute::Engine;
        }
        let scalar_route = if scalar_to_yaml {
            SetRoute::ProjectWorkspaceYaml
        } else {
            SetRoute::ProjectNpmrc
        };
        match setting_for_key(key) {
            // Bare map setting (`allowBuilds`, `overrides`, …): a single
            // scalar can't represent it, and upstream's per-entry fallback
            // writes `package.json#aube.<map>` — a foreign-brand manifest
            // field nub must never produce.
            Some(meta) if meta.type_ == "object" => SetRoute::Refuse(map_setting_error(meta.name)),
            // A layout scalar never goes to `pnpm-workspace.yaml`, whatever the
            // incumbent, because Nub never reads layout from that file.
            // Routing these by the pnpm-v11 scalar home would write
            // a key the very next install ignores — `config set` reporting
            // success, then the install header pointing at `nub.jsonc` /
            // `.npmrc` about the setting just written. `.npmrc` is
            // where the paired `keep_layout` allowlist reads them back from.
            Some(meta) if meta.layout => SetRoute::ProjectNpmrc,
            // A known scalar with NO `.npmrc` alias cannot be read back out of
            // the file this route writes: `write_plan` falls back to the key
            // verbatim, so the line lands, `config set` reports success, and
            // every reader looks somewhere else — the same silent no-op an
            // unsupported setting used to produce. Refused rather than declared
            // unsupported, because the surfaces these DO have (a CLI flag, the
            // workspace yaml under a pnpm incumbent) keep working and must keep
            // reading. Only the `.npmrc` route is decided here; the yaml route's
            // own `.npmrc` fallback re-asks in [`set_project_npmrc`].
            Some(meta) if !scalar_to_yaml && meta.npmrc_keys.is_empty() => {
                SetRoute::Refuse(no_npmrc_home_error(meta))
            }
            // Known scalar (including canonical dotted names like
            // `peerDependencyRules.allowedVersions`).
            Some(_) => scalar_route,
            None => {
                if let Some((prefix, _)) = key.split_once('.')
                    && let Some(meta) = setting_for_key(prefix)
                {
                    if meta.type_ == "object" {
                        return SetRoute::Refuse(map_entry_error(meta.name, key));
                    }
                    return SetRoute::Refuse(scalar_nested_error(meta, key));
                }
                // Free-form unknown key → project `.npmrc` verbatim. Even
                // under a pnpm incumbent an unknown key has no workspace-yaml
                // schema, so `.npmrc` (free-form) is the only safe home — this
                // matches the engine's own unknown-key handling.
                SetRoute::ProjectNpmrc
            }
        }
    }

    /// Write a non-shared scalar to `pnpm-workspace.yaml` (force-creating it),
    /// via the engine's typed, comment-preserving workspace-yaml writer. Falls
    /// back to the project `.npmrc` when the setting has no workspace-yaml key
    /// (e.g. a known scalar that only exists as an `.npmrc` alias) — keeping
    /// the value readable rather than dropping it.
    pub(super) fn set_project_workspace_yaml(key: &str, value: &str) -> Result<i32> {
        match aube::commands::config::set_project_scalar_to_workspace_yaml(key, value) {
            Ok(Some(path)) => {
                present::info(&format!("set {key}={value} ({})", path.display()));
                Ok(0)
            }
            // No workspace-yaml mapping for this scalar → neutral `.npmrc`.
            Ok(None) => set_project_npmrc(key, value),
            Err(report) => Ok(present::emit_report(&report)),
        }
    }

    /// Write `key=value` to the project `.npmrc`, sweeping alias spellings
    /// so a stale `auto-install-peers=` line can't shadow a fresh
    /// `autoInstallPeers=` write (the engine reads them last-write-wins).
    pub(super) fn set_project_npmrc(key: &str, value: &str) -> Result<i32> {
        if let Some(err) = no_npmrc_home_refusal(key) {
            return Err(err);
        }
        let path = project_root().join(".npmrc");
        let (sweep, write_key) = write_plan(key);
        npmrc_set(&path, &sweep, &write_key, value)?;
        report_set(&write_key, value, &path);
        Ok(0)
    }

    /// Write a NON-shared scalar to the user `~/.npmrc` — nub's NEUTRAL global
    /// config home. A global write (`config set --global`) must
    /// never touch a PM-branded global file (pnpm's `config.yaml`/`auth.ini`):
    /// in global mode there is no project and no incumbent PM, so nub can't
    /// know which PM's file is meant. `~/.npmrc` is brand-neutral and the
    /// resolver reads each setting's `.npmrc` alias from it, so the write is
    /// read-coherent. (Auth/registry keys take the engine's own user-`.npmrc`
    /// writer instead — see the `set` dispatch.)
    pub(super) fn set_user_npmrc(key: &str, value: &str) -> Result<i32> {
        if let Some(err) = no_npmrc_home_refusal(key) {
            return Err(err);
        }
        let Some(home) = home_dir() else {
            return Err(anyhow!(
                "nub config set --global: could not locate the home directory\n\
                 \x20\x20set HOME (or USERPROFILE on Windows) to point at your user config"
            ));
        };
        let path = home.join(".npmrc");
        let (sweep, write_key) = write_plan(key);
        npmrc_set(&path, &sweep, &write_key, value)?;
        report_set(&write_key, value, &path);
        Ok(0)
    }

    fn report_set(key: &str, value: &str, path: &Path) {
        let shown = if aube::commands::config::is_protected_key(key) {
            "(protected)"
        } else {
            value
        };
        present::info(&format!("set {key}={shown} ({})", path.display()));
    }

    /// The setting metadata for `key` iff it's a bare object-typed (map)
    /// setting — used by the global-write path to refuse a scalar set of a
    /// map setting before routing.
    pub(super) fn map_setting_meta(key: &str) -> Option<&'static SettingMeta> {
        setting_for_key(key).filter(|m| m.type_ == "object")
    }

    /// The same map-refusal error as the project path, by meta.
    pub(super) fn map_setting_error_for(meta: &SettingMeta) -> anyhow::Error {
        map_setting_error(meta.name)
    }

    /// `~/.npmrc` home, honoring `HOME` (Unix) / `USERPROFILE` (Windows).
    fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
    }

    /// Mirror of the engine's `is_npm_shared_key` (crate-private at the
    /// pinned API): per-host auth/cert templates, scoped registries, and
    /// settings flagged `npmShared` in the settings registry.
    pub(super) fn is_npm_shared_key(key: &str) -> bool {
        if key.starts_with("//") {
            return true;
        }
        if let Some(rest) = key.strip_prefix('@')
            && rest.ends_with(":registry")
        {
            return true;
        }
        setting_for_key(key).is_some_and(|meta| meta.npm_shared)
    }

    /// Mirror of the engine's `setting_for_key`: canonical name first,
    /// then any alias surface (npmrc/yaml/env/cli spellings).
    fn setting_for_key(key: &str) -> Option<&'static SettingMeta> {
        meta::find(key).or_else(|| {
            meta::all().find(|meta| {
                meta.npmrc_keys.contains(&key)
                    || meta.workspace_yaml_keys.contains(&key)
                    || meta.env_vars.contains(&key)
                    || meta.cli_flags.contains(&key)
            })
        })
    }

    /// The alias spellings to sweep and the spelling to write: the user's
    /// own spelling when it is a real `.npmrc` alias, else the setting's
    /// canonical `.npmrc` key, else the key verbatim (unknown keys, and
    /// the two known settings with no `.npmrc` alias upstream).
    fn write_plan(key: &str) -> (Vec<String>, String) {
        let Some(meta) = setting_for_key(key) else {
            return (vec![key.to_string()], key.to_string());
        };
        // Literal aliases only — `//host/:_authToken` / `@scope:registry`
        // template entries never reach here (they are npm-shared).
        let literals: Vec<&str> = meta
            .npmrc_keys
            .iter()
            .copied()
            .filter(|k| !k.starts_with("//") && !k.contains(':'))
            .collect();
        let mut sweep: Vec<String> = literals.iter().map(|s| s.to_string()).collect();
        for extra in [meta.name, key] {
            if !sweep.iter().any(|s| s == extra) {
                sweep.push(extra.to_string());
            }
        }
        let write_key = if literals.contains(&key) {
            key.to_string()
        } else {
            literals
                .first()
                .map(|s| s.to_string())
                .unwrap_or_else(|| key.to_string())
        };
        (sweep, write_key)
    }

    /// Minimal format-preserving `.npmrc` edit: the first line defining
    /// any swept spelling is replaced in place, later duplicates drop,
    /// everything else (comments, unrelated keys, ordering) is untouched;
    /// a missing key appends. (The engine's `NpmrcEdit` is crate-private
    /// at the pinned API.)
    fn npmrc_set(path: &Path, sweep: &[String], write_key: &str, value: &str) -> Result<()> {
        let original = std::fs::read_to_string(path).unwrap_or_default();
        let mut out: Vec<String> = Vec::new();
        let mut written = false;
        for line in original.lines() {
            let defines_key = line
                .split_once('=')
                .map(|(k, _)| k.trim())
                .is_some_and(|k| sweep.iter().any(|s| s == k));
            if defines_key {
                if !written {
                    out.push(format!("{write_key}={value}"));
                    written = true;
                }
            } else {
                out.push(line.to_string());
            }
        }
        if !written {
            out.push(format!("{write_key}={value}"));
        }
        let mut text = out.join("\n");
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
    }

    /// Project root for the `.npmrc` write: nearest ancestor with a
    /// `package.json` or `pnpm-workspace.yaml`, falling back to the cwd
    /// (approximates the engine's `project_root_or_cwd`, which is
    /// crate-private at the pinned API).
    fn project_root() -> PathBuf {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut dir = cwd.clone();
        for _ in 0..16 {
            if dir.join("package.json").is_file() || dir.join("pnpm-workspace.yaml").is_file() {
                return dir;
            }
            if !dir.pop() {
                break;
            }
        }
        cwd
    }

    /// The refusal for a key naming a setting nub's embedder profile declares
    /// it does not consume, `None` for every other key. Both write scopes ask
    /// this — the project route through [`classify_set`], the global one
    /// directly, since it has no router.
    ///
    /// `key` is echoed as the user spelled it; the advice is looked up by the
    /// CANONICAL name, which is where the profile hangs it.
    pub(super) fn unsupported_setting_refusal(key: &str) -> Option<anyhow::Error> {
        let meta = meta::unsupported_for_key(key)?;
        let advice = meta::unsupported_advice(meta.name).unwrap_or_default();
        Some(anyhow!(
            "nub config set {key}: `{key}` is not a nub setting\n\x20\x20{advice}"
        ))
    }

    /// The refusal for a key naming a real setting that has NO `.npmrc` alias,
    /// `None` for every other key — including an unknown one, which is free-form
    /// and legitimately lands in `.npmrc` verbatim.
    ///
    /// The choke point every `.npmrc` write asks, so the yaml route's own
    /// fallback and the global writer are covered as well as
    /// [`classify_set`]'s direct route.
    pub(super) fn no_npmrc_home_refusal(key: &str) -> Option<anyhow::Error> {
        setting_for_key(key)
            .filter(|meta| meta.npmrc_keys.is_empty())
            .map(no_npmrc_home_error)
    }

    /// Name the surfaces that DO read the setting. Both settings in this class
    /// today (`pnpmfilePath`, `globalPnpmfile`) carry a CLI flag, and only
    /// `pnpmfilePath` also has a workspace-yaml key.
    ///
    /// An `AUBE_*` variable is never offered: `env_prefix: None` means nub does
    /// not read the engine's env family, so naming one would replace a dead
    /// write with a dead export. That leaves both of these with real advice; a
    /// future setting sourced ONLY from `AUBE_*` would fall to the last line,
    /// which is the honest answer rather than a wrong pointer.
    fn no_npmrc_home_error(meta: &SettingMeta) -> anyhow::Error {
        let mut homes: Vec<String> = Vec::new();
        if let Some(flag) = meta.cli_flags.first() {
            let flag = flag.trim_start_matches('-');
            homes.push(format!("pass `--{flag} <value>` on the command line"));
        }
        if !meta.workspace_yaml_keys.is_empty() {
            homes.push(format!(
                "set `{}:` in pnpm-workspace.yaml under a pnpm project",
                meta.workspace_yaml_keys[0]
            ));
        }
        if let Some(var) = meta.env_vars.iter().find(|v| !v.starts_with("AUBE_")) {
            homes.push(format!("export `{var}`"));
        }
        let advice = if homes.is_empty() {
            "it has no config-file home at all".to_string()
        } else {
            homes.join(", or ")
        };
        anyhow!(
            "nub config set {}: `{}` is not readable from .npmrc, so writing it there would do nothing\n\
             \x20\x20{advice}",
            meta.name,
            meta.name
        )
    }

    fn map_setting_error(name: &str) -> anyhow::Error {
        anyhow!(
            "nub config set {name}: `{name}` is a workspace map setting and can't be set as a single value\n\
             \x20\x20edit `{name}:` in pnpm-workspace.yaml instead (one entry per line)"
        )
    }

    fn map_entry_error(map: &str, key: &str) -> anyhow::Error {
        let extra = if map == "allowBuilds" {
            "\n\x20\x20(for dependency build scripts, `nub approve-builds` manages this list)"
        } else {
            ""
        };
        anyhow!(
            "nub config set {key}: workspace map settings live in pnpm-workspace.yaml\n\
             \x20\x20add the entry under `{map}:` there — `.npmrc` lines for map entries are not read{extra}"
        )
    }

    fn scalar_nested_error(meta: &SettingMeta, key: &str) -> anyhow::Error {
        anyhow!(
            "nub config set {key}: `{}` is a scalar setting (type `{}`) and has no nested namespace\n\
             \x20\x20set it directly: nub config set {} <value>",
            meta.name,
            meta.type_,
            meta.name
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn shared_predicate_mirrors_the_settings_registry() {
            // Self-consistency against the generated metadata: every
            // canonical name classifies exactly per its npmShared flag, so
            // upstream registry changes surface here instead of silently
            // re-routing writes.
            for meta in meta::all() {
                assert_eq!(
                    is_npm_shared_key(meta.name),
                    meta.npm_shared,
                    "{} must classify per its npmShared flag",
                    meta.name
                );
            }
            // The two pattern surfaces the registry can't carry per-key.
            assert!(is_npm_shared_key("//registry.npmjs.org/:_authToken"));
            assert!(is_npm_shared_key("@myorg:registry"));
            // Unknown keys are not shared → project `.npmrc`.
            assert!(!is_npm_shared_key("totally-unknown-key"));
        }

        #[test]
        fn npm_shared_and_map_routing_is_independent_of_incumbency() {
            // registry is npm-shared → engine (.npmrc), regardless of incumbent.
            for pnpm in [true, false] {
                assert!(matches!(classify_set("registry", pnpm), SetRoute::Engine));
            }

            // Map settings: bare and dotted forms both refuse (upstream would
            // write package.json#aube.<map> — brand boundary), and a nested
            // spelling of a scalar setting refuses with the direct-set hint.
            // Refusal is independent of incumbency.
            for pnpm in [true, false] {
                for refused in ["allowBuilds", "allowBuilds.esbuild", "autoInstallPeers.x"] {
                    assert!(
                        matches!(classify_set(refused, pnpm), SetRoute::Refuse(_)),
                        "{refused} must be refused (pnpm_incumbent={pnpm})"
                    );
                }
            }
        }

        #[test]
        fn non_shared_scalar_routes_by_scalar_home() {
            // `scalar_to_yaml = true` is the pnpm-v11+ case: a non-shared scalar
            // lands in pnpm-workspace.yaml (v11 reads scalars solely from yaml)…
            assert!(matches!(
                classify_set("autoInstallPeers", true),
                SetRoute::ProjectWorkspaceYaml
            ));
            assert!(matches!(
                classify_set("auto-install-peers", true),
                SetRoute::ProjectWorkspaceYaml
            ));
            // …`false` is pnpm v10/v9, the unknown-version default, and every
            // non-pnpm / nub-identity surface: the neutral project `.npmrc` (no
            // pnpm-branded file emitted — brand boundary; v9/v10 read `.npmrc`).
            assert!(matches!(
                classify_set("autoInstallPeers", false),
                SetRoute::ProjectNpmrc
            ));
            assert!(matches!(
                classify_set("auto-install-peers", false),
                SetRoute::ProjectNpmrc
            ));

            // A known scalar `some-custom-key` is unknown to the registry, so
            // it's free-form → `.npmrc` even in the yaml-home (v11) case (no
            // workspace-yaml schema for an arbitrary key).
            assert!(matches!(
                classify_set("some-custom-key", true),
                SetRoute::ProjectNpmrc
            ));
            assert!(matches!(
                classify_set("some-custom-key", false),
                SetRoute::ProjectNpmrc
            ));
        }

        /// A layout scalar ignores the scalar home entirely. Routing it by the
        /// pnpm-v11 rule would write `pnpm-workspace.yaml`, which nothing reads
        /// back for layout — `config set` would report success and the very next
        /// install would point back at `nub.jsonc` / `.npmrc` about the key just
        /// written. The `autoInstallPeers` pair is the control: a
        /// non-layout scalar must still follow the scalar home.
        #[test]
        fn a_layout_scalar_never_routes_to_workspace_yaml() {
            for key in [
                "nodeLinker",
                "node-linker",
                "shamefully-hoist",
                "hoist-pattern",
                "modules-dir",
                "virtual-store-dir",
            ] {
                for scalar_to_yaml in [true, false] {
                    assert!(
                        matches!(classify_set(key, scalar_to_yaml), SetRoute::ProjectNpmrc),
                        "{key} is layout and must land in .npmrc (scalar_to_yaml={scalar_to_yaml})"
                    );
                }
            }
            assert!(
                matches!(
                    classify_set("autoInstallPeers", true),
                    SetRoute::ProjectWorkspaceYaml
                ),
                "control: a non-layout scalar still follows the pnpm-v11 scalar home"
            );
        }

        /// A real setting with NO `.npmrc` alias is refused on the `.npmrc`
        /// route and still allowed on the yaml one.
        ///
        /// Both halves matter and they pull opposite ways. `write_plan` falls
        /// back to the key verbatim, so without the refusal the line lands and
        /// nothing reads it; but `pnpmfilePath` DOES have a workspace-yaml key,
        /// which a pnpm-v11 incumbent reads back — so a blanket refusal would
        /// take away the one home that works. The invariant is per-ROUTE, not
        /// per-setting. `autoInstallPeers` is the control: an ordinary scalar
        /// with an `.npmrc` alias is untouched on both routes.
        #[test]
        fn a_setting_with_no_npmrc_alias_is_refused_on_the_npmrc_route() {
            for key in ["pnpmfilePath", "globalPnpmfile"] {
                assert!(
                    matches!(classify_set(key, false), SetRoute::Refuse(_)),
                    "{key} has no .npmrc alias and must not be written there"
                );
            }
            assert!(
                matches!(
                    classify_set("pnpmfilePath", true),
                    SetRoute::ProjectWorkspaceYaml
                ),
                "pnpmfilePath has a workspace-yaml key a pnpm-v11 incumbent reads back"
            );
            for scalar_to_yaml in [true, false] {
                assert!(
                    no_npmrc_home_refusal("autoInstallPeers").is_none(),
                    "control: a setting with an .npmrc alias is never refused for lacking one"
                );
                assert!(
                    !matches!(
                        classify_set("autoInstallPeers", scalar_to_yaml),
                        SetRoute::Refuse(_)
                    ),
                    "control: the ordinary scalar route is untouched (scalar_to_yaml={scalar_to_yaml})"
                );
            }
            // An UNKNOWN key names no setting, so it is free-form config and
            // still legal in `.npmrc`. Guarding by "has no alias" rather than by
            // "is a known setting" would have refused every custom key.
            assert!(no_npmrc_home_refusal("some-custom-key").is_none());
        }

        #[test]
        fn pnpm_scalar_home_table_gates_v11_yaml_from_v10_npmrc() {
            use super::super::config_model::{ScalarHome, pnpm_scalar_home};
            // pnpm v11+ → yaml; v10, v9, earlier, and unknown → .npmrc.
            assert_eq!(pnpm_scalar_home(Some(11)), ScalarHome::PnpmWorkspaceYaml);
            assert_eq!(pnpm_scalar_home(Some(12)), ScalarHome::PnpmWorkspaceYaml);
            assert_eq!(pnpm_scalar_home(Some(10)), ScalarHome::Npmrc);
            assert_eq!(pnpm_scalar_home(Some(9)), ScalarHome::Npmrc);
            assert_eq!(
                pnpm_scalar_home(None),
                ScalarHome::Npmrc,
                "unknown pnpm version must default to the dominant .npmrc model"
            );
        }

        #[test]
        fn npmrc_set_replaces_aliases_in_place_and_preserves_the_rest() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(".npmrc");
            std::fs::write(
                &path,
                "# keep me\nregistry=https://example.com\nauto-install-peers=true\nautoInstallPeers=true\n",
            )
            .unwrap();

            let (sweep, write_key) = write_plan("auto-install-peers");
            npmrc_set(&path, &sweep, &write_key, "false").unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                text, "# keep me\nregistry=https://example.com\nauto-install-peers=false\n",
                "first alias line replaced in place, duplicate alias swept, rest preserved"
            );

            // Missing key appends.
            npmrc_set(&path, &["store-dir".to_string()], "store-dir", "/tmp/s").unwrap();
            assert!(
                std::fs::read_to_string(&path)
                    .unwrap()
                    .ends_with("store-dir=/tmp/s\n")
            );
        }

        #[test]
        fn write_plan_prefers_the_users_alias_spelling() {
            // The user's spelling wins when it's a real .npmrc alias…
            let (_, key) = write_plan("autoInstallPeers");
            assert_eq!(key, "autoInstallPeers");
            let (_, key) = write_plan("auto-install-peers");
            assert_eq!(key, "auto-install-peers");
            // …and unknown keys write verbatim.
            let (sweep, key) = write_plan("my-team-flag");
            assert_eq!(
                (sweep, key),
                (vec!["my-team-flag".to_string()], "my-team-flag".to_string())
            );
        }
    }
}
