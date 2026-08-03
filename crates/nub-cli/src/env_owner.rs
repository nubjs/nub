//! Handing the environment to an external owner.
//!
//! When a project carries an `@env-spec` schema and its loader is installed, nub
//! does not load `.env*` at all. It spawns the loader in FRONT of Node —
//! `<loader> run -- <node> …` — and the loader owns the environment end to end:
//! resolution, validation, and redaction of the child's output.
//!
//! ## Why in front, and not inside
//!
//! nub could import the loader into the Node process instead. That was tried and
//! is worse on every axis that matters:
//!
//! - **It cannot redact.** In-process patches cover `console.*`; raw
//!   `process.stdout.write` — which is what real loggers use — and anything a
//!   subprocess prints both go out in the clear. Piping the child's streams is
//!   the only redaction worth the name, and only the loader's own `run` does it.
//! - **It is a partial product.** A user who chose this loader expects what it
//!   does. Loading half of it silently gives them less than they asked for.
//! - **It couples nub to internals.** In-process integration required nub to
//!   model blob-reuse conditions, environment-snapshot timing, and which flags
//!   silently disable a fast path — none of it documented or stable. `run` is a
//!   published command with a published contract.
//!
//! So nub's whole involvement is: notice, stand down, and put the loader in the
//! spawn chain. It resolves nothing, injects nothing, and redacts nothing.
//!
//! ## Replaceability
//!
//! nub is expected to grow its own schema-driven loader. The only
//! loader-specific knowledge here is [`LOADER_PACKAGE`] and the `run` verb.

use std::path::{Path, PathBuf};

/// The schema filename that signals an external owner.
///
/// Recognized, never claimed: `dotenv-extended` has defaulted to this name since
/// 2016 for an incompatible format, so its presence alone means nothing. What
/// decides is whether the loader is installed — see [`detect`].
pub(crate) const SCHEMA_FILE: &str = ".env.schema";

/// The loader nub integrates with. Its CLI ships inside the npm package
/// (`node_modules/.bin/varlock`) and is what a standalone install puts on `PATH`,
/// so one lookup covers every install shape.
const LOADER_PACKAGE: &str = "varlock";

/// Set on the loader process so a nested nub does not wrap again.
///
/// The loader's bin is a `#!/usr/bin/env node` script, so its own interpreter
/// resolves through nub's PATH shim and re-enters nub. Without this marker that
/// nub would detect the same project and wrap once more, without bound.
/// Internal `__NUB_*` plumbing, not a user knob.
pub(crate) const WRAPPED_ENV: &str = "__NUB_ENV_OWNER_WRAPPED";

/// A project whose environment belongs to an external loader.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnvOwner {
    root: PathBuf,
    cli: Option<PathBuf>,
    wrapped: bool,
}

impl EnvOwner {
    /// The loader CLI to put in front of Node, if it is installed.
    pub(crate) fn cli(&self) -> Option<&Path> {
        self.cli.as_deref()
    }

    /// The directory whose schema was found.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// What to put in front of Node: the loader, and the directory to point it at.
    ///
    /// `None` when nothing is to be spawned — either no loader is installed, or a
    /// parent nub already wrapped this process.
    pub(crate) fn spawn_target(&self) -> Option<(&Path, &Path)> {
        self.cli().map(|cli| (cli, self.root()))
    }

    /// Whether nub should stand down from its own `.env*` cascade.
    ///
    /// False when the loader is absent, and deliberately so. A schema file with
    /// nothing to read it is not evidence that anything owns this project — the
    /// filename is shared with other tools — and standing down there would leave
    /// the process with no environment at all. nub keeps loading instead.
    pub(crate) fn suppresses_env_files(&self) -> bool {
        // True in BOTH owned states. `cli` is Some when this process will put the
        // loader in front of Node; it is None-but-wrapped when a parent nub
        // already did, and the values are already in this environment. Loading
        // `.env*` in either case would layer nub's answer over the loader's.
        self.cli.is_some() || self.wrapped
    }

    /// Diagnostic for a schema whose loader is not installed.
    ///
    /// Two independent gates, because the filename is contested and a wrong
    /// warning tells a project its config is broken when it is not:
    ///
    /// 1. the file must actually look like `@env-spec`, and
    /// 2. no other tool that claims this filename may be a declared dependency.
    ///
    /// Warning on the filename alone told `dotenv-extended` projects their schema
    /// "was not applied" while that tool was applying it correctly, and
    /// recommended a package they had never asked for.
    pub(crate) fn missing_loader_warning(&self) -> Option<String> {
        let worth_saying = !self.wrapped
            && self.cli.is_none()
            && self.schema_looks_like_env_spec()
            && !self.rival_schema_tool_declared();
        worth_saying.then(|| {
            format!(
                "nub: {SCHEMA_FILE} needs {LOADER_PACKAGE}, which isn't installed; loaded .env \
                 files instead. Run `nub add -D {LOADER_PACKAGE}`."
            )
        })
    }

    /// Whether a package that also claims `.env.schema` is declared here.
    ///
    /// Deliberately a list of ONE. The bar is a package popular enough that its
    /// users meeting this warning is likely — `dotenv-extended` (~44k weekly
    /// downloads) has defaulted to this filename for its own incompatible format
    /// since 2016, which is the whole reason the filename is contested. Adding
    /// long-tail names would trade a real diagnostic for silence in projects that
    /// genuinely want the schema applied.
    fn rival_schema_tool_declared(&self) -> bool {
        const RIVAL_PACKAGES: [&str; 1] = ["dotenv-extended"];

        let Ok(text) = std::fs::read_to_string(self.root.join("package.json")) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        ["dependencies", "devDependencies", "optionalDependencies"]
            .iter()
            .filter_map(|field| manifest.get(field).and_then(serde_json::Value::as_object))
            .any(|deps| RIVAL_PACKAGES.iter().any(|rival| deps.contains_key(*rival)))
    }

    /// Whether the schema carries `@env-spec`'s own syntax: a `# ---` header
    /// divider or a `# @decorator` line.
    ///
    /// A deliberately shallow sniff, not a parse — nub never interprets the
    /// schema, and this only decides whether a diagnostic is honest. A false
    /// negative costs a hint; a false positive is what it exists to prevent.
    fn schema_looks_like_env_spec(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(self.root.join(SCHEMA_FILE)) else {
            return false;
        };
        text.lines().take(200).any(|line| {
            line.trim_start()
                .strip_prefix('#')
                .map(str::trim_start)
                .is_some_and(|rest| rest.starts_with("---") || rest.starts_with('@'))
        })
    }
}

/// Detect an external env owner, given the project root and — when it is a
/// workspace member — the workspace root.
///
/// Both are checked, nearest first. A monorepo overwhelmingly keeps one schema at
/// the workspace root while every member has its own `package.json`, so keying on
/// the nearest root alone would miss the schema from inside any member. A member
/// that ships its own schema still wins.
///
/// `None` means no schema — nub loads `.env*` exactly as before, which is the
/// overwhelmingly common case and costs one or two `stat`s.
pub(crate) fn detect(project_root: &Path, workspace_root: Option<&Path>) -> Option<EnvOwner> {
    let root = [Some(project_root), workspace_root]
        .into_iter()
        .flatten()
        .find(|dir| dir.join(SCHEMA_FILE).is_file())?
        .to_path_buf();
    // Already behind the loader: do NOT wrap again. Its bin is a
    // `#!/usr/bin/env node` script, so its own interpreter resolves through nub's
    // PATH shim and re-enters nub — which would otherwise detect this same
    // project and wrap once more, without bound.
    let wrapped = already_wrapped();
    let cli = (!wrapped).then(|| find_loader_cli(project_root)).flatten();
    Some(EnvOwner { root, cli, wrapped })
}

/// Whether a parent nub already put the loader in front of this process.
pub(crate) fn already_wrapped() -> bool {
    std::env::var_os(WRAPPED_ENV).is_some()
}

/// The loader CLI: the project's `node_modules/.bin` first, then `PATH`.
///
/// One lookup covers both install shapes, because the CLI ships inside the npm
/// package as well as being what a Homebrew or curl install drops on `PATH`.
fn find_loader_cli(project_root: &Path) -> Option<PathBuf> {
    // Windows needs both spellings: npm generates a `.cmd` shim, while a
    // standalone install ships `varlock.exe`.
    let names: Vec<String> = if cfg!(windows) {
        vec![
            format!("{LOADER_PACKAGE}.exe"),
            format!("{LOADER_PACKAGE}.cmd"),
        ]
    } else {
        vec![LOADER_PACKAGE.to_string()]
    };

    let mut dir = Some(project_root);
    while let Some(current) = dir {
        let bin = current.join("node_modules").join(".bin");
        if let Some(found) = names
            .iter()
            .map(|name| bin.join(name))
            .find(|candidate| candidate.is_file())
        {
            return Some(found);
        }
        dir = current.parent();
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
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

    /// `PATH` is process-global, so every test that reads or writes it takes this
    /// lock rather than racing a sibling.
    fn path_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn with_path<T>(dir: Option<&Path>, f: impl FnOnce() -> T) -> T {
        let _guard = path_lock();
        let saved = std::env::var_os("PATH");
        unsafe {
            match dir {
                Some(dir) => std::env::set_var("PATH", dir),
                None => std::env::remove_var("PATH"),
            }
        }
        let out = f();
        unsafe {
            match saved {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        out
    }

    /// The loader bin name this platform's lookup actually probes.
    fn bin_name() -> &'static str {
        if cfg!(windows) {
            "varlock.cmd"
        } else {
            "varlock"
        }
    }

    #[test]
    fn no_schema_means_nub_keeps_loading_env_files() {
        let dir = project(&[(".env", "A=1")]);
        assert_eq!(
            with_path(None, || detect(dir.path(), None)),
            None,
            "a project without {SCHEMA_FILE} must not be treated as owned"
        );
    }

    #[test]
    fn a_schema_with_the_loader_installed_is_owned() {
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("schema present");
        assert!(
            owner.cli().is_some(),
            "the loader CLI ships in the npm package, so an installed loader must be found"
        );
        assert!(
            owner.suppresses_env_files(),
            "with a loader present nub must stand down from its own cascade"
        );
    }

    #[test]
    fn the_loader_is_also_found_on_path() {
        // A Homebrew or curl install puts the binary on PATH with no node_modules
        // entry at all.
        let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
        let tools = project(&[(bin_name(), "#!/bin/sh\n")]);
        let owner =
            with_path(Some(tools.path()), || detect(dir.path(), None)).expect("schema present");
        assert!(
            owner.cli().is_some(),
            "a standalone install on PATH must be found"
        );
    }

    #[test]
    fn a_schema_without_the_loader_keeps_nub_loading() {
        let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("schema present");
        assert!(
            !owner.suppresses_env_files(),
            "with nothing to read the schema, standing down would leave the child \
             with no environment at all"
        );
        assert!(
            owner.missing_loader_warning().is_some(),
            "an @env-spec schema with no loader must say so"
        );
    }

    #[test]
    fn a_foreign_env_schema_is_left_alone_silently() {
        // dotenv-extended has defaulted to this filename since 2016, for an
        // incompatible format: bare `NAME=` lines, values still in `.env`.
        let dir = project(&[(".env.schema", "# Server\nPORT=\nAPI_URL=\n")]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("file present");
        assert!(
            !owner.suppresses_env_files(),
            "a foreign schema must not disturb nub's own loading"
        );
        assert_eq!(
            owner.missing_loader_warning(),
            None,
            "nub must not name another tool at a project that never asked for it"
        );
    }

    #[test]
    fn a_workspace_member_finds_the_root_schema() {
        // Every member has its own package.json, so the nearest project root is
        // the MEMBER; keying on it alone left members with no environment.
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
            ("pkgs/web/package.json", r#"{"name":"web"}"#),
        ]);
        let member = dir.path().join("pkgs/web");
        assert_eq!(
            with_path(None, || detect(&member, None)),
            None,
            "without the workspace root a member cannot see the root schema"
        );
        let owner =
            with_path(None, || detect(&member, Some(dir.path()))).expect("root schema visible");
        assert_eq!(
            owner.root(),
            dir.path(),
            "the owner root must be where the schema lives — that is what the \
             loader resolves from"
        );
        assert!(
            owner.cli().is_some(),
            "the lookup must walk up to the workspace root's node_modules/.bin"
        );
    }

    #[test]
    fn a_member_schema_wins_over_the_workspace_root() {
        let dir = project(&[
            (".env.schema", "# ---\nROOT=1\n"),
            ("pkgs/web/package.json", r#"{"name":"web"}"#),
            ("pkgs/web/.env.schema", "# ---\nMEMBER=1\n"),
        ]);
        let member = dir.path().join("pkgs/web");
        let owner = with_path(None, || detect(&member, Some(dir.path()))).expect("member schema");
        assert_eq!(
            owner.root(),
            member,
            "a package shipping its own schema must use it, not the root's"
        );
    }
}
