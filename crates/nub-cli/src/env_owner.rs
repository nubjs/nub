//! External env-owner detection: standing down when a schema-driven loader owns
//! the environment.
//!
//! When a project carries an `@env-spec` schema (`.env.schema`), an external tool
//! owns environment loading and nub skips its own `.env*` cascade entirely. This
//! is not a de-duplication nicety. nub's cascade selects its mode from `APP_ENV`,
//! while a schema names its own selector via `@currentEnv` — so a schema that
//! points at any other variable makes nub load a *different, wrong* file set and
//! report no error. Standing down is the only way to be correct.
//!
//! Nothing needs to be undone: detection is a `stat`, and it runs before the
//! cascade, which builds a child env map rather than mutating nub's own env.
//!
//! ## Replaceability
//!
//! nub is expected to grow its own schema-driven loader. Everything in this
//! module is written against the general "an external owner handles env"
//! contract; the only varlock-specific knowledge is [`LOADER_PACKAGE`], the CLI
//! name, and the adapter `.mjs` this points at. Swapping the loader means
//! replacing those, not rewiring the callers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The schema filename that signals an external owner. `@env-spec` is a
/// vendor-neutral format with its own parser package and public spec, so keying
/// on this name is standard interop rather than reading a branded config file.
pub(crate) const SCHEMA_FILE: &str = ".env.schema";

/// The npm package implementing the loader, resolved from the *project* (never
/// from nub's own install dir — see [`EnvOwner::InProcess`]).
const LOADER_PACKAGE: &str = "varlock";

/// Marker naming the owner nub handed loading to. Read by the adapter (to know
/// what to import) and by the verification preload (to know whether to warn).
/// Internal `__NUB_*` plumbing, not a user knob.
pub(crate) const OWNER_ENV: &str = "__NUB_ENV_OWNER";

/// Absolute project root, for the adapter. The loader discovers its schema from
/// the *current directory* only, so a workspace member has to be told where the
/// root is.
pub(crate) const OWNER_ROOT_ENV: &str = "__NUB_ENV_OWNER_ROOT";

/// Set by nub when it resolved the environment itself through the loader CLI, so
/// the verification preload does not warn about a load that already happened out
/// of process.
pub(crate) const OWNER_LOADED_ENV: &str = "__NUB_ENV_OWNER_LOADED";

/// Where the adapter should RESOLVE the loader package from — the nearest project
/// root, which is not always where the schema lives.
///
/// Under an isolated `node_modules` layout (nub's default linker), a workspace
/// member's dependencies land in `<member>/node_modules` while the schema sits at
/// the workspace root. Resolving from the schema root would miss the package
/// entirely, so the two directories are carried separately: resolve from here,
/// discover the schema from [`OWNER_ROOT_ENV`].
pub(crate) const OWNER_RESOLVE_ENV: &str = "__NUB_ENV_OWNER_RESOLVE_FROM";

/// An external loader owning env for this project.
///
/// `root` and `resolve_from` are deliberately separate. `root` is where the
/// schema lives, which is what the loader must discover from; `resolve_from` is
/// the nearest project root, which is where its package is installed. Under an
/// isolated `node_modules` layout they differ for every workspace member.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnvOwner {
    root: PathBuf,
    resolve_from: PathBuf,
    kind: OwnerKind,
}

/// How the loader is reachable, which decides how nub uses it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OwnerKind {
    /// The loader package is importable from the project, so the preload adapter
    /// loads it in-process: one module graph, no subprocess, and secret
    /// redaction can be installed because the loader is in *this* process.
    InProcess,
    /// Only a CLI binary is reachable. A Homebrew or curl install ships a
    /// standalone executable and no importable module, so nub resolves the graph
    /// by running it and injects the values itself. Redaction is unavailable on
    /// this path — it requires importing the loader.
    Cli(PathBuf),
    /// A schema is present but no loader was found in any form.
    Missing,
}

impl EnvOwner {
    /// Where the schema lives — the directory the loader must discover from.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Where the loader PACKAGE resolves from — the nearest project root.
    pub(crate) fn resolve_from(&self) -> &Path {
        &self.resolve_from
    }

    pub(crate) fn kind(&self) -> &OwnerKind {
        &self.kind
    }

    /// Whether nub should stand down from its own `.env*` cascade.
    ///
    /// False for [`Self::Missing`], and deliberately so. A schema file with no
    /// loader installed anywhere is not evidence that a loader owns this project
    /// — `.env.schema` may be committed for an editor extension, a CI step, or a
    /// tool nub has never heard of. Standing down there would leave the process
    /// with no environment at all, turning an unrelated file's presence into a
    /// silent breakage. nub keeps loading and warns instead.
    pub(crate) fn suppresses_env_files(&self) -> bool {
        !matches!(self.kind, OwnerKind::Missing)
    }

    /// The value published as [`OWNER_ENV`], naming who owns loading. `None`
    /// when nub is still loading env itself, so no marker is set and the
    /// verification preload stays silent.
    pub(crate) fn marker(&self) -> Option<&'static str> {
        self.suppresses_env_files().then_some(LOADER_PACKAGE)
    }

    /// One-line diagnostic for the case where a schema is present but no loader
    /// is installed. Warned rather than raised: the run is still correct (nub
    /// loaded `.env*` as usual), the user is just not getting schema validation.
    pub(crate) fn missing_loader_warning(&self) -> Option<String> {
        matches!(self.kind, OwnerKind::Missing).then(|| {
            format!(
                "nub: found {SCHEMA_FILE} but {LOADER_PACKAGE} is not installed, so the schema \
                 was not applied.\n      nub loaded .env files as usual. To use the schema: \
                 nub add {LOADER_PACKAGE}"
            )
        })
    }
}

/// Detect an external env owner for a project, given its root and — when it is a
/// workspace member — the workspace root.
///
/// Both are checked, nearest first. A monorepo overwhelmingly keeps one schema at
/// the workspace root while every member has its own `package.json`, so keying on
/// the nearest root alone would miss the schema from inside any member and leave
/// that package with no environment at all. A member that ships its own schema
/// still wins over the root's.
///
/// `None` means no schema — nub loads `.env*` exactly as before, which is the
/// overwhelmingly common case and costs one or two `stat`s.
pub(crate) fn detect(project_root: &Path, workspace_root: Option<&Path>) -> Option<EnvOwner> {
    let root = [Some(project_root), workspace_root]
        .into_iter()
        .flatten()
        .find(|dir| dir.join(SCHEMA_FILE).is_file())?
        .to_path_buf();
    // Resolution starts at the NEAREST root and walks up, so it finds the package
    // whether the layout hoisted it to the workspace root or kept it beside the
    // member. The schema root above is a separate question and must not be reused
    // here — see OWNER_RESOLVE_ENV.
    if loader_package_dir(project_root, workspace_root.or(Some(project_root))).is_some() {
        return Some(EnvOwner {
            root,
            resolve_from: project_root.to_path_buf(),
            kind: OwnerKind::InProcess,
        });
    }
    let (resolve_from, kind) = match find_loader_cli(project_root) {
        Some(bin) => match loader_package_from_cli(&bin) {
            // A global `npm i -g` install IS an importable package — its bin is a
            // symlink into `<prefix>/lib/node_modules/<pkg>/`. Following it moves
            // those users onto the in-process path, which gets them secret
            // redaction and drops a subprocess; the CLI path can offer neither.
            Some(package_root) => (package_root, OwnerKind::InProcess),
            // A Homebrew or curl install is a standalone executable with no module
            // behind it, so running it is the only option.
            None => (project_root.to_path_buf(), OwnerKind::Cli(bin)),
        },
        None => (project_root.to_path_buf(), OwnerKind::Missing),
    };
    Some(EnvOwner {
        root,
        resolve_from,
        kind,
    })
}

/// The importable package behind a loader CLI, if there is one.
///
/// Follows the bin through symlinks and walks up to the directory holding a
/// `package.json` named after the loader. `None` for a standalone binary, which
/// has no package to find.
fn loader_package_from_cli(bin: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(bin).ok()?;
    let mut dir = real.parent();
    while let Some(current) = dir {
        let manifest = current.join("package.json");
        if manifest.is_file() {
            // Confirm this is the LOADER's own manifest. Walking out of a bin
            // directory can pass an unrelated `package.json`, and treating that as
            // the loader would hand the adapter a root it cannot import from.
            let name = std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .and_then(|json| json.get("name")?.as_str().map(str::to_string));
            return (name.as_deref() == Some(LOADER_PACKAGE)).then(|| current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

/// The loader package's directory inside the project, if it is importable.
///
/// Deliberately a plain `node_modules` walk rather than a Node resolution: this
/// only decides *which strategy* to use, and the adapter re-resolves properly
/// with `createRequire` from the project root before importing. A false negative
/// costs the slower CLI path, never a wrong answer.
fn loader_package_dir(project_root: &Path, stop_after: Option<&Path>) -> Option<PathBuf> {
    let mut dir = Some(project_root);
    while let Some(current) = dir {
        let candidate = current.join("node_modules").join(LOADER_PACKAGE);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        // Stop at the outermost directory that belongs to this project. Walking on
        // to the filesystem root would let a stray `~/node_modules/varlock` — or
        // anything above the checkout — decide that an unrelated project has a
        // loader, turning on the stand-down for a project that never opted in.
        if stop_after.is_some_and(|boundary| current == boundary) {
            break;
        }
        dir = current.parent();
    }
    None
}

/// A loader CLI binary: the project's `node_modules/.bin` first, then `PATH`
/// (a Homebrew or curl install lands there).
fn find_loader_cli(project_root: &Path) -> Option<PathBuf> {
    // Windows needs BOTH spellings, for two different installs. `node_modules/.bin`
    // holds npm's generated `.cmd` shim — but an npm install is already caught by
    // `loader_package_dir` above and takes the in-process path, so `.cmd` alone
    // would leave this branch unable to find the only install it exists for: the
    // standalone one, which ships `varlock.exe` (varlock's `install.sh` names the
    // Windows zip's binary `varlock.exe`). Probe `.exe` first, then `.cmd`.
    let names: Vec<String> = if cfg!(windows) {
        vec![
            format!("{LOADER_PACKAGE}.exe"),
            format!("{LOADER_PACKAGE}.cmd"),
        ]
    } else {
        vec![LOADER_PACKAGE.to_string()]
    };
    let bin_dir = project_root.join("node_modules").join(".bin");
    if let Some(local) = names
        .iter()
        .map(|name| bin_dir.join(name))
        .find(|candidate| candidate.is_file())
    {
        return Some(local);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// The extra `--import` tokens nub appends AFTER the user's own `preload`
/// entries.
///
/// Ordering is the whole design here. Node runs `--require` preloads, then each
/// `--import` in argv order, then the entry module. nub's own preload is the
/// fast-tier `--require`, so a verification pass placed there would run *before*
/// any loader and warn on every run; deferring it from there does not help either
/// (`process.nextTick` still fires ahead of `--import` modules, `setImmediate`
/// fires after the entry module has run). Appending here is what puts the check
/// last among the preloads and still ahead of user code.
///
/// Empty when the runtime directory cannot be located — the same broken-install
/// condition that already leaves nub un-augmented, which degrades to "no schema
/// handling" rather than aborting the run.
pub(crate) fn preload_tokens(
    owner: &EnvOwner,
    nub_binary: &Path,
    version: &nub_core::node::version::NodeVersion,
) -> Vec<String> {
    if !owner.suppresses_env_files() {
        return Vec::new();
    }
    let Some(runtime_dir) = nub_core::node::spawn::find_public_preload(nub_binary)
        .and_then(|preload| Path::new(&preload).parent().map(Path::to_path_buf))
    else {
        return Vec::new();
    };

    let mut modules = Vec::new();
    // Only the in-process path imports the loader from the child. On the CLI
    // path nub has already resolved the graph and injected the values, so all
    // that remains is the verification pass.
    if matches!(owner.kind(), OwnerKind::InProcess) {
        modules.push("env-owner-load.mjs");
    }
    modules.push("env-owner-check.mjs");

    // Reuse the user-preload injector rather than hand-building tokens: it owns
    // the `--require`/`--import` choice, the file-URL form, and the Windows path
    // quirks, and these modules must be injected exactly like any other preload.
    let specs: Vec<String> = modules
        .into_iter()
        .map(|module| runtime_dir.join(module).display().to_string())
        .collect();
    nub_core::node::spawn::user_preload_injections(&specs, version)
        .iter()
        .map(|injection| injection.node_options_token())
        .collect()
}

/// Resolve the environment by running the loader CLI and parsing its graph.
///
/// Used only on [`EnvOwner::Cli`]. Two things are load-bearing:
///
/// - `NODE_OPTIONS` is **scrubbed** from the child. The CLI is itself a Node
///   process, so inheriting nub's preload tokens would re-enter this same code
///   in the child and recurse without bound.
/// - `--path` pins discovery to the project root, because the loader searches
///   only its current directory and nub may be running from a workspace member.
pub(crate) fn load_via_cli(bin: &Path, root: &Path) -> Result<HashMap<String, String>> {
    let mut command = std::process::Command::new(bin);
    command
        .args(["load", "--format", "json-full", "--compact", "--path"])
        .arg(root)
        .current_dir(root)
        .env_remove("NODE_OPTIONS");
    // Removing NODE_OPTIONS is not enough on its own: PATH is a SECOND channel
    // back into nub. A globally npm-installed loader is a `#!/usr/bin/env node`
    // script, so with nub's shim still on PATH its shebang resolves `node` to nub,
    // which re-detects this same owner and runs the loader again — forever, and
    // before the script's own body ever executes. Measured: 8+ levels deep in one
    // `nub run` before the timeout. The loader must reach a REAL node.
    if let Some(path) = strip_node_shim_from_path(std::env::var_os("PATH")) {
        command.env("PATH", path);
    }
    let output = command
        .output()
        .with_context(|| format!("running {}", bin.display()))?;

    if !output.status.success() {
        // The loader already wrote a formatted diagnostic to stderr; pass it
        // through rather than re-wrapping it in nub's own error prose.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            eprint!("{stderr}");
        }
        anyhow::bail!("{LOADER_PACKAGE} could not resolve the environment");
    }

    parse_graph(&String::from_utf8_lossy(&output.stdout))
}

/// Drop nub's `node`-shim directories from a `PATH` value, so a spawned tool with
/// a `#!/usr/bin/env node` shebang reaches the real Node rather than re-entering
/// nub. Returns `None` when there was no `PATH` to rewrite.
fn strip_node_shim_from_path(path: Option<std::ffi::OsString>) -> Option<std::ffi::OsString> {
    let path = path?;
    let kept: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|entry| {
            !entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(nub_core::node::spawn::PATH_SHIM_PREFIX))
        })
        .collect();
    std::env::join_paths(kept).ok()
}

/// Whether a parent nub already resolved this project's environment and injected
/// the values, which every descendant inherits.
///
/// Without this, each nested `node` in an owned project pays another full loader
/// subprocess for an answer it already has. It is also a second line of defense
/// against re-entry: even if a shim slipped back onto `PATH`, the nested nub
/// declines to resolve again.
pub(crate) fn already_resolved_by_parent() -> bool {
    std::env::var_os(OWNER_LOADED_ENV).is_some()
}

/// Extract `KEY=value` pairs from the loader's `json-full` graph.
///
/// Items with no resolved value are skipped rather than injected empty, so an
/// optional-and-unset variable stays genuinely absent from `process.env`.
fn parse_graph(stdout: &str) -> Result<HashMap<String, String>> {
    let graph: serde_json::Value =
        serde_json::from_str(stdout.trim()).context("parsing the resolved environment graph")?;
    let mut resolved = HashMap::new();
    let Some(config) = graph.get("config").and_then(|c| c.as_object()) else {
        return Ok(resolved);
    };
    for (key, item) in config {
        let Some(value) = item.get("value") else {
            continue;
        };
        // `@type=port`/`boolean` items arrive already coerced, so a value can be
        // a JSON number or bool; the child environment takes strings only.
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => continue,
            other => other.to_string(),
        };
        resolved.insert(key.clone(), text);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, contents) in files {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, contents).expect("write");
        }
        dir
    }

    #[test]
    fn no_schema_means_nub_keeps_loading_env_files() {
        let dir = project(&[(".env", "A=1")]);
        assert_eq!(
            detect(dir.path(), None),
            None,
            "a project without {SCHEMA_FILE} must not be treated as owned"
        );
    }

    #[test]
    fn importable_package_selects_the_in_process_path() {
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            ("node_modules/varlock/package.json", "{}"),
        ]);
        let owner = detect(dir.path(), None).expect("schema present");
        assert!(
            matches!(owner.kind(), OwnerKind::InProcess),
            "an importable loader package must use the in-process adapter, got {owner:?}"
        );
    }

    #[test]
    fn bin_without_package_falls_back_to_the_cli_path() {
        // A Homebrew/curl install ships an executable and no importable module.
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            ("node_modules/.bin/varlock", "#!/bin/sh\n"),
        ]);
        let owner = detect(dir.path(), None).expect("schema present");
        assert!(
            matches!(owner.kind(), OwnerKind::Cli(_)),
            "a CLI-only install must use the CLI path, got {owner:?}"
        );
    }

    #[test]
    fn a_workspace_member_finds_the_root_schema() {
        // Caught by an end-to-end sweep, not by review: every member has its own
        // package.json, so the nearest project root is the MEMBER, and keying on
        // that alone left the member with no environment at all.
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            ("node_modules/varlock/package.json", "{}"),
            ("pkgs/web/package.json", r#"{"name":"web"}"#),
        ]);
        let member = dir.path().join("pkgs/web");
        assert_eq!(
            detect(&member, None),
            None,
            "without the workspace root a member cannot see the root schema — \
             this is the exact shape of the bug"
        );
        let owner = detect(&member, Some(dir.path())).expect("root schema is visible");
        assert_eq!(
            owner.root(),
            dir.path(),
            "the owner root must be where the schema lives, since that is the \
             directory the adapter hands the loader"
        );
    }

    #[test]
    fn a_member_schema_wins_over_the_workspace_root() {
        let dir = project(&[
            (".env.schema", "# ---\nROOT=1\n"),
            ("node_modules/varlock/package.json", "{}"),
            ("pkgs/web/package.json", r#"{"name":"web"}"#),
            ("pkgs/web/.env.schema", "# ---\nMEMBER=1\n"),
        ]);
        let member = dir.path().join("pkgs/web");
        let owner = detect(&member, Some(dir.path())).expect("member schema");
        assert_eq!(
            owner.root(),
            member,
            "a package shipping its own schema must use it, not the root's"
        );
    }

    #[test]
    fn schema_without_any_loader_reports_missing() {
        let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
        // PATH is process-global; empty it so a developer machine with varlock
        // installed cannot turn this into a Cli detection.
        let owner = temp_env_path_cleared(|| detect(dir.path(), None)).expect("schema present");
        assert!(
            matches!(owner.kind(), OwnerKind::Missing),
            "a schema with no loader anywhere must report Missing, got {owner:?}"
        );
    }

    /// `PATH` is process-wide, so this serializes rather than racing a sibling
    /// test that also reads it.
    fn temp_env_path_cleared<T>(f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var_os("PATH");
        unsafe { std::env::remove_var("PATH") };
        let out = f();
        if let Some(saved) = saved {
            unsafe { std::env::set_var("PATH", saved) };
        }
        out
    }

    #[test]
    fn graph_values_coerce_to_child_env_strings() {
        let resolved = parse_graph(
            r#"{"config":{
                 "GREETING":{"value":"hi","isSensitive":false},
                 "PORT":{"value":3000},
                 "FLAG":{"value":true},
                 "UNSET":{"isSensitive":false},
                 "NULLED":{"value":null}
               }}"#,
        )
        .expect("valid graph");
        assert_eq!(resolved.get("GREETING").map(String::as_str), Some("hi"));
        assert_eq!(
            resolved.get("PORT").map(String::as_str),
            Some("3000"),
            "a coerced numeric value must reach the child as its plain string form"
        );
        assert_eq!(resolved.get("FLAG").map(String::as_str), Some("true"));
        assert!(
            !resolved.contains_key("UNSET"),
            "an item with no resolved value must stay absent, not become empty"
        );
        assert!(
            !resolved.contains_key("NULLED"),
            "an explicit null must stay absent"
        );
    }

    #[test]
    fn a_graph_without_config_is_not_an_error() {
        // Defensive: a loader version that omits the key should degrade to "no
        // values", not abort the run.
        assert!(parse_graph(r#"{"sources":[]}"#).expect("parses").is_empty());
    }
}
