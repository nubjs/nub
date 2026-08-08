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

use std::io::Write as _;
use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode};

use crate::project_config::ConfigError;

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

/// The two files `nub config init` can create. Their accepted schemas overlap,
/// but `dlx` belongs only to the global file, so the templates must not be one
/// undifferentiated catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InitScope {
    Project,
    Global,
}

impl InitScope {
    fn template(self) -> &'static str {
        match self {
            Self::Project => PROJECT_INIT_TEMPLATE,
            Self::Global => GLOBAL_INIT_TEMPLATE,
        }
    }
}

/// A short, behavior-neutral project config. `$schema` is the only active key;
/// every setting is an example to opt into rather than today's default frozen
/// into a checkout. The published schema supplies exhaustive descriptions and
/// completion, leaving comments for section and value-shape guidance only.
pub(crate) const PROJECT_INIT_TEMPLATE: &str = r#"{
  "$schema": "https://nubjs.com/schema/latest.json",

  // runtime
  // "preload": ["./setup.ts"],
  // "nodeOptions": ["--enable-source-maps"],
  // "v8Flags": ["--stack-size=2000"],
  // "nodeCompat": true, // plain Node behavior, with Nub's version selection
  // "envFile": [".env", ".env.local"], // true | false | path | paths
  // "loader": { ".graphql": "text" },
  // "conditions": ["development"],
  // "tsconfig": "./tsconfig.runtime.json", // runtime transforms, not type checking
  // "jsx": "react-jsx", // react | react-jsx | react-jsxdev
  // "jsxImportSource": "preact",
  // "jsxFactory": "createElement",
  // "jsxFragmentFactory": "Fragment",
  // "decorators": "legacy",
  // "emitDecoratorMetadata": true,
  // "verifyDeps": "warn", // warn | error | true | false

  // installs — applied only when Nub is the project's package manager
  // "install": {
  //   "linker": "global-virtual-store", // global-virtual-store | isolated | hoisted
  //   "publicHoist": ["@types/*"],
  //   "minimumReleaseAge": "3d", // <integer><s|m|h|d|w>
  //   "minimumReleaseAgeExclude": ["@company/*"],
  // },
}
"#;

/// The global file accepts the project fields as personal defaults and adds the
/// machine-wide implicit-fetch policy. It stays behavior-neutral for the same
/// reason as [`PROJECT_INIT_TEMPLATE`].
pub(crate) const GLOBAL_INIT_TEMPLATE: &str = r#"{
  "$schema": "https://nubjs.com/schema/latest.json",

  // runtime
  // "preload": ["./setup.ts"],
  // "nodeOptions": ["--enable-source-maps"],
  // "v8Flags": ["--stack-size=2000"],
  // "nodeCompat": true, // plain Node behavior, with Nub's version selection
  // "envFile": [".env", ".env.local"], // true | false | path | paths
  // "loader": { ".graphql": "text" },
  // "conditions": ["development"],
  // "tsconfig": "./tsconfig.runtime.json", // runtime transforms, not type checking
  // "jsx": "react-jsx", // react | react-jsx | react-jsxdev
  // "jsxImportSource": "preact",
  // "jsxFactory": "createElement",
  // "jsxFragmentFactory": "Fragment",
  // "decorators": "legacy",
  // "emitDecoratorMetadata": true,
  // "verifyDeps": "warn", // warn | error | true | false

  // installs — personal defaults for projects where Nub is the package manager
  // "install": {
  //   "linker": "global-virtual-store", // global-virtual-store | isolated | hoisted
  //   "publicHoist": ["@types/*"],
  //   "minimumReleaseAge": "3d", // <integer><s|m|h|d|w>
  //   "minimumReleaseAgeExclude": ["@company/*"],
  // },

  // temporary package runs
  // "dlx": { "consent": "prompt" }, // prompt | never
}
"#;

/// Create one initial config without ever replacing an existing path.
///
/// `create_new` is the concurrency boundary: two commands racing for the same
/// target cannot both report success. A failed write removes only the inode this
/// call just created, so a short or empty config is not left behind.
pub(crate) fn init_file(path: &Path, scope: InitScope) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    if let Err(error) = file.write_all(scope.template().as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
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

/// Get the root object of the file at `path`, creating a fresh `{}` only when
/// there is genuinely nothing to preserve — an absent, empty, or comment-only
/// file.
///
/// Every other failure is an ERROR, never a blank slate. The caller writes this
/// document back OVER `path`, so treating a file nub cannot read or parse as
/// empty silently replaces a hand-authored, commented config with the single key
/// being set. Reads already refuse these same files, and a `set` that destroys
/// what a `get` declined to read is the worst outcome this surface can produce.
///
/// The returned `CstObject` borrows into `root`, so the caller MUST keep `root`
/// alive for the whole edit (the CST panics if the root is dropped while a
/// descendant is used).
fn root_object(path: &Path) -> std::io::Result<(CstRootNode, CstObject)> {
    let refuse = |error: ConfigError| refuse_edit(path, error);
    let text = match crate::jsonc::read_guarded(path) {
        Ok(text) => text,
        // Absence is the one blank slate — the file is about to be created.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(refuse(ConfigError::Io(e))),
    };

    // Bounded before the CST parser sees it, for the same reason the value
    // reader is: an over-nested document overflows the stack, which aborts the
    // process instead of failing. It also keeps the writer from accepting a file
    // the next read would refuse.
    crate::jsonc::check_nesting_depth(&text).map_err(|e| refuse(ConfigError::Parse(e)))?;
    let root = CstRootNode::parse(&text, &crate::jsonc::parse_options())
        .map_err(|e| refuse(ConfigError::Parse(e.to_string())))?;
    // `_or_create`, never `_or_set`: it creates the object only when there is NO
    // root value (the empty/comment-only document, where creating it is right and
    // the comments survive) and returns `None` for a root that exists and is not
    // an object, rather than overwriting it.
    let obj = root.object_value_or_create().ok_or_else(|| {
        refuse(ConfigError::Type {
            path: "<root>".into(),
            expected: "an object",
        })
    })?;
    if let Some(key) = duplicate_key(&obj, "") {
        return Err(refuse(ConfigError::Value {
            path: key,
            message: "appears twice — nub reads the last one, a write edits the first".into(),
        }));
    }
    Ok((root, obj))
}

/// Turn a configuration document error into the shared no-write refusal.
///
/// Set and delete both edit a CST and then atomically replace the source file,
/// so neither may paper over a document they cannot edit faithfully.
fn refuse_edit(path: &Path, error: ConfigError) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "{}\n\x20\x20nothing was written — fix the file, or delete it, and retry",
            error.in_file(path)
        ),
    )
}

/// The dotted path of the first key some object in `object` names twice.
///
/// A duplicate makes the reader and the writer disagree about which occurrence
/// is authoritative: the value reader is `serde_json`, whose map visitor keeps
/// the LAST, while every CST lookup here matches the FIRST. So a `set` edits an
/// occurrence the next read ignores and reports success for a change with no
/// effect, and a duplicated intermediate object leaves no unambiguous parent
/// for [`ensure_object`]. Refusing is the same ground the rest of
/// [`root_object`] stands on — a file whose meaning nub cannot pin down is not
/// one to rewrite.
///
/// The whole document is walked rather than just the path being edited: the
/// ambiguity belongs to the file, not to one edit, and a `set` that succeeds
/// while leaving a differently-resolving sibling in place is the same trap one
/// key over.
fn duplicate_key(object: &CstObject, prefix: &str) -> Option<String> {
    fn in_node(node: &CstNode, prefix: &str) -> Option<String> {
        if let Some(object) = node.as_object() {
            return duplicate_key(&object, prefix);
        }
        // Array elements inherit their parent's path. Nub's schema holds no
        // object inside an array, so descending here is forward-compat cover and
        // not worth naming an index for.
        node.as_array()
            .and_then(|array| array.elements().iter().find_map(|el| in_node(el, prefix)))
    }

    let mut seen = std::collections::HashSet::new();
    for prop in object.properties() {
        // A name the parser cannot decode is skipped for the reason `get` skips
        // it: no lookup can ever match it.
        let Some(name) = prop.name().and_then(|n| n.decoded_value().ok()) else {
            continue;
        };
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(found) = prop.value().as_ref().and_then(|v| in_node(v, &path)) {
            return Some(found);
        }
        if !seen.insert(name) {
            return Some(path);
        }
    }
    None
}

/// Get-or-create the object property `name` of `obj`.
///
/// `_or_create`, never `_or_set` — the same distinction [`root_object`] makes at
/// the root: an existing scalar or array is not an absent parent, and replacing
/// it would silently discard the author's value, so `None` becomes a refusal of
/// the whole edit. The duplicate guard in [`root_object`] makes the lookup by
/// name unambiguous.
fn ensure_object(
    obj: &CstObject,
    name: &str,
    path: &Path,
    dotted_path: &str,
) -> std::io::Result<CstObject> {
    obj.object_value_or_create(name).ok_or_else(|| {
        refuse_edit(
            path,
            ConfigError::Type {
                path: dotted_path.to_string(),
                expected: "an object",
            },
        )
    })
}

/// Write `value` at the object path `segments` (the last element is the leaf
/// key), preserving every comment, trailing comma, and key order elsewhere in
/// the file. Missing intermediate objects are created, as is the file and its
/// parent directory. A malformed document or a non-object intermediate is an
/// error, because changing it would discard hand-authored configuration.
///
/// This is the ONE write path for both `nub.jsonc` files. `nub config set` runs
/// through it so a hand-authored, heavily-commented file survives an edit that
/// touches one key.
pub(crate) fn set_json_path(
    path: &Path,
    segments: &[&str],
    value: CstInputValue,
) -> std::io::Result<()> {
    let (root, obj) = root_object(path)?;

    let (leaf, parents) = segments.split_last().expect("a setting path has a leaf");
    let mut cursor = obj;
    for (i, name) in parents.iter().enumerate() {
        cursor = ensure_object(&cursor, name, path, &parents[..=i].join("."))?;
    }
    match cursor.get(leaf) {
        Some(prop) => prop.set_value(value),
        None => {
            cursor.append(leaf, value);
        }
    }

    write_preserving_mode(path, &root.to_string())
}

/// Write through the atomic temp-and-rename, verify the update landed, and carry
/// across the two properties of the prior file that the new inode would
/// otherwise lose: its mode, and a leading UTF-8 BOM.
///
/// The rename installs a NEW inode carrying the temp file's default permissions,
/// so a config the user had narrowed — `600` on a file they consider private —
/// would silently come back `644`. Widening someone's permissions is not ours to
/// do as a side effect of setting a key. The mode rides along on the temp file
/// so it lands in the same atomic step as the content: re-applying it after the
/// rename would instead publish a briefly-widened file, and leave it widened for
/// good if we were killed in that window. A brand-new file keeps the default;
/// there is no prior mode to carry, and inventing a narrower one would be its
/// own surprise.
///
/// The BOM is the same concern one layer up: the reader strips it so the parser
/// accepts a Windows-authored file, which leaves it absent from the CST's
/// rendering. Not restoring it would make a one-key `set` rewrite line 1 too —
/// defeating, for exactly the files that carry a BOM, the point of the
/// comment-preserving CST write. It is the byte-level sibling of keeping the
/// author's CRLF line endings.
fn write_preserving_mode(path: &Path, text: &str) -> std::io::Result<()> {
    let prior = std::fs::metadata(path).ok().map(|m| m.permissions());
    let bytes = if crate::jsonc::starts_with_bom(path) {
        [crate::jsonc::UTF8_BOM, text.as_bytes()].concat()
    } else {
        text.as_bytes().to_vec()
    };
    aube_util::fs_atomic::atomic_write_with_permissions(path, &bytes, prior)?;

    // `Ok(())` from the atomic write is not proof the write landed: its rename
    // reports success whenever the destination exists, which is right for the
    // content-addressed store it was built for (a racing writer committed
    // bit-identical bytes) and wrong here, where the destination always exists
    // and the bytes are unique — so a file that cannot be renamed over leaves
    // `nub config set` printing its success line over unchanged content. Reading
    // back is the only check that separates the two; a length-and-mtime stat
    // cannot see the common case of a key set to a same-width value. The
    // comparison runs through the guarded reader against `text`, so it asserts
    // what the success line actually claims — that the next read sees this edit.
    if crate::jsonc::read_guarded(path).is_ok_and(|on_disk| on_disk == text) {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "{} does not hold the update after writing it — the file may be read-only or immutable",
        path.display()
    )))
}

/// Remove the key at `segments`, preserving the rest of the file. An absent
/// file, path, or key is a no-op success — nothing to clear is not an error —
/// and leaves the file untouched rather than rewriting it. A file that cannot
/// be parsed or edited faithfully is an error, just as it is for `set` and
/// `get`. Reports whether a key was actually removed so a caller can avoid
/// claiming a change it did not make.
pub(crate) fn unset_json_path(path: &Path, segments: &[&str]) -> std::io::Result<bool> {
    let (root, obj) = root_object(path)?;

    let (leaf, parents) = segments.split_last().expect("a setting path has a leaf");
    let mut cursor = obj;
    for (i, name) in parents.iter().enumerate() {
        let Some(next) = cursor.object_value(name) else {
            // A non-object intermediate is the same refusal `set` raises; an
            // absent one is simply nothing to remove.
            if cursor.get(name).is_some() {
                return Err(refuse_edit(
                    path,
                    ConfigError::Type {
                        path: parents[..=i].join("."),
                        expected: "an object",
                    },
                ));
            }
            return Ok(false);
        };
        cursor = next;
    }
    let Some(prop) = cursor.get(leaf) else {
        return Ok(false);
    };
    prop.remove();

    write_preserving_mode(path, &root.to_string())?;
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

    /// `unset` rewrites the whole file through the same CST path `set` uses, so
    /// it owes the same guarantees — and had none of them pinned: the test above
    /// checks only the resolved value, on a file nub itself wrote. A delete that
    /// flattened a hand-authored file passed.
    #[test]
    fn unset_keeps_everything_it_did_not_remove() {
        with_config_home(|home| {
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            // BOM, comment, CRLF, and an unrelated sibling key — every portable
            // content property the write path is meant to carry across.
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(crate::jsonc::UTF8_BOM).unwrap();
            write!(
                f,
                "{{\r\n  // hand-authored, keep me\r\n  \"telemetry\": false,\r\n  \"exec\": {{ \"implicitDlx\": \"never\" }}\r\n}}\r\n"
            )
            .unwrap();
            drop(f);
            assert_eq!(implicit_dlx(), ImplicitDlx::Never, "precondition");
            unset_implicit_dlx().unwrap();

            let raw = std::fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&raw);
            assert!(raw.starts_with(crate::jsonc::UTF8_BOM), "BOM survived");
            assert!(text.contains("hand-authored, keep me"), "comment survived");
            assert!(text.contains("\"telemetry\""), "sibling key survived");
            assert!(text.contains("\r\n"), "CRLF survived");
            assert_eq!(implicit_dlx(), ImplicitDlx::Prompt, "and the key is gone");
        });
    }

    /// The reader refuses anything that is not a regular file, so a `nub.jsonc`
    /// that is a FIFO cannot block the process on open. The guard was covered
    /// for `tsconfig` but never for the config file — and a hang here blocks
    /// every command, not one resolution.
    #[cfg(unix)]
    #[test]
    fn a_config_that_is_not_a_regular_file_is_refused_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("nub.jsonc");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a fresh path in a tempdir nothing else holds open.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

        let err = crate::jsonc::read_guarded(&fifo).expect_err("a FIFO must not be read");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// A config the user narrowed stays narrowed. The write installs a NEW
    /// inode, so the mode has to be carried across deliberately — and it is
    /// carried on the temp file, before the rename, so there is no moment when
    /// the committed file is readable more widely than the one it replaced.
    ///
    /// Without this, dropping the mode from the write is invisible: every other
    /// test passes while each `nub config set` quietly republishes a private
    /// file at the process umask.
    #[cfg(unix)]
    #[test]
    fn set_keeps_a_narrowed_file_narrow() {
        use std::os::unix::fs::PermissionsExt as _;

        // Running as root defeats the premise — the kernel ignores the mode
        // bits entirely, so the assertion would be measuring nothing. AGENTS.md
        // routes config work through `docker run --rm`, which is root by
        // default, so this is reachable rather than hypothetical.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        with_config_home(|home| {
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{\n  \"dlx\": { \"consent\": \"prompt\" }\n}\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

            set_implicit_dlx(ImplicitDlx::Never).unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "a 600 config must not come back {mode:o} after a set"
            );
            assert_eq!(implicit_dlx(), ImplicitDlx::Never, "and the write landed");
        });

        // A file nub creates itself has no prior mode to carry, and inventing a
        // narrower one would be its own surprise — so it takes the platform
        // default like any other new file.
        with_config_home(|home| {
            let path = home.join("nub").join("nub.jsonc");
            set_implicit_dlx(ImplicitDlx::Never).unwrap();
            assert!(path.is_file(), "the setter created the file");
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_ne!(mode, 0, "a brand-new file is left at the platform default");
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

    /// The write path must never treat a file it cannot read or parse as a blank
    /// slate: it writes its document back OVER the file, so doing so replaces a
    /// hand-authored config with the one key being set. Each input here destroyed
    /// the whole file before the guard existed.
    #[test]
    fn set_refuses_a_file_it_cannot_read_or_parse_and_leaves_it_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        let over_depth = format!("{{\"a\": {}1{}}}", "[".repeat(70), "]".repeat(70));
        let cases: [&[u8]; 7] = [
            // Malformed: a brace the author forgot to close.
            b"{\n  // hand-authored\n  \"preload\": [\"./a.ts\"],\n",
            // Parses, but the root is not an object.
            b"[\"not an object\"]",
            // Not UTF-8 at all: unreadable, which is not the same as absent.
            b"{ \"tsconfig\": \"caf\xe9.json\" }",
            over_depth.as_bytes(),
            // These became permissive jsonc-parser 0.32 defaults, but are
            // outside nub's published JSONC/schema dialect and must not be
            // preserved-and-edited by the CST writer.
            b"{ \"nodeCompat\": 0x1 }",
            b"{ \"nodeCompat\": +1 }",
            b"{ \"nodeCompat\": true \"preload\": [] }",
        ];
        for original in cases {
            std::fs::write(&path, original).unwrap();
            let err = set_json_path(&path, &["nodeCompat"], CstInputValue::Bool(true))
                .expect_err("a file that cannot be parsed is not a blank slate");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                original,
                "left the file byte-identical: {err}"
            );
        }
    }

    /// A UTF-8 BOM is not one of the refusals above. Windows editors write one
    /// by default, so rejecting it would turn away a file that looks correct to
    /// its author; the reader strips it and the writer puts it back, exactly as
    /// it keeps their comments and CRLFs.
    #[test]
    fn set_carries_the_authors_bom_across_an_edit_and_never_invents_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        std::fs::write(
            &path,
            b"\xef\xbb\xbf{\n  // kept\n  \"nodeCompat\": false\n}\n",
        )
        .unwrap();
        set_json_path(&path, &["preload"], CstInputValue::Array(Vec::new()))
            .expect("a BOM is stripped, not a parse failure");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            raw.starts_with(crate::jsonc::UTF8_BOM),
            "the author's BOM survives the edit, like their comments and CRLFs"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("// kept"), "kept the comment: {after}");
        assert!(after.contains("\"preload\""), "applied the edit: {after}");

        // Exactly one: the reader strips only a leading marker, so restoring it
        // must not stack a second one on the next edit.
        set_json_path(
            &path,
            &["tsconfig"],
            CstInputValue::String("./a.json".into()),
        )
        .unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(raw.starts_with(crate::jsonc::UTF8_BOM), "still BOM'd");
        assert!(
            !raw[crate::jsonc::UTF8_BOM.len()..].starts_with(crate::jsonc::UTF8_BOM),
            "a repeated edit must not stack BOMs"
        );

        std::fs::write(&path, b"{\n  \"nodeCompat\": false\n}\n").unwrap();
        set_json_path(&path, &["preload"], CstInputValue::Array(Vec::new())).unwrap();
        assert!(
            !std::fs::read(&path)
                .unwrap()
                .starts_with(crate::jsonc::UTF8_BOM),
            "a plain file never gains a BOM"
        );
    }

    /// The blank slates that legitimately remain, given the refusals above:
    /// absent, empty, and comment-only — the last of which keeps its comment,
    /// because a document with nothing but comments still has an author.
    #[test]
    fn set_treats_only_an_absent_empty_or_comment_only_file_as_a_blank_slate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        for (blank, keeps) in [("", ""), ("{}", ""), ("// keep me\n", "// keep me")] {
            if !blank.is_empty() {
                std::fs::write(&path, blank).unwrap();
            }
            set_json_path(&path, &["nodeCompat"], CstInputValue::Bool(true)).unwrap();
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(
                body.contains("\"nodeCompat\": true"),
                "wrote into {blank:?}"
            );
            assert!(
                body.contains(keeps),
                "kept {keeps:?} from {blank:?}: {body}"
            );
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// A CRLF file is what a Windows editor writes. It panicked the CST writer
    /// until the `jsonc-parser` bump, so this pins both that the edit succeeds
    /// and that it does not convert the author's line endings to LF.
    #[test]
    fn set_handles_crlf_files_and_keeps_their_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        std::fs::write(
            &path,
            "{\r\n  // note\r\n  \"tsconfig\": \"./a.json\"\r\n}\r\n",
        )
        .unwrap();

        set_json_path(&path, &["nodeCompat"], CstInputValue::Bool(true)).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"nodeCompat\": true"), "{body}");
        assert!(body.contains("// note"), "{body}");
        assert_eq!(
            body.matches('\n').count(),
            body.matches("\r\n").count(),
            "every newline stayed CRLF: {body:?}"
        );
    }

    /// A string value is escaped by the writer, so a Windows path or a quote
    /// cannot produce a file nub itself can no longer parse.
    #[test]
    fn set_escapes_values_that_would_otherwise_break_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        for value in [
            r"C:\Users\dev\tsconfig.json",
            r"ends-with-a-backslash\",
            "a\"b",
            "a\nb",
        ] {
            std::fs::write(&path, "{}").unwrap();
            set_json_path(
                &path,
                &["tsconfig"],
                CstInputValue::String(value.to_string()),
            )
            .unwrap();
            let body = std::fs::read_to_string(&path).unwrap();
            let parsed = crate::jsonc::parse_to_value(&body)
                .unwrap_or_else(|e| panic!("wrote a document it cannot parse ({e}): {body}"))
                .expect("a document with one key");
            assert_eq!(parsed["tsconfig"], serde_json::json!(value), "{body}");
        }
    }

    #[test]
    fn set_refuses_a_non_object_intermediate_and_leaves_it_byte_identical() {
        with_config_home(|home| {
            // A hand-authored `exec` scalar or array is not an absent parent;
            // replacing it would silently erase a user value.
            let path = home.join("nub").join("nub.jsonc");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            for junk in ["{ \"exec\": 5 }", "{ \"exec\": [1, 2] }"] {
                std::fs::write(&path, junk).unwrap();
                let err = set_implicit_dlx(ImplicitDlx::Never)
                    .expect_err("a scalar intermediate must be refused");
                let message = err.to_string();
                assert!(message.contains("exec"), "names the parent: {err}");
                assert!(
                    message.contains("must be an object"),
                    "pins object_value_or_create's non-object refusal: {err}"
                );
                assert_eq!(std::fs::read_to_string(&path).unwrap(), junk, "{err}");
            }
        });
    }

    /// The atomic rename underneath reports success whenever the destination
    /// already exists — correct for the content-addressed store it was built
    /// for, and wrong here, where the destination always exists. Without the
    /// read-back, a file that cannot be renamed over leaves `nub config set`
    /// printing its success line while the user's policy is unchanged.
    ///
    /// A directory at the destination is the portable way to make that rename
    /// fail: it needs neither an immutable flag nor root, and it fails on every
    /// platform.
    #[test]
    fn a_write_that_cannot_land_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let occupied = dir.path().join("nub.jsonc");
        std::fs::create_dir(&occupied).unwrap();

        let err = write_preserving_mode(&occupied, "{ \"nodeCompat\": true }\n")
            .expect_err("a write that did not land must not report success");
        assert!(
            err.to_string().contains("does not hold the update"),
            "says the update is not there: {err}"
        );
        assert!(occupied.is_dir(), "and the destination is untouched");
    }

    /// A duplicated key makes the reader and the writer disagree about which
    /// occurrence is authoritative — `serde_json` keeps the LAST, the CST edits
    /// the FIRST — so a `set` would report success for a change no later read
    /// can see, and a duplicated intermediate object leaves the parent lookup in
    /// [`ensure_object`] no single object to land in. Both are refused, naming
    /// the key, and the file is left exactly as its author wrote it.
    #[test]
    fn set_refuses_a_duplicated_key_and_leaves_the_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        let cases: [(&str, &[&str], &str); 4] = [
            // The leaf being written: the edit lands on the first, every read
            // resolves the second.
            (
                "{\n  // kept\n  \"nodeCompat\": false,\n  \"nodeCompat\": false\n}\n",
                &["nodeCompat"],
                "nodeCompat",
            ),
            // A duplicated intermediate object on the path being written — the
            // input that panicked the writer's parent lookup before this guard.
            (
                r#"{ "install": 1, "install": 2 }"#,
                &["install", "linker"],
                "install",
            ),
            // Nested, and named by its full path so the author is not sent
            // hunting for which one.
            (
                r#"{ "exec": { "implicitDlx": "prompt", "implicitDlx": "never" } }"#,
                &["nodeCompat"],
                "exec.implicitDlx",
            ),
            // In a subtree this edit never touches: the ambiguity belongs to the
            // file, not to one edit.
            (
                r#"{ "install": { "minimumReleaseAge": 1, "minimumReleaseAge": 2 } }"#,
                &["nodeCompat"],
                "install.minimumReleaseAge",
            ),
        ];
        for (original, segments, named) in cases {
            std::fs::write(&path, original).unwrap();
            let err = set_json_path(&path, segments, CstInputValue::Bool(true))
                .expect_err("a duplicated key gives a write no single place to land");
            assert!(err.to_string().contains(named), "names `{named}`: {err}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "left the file byte-identical: {err}"
            );
        }

        // An object inside an array is walked too, so the refusal cannot be
        // stepped around by nesting one level further out than the schema goes.
        std::fs::write(&path, r#"{ "c": [{ "x": 1, "x": 2 }] }"#).unwrap();
        set_json_path(&path, &["nodeCompat"], CstInputValue::Bool(true))
            .expect_err("the walk descends into array elements");

        // The same name in two DIFFERENT objects is not a duplicate. Without
        // this the guard could reject every real config and the cases above
        // would still pass.
        let fine = r#"{ "a": { "x": 1 }, "b": { "x": 2 }, "c": [{ "x": 1 }, { "x": 2 }] }"#;
        std::fs::write(&path, fine).unwrap();
        set_json_path(&path, &["nodeCompat"], CstInputValue::Bool(true))
            .expect("distinct objects may each carry the same key name");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("\"nodeCompat\": true"),
            "the edit lands: {body}"
        );
    }

    #[test]
    fn unset_refuses_a_duplicated_key_and_leaves_it_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        let original = r#"{ "exec": { "implicitDlx": "never", "implicitDlx": "never" } }"#;
        std::fs::write(&path, original).unwrap();

        let err = unset_json_path(&path, &[TABLE, KEY])
            .expect_err("a duplicated document cannot be edited faithfully");
        assert!(err.to_string().contains("exec.implicitDlx"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "and left the file alone"
        );
    }

    #[test]
    fn unset_refuses_unreadable_or_unparseable_documents_and_only_noops_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        let over_depth = format!("{{\"a\": {}1{}}}", "[".repeat(70), "]".repeat(70));
        let cases: [&[u8]; 5] = [
            b"{ \"nodeCompat\": true",
            b"[\"not an object\"]",
            b"{ \"tsconfig\": \"caf\xe9.json\" }",
            over_depth.as_bytes(),
            b"{ \"exec\": 5 }",
        ];
        for original in cases {
            std::fs::write(&path, original).unwrap();
            let err = unset_json_path(&path, &[TABLE, KEY])
                .expect_err("a malformed document is not an absent setting");
            assert_eq!(std::fs::read(&path).unwrap(), original, "{err}");
        }

        std::fs::remove_file(&path).unwrap();
        assert!(
            !unset_json_path(&path, &[TABLE, KEY]).unwrap(),
            "absent file"
        );
        assert!(!path.exists(), "an absent delete must not create a file");
        std::fs::write(&path, r#"{ "exec": {} }"#).unwrap();
        assert!(
            !unset_json_path(&path, &[TABLE, KEY]).unwrap(),
            "absent leaf"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{ "exec": {} }"#);
        std::fs::write(&path, r#"{ "nodeCompat": true }"#).unwrap();
        assert!(
            !unset_json_path(&path, &[TABLE, KEY]).unwrap(),
            "absent parent"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{ "nodeCompat": true }"#
        );
    }

    /// Templates are a discovery surface, so a parser field omitted from them
    /// is the same kind of drift as a field omitted from `nub config set`.
    #[test]
    fn init_templates_cover_their_schema_scopes_and_configure_nothing() {
        let has = |template: &str, key: &str| template.contains(&format!(r#""{key}":"#));

        for key in crate::project_config::ROOT_KEYS {
            if *key == "dlx" {
                assert!(!has(PROJECT_INIT_TEMPLATE, key), "project lists {key}");
            } else {
                assert!(has(PROJECT_INIT_TEMPLATE, key), "project omits {key}");
            }
            assert!(has(GLOBAL_INIT_TEMPLATE, key), "global omits {key}");
        }
        for key in crate::project_config::INSTALL_KEYS {
            assert!(has(PROJECT_INIT_TEMPLATE, key), "project omits {key}");
            assert!(has(GLOBAL_INIT_TEMPLATE, key), "global omits {key}");
        }
        for key in crate::project_config::DLX_KEYS {
            assert!(!has(PROJECT_INIT_TEMPLATE, key), "project lists {key}");
            assert!(has(GLOBAL_INIT_TEMPLATE, key), "global omits {key}");
        }

        let project = crate::project_config::parse_project_config(PROJECT_INIT_TEMPLATE)
            .expect("project template parses");
        assert_eq!(project, crate::project_config::ProjectConfig::default());

        let dir = tempfile::tempdir().unwrap();
        let global_path = dir.path().join("nub.jsonc");
        std::fs::write(&global_path, GLOBAL_INIT_TEMPLATE).unwrap();
        let global = crate::project_config::read_global_config_at(&global_path)
            .expect("global template parses");
        assert_eq!(
            global.values,
            crate::project_config::ProjectConfig::default()
        );
    }

    #[test]
    fn init_refuses_an_existing_file_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nub.jsonc");
        let original = b"{\n  // mine\n}\n";
        std::fs::write(&path, original).unwrap();

        let error = init_file(&path, InitScope::Project)
            .expect_err("an existing file must not be replaced");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn concurrent_init_has_one_winner_and_a_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/nub.jsonc");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                init_file(&path, InitScope::Project)
            }));
        }
        let outcomes: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists))
                .count(),
            1
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            PROJECT_INIT_TEMPLATE
        );
    }
}
