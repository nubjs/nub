//! Handing the environment to an external owner.
//!
//! When a project carries a `.env.schema` and its loader resolves, nub does not
//! load `.env*` at all. It spawns the loader in FRONT of Node —
//! `<loader> run -- <node> …` — and the loader owns the environment end to end:
//! resolution, validation, and redaction of the child's output.
//!
//! ## What a schema does NOT decide
//!
//! A schema is INFERRED intent: the file is present, so a loader is presumed to
//! own the environment. An `envFile` value is DECLARED intent, and declared
//! wins — see `cli::env_file_displaces_owner`, which suppresses DETECTION
//! outright rather than unpicking a hand-over further down. So this module only
//! ever sees a project that declared nothing, or one that declared `varlock`,
//! and it never has to reason about the interaction.
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

/// The error for `envFile: "varlock"` at a project with no schema to resolve.
///
/// The loader has nothing to read without one, so the run would silently get
/// nub's own cascade under a name that asked for something else. Naming the
/// missing file is the whole fix.
pub(crate) fn missing_schema_for_declared_loader() -> String {
    format!(
        "`envFile` asks for {LOADER_PACKAGE}, but this project has no {SCHEMA_FILE}.\n\
         Add one, or remove the `envFile` setting."
    )
}

/// A project whose environment belongs to an external loader.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EnvOwner {
    root: PathBuf,
    /// Every directory the lookup walked, nearest first — see [`search_roots`].
    ///
    /// The manifest checks read more than [`Self::root`] alone, and must: the
    /// schema's own directory need not carry a `package.json` at all, and the
    /// package being RUN can be a different, lower one whose declarations are
    /// about its own run. How much more differs between the two checks — see
    /// [`Self::governed`].
    search_roots: Vec<PathBuf>,
    /// Where [`Self::root`] sits in [`Self::search_roots`].
    schema_index: usize,
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
    /// False when the loader is absent — but with a schema present that state no
    /// longer reaches a running program, because [`SchemaProblem`] is fatal. The
    /// one case that survives is a project declaring a rival claimant of the
    /// filename, which nub is not entitled to interpret and whose `.env*` it must
    /// therefore keep loading.
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
    ///
    /// Reads the WHOLE chain, unlike [`Self::rival_schema_tool_declared`]. A
    /// devDependency at the workspace root is how a monorepo declares the loader
    /// for every member, so an ancestor's declaration is real evidence here — and
    /// this only chooses between two fatal messages, so a wrong answer misnames a
    /// fix rather than changing what runs.
    fn loader_declared(&self) -> bool {
        self.declares(&self.search_roots, &[LOADER_PACKAGE])
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
    ///
    /// Reads only what this schema GOVERNS — see [`Self::governed`].
    fn rival_schema_tool_declared(&self) -> bool {
        const RIVAL_PACKAGES: [&str; 1] = ["dotenv-extended"];

        self.declares(self.governed(), &RIVAL_PACKAGES)
    }

    /// The directories this schema governs: the package being run, anything
    /// between it and the schema, and the schema's own directory last.
    ///
    /// Everything ABOVE the schema is governed by a different schema or by none,
    /// so its dependencies are not evidence about this file. Scanning them was a
    /// real regression, caught in review: with `dotenv-extended` hoisted at a
    /// workspace root for some sibling's benefit, a member that ships its own
    /// schema, declares the loader and has it installed lost the hand-over — and
    /// lost it SILENTLY, because [`Self::schema_problem`] is gated on the same
    /// `is_ours` that just flipped. Standing down is not the free, cautious choice
    /// it looks like; it substitutes a different environment with no diagnostic,
    /// which is what this module exists to refuse.
    fn governed(&self) -> &[PathBuf] {
        &self.search_roots[..=self.schema_index]
    }

    /// Whether any manifest in `dirs` declares one of `packages`.
    ///
    /// `dirs` is always a prefix of [`Self::search_roots`], so it only ever holds
    /// the package being run and directories that contain it — never a sibling,
    /// never anything outside the workspace.
    fn declares(&self, dirs: &[PathBuf], packages: &[&str]) -> bool {
        dirs.iter().any(|dir| {
            let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
                return false;
            };
            // Every other manifest reader in the tree strips the BOM first, and a
            // chain scan makes skipping it worse: one BOM'd manifest anywhere would
            // read as declaring nothing.
            let Ok(manifest) =
                serde_json::from_str::<serde_json::Value>(nub_core::strip_utf8_bom(&text))
            else {
                return false;
            };
            ["dependencies", "devDependencies", "optionalDependencies"]
                .iter()
                .filter_map(|field| manifest.get(field).and_then(serde_json::Value::as_object))
                .any(|deps| packages.iter().any(|name| deps.contains_key(*name)))
        })
    }
}

/// Detect an external env owner, given the project root and — when it is a
/// workspace member — the workspace root.
///
/// The lookup walks [`search_roots`]: the project root, then each parent up to and
/// including the workspace root. Nearest wins, so a package shipping its own
/// schema keeps it.
///
/// It used to check exactly two directories — the project root and the workspace
/// root — which covered the shapes it was written for and silently missed the one
/// between them: a schema at a package that CONTAINS the package being run
/// (`apps/web/.env.schema`, run from `apps/web/functions/`) was invisible, and
/// fell back to nub's own `.env*` cascade with no diagnostic — the exact silent
/// substitution [`SchemaProblem`] exists to refuse. Scanning the chain is also
/// what the loader lookup already does, with the same ceiling for the same
/// reason, so the two now share one walk instead of disagreeing about reach.
///
/// ## Any schema inside the ceiling counts, manifest or not
///
/// A candidate needs the schema and nothing else. Requiring a `package.json`
/// beside it was tried and dropped: the ceiling is what bounds this walk, and
/// within it — the project root through the workspace root, usually one to three
/// directories — a `.env.schema` is a file somebody put there on purpose.
/// Ignoring it would do so SILENTLY, which is the one failure mode this module
/// exists to refuse.
///
/// It also bought nothing mechanically. [`EnvOwner::root`] is handed to the
/// loader as `--path`, and that argument needs the directory to exist and nothing
/// more: the loader returns early on it and explicitly ignores the
/// `varlock.loadPath` it would otherwise read from a `package.json`. The rival
/// carve-out and the diagnostic both still work, because they read the running
/// package's manifest rather than the schema directory's — see
/// [`EnvOwner::governed`].
///
/// nub walks at all only because it resolves the PROJECT ROOT while the loader
/// resolves the working directory and never searches upward. `--path` is the
/// loader's own answer for that gap, so supplying it is not a second opinion
/// layered over the loader's — it is the input the loader leaves to its caller.
///
/// `None` means no schema — nub loads `.env*` exactly as before, which is the
/// overwhelmingly common case and costs a couple of `stat`s per level of a walk
/// that is almost always one level deep.
pub(crate) fn detect(project_root: &Path, workspace_root: Option<&Path>) -> Option<EnvOwner> {
    let search_roots = search_roots(project_root, workspace_root);
    let schema_index = search_roots
        .iter()
        .position(|dir| dir.join(SCHEMA_FILE).is_file())?;
    let root = search_roots[schema_index].clone();
    // Already behind the loader: do NOT wrap again. Its bin is a
    // `#!/usr/bin/env node` script, so its own interpreter resolves through nub's
    // PATH shim and re-enters nub — which would otherwise detect this same
    // project and wrap once more, without bound.
    let wrapped = wrapped_for(&root);
    let mut owner = EnvOwner {
        root,
        search_roots,
        schema_index,
        cli: None,
        wrapped,
    };
    // `is_ours` gates the hand-over, not just the diagnostic: a project that
    // declares a rival claimant of this filename gets neither.
    if !wrapped && owner.is_ours() {
        owner.cli = find_loader_cli(&owner.search_roots);
    }
    Some(owner)
}

/// The directories a lookup may consult: the project root, then each parent up to
/// and including the workspace root — or the project root alone when there is no
/// workspace. Nearest first.
///
/// The ceiling is the whole point. Unbounded, the walk climbs to `/`, where a
/// stray `.env.schema` or `node_modules/.bin/varlock` in `$HOME` — or anywhere
/// above an unrelated project — would decide how this project loads its
/// environment. Nothing reachable only from outside the workspace belongs to it.
///
/// The bound is `starts_with`, not an equality test against the ceiling, so a
/// `workspace_root` that is not actually an ancestor of `project_root` yields the
/// project root alone rather than a walk to `/`. Component-wise comparison also
/// makes it immune to the spellings that break a string prefix: a trailing slash
/// and a `.` component both match, and `/a/bb` does not match `/a/b`.
fn search_roots(project_root: &Path, workspace_root: Option<&Path>) -> Vec<PathBuf> {
    let ceiling = workspace_root.unwrap_or(project_root);
    let mut roots = vec![project_root.to_path_buf()];
    let mut dir = project_root.parent();
    while let Some(current) = dir.filter(|dir| dir.starts_with(ceiling)) {
        roots.push(current.to_path_buf());
        dir = current.parent();
    }
    roots
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
/// `roots` is [`search_roots`], so this stops at the workspace root for the same
/// reason the schema lookup does: a dependency reachable only from outside the
/// workspace is not this project's dependency.
fn find_loader_cli(roots: &[PathBuf]) -> Option<PathBuf> {
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

    let found = roots.iter().find_map(|dir| {
        let bin = dir.join("node_modules").join(".bin");
        names
            .iter()
            .map(|name| bin.join(name))
            .find(|candidate| candidate.is_file())
    });
    if found.is_some() {
        return found;
    }

    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project rooted at a temp dir. It gets a `package.json` unless the caller
    /// supplies its own — not because the lookup needs one, which it does not, but
    /// so a fixture exercising the manifest checks has something for them to read.
    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |path: &str, contents: &str| {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, contents).expect("write");
        };
        write("package.json", r#"{"name":"fx"}"#);
        for (path, contents) in files {
            write(path, contents);
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

    #[test]
    fn a_package_inside_a_package_finds_the_enclosing_schema() {
        // The gap the two-directory lookup left. `functions/` has its own
        // manifest, so it is the project root and the schema one level up was
        // neither that nor the workspace root — the run fell through to nub's own
        // cascade with no diagnostic at all.
        let dir = project(&[
            ("package.json", r#"{"name":"ws","workspaces":["apps/*"]}"#),
            ("apps/web/package.json", r#"{"name":"web"}"#),
            ("apps/web/.env.schema", "# ---\nWEB=1\n"),
            ("apps/web/functions/package.json", r#"{"name":"fns"}"#),
        ]);
        let nested = dir.path().join("apps/web/functions");
        let owner =
            with_path(None, || detect(&nested, Some(dir.path()))).expect("enclosing schema");
        assert_eq!(
            owner.root(),
            dir.path().join("apps/web"),
            "the schema of the package that CONTAINS this one must be found"
        );
    }

    #[test]
    fn a_schema_needs_no_manifest_beside_it() {
        // The walk stops at a schema, not at a package boundary, so `pkgs/` needs
        // no manifest of its own. Requiring one was tried and dropped: inside the
        // ceiling a `.env.schema` is deliberate, and ignoring it would be silent.
        // Its lacking a manifest is exactly why the checks read the chain — the
        // member below is the only place naming the loader this project asked for.
        let dir = project(&[
            ("package.json", r#"{"name":"ws","workspaces":["pkgs/*"]}"#),
            ("pkgs/.env.schema", "# ---\nSHARED=1\n"),
            (
                "pkgs/web/package.json",
                r#"{"name":"web","devDependencies":{"varlock":"^1.0.0"}}"#,
            ),
        ]);
        let member = dir.path().join("pkgs/web");
        let owner = with_path(None, || detect(&member, Some(dir.path()))).expect("shared schema");
        assert_eq!(
            owner.root(),
            dir.path().join("pkgs"),
            "a schema between two packages must resolve from its own directory"
        );
        assert_eq!(
            owner.schema_problem(),
            Some(SchemaProblem::LoaderDeclaredButMissing),
            "the manifest naming the loader sits at the member, below the schema — \
             reading only the schema's own directory would recommend `nub add` to a \
             project whose install is merely broken"
        );
    }

    #[test]
    fn the_walk_stops_at_the_workspace_root() {
        // The ceiling, and the reason there is one: everything above a workspace
        // is somebody else's. Without it this walk reaches `$HOME`, where one
        // stray schema would silently decide how every project below it loads.
        let dir = project(&[
            (".env.schema", "# ---\nOUTSIDE=1\n"),
            (
                "ws/package.json",
                r#"{"name":"ws","workspaces":["pkgs/*"]}"#,
            ),
            ("ws/pkgs/web/package.json", r#"{"name":"web"}"#),
        ]);
        let member = dir.path().join("ws/pkgs/web");
        let workspace = dir.path().join("ws");
        assert_eq!(
            with_path(None, || detect(&member, Some(&workspace))),
            None,
            "a schema above the workspace root must not be reachable"
        );
    }

    #[test]
    fn a_rival_declared_at_the_running_package_blocks_an_ancestor_schema() {
        // The stand-down gate reads the chain for the same reason the diagnostic
        // does. A rival claimant declared where the run happens says this filename
        // is not ours, whether or not the schema's own directory carries a
        // manifest — and getting this wrong routes the whole run through `varlock
        // run` against a schema written in another format, silently.
        let dir = project(&[
            ("package.json", r#"{"name":"ws","workspaces":["pkgs/*"]}"#),
            (".env.schema", "# Server\nPORT=\n"),
            (
                "pkgs/web/package.json",
                r#"{"name":"web","devDependencies":{"dotenv-extended":"^2.9.0"}}"#,
            ),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let member = dir.path().join("pkgs/web");
        let owner = with_path(None, || detect(&member, Some(dir.path()))).expect("file present");
        assert_eq!(
            owner.cli(),
            None,
            "a rival declared at the running package must block the hand-over"
        );
        assert!(
            !owner.suppresses_env_files(),
            "and nub must keep loading its own `.env*` for that package"
        );
    }

    #[test]
    fn an_ancestors_rival_does_not_disclaim_a_member_schema() {
        // The other side of the chain scan, and a regression this PR shipped
        // before review caught it. `dotenv-extended` hoisted at the workspace root
        // for some sibling's benefit says nothing about a schema a member ships,
        // declares the loader for, and has installed — but reading it flipped
        // `is_ours`, which drops the hand-over AND silences the diagnostic gated on
        // it, leaving nub's own cascade in place with no way to notice.
        let dir = project(&[
            (
                "package.json",
                r#"{"name":"ws","workspaces":["apps/*"],"devDependencies":{"dotenv-extended":"^2.9.0"}}"#,
            ),
            (
                "apps/web/package.json",
                r#"{"name":"web","devDependencies":{"varlock":"^1.0.0"}}"#,
            ),
            ("apps/web/.env.schema", "# ---\nA=1\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        let member = dir.path().join("apps/web");
        let owner = with_path(None, || detect(&member, Some(dir.path()))).expect("member schema");
        assert!(
            owner.cli().is_some(),
            "a rival declared ABOVE the schema must not block the member's hand-over"
        );
        assert!(
            owner.suppresses_env_files(),
            "and the hand-over must still stand nub's own cascade down"
        );
    }
}
