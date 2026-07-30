//! nub's global settings file — `~/.config/nub/nub.jsonc` (`$XDG_CONFIG_HOME/nub`,
//! `%APPDATA%\nub` on Windows).
//!
//! This is nub's OWN durable settings home, distinct from the registry/PM tuning
//! that rides `.npmrc` and the ephemeral `NUB_*` env knobs: a setting lands here
//! only when no neutral standard field expresses it AND it must survive a `nub
//! cache clear` (the config-home ladder). Today the sole key is the dlx consent
//! kill-switch `exec.implicitDlx`. It lives under `exec` because dlx literally
//! means *download and exec* — a fetch-then-exec variant of local-binary exec,
//! the same behavior class, not a separate domain — so `exec` holds config for
//! both exec and dlx. (Config sections split by behavior class, not the nubx tier
//! chain: `run` = scripts, `exec` = tool/binary execution; this matches pnpm,
//! where exec/dlx are tools and run is scripts.)
//!
//! The file is JSONC (JSON + comments + trailing commas). Reads go through
//! [`crate::jsonc`] (best-effort — a malformed, over-nested, or absent file
//! yields the default, never a hard failure, because the read sits on nubx's hot
//! consent path). Writes go through the `jsonc_parser::cst` module — a
//! comment/whitespace/key-order-preserving CST edit — so a `set` that touches one
//! key leaves the rest of a hand-authored file intact. Writes are atomic (temp +
//! rename via `aube_util`). Only `nub.jsonc` is accepted; `nub.json` is never read.
//!
//! [`set_json_path`] and [`unset_json_path`] are that CST edit, generalized to an
//! arbitrary path: they serve this file's own `exec.implicitDlx` key AND every
//! schema field [`crate::config_fields`] writes, in the PROJECT file as well as
//! this one, so there is exactly one writer both files go through.
//!
//! The `nub config get/set …` surface is NOT a separate clap verb (the `config`
//! verb already exists as the engine's `.npmrc` config): a key naming a nub
//! setting is intercepted in `pm_engine::store_config_family` and routed here or
//! to [`crate::config_fields`], while every other key stays on the `.npmrc` path.

use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};

/// The `exec` object name and the key within it. One `const` pair so the reader,
/// the writer, and the config-verb interception can't drift.
const TABLE: &str = "exec";
const KEY: &str = "implicitDlx";

/// The dlx consent tier. Values are `prompt` (default) and `never`; `never`
/// mirrors the interactive select's `Never` label. Reserves `allow`
/// (auto-consent) as a future value — NOT valid today.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImplicitDlx {
    /// Ask (the interactive select) on the first implicit registry fetch.
    Prompt,
    /// The implicit tier is disabled globally — fail closed, no prompt/network.
    Never,
    // Allow — reserved: auto-consent without a prompt. NOT implemented yet.
}

impl ImplicitDlx {
    pub fn as_str(self) -> &'static str {
        match self {
            ImplicitDlx::Prompt => "prompt",
            ImplicitDlx::Never => "never",
        }
    }

    pub fn parse(s: &str) -> Option<ImplicitDlx> {
        match s {
            "prompt" => Some(ImplicitDlx::Prompt),
            "never" => Some(ImplicitDlx::Never),
            _ => None,
        }
    }
}

/// Path to `~/.config/nub/nub.jsonc`. `None` only when no home/config root
/// resolves at all (a broken environment) — every caller treats that as "use the
/// default and don't persist."
pub fn config_path() -> Option<PathBuf> {
    Some(nub_core::node::discovery::config_dir()?.join("nub.jsonc"))
}

/// Load the shared typed schema from the global config home. The global layer is
/// deliberately best-effort: absent, unreadable, malformed, or invalid input is
/// treated as no typed layer. The legacy `exec.implicitDlx` hot path below stays
/// independent until its public config surface is migrated.
///
/// A degraded read is announced rather than swallowed. Dropping the layer drops
/// every key in it, so one typo elsewhere in the file silently WIDENS
/// `dlx.consent` from `never` back to the `prompt` default — a security posture
/// loosening from an unrelated mistake. Best-effort means the command still
/// runs; it does not mean the user should have to guess why their policy stopped
/// applying. Absence is not a degrade and stays silent.
pub(crate) fn load_global_config() -> Option<crate::project_config::LoadedConfig> {
    let path = config_path()?;
    match crate::project_config::read_global_config_at(&path) {
        Ok(loaded) => Some(loaded),
        Err(_) if !path.exists() => None,
        Err(error) => {
            eprintln!("nub: {error}\n\x20\x20ignored — global settings are not applied");
            None
        }
    }
}

/// Read `exec.implicitDlx`. Absent file / absent key / unparseable value / any
/// unknown sibling key all mean the default (`Prompt`) — config is best-effort and
/// never fails the gate.
pub fn implicit_dlx() -> ImplicitDlx {
    let Some(path) = config_path() else {
        return ImplicitDlx::Prompt;
    };
    let Ok(text) = crate::jsonc::read_guarded(&path) else {
        return ImplicitDlx::Prompt;
    };
    let Ok(Some(value)) = crate::jsonc::parse_to_value(&text) else {
        return ImplicitDlx::Prompt;
    };
    value
        .get(TABLE)
        .and_then(|exec| exec.get(KEY))
        .and_then(|v| v.as_str())
        .and_then(ImplicitDlx::parse)
        .unwrap_or(ImplicitDlx::Prompt)
}

/// Get the root object of `text`, creating a fresh `{}` when the file is
/// absent/empty/unparseable or its root value is not an object. The returned
/// `CstObject` borrows into `root`, so the caller MUST keep `root` alive for the
/// whole edit (the CST panics if the root is dropped while a descendant is used).
fn root_object(text: &str) -> (CstRootNode, CstObject) {
    let parse = |t: &str| {
        CstRootNode::parse(t, &ParseOptions::default())
            .ok()
            .and_then(|root| root.ensure_object_value().map(|obj| (root, obj)))
    };
    // Best-effort: a malformed or non-object existing file is replaced by a fresh
    // object rather than surfacing a parse error on a `set`.
    parse(text).unwrap_or_else(|| parse("{}").expect("`{}` parses to an object"))
}

/// Get-or-create the object property `name` of `obj`.
///
/// A pre-existing `name` that is NOT an object (a hand-authored scalar/array) is
/// best-effort REPLACED with a fresh object — dropping the malformed value
/// rather than panicking or leaving a duplicate key. `get_object` matches the
/// FIRST prop by name, so a stray non-object one must be removed before the
/// append or the re-fetch would still find it and be `None`.
fn ensure_object(obj: &CstObject, name: &str) -> CstObject {
    obj.get_object(name).unwrap_or_else(|| {
        if let Some(stray) = obj.get(name) {
            stray.remove();
        }
        obj.append(name, CstInputValue::Object(Vec::new()));
        obj.get_object(name)
            .expect("just-appended object is present")
    })
}

/// Write `value` at the object path `segments` (the last element is the leaf
/// key), preserving every comment, trailing comma, and key order elsewhere in
/// the file. Missing intermediate objects are created, as is the file and its
/// parent directory. Returns an error only on an I/O failure the caller should
/// surface — an in-memory edit never fails.
///
/// This is the ONE write path for both `nub.jsonc` files. `nub config set` runs
/// through it so a hand-authored, heavily-commented file survives an edit that
/// touches one key.
pub(crate) fn set_json_path(
    path: &Path,
    segments: &[&str],
    value: CstInputValue,
) -> std::io::Result<()> {
    let text = crate::jsonc::read_guarded(path).unwrap_or_default();
    let (root, obj) = root_object(&text);

    let (leaf, parents) = segments.split_last().expect("a setting path has a leaf");
    let mut cursor = obj;
    for name in parents {
        cursor = ensure_object(&cursor, name);
    }
    match cursor.get(leaf) {
        Some(prop) => prop.set_value(value),
        None => cursor.append(leaf, value),
    }

    aube_util::fs_atomic::atomic_write(path, root.to_string().as_bytes())
}

/// Remove the key at `segments`, preserving the rest of the file. An absent
/// file, path, or key is a no-op success — nothing to clear is not an error —
/// and leaves the file untouched rather than rewriting it. Reports whether a key
/// was actually removed so a caller can avoid claiming a change it did not make.
pub(crate) fn unset_json_path(path: &Path, segments: &[&str]) -> std::io::Result<bool> {
    let Ok(text) = crate::jsonc::read_guarded(path) else {
        return Ok(false);
    };
    let Ok(root) = CstRootNode::parse(&text, &ParseOptions::default()) else {
        return Ok(false);
    };
    let Some(obj) = root.root_value().and_then(|v| v.as_object()) else {
        return Ok(false);
    };

    let (leaf, parents) = segments.split_last().expect("a setting path has a leaf");
    let mut cursor = obj;
    for name in parents {
        let Some(next) = cursor.get_object(name) else {
            return Ok(false);
        };
        cursor = next;
    }
    let Some(prop) = cursor.get(leaf) else {
        return Ok(false);
    };
    prop.remove();

    aube_util::fs_atomic::atomic_write(path, root.to_string().as_bytes())?;
    Ok(true)
}

/// Write `exec.implicitDlx = <value>`. Creates the file + `nub/` dir if absent.
pub fn set_implicit_dlx(value: ImplicitDlx) -> std::io::Result<()> {
    let path = config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve nub's config directory",
        )
    })?;
    set_json_path(
        &path,
        &[TABLE, KEY],
        CstInputValue::String(value.as_str().to_string()),
    )
}

/// Remove `exec.implicitDlx`, restoring the `prompt` default. A `config
/// unset`/`delete` on this key routes here rather than the engine's `.npmrc`
/// delete.
pub fn unset_implicit_dlx() -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    unset_json_path(&path, &[TABLE, KEY]).map(|_| ())
}

/// ONE process-wide lock every test that mutates a shared env var (`XDG_*`, `CI`)
/// must hold. Both this module's `with_config_home` and `nubx_consent`'s
/// `with_isolated_env` set process-global env; if each guarded with its OWN
/// mutex they wouldn't serialize against each other and would race under cargo's
/// multi-thread runner (leaked isolation, poisoned locks). This single lock is the
/// serialization point across BOTH modules.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &ENV_LOCK
}

/// Run `f` with the config path pointed at a fresh temp dir. `XDG_CONFIG_HOME`
/// wins in `config_dir()`, so this both isolates the file and keeps the
/// developer's real `~/.config/nub/nub.jsonc` out of what the test asserts.
/// Holds the process-wide [`test_env_lock`] because it mutates a global env var.
///
/// Restoration rides a drop guard, not a tail statement: a panicking assertion
/// inside `f` unwinds, and a tail restore would leave every later test in the
/// binary pointed at this deleted temp dir.
#[cfg(test)]
pub(crate) fn with_config_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    struct Restore {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: `_lock` is still held here, so no other test thread reads
            // or writes the var while it is restored.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    let lock = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _restore = Restore {
        prev: std::env::var_os("XDG_CONFIG_HOME"),
        _lock: lock,
    };
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: guarded by test_env_lock; `Restore` puts the previous value back.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
    f(dir.path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn defaults_to_prompt_when_absent() {
        with_config_home(|_| {
            assert_eq!(implicit_dlx(), ImplicitDlx::Prompt);
        });
    }

    #[test]
    fn set_never_then_read_never_roundtrips() {
        with_config_home(|home| {
            set_implicit_dlx(ImplicitDlx::Never).unwrap();
            assert_eq!(implicit_dlx(), ImplicitDlx::Never);

            // The written file is the nested `exec` object form we document.
            let body = std::fs::read_to_string(home.join("nub").join("nub.jsonc")).unwrap();
            assert!(body.contains("\"exec\""), "wrote an exec object: {body}");
            assert!(
                body.contains("\"implicitDlx\": \"never\""),
                "wrote the key: {body}"
            );

            // Re-enabling flips it back.
            set_implicit_dlx(ImplicitDlx::Prompt).unwrap();
            assert_eq!(implicit_dlx(), ImplicitDlx::Prompt);
        });
    }

    #[test]
    fn unset_clears_the_key_back_to_default() {
        with_config_home(|_| {
            set_implicit_dlx(ImplicitDlx::Never).unwrap();
            assert_eq!(implicit_dlx(), ImplicitDlx::Never);
            unset_implicit_dlx().unwrap();
            assert_eq!(implicit_dlx(), ImplicitDlx::Prompt, "cleared to default");
            // Unset on an already-clear key is a no-op success.
            unset_implicit_dlx().unwrap();
        });
    }

    #[test]
    fn set_preserves_comments_trailing_commas_and_unrelated_keys() {
        with_config_home(|home| {
            // A pre-existing JSONC file with a line comment, a block comment, a
            // trailing comma, an unrelated top-level key, and an unrelated key
            // inside `exec`. The comment-aware CST write must keep all of it — the
            // real regression guard for the jsonc-parser migration.
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut f = std::fs::File::create(&path).unwrap();
            write!(
                f,
                "{{\n  // nub settings — hand-authored\n  \"telemetry\": false,\n  /* an unrelated block */\n  \"exec\": {{\n    \"shell\": \"bash\",\n  }},\n}}\n"
            )
            .unwrap();
            drop(f);

            set_implicit_dlx(ImplicitDlx::Never).unwrap();

            let body = std::fs::read_to_string(&path).unwrap();
            assert!(
                body.contains("// nub settings — hand-authored"),
                "line comment preserved: {body}"
            );
            assert!(
                body.contains("\"telemetry\": false"),
                "unrelated top key preserved: {body}"
            );
            assert!(
                body.contains("/* an unrelated block */"),
                "block comment preserved: {body}"
            );
            assert!(
                body.contains("\"shell\": \"bash\""),
                "unrelated exec key preserved: {body}"
            );
            assert!(
                body.contains("\"implicitDlx\": \"never\""),
                "new key written: {body}"
            );
            // The value round-trips back through the reader.
            assert_eq!(implicit_dlx(), ImplicitDlx::Never);
        });
    }

    #[test]
    fn unknown_keys_and_malformed_files_degrade_to_default() {
        with_config_home(|home| {
            // A `$schema` pointer and a typo'd sibling key must NOT fail the read —
            // best-effort parsing returns the real value.
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                "{\n  \"$schema\": \"https://nubjs.com/schema.json\",\n  \"unknownKey\": 42,\n  \"exec\": { \"implicitDlx\": \"never\" }\n}\n",
            )
            .unwrap();
            assert_eq!(implicit_dlx(), ImplicitDlx::Never);

            // A malformed file degrades to the default rather than erroring.
            std::fs::write(&path, "{ this is not valid json").unwrap();
            assert_eq!(implicit_dlx(), ImplicitDlx::Prompt);
        });
    }

    #[test]
    fn malformed_shared_schema_is_best_effort_globally() {
        with_config_home(|home| {
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, r#"{ "nodeCompat": "not-a-bool" }"#).unwrap();
            assert_eq!(load_global_config(), None);

            std::fs::write(&path, r#"{ "nodeComapt": true }"#).unwrap();
            assert_eq!(
                load_global_config().unwrap().values,
                crate::project_config::ProjectConfig::default()
            );

            let project = crate::project_config::read_project_config_at(&path).unwrap_err();
            assert!(matches!(
                project.kind(),
                crate::project_config::ConfigError::UnknownKey { .. }
            ));
        });
    }

    #[test]
    fn typed_global_layer_accepts_legacy_consent_without_schema_drift() {
        with_config_home(|home| {
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r#"{
                  "exec": { "implicitDlx": "never" },
                  "unknownLegacySibling": 42,
                  "nodeCompat": false,
                  "preload": []
                }"#,
            )
            .unwrap();

            let loaded = load_global_config().expect("best-effort typed global layer");
            assert_eq!(loaded.values.dlx.consent, Some(ImplicitDlx::Never));
            assert_eq!(loaded.values.node_compat, Some(false));
            assert_eq!(loaded.values.preload, Some(Vec::new()));
            assert_eq!(implicit_dlx(), ImplicitDlx::Never);
        });
    }

    #[test]
    fn set_replaces_a_non_object_exec_without_panicking() {
        with_config_home(|home| {
            // A hand-authored `exec` that is NOT an object (a scalar or array)
            // must not panic the write (`get_object` returns None, so the stray
            // prop is removed before the fresh object is appended) — best-effort
            // config never crashes on malformed input.
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            for junk in ["{ \"exec\": 5 }", "{ \"exec\": [1, 2] }"] {
                std::fs::write(&path, junk).unwrap();
                set_implicit_dlx(ImplicitDlx::Never).unwrap();
                assert_eq!(implicit_dlx(), ImplicitDlx::Never, "recovered from {junk}");
                let body = std::fs::read_to_string(&path).unwrap();
                assert!(
                    body.contains("\"implicitDlx\": \"never\""),
                    "wrote into a fresh exec object: {body}"
                );
            }
        });
    }
}
