//! Handing the environment to an external owner.
//!
//! When a project carries a `.env.schema` and its loader resolves, nub does not
//! load `.env*` at all. It spawns the loader in FRONT of Node —
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

/// An `@env-spec` schema nub cannot act on, and why.
///
/// The two cases get opposite treatment because the project's INTENT differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaProblem {
    /// The manifest asks for the loader and it is not resolvable — a broken
    /// install (a pruned `--prod` tree, a partial `node_modules`). Falling back to
    /// `.env*` here would run the project with an environment it never asked for:
    /// no defaults, no validation, no providers, and for a schema-only project
    /// with no committed `.env`, nothing at all. Refuse instead.
    LoaderDeclaredButMissing,
    /// A schema with no loader declared anywhere. The project may not know the
    /// file means anything to nub, so nub keeps loading `.env*` and says so.
    LoaderNotDeclared,
}

impl SchemaProblem {
    /// The message, and whether it is fatal.
    pub(crate) fn message(self) -> String {
        match self {
            Self::LoaderDeclaredButMissing => format!(
                "{SCHEMA_FILE} needs {LOADER_PACKAGE}, which is in package.json but not \
                 installed. Run `nub install`."
            ),
            Self::LoaderNotDeclared => format!(
                "{SCHEMA_FILE} needs {LOADER_PACKAGE}, which isn't installed; loaded .env files \
                 instead. Run `nub add -D {LOADER_PACKAGE}`."
            ),
        }
    }

    pub(crate) fn is_fatal(self) -> bool {
        matches!(self, Self::LoaderDeclaredButMissing)
    }
}

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

    /// Whether this `.env.schema` is one nub should act on at all.
    ///
    /// One gate: no other tool that claims this filename may be a declared
    /// dependency. The file's CONTENTS are deliberately never read.
    ///
    /// nub used to sniff for `@env-spec`'s own syntax — a `# ---` divider or a
    /// `# @decorator` line — and infer ownership from that. It put nub in the
    /// business of guessing at a format it does not parse, on evidence weak in
    /// both directions: a decorator-free `@env-spec` schema reads as foreign, and
    /// a `# ---` comment in anyone's file reads as ours. Declared intent replaces
    /// it. What decides is whether the loader RESOLVES — something the project
    /// did, by installing it or listing it — and not something nub inferred from
    /// bytes it cannot interpret.
    ///
    /// This governs the STAND-DOWN as well as the diagnostic, and that pairing is
    /// the point. Gating only the warning leaves the worse half live: a
    /// `dotenv-extended` schema in a project where any varlock happened to be
    /// reachable would suppress nub's own cascade and route the whole run through
    /// `varlock run` against a schema written for a different format — silently,
    /// since the warning is the thing being suppressed.
    fn is_ours(&self) -> bool {
        !self.rival_schema_tool_declared()
    }

    /// What is wrong with this schema, if anything — see [`SchemaProblem`].
    ///
    /// Silent for a project that declares a rival tool: telling it the schema "was
    /// not applied" is false while that tool is applying it correctly, and
    /// recommends a package it never asked for.
    pub(crate) fn schema_problem(&self) -> Option<SchemaProblem> {
        if self.wrapped || self.cli.is_some() || !self.is_ours() {
            return None;
        }
        Some(if self.loader_declared() {
            SchemaProblem::LoaderDeclaredButMissing
        } else {
            SchemaProblem::LoaderNotDeclared
        })
    }

    /// Whether the project asked for the loader in its own manifest.
    ///
    /// This is what separates "your install is broken" from "you have a schema and
    /// no loader" — the first is a project that intends to use the loader, the
    /// second may not know the file means anything to nub.
    fn loader_declared(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(self.root.join("package.json")) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        ["dependencies", "devDependencies", "optionalDependencies"]
            .iter()
            .filter_map(|field| manifest.get(field).and_then(serde_json::Value::as_object))
            .any(|deps| deps.contains_key(LOADER_PACKAGE))
    }

    /// Whether a package that also claims `.env.schema` is declared here.
    ///
    /// The sole carve-out, so it carries the whole weight of the contested
    /// filename: with the contents never read, a declared rival is the one signal
    /// nub has that this file belongs to something else.
    ///
    /// Deliberately a list of ONE. The bar is a package popular enough that its
    /// users meeting this is likely — `dotenv-extended` (~44k weekly downloads)
    /// has defaulted to this filename for its own incompatible format since 2016,
    /// which is the whole reason the filename is contested. Adding long-tail names
    /// would trade a real diagnostic for silence in projects that genuinely want
    /// the schema applied.
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
    let wrapped = wrapped_for(&root);
    let mut owner = EnvOwner {
        root,
        cli: None,
        wrapped,
    };
    // `is_ours` gates the hand-over, not just the diagnostic: a project that
    // declares a rival claimant of this filename gets neither.
    if !wrapped && owner.is_ours() {
        owner.cli = find_loader_cli(project_root, workspace_root);
    }
    Some(owner)
}

/// Whether a parent nub already put the loader in front of THIS project.
///
/// Comparing the root is what makes the marker safe. A bare "something wrapped"
/// flag stands down for any project reached from inside the wrap — so a run in a
/// second, differently-configured schema project would silently inherit the outer
/// project's environment, with its own schema never resolved and no warning,
/// because the missing-loader diagnostic is gated on this too.
fn wrapped_for(root: &Path) -> bool {
    let Some(marked) = std::env::var_os(WRAPPED_ENV) else {
        return false;
    };
    // Canonicalize both sides: the marker travels through a spawn, and a symlinked
    // or `..`-relative root would otherwise compare unequal to the same directory.
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    same(Path::new(&marked), root)
}

/// The value to stamp so a nested nub can tell WHICH project is wrapped.
pub(crate) fn wrapped_marker(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// The loader CLI: the project's `node_modules/.bin` first, then `PATH`.
///
/// One lookup covers both install shapes, because the CLI ships inside the npm
/// package as well as being what a Homebrew or curl install drops on `PATH`.
///
/// The upward walk STOPS at the workspace root (or the project root when there is
/// no workspace). Unbounded, it would climb to `/` and let a stray
/// `node_modules/.bin/varlock` in `$HOME` — or anywhere else above an unrelated
/// project — decide that nub should stand down from its own `.env` loading. A
/// dependency reachable from outside the workspace is not this project's
/// dependency.
fn find_loader_cli(project_root: &Path, workspace_root: Option<&Path>) -> Option<PathBuf> {
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

    let ceiling = workspace_root.unwrap_or(project_root);
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
        if current == ceiling {
            break;
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
        assert_eq!(
            owner.schema_problem(),
            Some(SchemaProblem::LoaderNotDeclared),
            "an @env-spec schema with no loader DECLARED must say so, and must not \
             be fatal — the project may not know the file means anything to nub"
        );
    }

    #[test]
    fn a_declared_rival_tool_keeps_its_own_schema() {
        // dotenv-extended has defaulted to this filename since 2016, for an
        // incompatible format: bare `NAME=` lines, values still in `.env`. Its
        // presence in the manifest is the one signal that this file is not ours —
        // the contents are never consulted, so nothing else can say so.
        let dir = project(&[
            (
                "package.json",
                r#"{"name":"f","devDependencies":{"dotenv-extended":"^2.9.0"}}"#,
            ),
            (".env.schema", "# Server\nPORT=\nAPI_URL=\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("file present");
        assert_eq!(
            owner.cli(),
            None,
            "a declared rival must block the hand-over even with the loader installed"
        );
        assert!(
            !owner.suppresses_env_files(),
            "a rival's schema must not disturb nub's own loading"
        );
        assert_eq!(
            owner.schema_problem(),
            None,
            "nub must not name another tool at a project that never asked for it"
        );
    }

    #[test]
    fn the_schema_contents_are_never_inspected() {
        // A schema with no `@env-spec` syntax in it at all. nub used to read the
        // file and infer ownership from a `# ---` divider or a `# @decorator`
        // line; what decides now is that the loader RESOLVES.
        let dir = project(&[
            (".env.schema", "PORT=\nAPI_URL=\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("schema present");
        assert!(
            owner.cli().is_some(),
            "an installed loader owns the schema whatever the file's syntax looks like"
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
