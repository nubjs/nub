//! Store/config family — global-store and cache forensics plus settings
//! through the embedded aube engine: `store` (add/path/prune/status),
//! `cache` (list/view/delete/prune/list-registries), `cat-file`,
//! `cat-index`, `find-hash`, `config` (+`c`) with the hidden top-level
//! `get`/`set` shorthands, and the npm-fallback verbs `pkg`, `set-script`.
//!
//! The wiring helpers (`parse_verb`, `run_async`, `run_npm_fallback`) are
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
//! - `config` write routing is npmrc-first (decision 2026-06; **no
//!   `config.toml`, ever**): npm-shared keys (`registry`, proxies, per-host
//!   auth templates, `@scope:registry`, …) delegate to the engine, which
//!   writes `.npmrc` at the requested `--location` (default user) so
//!   npm/yarn see the same value; **every other key** is written by nub to
//!   the *project* `.npmrc` (`--location`/`--local` are ignored for these —
//!   the report line names the file written). The engine reads every
//!   setting from `.npmrc`, so the write is read-coherent with install.
//!   Upstream would route these keys to `~/.config/aube/config.toml`,
//!   which nub never writes (reading one that already exists is engine
//!   behavior, unchanged). Workspace *map* settings (`allowBuilds.<pkg>`,
//!   `overrides.<pkg>`, bare `allowBuilds`, …) are refused with a
//!   pnpm-workspace.yaml pointer: upstream's fallback would write a
//!   `package.json#aube.<map>` field — a foreign-brand field in the user's
//!   manifest — and `.npmrc` lines for map entries would be silently
//!   unread. Two known scalars (`pnpmfilePath`, `globalPnpmfile`) have no
//!   `.npmrc` alias upstream; nub still writes them verbatim rather than
//!   inventing a config file (documented trade-off: stored, possibly
//!   unread).
//! - `config delete`/`list`/`get` delegate to the engine unchanged. Note
//!   the scope asymmetry delete inherits: it defaults to `--location user`,
//!   while nub writes non-shared keys project-scope — `nub config delete
//!   --local <key>` removes those.
//! - `config explain` / `config find` / `config tui` stay unwired: they
//!   print engine reference docs straight to stdout, bypassing the brand
//!   rewrite.

use anyhow::Result;
use aube::commands::config::{ConfigArgs, ConfigCommand};

use super::publish_family::{Parsed, VerbArgs, parse_verb, run_async, run_npm_fallback};
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
        "pkg" | "set-script" => run_npm_fallback(spec.canonical, typed, args),
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
    dispatch_config(parsed)
}

fn dispatch_config(parsed: ConfigArgs) -> Result<i32> {
    match &parsed.command {
        // Writes follow the npmrc-first routing (module doc): only
        // npm-shared keys delegate to the engine's own `.npmrc` writer.
        Some(ConfigCommand::Set(set)) => {
            match npmrc_first::classify_set(&set.key) {
                npmrc_first::SetRoute::Engine => {} // fall through to delegate
                npmrc_first::SetRoute::ProjectNpmrc => {
                    return npmrc_first::set_project_npmrc(&set.key, &set.value);
                }
                npmrc_first::SetRoute::Refuse(err) => return Err(err),
            }
        }
        Some(ConfigCommand::Explain(_)) => return Err(unwired_config_sub("explain")),
        Some(ConfigCommand::Find(_)) => return Err(unwired_config_sub("find")),
        Some(ConfigCommand::Tui) => return Err(unwired_config_sub("tui")),
        // `get` / `list` / `delete` / bare `config` delegate unchanged.
        _ => {}
    }
    let session = super::engine_session(None)?;
    match session
        .runtime
        .block_on(aube::commands::config::run(parsed))
    {
        Ok(()) => Ok(0),
        Err(report) => Ok(present::emit_report(&report)),
    }
}

/// The engine prints reference docs for these straight to stdout (no
/// rewrite seam), so they stay unwired rather than risk a brand leak.
fn unwired_config_sub(sub: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "nub config {sub}: not supported yet\n\
         \x20\x20settings reference: https://pnpm.io/settings (nub reads the same `.npmrc` keys)"
    )
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
        /// honored, stale `config.toml` shadows swept by upstream).
        Engine,
        /// Everything writable that isn't npm-shared → project `.npmrc`.
        ProjectNpmrc,
        /// Workspace map settings and scalar-nested misuses.
        Refuse(anyhow::Error),
    }

    /// Classify a `config set` key. Pure (no fs) so the routing table is
    /// unit-testable; the write itself happens in [`set_project_npmrc`].
    pub(super) fn classify_set(key: &str) -> SetRoute {
        if is_npm_shared_key(key) {
            return SetRoute::Engine;
        }
        match setting_for_key(key) {
            // Bare map setting (`allowBuilds`, `overrides`, …): a single
            // scalar can't represent it, and upstream's per-entry fallback
            // writes `package.json#aube.<map>` — a foreign-brand manifest
            // field nub must never produce.
            Some(meta) if meta.type_ == "object" => SetRoute::Refuse(map_setting_error(meta.name)),
            // Known scalar (including canonical dotted names like
            // `peerDependencyRules.allowedVersions`) → project `.npmrc`.
            Some(_) => SetRoute::ProjectNpmrc,
            None => {
                if let Some((prefix, _)) = key.split_once('.')
                    && let Some(meta) = setting_for_key(prefix)
                {
                    if meta.type_ == "object" {
                        return SetRoute::Refuse(map_entry_error(meta.name, key));
                    }
                    return SetRoute::Refuse(scalar_nested_error(meta, key));
                }
                // Free-form unknown key → project `.npmrc` verbatim.
                SetRoute::ProjectNpmrc
            }
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
        fn set_routing_refuses_map_shapes_and_keeps_scalars_project_scoped() {
            // registry is npm-shared → engine; autoInstallPeers (either
            // spelling) is pnpm-surface → project .npmrc; unknown keys are
            // free-form project .npmrc.
            assert!(matches!(classify_set("registry"), SetRoute::Engine));
            assert!(matches!(
                classify_set("autoInstallPeers"),
                SetRoute::ProjectNpmrc
            ));
            assert!(matches!(
                classify_set("auto-install-peers"),
                SetRoute::ProjectNpmrc
            ));
            assert!(matches!(
                classify_set("some-custom-key"),
                SetRoute::ProjectNpmrc
            ));

            // Map settings: bare and dotted forms both refuse (upstream
            // would write package.json#aube.<map> — brand boundary), and
            // a nested spelling of a scalar setting refuses with the
            // direct-set hint.
            for refused in ["allowBuilds", "allowBuilds.esbuild", "autoInstallPeers.x"] {
                assert!(
                    matches!(classify_set(refused), SetRoute::Refuse(_)),
                    "{refused} must be refused"
                );
            }
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
