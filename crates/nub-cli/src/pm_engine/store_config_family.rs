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
//! - KNOWN GAP (inherited — see `super::nub_setting_defaults`): `cache`
//!   operates on the engine's packument cache at `<XDG_CACHE_HOME>/aube/…`
//!   because `cacheDir` can't ride the embedder-defaults tier at the pinned
//!   API. Paths printed by `cache view --json` / `cache delete` are real
//!   on-disk paths, which the rewrite policy deliberately preserves.
//! - `config` write routing is pnpm-VERSION-AWARE (decision 2026-06-20,
//!   supersedes the earlier "npmrc-first" routing; **no `config.toml`, ever**).
//!   The config home for SCALAR settings is pnpm-version-dependent — there is
//!   no single file that round-trips on every pnpm — so the router gates on the
//!   incumbent pnpm version (see [`config_model`] + [`project_scalar_home`]).
//!   npm-shared keys (`registry`, proxies, per-host auth templates,
//!   `@scope:registry`, bare auth scalars, …) → `.npmrc` (engine writer), so
//!   npm/yarn/pnpm of every version see the same value (unchanged). Non-shared
//!   scalars under a pnpm-v11+ incumbent → `pnpm-workspace.yaml` (created if
//!   absent), because v11 reads scalars SOLELY from the workspace yaml
//!   (`isIniConfigKey` keeps only auth/network in `.npmrc`) so a `.npmrc` scalar
//!   would no-op. Non-shared scalars under a pnpm-v10/v9 incumbent, the
//!   UNKNOWN-pnpm-version default, and nub identity / npm / yarn / bun → the
//!   *project* `.npmrc` (the neutral home): v10/v9 read scalars from `.npmrc`,
//!   and the unknown default picks `.npmrc` as the safest target for the
//!   dominant v9/v10 base (a v11-shaped yaml written into a v10 project silently
//!   no-ops). Never a pnpm-branded file for these, never `config.toml`;
//!   `--location`/`--local` are ignored. READS are version-AGNOSTIC and need no
//!   gate: the resolver reads every scalar from BOTH `pnpm-workspace.yaml` AND
//!   `.npmrc`, so nub honors a v10 project's `.npmrc` scalars and a v11
//!   project's yaml scalars at once — only the WRITE target is version-dependent.
//!   Workspace *map* settings (`allowBuilds.<pkg>`, `overrides.<pkg>`, bare
//!   `allowBuilds`, …) are refused with a pnpm-workspace.yaml pointer at any
//!   incumbency/version (upstream's fallback would write a
//!   `package.json#aube.<map>` field, and `.npmrc` lines for map entries are
//!   unread). A free-form unknown key has no workspace-yaml schema, so it goes
//!   to `.npmrc` verbatim even under a pnpm-v11 incumbent.
//! - **GLOBAL config is ASYMMETRIC — read broad, write neutral** (decision
//!   2026-06-20):
//!     - **Reads:** nub honors WHATEVER global config the user already has,
//!       from any tool — npm's `~/.npmrc`, pnpm's global `config.yaml`, pnpm's
//!       global `auth.ini` — PM-AGNOSTICALLY and UNGATED by the cwd's
//!       incumbent PM. The original bug was that these global reads were GATED
//!       on the cwd-derived `read_branded_pnpm_config` (nub read pnpm's global
//!       config only when standing in a pnpm project); the fix DECOUPLES them
//!       from the cwd via the separate `read_pnpm_global_config = true` posture
//!       (set unconditionally in `engine_brand_preflight`), NOT to stop reading
//!       them.
//!     - **Writes** (`config set --location user|global`): NEVER a PM-branded
//!       global file. In global mode there is no project → no incumbent PM → nub
//!       can't know which PM's global file is meant, so writes go NEUTRAL:
//!       npm-shared/auth keys → `~/.npmrc` (every tool reads it); every other
//!       scalar → nub's neutral global home (also `~/.npmrc`). Never pnpm's
//!       `config.yaml`/`auth.ini`, never `config.toml`. nub's default and
//!       `--local`/`--location project` stay PROJECT scope (the incumbency
//!       split above); only an explicit `--location user|global` takes the
//!       neutral global-write path.
//! - `config delete`/`list`/`get` delegate to the engine unchanged, with
//!   one carve-out: `config get registry` at the default merged view
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
//!   delete inherits: it defaults to `--location user`, while nub writes
//!   non-shared keys project-scope — `nub config delete --local <key>`
//!   removes those.
//! - `config explain` / `config find` / `config tui` stay unwired: they
//!   print engine reference docs straight to stdout, bypassing the brand
//!   rewrite.

use anyhow::Result;
use aube::commands::config::{ConfigArgs, ConfigCommand};

use super::publish_family::{Parsed, VerbArgs, parse_verb, run_async};
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
    let (bin, argv): (String, Vec<String>) = match canonical {
        "config" => (format!("nub {typed}"), args.to_vec()),
        shorthand => (
            "nub".to_string(),
            std::iter::once(shorthand.to_string())
                .chain(args.iter().cloned())
                .collect(),
        ),
    };
    let parsed = match parse_verb::<VerbArgs<ConfigArgs>>(&bin, &argv) {
        Parsed::Ok(wrap) => wrap.args,
        Parsed::Exit(code) => return Ok(code),
    };
    dispatch_config(parsed, explicit_global_scope(args))
}

/// Whether the user EXPLICITLY asked for a global-scope write —
/// `--location user` or `--location global` in the raw args. nub's default
/// (no scope flag) and `--local` / `--location project` are PROJECT scope;
/// only an explicit global request flips to the neutral global-write path. We
/// read the raw args rather than the parsed `Location` because clap defaults
/// `location` to `User`, so the parsed struct can't distinguish an explicit
/// `--location user` from the no-flag default — and nub's contract is
/// "default = project" (the engine's User default is overridden here).
/// (`--location` is the only scope spelling the engine's `set` accepts; a bare
/// `--global`/`-g` isn't a valid flag and never reaches here.)
fn explicit_global_scope(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--location" {
            if matches!(it.next().map(String::as_str), Some("user") | Some("global")) {
                return true;
            }
        } else if let Some(v) = a.strip_prefix("--location=")
            && matches!(v, "user" | "global")
        {
            return true;
        }
    }
    false
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

fn dispatch_config(parsed: ConfigArgs, explicit_global: bool) -> Result<i32> {
    match &parsed.command {
        // Write routing (module doc). GLOBAL writes (`--location user|global`)
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
            if explicit_global {
                // Neutral global write. npm-shared/auth keys FIRST (a key like
                // `registry` is auth, not the `registries` map — the shared
                // check must win before the map refusal below).
                if npmrc_first::is_npm_shared_key(&set.key) {
                    // Auth/registry → engine's `~/.npmrc` writer at user scope.
                    // Fall through to delegate (location already `user`/`global`).
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
        Some(ConfigCommand::Get(get))
            if get.key == "registry"
                && !get.local
                && matches!(get.location, aube::commands::config::ListLocation::Merged) =>
        {
            let json = get.json;
            return run_config_get_registry(parsed, json);
        }
        Some(ConfigCommand::Explain(_)) => return Err(unwired_config_sub("explain")),
        Some(ConfigCommand::Find(_)) => return Err(unwired_config_sub("find")),
        Some(ConfigCommand::Tui) => return Err(unwired_config_sub("tui")),
        // `get` / `list` / `delete` / bare `config` delegate unchanged.
        _ => {}
    }
    let session = super::engine_session_quiet(None)?;
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
    let session = super::engine_session_quiet(None)?;
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
    use crate::pm_engine::present;

    /// Reviewer #9: `nub config set --help` must state nub's actual
    /// `--location` contract — non-shared keys go to the project `.npmrc`
    /// with the location flags ignored, and workspace map writes are
    /// refused — with zero engine-brand or config.toml vocabulary left.
    /// This pins the rewrite_help VOCAB entries against upstream doc drift
    /// after a pin bump.
    #[test]
    fn config_set_help_states_the_location_divergence() {
        use clap::CommandFactory as _;
        #[derive(clap::Parser)]
        struct SetCli {
            #[command(flatten)]
            args: aube::commands::config::ConfigArgs,
        }
        let help = present::rewrite_help(
            SetCli::command()
                .name("nub config".to_string())
                .bin_name("nub config".to_string())
                .render_long_help()
                .to_string(),
        );
        // The long help of the `set` subcommand renders through the same
        // path users hit (`nub config set --help`).
        let mut cmd = SetCli::command()
            .name("nub config".to_string())
            .bin_name("nub config".to_string());
        let set_help = present::rewrite_help(
            cmd.find_subcommand_mut("set")
                .expect("config has a set subcommand")
                .render_long_help()
                .to_string(),
        );
        for (name, text) in [("config", &help), ("config set", &set_help)] {
            assert!(
                !text.to_lowercase().contains("aube") && !text.contains("config.toml"),
                "nub {name} help must be brand-clean and config.toml-free: {text}"
            );
        }
        assert!(
            set_help.contains("regardless of `--location`/`--local`"),
            "set help must state the location-ignored contract: {set_help}"
        );
        assert!(
            set_help.contains("are refused at any location"),
            "set help must state the map-write refusal: {set_help}"
        );
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
        /// Non-shared scalar under a pnpm-**v11+** incumbent →
        /// `pnpm-workspace.yaml`. v11 reads scalar settings SOLELY from the
        /// workspace yaml (`isIniConfigKey` keeps only auth/network in
        /// `.npmrc`), so a scalar written to `.npmrc` would no-op; nub mirrors
        /// v11 and creates the yaml so a subsequent `pnpm config get` / install
        /// reads it back. Only fires under a provable pnpm-v11+ incumbent — a
        /// pnpm-named file is never written for v10/v9 (they read `.npmrc`), nor
        /// for non-pnpm / nub identity (brand boundary).
        ProjectWorkspaceYaml,
        /// Non-shared scalar everywhere else → the project `.npmrc` (the
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
    /// version whose config home for scalar settings is `pnpm-workspace.yaml`
    /// (see `pnpm_uses_yaml_scalar_home`). It decides ONLY where a non-shared
    /// scalar lands: `pnpm-workspace.yaml` under v11, the neutral project
    /// `.npmrc` for pnpm v10/v9, the unknown-version default, and every
    /// non-pnpm / nub-identity surface. npm-shared keys (`.npmrc`) and map
    /// refusals are independent of this signal.
    pub(super) fn classify_set(key: &str, scalar_to_yaml: bool) -> SetRoute {
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
        let path = project_root().join(".npmrc");
        let (sweep, write_key) = write_plan(key);
        npmrc_set(&path, &sweep, &write_key, value)?;
        present::info(&format!("set {write_key}={value} ({})", path.display()));
        Ok(0)
    }

    /// Write a NON-shared scalar to the user `~/.npmrc` — nub's NEUTRAL global
    /// config home. A global write (`config set --location user|global`) must
    /// never touch a PM-branded global file (pnpm's `config.yaml`/`auth.ini`):
    /// in global mode there is no project and no incumbent PM, so nub can't
    /// know which PM's file is meant. `~/.npmrc` is brand-neutral and the
    /// resolver reads each setting's `.npmrc` alias from it, so the write is
    /// read-coherent. (Auth/registry keys take the engine's own user-`.npmrc`
    /// writer instead — see the `set` dispatch.)
    pub(super) fn set_user_npmrc(key: &str, value: &str) -> Result<i32> {
        let Some(home) = home_dir() else {
            return Err(anyhow!(
                "nub config set --global: could not locate the home directory\n\
                 \x20\x20set HOME (or USERPROFILE on Windows) to point at your user config"
            ));
        };
        let path = home.join(".npmrc");
        let (sweep, write_key) = write_plan(key);
        npmrc_set(&path, &sweep, &write_key, value)?;
        present::info(&format!("set {write_key}={value} ({})", path.display()));
        Ok(0)
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
            meta::all().iter().find(|meta| {
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
