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
//! ## When NOT to put it in the chain
//!
//! Three cases. Each covers a moment the other two cannot see.
//!
//! - **The loader already ran** — [`LOADER_ENV_BLOB`] is in the environment and
//!   names a resolution anchored at or below the schema nub found. This is the
//!   general rule: it reads the loader's OWN surface, so it covers launchers nub
//!   never observes — a script's own `varlock run`, a Makefile, a CI wrapper. A
//!   second resolution on top of one is what made nub fire `exec()` resolvers a
//!   `--filter` had excluded, and made a script's own `--path` run die on the
//!   root schema's validation before it ever executed.
//! - **The loader is what nub is about to run** — see
//!   [`EnvOwner::launches_loader`]. No blob exists yet at that moment, by
//!   construction, so nub has to recognize the program. The loader's bin is a
//!   `#!/usr/bin/env node` script whose interpreter re-enters nub through the
//!   PATH shim, which makes this the recursion guard for every loader invocation
//!   nub did not write itself.
//! - **A parent nub already put the loader in this chain** — [`WRAPPED_ENV`]. The
//!   one gap the first two leave: a configured `prefix` sits in FRONT of the
//!   loader, so a prefix whose bin is itself a Node script re-enters nub before
//!   the loader has published anything, carrying the prefix's entry rather than
//!   the loader's.
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

/// Set on everything nub puts in front of Node, so a nested nub does not wrap
/// again — see the module docs for why the loader's own blob cannot cover this.
/// Only ever stamped on a chain nub BUILT; a loader the user invoked is
/// recognized by [`LOADER_ENV_BLOB`] and [`EnvOwner::launches_loader`] instead.
/// Internal `__NUB_*` plumbing, not a user knob.
pub(crate) const WRAPPED_ENV: &str = "__NUB_ENV_OWNER_WRAPPED";

/// The blob the loader publishes to every child it launches.
///
/// How nub learns the environment is already resolved by a launcher it never
/// saw. Measured shape:
///
/// ```json
/// {"basePath": "/abs/dir",
///  "sources": [{"type": "schema", "path": "<relative to basePath>"}, …]}
/// ```
///
/// Plain JSON in the loader's default inject mode, `@encryptInjectedEnv`
/// included — measured on 1.16.1 and 1.24.0. It is NOT always readable:
/// `--inject blob` with `@encryptInjectedEnv` encrypts the whole envelope
/// (`varlock:v1:…`), as does the in-process `auto-load` under that setting, and
/// `--inject vars` publishes no blob at all. Anything nub cannot parse is
/// treated as absent, so those shapes — and any future one — degrade to a
/// second resolution rather than to a silently empty environment.
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
    /// `None` when nothing is to be spawned — either no loader is installed, or
    /// the environment is already resolved (see the module docs).
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
        // loader in front of Node; `already_resolved` is true when the loader
        // already ran above, and its values are already in this environment.
        // Loading `.env*` in either case would layer nub's answer over the
        // loader's.
        //
        // Deliberately NOT gated on whether this particular launch will wrap:
        // when nub declines because the loader itself is what it is launching
        // ([`Self::launches_loader`]), the loader still owns the environment, so
        // nub must not feed its own cascade to the loader's own process.
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
    // Already behind the loader: do NOT resolve on top of it.
    let already_resolved = wrapped_for(&root) || resolved_by_loader(&root);
    let mut owner = EnvOwner {
        root,
        search_roots,
        schema_index,
        cli: None,
        already_resolved,
    };
    // `is_ours` gates the hand-over, not just the diagnostic: a project that
    // declares a rival claimant of this filename gets neither.
    if !already_resolved && owner.is_ours() {
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
    std::env::var_os(WRAPPED_ENV)
        .is_some_and(|marked| canonical(Path::new(&marked)) == canonical(root))
}

/// The value to stamp so a nested nub can tell WHICH project is wrapped.
pub(crate) fn wrapped_marker(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// Resolve a path as far as the filesystem allows, so two spellings of one
/// directory compare equal.
///
/// The loader's paths and nub's roots routinely differ in spelling — macOS's
/// `/tmp` is `/private/tmp`, and a symlinked `--path` keeps its symlink spelling
/// in `basePath`. A path that cannot be canonicalized is compared as written,
/// which is the best available answer and never worse than not comparing.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Whether the loader has ALREADY resolved the schema nub found here.
///
/// The test is what the loader READ, and two answers stand nub down:
///
/// - THIS schema is among the files it read, as its entry or through an
///   `@import` from anywhere.
/// - The caller pointed the loader INSIDE this project (`run --path ./config`):
///   `basePath` is at or below the schema directory, and a schema at or below
///   `basePath` was read. Their entry point is more specific than nub's, they
///   chose it, and it resolves the same project.
///
/// Everything else leaves this schema unresolved, so nub must still resolve it:
///
/// - The loader resolved an ancestor while nub found a nearer schema — a
///   workspace root's run reaching a member that ships its own. A member schema
///   wins over the root's.
/// - A different project: the case where standing down on a bare "the loader
///   ran" flag would hand project B project A's values. That run `@import`ing
///   some file below this directory changes nothing, because this project's own
///   schema still went unread.
/// - `basePath` inside the project with no schema read there, which is why it
///   cannot decide this alone. Two measured shapes: `run --path .env.prod` reads
///   only that values file, and several `-p` flags collapse `basePath` to the
///   working directory while every schema read lives elsewhere.
fn resolved_by_loader(schema_dir: &Path) -> bool {
    let Some(text) = std::env::var_os(LOADER_ENV_BLOB).and_then(|raw| raw.into_string().ok())
    else {
        return false;
    };
    let Ok(blob) = serde_json::from_str::<serde_json::Value>(&text) else {
        // Not JSON — an encrypted or future envelope nub has no claim to
        // interpret. Wrapping costs a second resolution; standing down on a blob
        // nub cannot read would hand the program whatever it happened to hold.
        return false;
    };
    let Some(base) = blob.get("basePath").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let base = Path::new(base);
    let canonical_base = canonical(base);
    let own_schema = canonical(&schema_dir.join(SCHEMA_FILE));
    let selected_inside = canonical_base.starts_with(canonical(schema_dir));
    blob.get("sources")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|sources| {
            sources
                .iter()
                .filter(|source| {
                    source.get("type").and_then(serde_json::Value::as_str) == Some("schema")
                })
                .filter_map(|source| source.get("path").and_then(serde_json::Value::as_str))
                .map(|path| canonical(&base.join(path)))
                .any(|read| {
                    read == own_schema || (selected_inside && read.starts_with(&canonical_base))
                })
        })
}

// Kept beside the other stand-down rules rather than up in the main block:
// reading one without the others invites putting the loader in front of itself.
impl EnvOwner {
    /// Whether the program nub is about to launch IS the loader's own CLI.
    ///
    /// The one stand-down that cannot be a sentinel. At this moment the loader
    /// has not run, so it has published nothing; nub must recognize the program
    /// instead. The loader's bin is a `#!/usr/bin/env node` script, so its
    /// interpreter comes back through nub's PATH shim, finds the same schema, and
    /// would wrap again — around a loader invocation the user wrote, with nub's
    /// arguments in place of theirs.
    ///
    /// Only the ENTRY decides — see [`nub_core::node::argv::entry_index`]. Its arguments
    /// are the program's data: matching them would strip the wrap from
    /// `nub check.js node_modules/varlock/package.json`, and with nub's own cascade
    /// already suppressed that program would run with no environment at all.
    ///
    /// Two clauses, because no single one covers every install shape:
    ///
    /// - The bin nub RESOLVED, canonicalized. `node_modules/.bin` normally holds
    ///   a symlink into the package, so this and the next clause agree — but an
    ///   install that copies the entry there instead leaves this as the only
    ///   match.
    /// - Any path inside a `node_modules/<loader>/` directory. This is what
    ///   catches Windows, where `.bin` holds a generated `.cmd` shim that hands
    ///   Node the package's entry; it also catches a global install's
    ///   `<prefix>/lib/node_modules/<loader>/…`, a pnpm store path, and someone
    ///   running the entry by hand.
    pub(crate) fn launches_loader(&self, args: &[String], cwd: &Path) -> bool {
        let Some(at) = nub_core::node::argv::entry_index(args) else {
            return false;
        };
        let entry = canonical(&cwd.join(&args[at]));
        self.cli
            .as_deref()
            .is_some_and(|cli| canonical(cli) == entry)
            || in_loader_package(&entry)
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
        serde_json::json!({"basePath": base, "sources": sources}).to_string()
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
        // The other side of the chain scan, and a regression a whole-chain read
        // once introduced. `dotenv-extended` hoisted at the workspace root
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
    /// The loader wrote the blob for THIS schema, so a second resolution on top
    /// would be nub asking again for an answer it already has — and asking with
    /// its own flags rather than the caller's.
    #[test]
    fn a_loader_run_over_this_schema_stands_nub_down() {
        let dir = project(&[
            (".env.schema", "# ---\nA=1\n"),
            ("config/.env.schema", "# ---\nA=2\n"),
            (&format!("node_modules/.bin/{}", bin_name()), "#!/bin/sh\n"),
        ]);
        for base in [dir.path().to_path_buf(), dir.path().join("config")] {
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
        let other = project(&[(".env.schema", "# ---\nB=1\n")]);
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

    /// An import is evidence about the file it names, not about the directory
    /// that file sits in: another project pulling in a fragment from below this
    /// one leaves this project's own schema unread.
    #[test]
    fn an_unrelated_run_importing_a_fragment_from_here_does_not_stand_nub_down() {
        let dir = project(&[
            ("app/.env.schema", "# ---\nA=1\n"),
            ("app/shared/.env.schema", "# ---\nS=1\n"),
            (
                "other/.env.schema",
                "# @import(../app/shared/.env.schema)\n# ---\n",
            ),
            (
                &format!("app/node_modules/.bin/{}", bin_name()),
                "#!/bin/sh\n",
            ),
        ]);
        let app = dir.path().join("app");
        let blob = blob(
            &dir.path().join("other"),
            &[".env.schema", "../app/shared/.env.schema"],
        );
        let owner = with_env(None, Some(&blob), || detect(&app, None)).expect("schema present");
        assert!(
            owner.spawn_target().is_some(),
            "this project's own schema was never read"
        );
    }

    /// `basePath` inside the project is not evidence the schema was read. Both
    /// shapes are real loader output, and standing down on either hands the
    /// program an environment the schema never resolved.
    #[test]
    fn a_loader_run_that_read_no_schema_here_does_not_stand_nub_down() {
        let dir = project(&[
            ("app/.env.schema", "# ---\nA=1\n"),
            ("app/.env.prod", "A=prod\n"),
            ("elsewhere/.env.schema", "# ---\nB=1\n"),
            (
                &format!("app/node_modules/.bin/{}", bin_name()),
                "#!/bin/sh\n",
            ),
        ]);
        let app = dir.path().join("app");
        // `run --path .env.prod`: anchored here, but only the values file read.
        let values_only = serde_json::json!({
            "basePath": app,
            "sources": [{"type": "values", "path": ".env.prod"}],
        })
        .to_string();
        // Several `-p` flags: `basePath` is the working directory, and the
        // schemas read live outside it.
        let elsewhere = blob(&app, &["../elsewhere/.env.schema"]);
        for blob in [values_only, elsewhere] {
            let owner = with_env(None, Some(&blob), || detect(&app, None)).expect("schema present");
            assert!(
                owner.spawn_target().is_some(),
                "a resolution that never read this schema must not stand nub down: {blob}"
            );
        }
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
            // Relative, as typed: resolved against the run's own directory.
            format!("node_modules/.bin/{}", bin_name()),
        ];
        for entry in &entries {
            assert!(
                owner.launches_loader(&[entry.into(), "run".into()], dir.path()),
                "{entry} is the loader's own code, whoever invoked it"
            );
        }
        let loader = format!("/app/node_modules/{LOADER_PACKAGE}/bin/cli.js");
        for preload in [
            vec!["--require".to_string(), "./preload.cjs".to_string()],
            vec!["--require=./preload.cjs".to_string()],
        ] {
            let args: Vec<String> = preload.into_iter().chain([loader.clone()]).collect();
            assert!(
                owner.launches_loader(&args, dir.path()),
                "a preload option's value is not the entry: {args:?}"
            );
        }
        assert!(
            !owner.launches_loader(
                &["-r".into(), loader.clone(), "/app/index.js".into()],
                dir.path()
            ),
            "a preload from the loader's package is not the program"
        );
        assert!(
            !owner.launches_loader(&["/app/src/index.js".into(), "--path".into()], dir.path()),
            "an ordinary entry file must still be wrapped"
        );
        assert!(
            !owner.launches_loader(
                &[
                    "/app/scripts/check.js".into(),
                    format!("/app/node_modules/{LOADER_PACKAGE}/package.json"),
                ],
                dir.path()
            ),
            "an ARGUMENT inside the loader's package is the program's data, not \
             the program — matching it would run the script with no environment"
        );
        assert!(
            !owner.launches_loader(
                &[
                    format!("--require=/app/node_modules/{LOADER_PACKAGE}/x.js"),
                    "/app/src/index.js".into()
                ],
                dir.path()
            ),
            "a flag is not an entry point — matching one would strip the wrap from \
             any command that merely mentions the loader"
        );
    }
}
