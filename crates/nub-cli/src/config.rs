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
//! `jsonc_parser::parse_to_serde_value` (best-effort — a malformed or absent file
//! yields the default, never a hard failure, because the read sits on nubx's hot
//! consent path). Writes go through the `jsonc_parser::cst` module — a
//! comment/whitespace/key-order-preserving CST edit — so a `set` that touches one
//! key leaves the rest of a hand-authored file intact. Writes are atomic (temp +
//! rename via `aube_util`). Only `nub.jsonc` is accepted; `nub.json` is never read.
//!
//! The `nub config get/set exec.implicitDlx …` surface is NOT a separate clap
//! verb (the `config` verb already exists as the engine's `.npmrc` config): the
//! nub-namespaced dotted key is intercepted in `pm_engine::store_config_family`
//! and routed here, while every other key stays on the `.npmrc` path.

use std::path::PathBuf;

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

/// Read `exec.implicitDlx`. Absent file / absent key / unparseable value / any
/// unknown sibling key all mean the default (`Prompt`) — config is best-effort and
/// never fails the gate.
pub fn implicit_dlx() -> ImplicitDlx {
    let Some(path) = config_path() else {
        return ImplicitDlx::Prompt;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return ImplicitDlx::Prompt;
    };
    let Ok(Some(value)) = jsonc_parser::parse_to_serde_value(&text, &ParseOptions::default())
    else {
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

/// Write `exec.implicitDlx = <value>`, preserving every other key/comment in the
/// file (comment-aware CST read-modify-write). Creates the file + `nub/` dir if
/// absent. Returns an error only on an I/O failure the caller should surface — an
/// in-memory edit never fails.
pub fn set_implicit_dlx(value: ImplicitDlx) -> std::io::Result<()> {
    let path = config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve nub's config directory",
        )
    })?;

    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let (root, obj) = root_object(&text);

    // Get-or-create the `exec` object, then set-or-append the key inside it.
    // A pre-existing `exec` that is NOT an object (a hand-authored scalar/array)
    // is best-effort REPLACED with a fresh object — dropping the malformed value
    // rather than panicking or leaving a duplicate `exec` key. `get_object`
    // matches the FIRST `exec` prop by name, so a stray non-object one must be
    // removed before the append or the re-fetch would still find it and be None.
    let exec = obj.get_object(TABLE).unwrap_or_else(|| {
        if let Some(stray) = obj.get(TABLE) {
            stray.remove();
        }
        obj.append(TABLE, CstInputValue::Object(Vec::new()));
        obj.get_object(TABLE)
            .expect("just-appended `exec` object is present")
    });
    match exec.get(KEY) {
        Some(prop) => prop.set_value(CstInputValue::String(value.as_str().to_string())),
        None => exec.append(KEY, CstInputValue::String(value.as_str().to_string())),
    }

    aube_util::fs_atomic::atomic_write(&path, root.to_string().as_bytes())
}

/// Remove `exec.implicitDlx` (restoring the `prompt` default), preserving the
/// rest of the file. A `config unset`/`delete` on this key routes here rather than
/// the engine's `.npmrc` delete. Absent file/key → a no-op success (nothing to
/// clear is not an error).
pub fn unset_implicit_dlx() -> std::io::Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let Ok(root) = CstRootNode::parse(&text, &ParseOptions::default()) else {
        return Ok(());
    };
    let Some(obj) = root.root_value().and_then(|v| v.as_object()) else {
        return Ok(());
    };
    if let Some(prop) = obj.get_object(TABLE).and_then(|exec| exec.get(KEY)) {
        prop.remove();
    }
    aube_util::fs_atomic::atomic_write(&path, root.to_string().as_bytes())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Point the config path at a temp dir for the duration of the closure.
    /// `XDG_CONFIG_HOME` wins in `config_dir()`, so this fully isolates the file.
    /// Holds the process-wide [`test_env_lock`] because it mutates a global env var.
    fn with_config_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = test_env_lock().lock().unwrap_or_else(|e| e.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: guarded by test_env_lock; restored before the guard drops.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let out = f(dir.path());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        out
    }

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
