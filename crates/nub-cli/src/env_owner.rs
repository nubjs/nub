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
//! ## When NOT to put it in the chain
//!
//! Two cases, and between them they replace the `__NUB_ENV_OWNER_WRAPPED` marker
//! this module used to stamp. A marker only ever covered a loader nub itself
//! spawned; these cover every launcher.
//!
//! - **The loader already ran** — [`LOADER_ENV_BLOB`] is in the environment and
//!   names a resolution anchored at or below the schema nub found. Adding a
//!   second resolution on top of it is what made nub fire `exec()` resolvers a
//!   `--filter` had excluded, and what made a script's own `--path` run die on
//!   the root schema's validation before it ever executed.
//! - **The loader is what nub is about to run** — see [`launches_loader`]. No
//!   blob exists yet at that moment, by construction, so this one cannot be a
//!   sentinel: nub has to recognize the program. It doubles as the recursion
//!   guard, and a structural one beats a flag, because the loader's bin is a
//!   `#!/usr/bin/env node` script whose interpreter re-enters nub through the
//!   PATH shim.
//!
//! ## Replaceability
//!
//! nub is expected to grow its own schema-driven loader. The loader-specific
//! knowledge here is [`LOADER_PACKAGE`], the `run` verb, and the shape of
//! [`LOADER_ENV_BLOB`].

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

/// The blob the loader publishes to every child it launches.
///
/// This is how nub learns the environment is already resolved, and reading the
/// LOADER'S OWN surface rather than a nub marker is what makes it general: a
/// marker only covers a loader nub itself spawned, while this covers a Makefile,
/// a CI wrapper, a standalone binary, or a bare shell invocation — none of which
/// nub can observe. Measured shape:
///
/// ```json
/// {"basePath": "/abs/dir",
///  "sources": [{"type": "schema", "path": "<relative to basePath>"}, …]}
/// ```
///
/// `basePath` and `sources` stay plain JSON even when the loader encrypts the
/// injected values, which covers the envelope's contents and not the envelope.
/// Anything nub cannot parse is treated as absent, so an unrecognized future
/// shape degrades to wrapping rather than to a silently empty environment.
const LOADER_ENV_BLOB: &str = "__VARLOCK_ENV";

/// A schema nub cannot act on, and why. Always fatal.
///
/// A `.env.schema` this project has not disclaimed is a statement that the
/// environment is schema-resolved. Running anyway would hand the program an
/// environment it never asked for — no defaults, no validation, no providers, and
/// for a schema-only project with no committed `.env`, nothing at all — and it
/// would do it silently, which is the failure mode worth refusing. Falling back
/// to `.env*` is not a lesser version of what was asked for; it is a different
/// answer wearing the same shape.
///
/// The two cases differ only in the fix to recommend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaProblem {
    /// The manifest asks for the loader and it is not resolvable — a broken tree
    /// (a pruned `--prod` install, a partial `node_modules`). An install fixes it.
    LoaderDeclaredButMissing,
    /// A schema with no loader declared anywhere and none on `PATH`. The project
    /// needs to add one.
    LoaderNotDeclared,
}

impl SchemaProblem {
    pub(crate) fn message(self) -> String {
        match self {
            Self::LoaderDeclaredButMissing => format!(
                "{SCHEMA_FILE} needs {LOADER_PACKAGE}, which is in package.json but not \
                 installed. Run `nub install`."
            ),
            Self::LoaderNotDeclared => format!(
                "{SCHEMA_FILE} needs {LOADER_PACKAGE}, which isn't installed. Run \
                 `nub add -D {LOADER_PACKAGE}`."
            ),
        }
    }
}

/// The error for an explicit env-file instruction alongside an external owner.
///
/// Both are deliberate and they contradict: one asks nub to load a file set, the
/// other says the loader owns the environment end to end. Letting either win
/// silently drops half of what the project asked for, and the dropped half is
/// invisible — nothing prints which files did or did not arrive. So nub refuses and
/// names both sides.
///
/// `--no-env-file` is deliberately NOT one of these. It asks nub to load nothing,
/// which is what standing down already does; only a LOAD instruction contradicts
/// the hand-over.
pub(crate) fn explicit_env_file_conflict(source: &str) -> String {
    format!(
        "{source} conflicts with {SCHEMA_FILE} — {LOADER_PACKAGE} owns the environment.\n\
         Use one or the other."
    )
}

/// A project whose environment belongs to an external loader.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnvOwner {
    root: PathBuf,
    cli: Option<PathBuf>,
    already_resolved: bool,
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
    /// False when the loader is absent — but with a schema present that state no
    /// longer reaches a running program, because [`SchemaProblem`] is fatal. The
    /// one case that survives is a project declaring a rival claimant of the
    /// filename, which nub is not entitled to interpret and whose `.env*` it must
    /// therefore keep loading.
    pub(crate) fn suppresses_env_files(&self) -> bool {
        // True in BOTH owned states. `cli` is Some when this process will put the
        // loader in front of Node; `already_resolved` is true when the loader has
        // run somewhere above and its values are already here. Loading `.env*` in
        // either case would layer nub's answer over the loader's.
        //
        // Deliberately NOT gated on whether this particular launch will wrap:
        // when nub declines because the loader itself is what it is launching
        // ([`launches_loader`]), the loader still owns the environment, so nub
        // must not feed its own cascade to the loader's own process.
        self.cli.is_some() || self.already_resolved
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
        if self.already_resolved || self.cli.is_some() || !self.is_ours() {
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
    // Already behind the loader: do NOT resolve on top of it.
    let already_resolved = already_resolved_for(&root);
    let mut owner = EnvOwner {
        root,
        cli: None,
        already_resolved,
    };
    // `is_ours` gates the hand-over, not just the diagnostic: a project that
    // declares a rival claimant of this filename gets neither.
    if !already_resolved && owner.is_ours() {
        owner.cli = find_loader_cli(project_root, workspace_root);
    }
    Some(owner)
}

/// Resolve a path as far as the filesystem allows, so two spellings of one
/// directory compare equal.
///
/// The loader reports an already-canonical `basePath` (`/private/tmp/…` on macOS)
/// while nub's own roots routinely are not (`/tmp/…`), and either side can carry a
/// symlink or a `..`. A path that cannot be canonicalized is compared as written,
/// which is the best available answer and never worse than not comparing.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether the loader has ALREADY resolved the schema nub found here.
///
/// The test is CONTAINMENT against the schema directory, not equality, and the
/// asymmetry is deliberate — each direction is a different question:
///
/// - `basePath` at or BELOW the schema dir means someone pointed the loader
///   inside this project (`run --path ./config`). Their entry point is more
///   specific than nub's, they chose it, and it resolves the same project. Stand
///   down.
/// - `basePath` ABOVE it means the loader resolved an ancestor while nub found a
///   nearer schema — a workspace root's run reaching a member that ships its own.
///   The member's schema is the one that has NOT been resolved, and a member
///   schema wins over the root's, so nub must still resolve it.
/// - Unrelated means a different project entirely: the case where standing down
///   on a bare "the loader ran" flag would hand project B project A's values.
///
/// The `sources` scan then adds back the one case containment misses: a root
/// schema that `@import`s the member's own file HAS resolved it, even though the
/// root sits above. The loader lists every schema it read, each path relative to
/// `basePath`.
fn already_resolved_for(schema_dir: &Path) -> bool {
    let Some(raw) = std::env::var_os(LOADER_ENV_BLOB) else {
        return false;
    };
    let Some(text) = raw.to_str() else {
        return false;
    };
    let Ok(blob) = serde_json::from_str::<serde_json::Value>(text) else {
        // Not JSON — an opaque or future envelope nub has no claim to interpret.
        // Wrapping costs a second resolution; standing down on a blob nub cannot
        // read would hand the program whatever that envelope happened to hold.
        return false;
    };
    let Some(base) = blob.get("basePath").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let base = canonical(Path::new(base));
    if base.starts_with(canonical(schema_dir)) {
        return true;
    }
    let schema = canonical(&schema_dir.join(SCHEMA_FILE));
    blob.get("sources")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|sources| {
            sources
                .iter()
                .filter(|source| {
                    source.get("type").and_then(serde_json::Value::as_str) == Some("schema")
                })
                .filter_map(|source| source.get("path").and_then(serde_json::Value::as_str))
                .any(|path| canonical(&base.join(path)) == schema)
        })
}

// Kept beside `already_resolved_for` rather than up in the main block: the two
// are the pair of stand-down rules, and reading either one without the other
// invites putting the loader in front of itself.
impl EnvOwner {
    /// Whether the command nub is about to launch IS the loader's own CLI.
    ///
    /// This is the one stand-down that cannot be a sentinel. At this moment the
    /// loader has not run, so it has published nothing; nub must recognize the
    /// program instead. It is also the recursion guard, replacing the marker nub
    /// used to stamp on the loader it spawned — the loader's bin is a
    /// `#!/usr/bin/env node` script, so its interpreter comes back through nub's
    /// PATH shim, finds the same schema, and would wrap again without bound.
    ///
    /// Two clauses, because no single one covers every install shape:
    ///
    /// - The bin nub RESOLVED, canonicalized. `node_modules/.bin` normally holds
    ///   a symlink into the package, so this and the next clause agree — but an
    ///   install that copies the entry there instead leaves this as the only
    ///   match.
    /// - Any path inside a `node_modules/<loader>/` directory. This is what
    ///   catches Windows, where `.bin` holds a generated `.cmd` shim that
    ///   resolves to itself and hands Node the package's entry; it also catches a
    ///   global install's `<prefix>/lib/node_modules/<loader>/…`, a pnpm store
    ///   path, an unplugged PnP path, and someone running the entry by hand.
    ///
    /// Flags are skipped, so a command that merely MENTIONS the loader — a
    /// `--require` of something inside it — does not lose its wrap.
    pub(crate) fn launches_loader(&self, args: &[String]) -> bool {
        let cli = self.cli.as_deref().map(canonical);
        args.iter().filter(|arg| !arg.starts_with('-')).any(|arg| {
            let path = canonical(Path::new(arg));
            cli.as_ref().is_some_and(|cli| *cli == path) || in_loader_package(&path)
        })
    }
}

/// Whether a path lies inside a `node_modules/<loader>/` directory.
fn in_loader_package(path: &Path) -> bool {
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == "node_modules"
            && components
                .clone()
                .next()
                .is_some_and(|next| next.as_os_str() == LOADER_PACKAGE)
        {
            return true;
        }
    }
    false
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

    /// The environment is process-global, so every test that reads or writes
    /// `PATH` or the loader's blob takes this lock rather than racing a sibling.
    /// One lock covers both, because `with_env` sets both and a second lock would
    /// only invite a nested-acquisition deadlock.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn with_env<T>(dir: Option<&Path>, blob: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = env_lock();
        let saved_path = std::env::var_os("PATH");
        let saved_blob = std::env::var_os(LOADER_ENV_BLOB);
        let set = |key: &str, value: Option<&std::ffi::OsStr>| unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        };
        set("PATH", dir.map(Path::as_os_str));
        set(LOADER_ENV_BLOB, blob.map(std::ffi::OsStr::new));
        let out = f();
        set("PATH", saved_path.as_deref());
        set(LOADER_ENV_BLOB, saved_blob.as_deref());
        out
    }

    fn with_path<T>(dir: Option<&Path>, f: impl FnOnce() -> T) -> T {
        with_env(dir, None, f)
    }

    /// A loader blob resolved at `base`, listing the schema files at `schemas`
    /// (paths relative to `base`, as the loader reports them).
    fn blob(base: &Path, schemas: &[&str]) -> String {
        let sources: Vec<_> = schemas
            .iter()
            .map(|path| serde_json::json!({"type": "schema", "path": path}))
            .collect();
        serde_json::json!({"basePath": base.to_str().expect("utf8"), "sources": sources})
            .to_string()
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
    fn a_schema_with_no_loader_anywhere_is_reported() {
        let dir = project(&[(".env.schema", "# ---\nA=1\n")]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("schema present");
        assert_eq!(
            owner.schema_problem(),
            Some(SchemaProblem::LoaderNotDeclared),
            "a schema with nothing to read it must be reported — the CLI refuses \
             the run rather than falling back to its own cascade"
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

    /// The loader wrote the blob for THIS schema, so a second resolution on top
    /// would be nub asking again for an answer it already has — and asking with
    /// its own flags rather than the caller's.
    #[test]
    fn a_loader_run_over_this_schema_stands_nub_down() {
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        for base in [dir.path().to_path_buf(), dir.path().join("config")] {
            std::fs::create_dir_all(&base).expect("mkdir");
            let owner = with_env(None, Some(&blob(&base, &[".env.schema"])), || {
                detect(dir.path(), None)
            })
            .expect("schema present");
            assert_eq!(
                owner.spawn_target(),
                None,
                "a resolution anchored at {} must not be wrapped in a second one",
                base.display()
            );
            assert!(
                owner.suppresses_env_files(),
                "the loader owns the environment, so nub's own cascade stays off"
            );
        }
    }

    /// The sharp one: standing down here would hand the member the ROOT's
    /// environment and never resolve its own schema, silently — which is what a
    /// bare "the loader ran" flag would have done.
    #[test]
    fn a_loader_run_above_a_member_schema_does_not_stand_it_down() {
        let dir = project(&[
            (".env.schema", "# ---\nROOT=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
            ("pkgs/web/package.json", r#"{"name":"web"}"#),
            ("pkgs/web/.env.schema", "# ---\nMEMBER=1\n"),
        ]);
        let member = dir.path().join("pkgs/web");
        let owner = with_env(None, Some(&blob(dir.path(), &[".env.schema"])), || {
            detect(&member, Some(dir.path()))
        })
        .expect("member schema");
        assert!(
            owner.spawn_target().is_some(),
            "the member's own schema is the one that has NOT been resolved"
        );
    }

    #[test]
    fn a_loader_run_in_another_project_does_not_stand_nub_down() {
        let other = project(&[]);
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let owner = with_env(None, Some(&blob(other.path(), &[".env.schema"])), || {
            detect(dir.path(), None)
        })
        .expect("schema present");
        assert!(
            owner.spawn_target().is_some(),
            "another project's resolution says nothing about this one"
        );
    }

    /// Containment alone would miss this: the root sits ABOVE the member, but it
    /// `@import`ed the member's file, so that schema really has been resolved.
    #[test]
    fn an_imported_schema_counts_as_already_resolved() {
        let dir = project(&[
            (
                ".env.schema",
                "# @import(\"./pkgs/web/.env.schema\")\n# ---\n",
            ),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
            ("pkgs/web/package.json", r#"{"name":"web"}"#),
            ("pkgs/web/.env.schema", "# ---\nMEMBER=1\n"),
        ]);
        let member = dir.path().join("pkgs/web");
        let sources = [".env.schema", "pkgs/web/.env.schema"];
        let owner = with_env(None, Some(&blob(dir.path(), &sources)), || {
            detect(&member, Some(dir.path()))
        })
        .expect("member schema");
        assert_eq!(
            owner.spawn_target(),
            None,
            "the loader listed this member's schema among the files it read"
        );
    }

    /// Degrade toward resolving, never toward an empty environment: a blob nub
    /// cannot read may hold anything, including another project's values.
    #[test]
    fn a_blob_nub_cannot_read_is_treated_as_absent() {
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        for opaque in ["varlock:v1:ZW5jcnlwdGVk", "{}", "not json at all"] {
            let owner =
                with_env(None, Some(opaque), || detect(dir.path(), None)).expect("schema present");
            assert!(
                owner.spawn_target().is_some(),
                "an unreadable blob ({opaque}) must not be trusted to have resolved anything"
            );
        }
    }

    /// The other stand-down: nub is LAUNCHING the loader, so it must not put the
    /// loader in front of it. Also the recursion guard — the loader's own
    /// interpreter comes back through the PATH shim and finds this same schema.
    #[test]
    fn the_loaders_own_cli_is_recognized_wherever_it_lives() {
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let owner = with_path(None, || detect(dir.path(), None)).expect("schema present");
        let bin = dir.path().join("node_modules/.bin").join(bin_name());

        let entries = [
            // The bin nub resolved — the shape an install leaves when it copies
            // the entry into `.bin` instead of symlinking it.
            bin.to_string_lossy().into_owned(),
            // Inside the package, wherever the package lives.
            format!("/app/node_modules/{LOADER_PACKAGE}/bin/cli.js"),
            format!("/usr/local/lib/node_modules/{LOADER_PACKAGE}/bin/cli.js"),
        ];
        for entry in &entries {
            assert!(
                owner.launches_loader(&["--enable-source-maps".into(), entry.into(), "run".into()]),
                "{entry} is the loader's own code, whoever invoked it"
            );
        }
        assert!(
            !owner.launches_loader(&["/app/src/index.js".into(), "--path".into()]),
            "an ordinary entry file must still be wrapped"
        );
        assert!(
            !owner.launches_loader(&[format!("--require=/app/node_modules/{LOADER_PACKAGE}/x.js")]),
            "a flag is not an entry point — matching one would strip the wrap from \
             any command that merely mentions the loader"
        );
    }
}
