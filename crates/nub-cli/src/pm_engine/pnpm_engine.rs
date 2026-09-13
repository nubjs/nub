//! The pnpm 12 engine, embedded in-process (feature `pm-pnpm`).
//!
//! Selected by `NUB_PM_ENGINE` while aube and pnpm coexist, so the default
//! binary and every existing path stay untouched. nub's PM grammar is pnpm's,
//! so `nub install …` is `pnpm install …` to the engine's parser.
//!
//! `NUB_PM_ENGINE=auto` lets the project's identity choose, which is what
//! project routing grows into. The two forced values stay beside it because a
//! differential needs them: `pnpm` runs the engine under pnpm's own rules on a
//! fixture nub would claim, and `pnpm-nub` under nub's (`nub.lock`,
//! `node_modules/.store`, and the settings nub resolves) on one pnpm would.

use super::host_settings;
use super::project_identity::{self, ProjectIdentity};
use anyhow::Result;
use pnpm_config::Embedder;
use std::path::{Path, PathBuf};

/// nub's naming for the files and directories the engine owns.
const NUB: Embedder = Embedder {
    program_name: "nub",
    program_version: env!("CARGO_PKG_VERSION"),
    // nub provisions Node and pins its own version; the engine must not act
    // on a packageManager pin or a devEngines.runtime entry on its behalf.
    manage_package_manager_versions: false,
    manage_runtimes: false,
    // nub-incumbent projects declare their members in `package.json`, the
    // neutral spelling every package manager reads; nub writes no
    // `pnpm-workspace.yaml`.
    workspaces_from_package_manifest: true,
    lockfile_basename: "nub.lock",
    virtual_store_dirname: ".store",
    // A nub project's configuration is `nub.jsonc`, `package.json` and
    // `.npmrc`, never pnpm's files; `profile` supplies what nub resolved.
    reads_pnpm_config: false,
    workspace_settings: None,
    compat_package_extensions: None,
    allow_builds_writer: Some(record_allow_scripts),
    extract_observer: Some(super::phantom_hooks::extract_observer),
    materialize_policy: Some(super::phantom_hooks::materialize_policy),
};

/// What this process answers the engine with when it asks for the host's
/// settings. The engine asks while nub is inside its entry point, so the
/// lock guards the slot and is never held across an engine call.
static HOST_SETTINGS: std::sync::Mutex<Option<&'static pnpm_config::WorkspaceSettings>> =
    std::sync::Mutex::new(None);

/// The settings nub resolved for this project.
///
/// pnpm re-reads `pnpm-workspace.yaml` for every configuration it builds, so
/// a command that writes its own settings and then reloads sees what it
/// wrote. This slot is nub's equivalent: resolved once at startup, and
/// replaced by whatever goes on to write the project's configuration.
///
/// The directory is ignored because nub resolves ONE project's settings —
/// [`host_settings::resolve`] walks to the workspace root itself — and every
/// configuration a run builds belongs to that project.
fn host_workspace_settings(
    _dir: &std::path::Path,
) -> Option<&'static pnpm_config::WorkspaceSettings> {
    *HOST_SETTINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The store this run reads and writes, as the engine resolves it.
///
/// Taken from the settings published above rather than re-derived, so a
/// project that sets `storeDir` in its `.npmrc` or `install.settings` is
/// answered with the store the engine will actually use. Absent until a
/// profile has been built, and absent under pnpm's own identity, which
/// resolves its store itself.
pub(crate) fn host_store_dir() -> Option<std::path::PathBuf> {
    host_workspace_settings(std::path::Path::new(""))?
        .store_dir
        .as_deref()
        .map(std::path::PathBuf::from)
}

/// Answer with `settings` from here on. Leaked because the engine holds its
/// configuration for as long as the run, and called once per resolve — at
/// startup, and again when a command writes the project's own settings.
fn publish_host_settings(settings: pnpm_config::WorkspaceSettings) {
    *HOST_SETTINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::leak(Box::new(settings)));
}

/// Fold an approve-builds decision into what this process answers with.
///
/// Writing `package.json` settles the question for the NEXT run; the rebuild
/// `approve-builds` runs straight after is part of THIS one, and it builds a
/// fresh configuration. Without this it would be answered from the resolve
/// that ran before the write, report the build as still ignored, and exit
/// non-zero on the very decision the user just made.
fn republish_allow_builds(decisions: &[(&str, bool)]) {
    let mut slot = HOST_SETTINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut settings = slot.map_or_else(Default::default, Clone::clone);
    let allowed = settings.allow_builds.get_or_insert_with(Default::default);
    for (package, may_run) in decisions {
        allowed.insert(
            (*package).to_owned(),
            pnpm_config::AllowBuild::Decided(*may_run),
        );
    }
    *slot = Some(Box::leak(Box::new(settings)));
}

/// Record an `approve-builds` decision where a nub project reads it back:
/// the `allowScripts` field of its `package.json`.
///
/// The engine would otherwise write `allowBuilds` into `pnpm-workspace.yaml`,
/// which a nub project reads nothing from — so the approval would be lost on
/// the next install, and worse, the file itself makes the project read as
/// pnpm's, which would send every later command down the wrong identity.
///
/// A decision REPLACES the entry it names and leaves the rest alone, because
/// the user is deciding one package at a time.
fn record_allow_scripts(dir: &std::path::Path, decisions: &[(&str, bool)]) -> std::io::Result<()> {
    nub_core::pm::resolve::edit_root_manifest(dir, |manifest| {
        // Edited in place where the field already exists, so an approval
        // moves nothing else in the file; a malformed value is replaced,
        // since the engine could not have read it either.
        if let Some(serde_json::Value::Object(allowed)) =
            manifest.get_mut(host_settings::ALLOW_SCRIPTS_FIELD)
        {
            decide(allowed, decisions);
            return;
        }
        let mut allowed = serde_json::Map::new();
        decide(&mut allowed, decisions);
        manifest.insert(
            host_settings::ALLOW_SCRIPTS_FIELD.to_owned(),
            serde_json::Value::Object(allowed),
        );
    })
    .map_err(std::io::Error::other)?;
    republish_allow_builds(decisions);
    Ok(())
}

/// Apply each decision, replacing whatever the field said about that package.
fn decide(allowed: &mut serde_json::Map<String, serde_json::Value>, decisions: &[(&str, bool)]) {
    for (package, may_run) in decisions {
        allowed.insert((*package).to_owned(), serde_json::Value::Bool(*may_run));
    }
}

/// nub's own compatibility rules, in the shape the engine's database takes.
///
/// The engine already carries Yarn's rules and pnpm's three additions; these
/// are the machine-derived ones `@nubjs/extensions` adds on top, which the
/// profile layers beneath them. A rule the engine cannot read is dropped
/// rather than failing the install, matching how `compat_db` treats a
/// database it cannot parse at all.
fn host_compat_rules() -> &'static indexmap::IndexMap<String, pnpm_config::PackageExtension> {
    static RULES: std::sync::OnceLock<indexmap::IndexMap<String, pnpm_config::PackageExtension>> =
        std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        super::compat_db::bundled_package_extensions()
            .iter()
            .filter_map(|(selector, body)| {
                serde_json::from_value(body.clone())
                    .ok()
                    .map(|extension| (selector.clone(), extension))
            })
            .collect()
    })
}

/// How this invocation decides which rules the engine runs under.
enum Selection {
    /// The project's own identity decides — what project routing becomes
    /// once the engine owns the PM verbs outright.
    Auto,
    /// One identity, regardless of the project. This is what makes a
    /// differential against real pnpm possible on a fixture that nub would
    /// otherwise claim, and vice versa.
    Forced(ProjectIdentity),
}

/// The selection this invocation asked for, if the engine is selected at all.
fn selection() -> Option<Selection> {
    match std::env::var_os("NUB_PM_ENGINE")?.to_str()? {
        "pnpm" => Some(Selection::Forced(ProjectIdentity::Pnpm)),
        "pnpm-nub" => Some(Selection::Forced(ProjectIdentity::Nub)),
        "auto" => Some(Selection::Auto),
        _ => None,
    }
}

/// Whether the pnpm engine is selected for this invocation.
pub(crate) fn selected() -> bool {
    selection().is_some()
}

/// The profile this invocation runs the engine under.
///
/// This is also where a configuration that cannot be honoured is refused,
/// because it is the first point at which both the identity and nub's own
/// config file are in hand.
fn profile(selection: Selection, cwd: &Path) -> Result<Embedder> {
    let identity = match selection {
        Selection::Forced(identity) => identity,
        Selection::Auto => project_identity::detect(cwd),
    };
    let loaded = crate::project_config::load_project_config(cwd)?;
    if let Some(loaded) = &loaded
        && let Some(path) = loaded.source.path.as_deref()
    {
        project_identity::check_install_block(identity, path, &loaded.values.install)?;
    }
    Ok(match identity {
        ProjectIdentity::Pnpm => Embedder::PNPM,
        ProjectIdentity::Nub => {
            let install = loaded
                .map(|loaded| loaded.values.install)
                .unwrap_or_default();
            // `install.linker.eject` names the packages this project keeps
            // out of the shared store. The other engine publishes it while
            // building the session this path never enters, so without this
            // the policy would see nub's built-in names and nothing the
            // project asked for.
            super::phantom_closure::set_native_config_seed(match &install.linker {
                Some(crate::project_config::LinkerConfig::Global { eject: Some(eject) }) => {
                    eject.clone()
                }
                _ => Vec::new(),
            });
            publish_host_settings(host_settings::resolve(cwd, &install)?);
            warn_about_a_stray_workspace_yaml(cwd);
            Embedder {
                workspace_settings: Some(host_workspace_settings),
                compat_package_extensions: Some(host_compat_rules()),
                ..NUB
            }
        }
    })
}

/// Say so when a nub project still carries a `pnpm-workspace.yaml`.
///
/// Never read and never silent. The file gets there by a branch merge or a
/// copied tutorial, and under nub's own identity the settings come from
/// `nub.jsonc` instead — so a project whose author believes that file is
/// configuring the install would otherwise get no sign that it is inert.
/// Both ways out are named, because which one is right is the project's
/// call and not nub's.
///
/// Anchored at the root the settings themselves resolve against, so a
/// command run in a workspace member reports the file once, in the same
/// words, rather than missing it. Once per process, which is once per
/// command: this runs where the identity is decided.
fn warn_about_a_stray_workspace_yaml(cwd: &Path) {
    if host_settings::workspace_root(cwd)
        .join("pnpm-workspace.yaml")
        .is_file()
    {
        eprintln!(
            "nub: pnpm-workspace.yaml is not read under nub identity — migrate it \
             (`nub pm use nub`), delete it, or return to pnpm (`nub pm use pnpm`)."
        );
    }
}

/// Rebrand a rendered engine report for nub's users.
///
/// Two families of name survive the profile and reach here as text. The
/// engine declares ~800 `ERR_PNPM_*` codes as compile-time attributes, and
/// two of its own code paths compare those strings literally, so renaming
/// them at construction would change engine behavior. A handful of misuse
/// errors likewise bake `Usage: pnpm <verb>` into a `#[display]` attribute,
/// which the runtime program name the profile sets cannot reach.
///
/// The commands a report tells the user to RUN are rewritten too, by
/// [`rewrite_suggestions`].
fn rebrand(rendered: &str, embedder: Embedder) -> String {
    rewrite_suggestions(rendered, embedder.program_name)
        .replace(
            "Usage: pnpm ",
            &format!("Usage: {} ", embedder.program_name),
        )
        .replace("ERR_PNPM_", "ERR_NUB_")
        .replace("WARN_PNPM_", "WARN_NUB_")
}

/// Verbs whose advice nub has to respell rather than just rename.
///
/// The engine's `self-update` replaces the pnpm it runs as. nub's
/// self-update is `upgrade`, and nub's `update` — which is what the engine
/// reads `upgrade` as — updates dependencies, so swapping the program name
/// alone here would point the user at a command that does something else.
const SUGGESTION_RENAMES: [(&str, &str); 1] = [("self-update", "upgrade")];

/// Put the running program's name where the engine wrote its own.
///
/// Every occurrence of the bare WORD is substituted, not just a quoted
/// command. Under nub's identity the engine IS nub's package manager, so a
/// sentence about what it requires, refuses or has not implemented is a
/// sentence about nub — and the reader has no other package manager to
/// attach the name to. Narrower drafts of this rewrote only a command
/// inside quotes or backticks, on the reasoning that ordinary prose used
/// the name as a subject and should be left alone; that left 71 diagnostic
/// sites naming pnpm to a nub user, most of them commands written in some
/// other quoting style (`(e.g., pnpm access …)`, `'pnpm dlx' requires …`,
/// `Run pnpm dedupe to …`).
///
/// What is NOT substituted is anything where `pnpm` is part of a larger
/// token rather than the program's name: a file (`pnpm-lock.yaml`,
/// `.pnpmfile.cjs`), a path segment (`node_modules/.pnpm`,
/// `~/Library/pnpm/store`) and a host (`pnpm.io`). Those name real things
/// on disk and on the network that keep their names whoever is running, so
/// rewriting one would produce a path that does not exist. That is the
/// whole reason this is a word-boundary walk rather than a replace.
///
/// A pnpm-incumbent project never reaches here: its reports go out
/// verbatim, which is what makes them pnpm's own.
fn rewrite_suggestions(rendered: &str, program: &str) -> String {
    /// Whether `pnpm` sitting at `at` is the program's name rather than
    /// part of a filename, a path segment or a hostname.
    fn is_the_program_name(rendered: &str, at: usize) -> bool {
        let joins = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'/');
        let bytes = rendered.as_bytes();
        if at > 0 && (joins(bytes[at - 1]) || bytes[at - 1] == b'.') {
            return false;
        }
        match bytes.get(at + "pnpm".len()) {
            None => true,
            Some(&next) if joins(next) => false,
            // A trailing dot ends a sentence unless something follows it,
            // which makes it a hostname (`pnpm.io`) or a filename.
            Some(b'.') => !bytes
                .get(at + "pnpm.".len())
                .is_some_and(|c| c.is_ascii_alphanumeric()),
            Some(_) => true,
        }
    }

    let mut out = String::with_capacity(rendered.len());
    let mut rest = rendered;
    let mut consumed = 0;
    while let Some(at) = rest.find("pnpm") {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        rest = &from["pnpm".len()..];
        if !is_the_program_name(rendered, consumed + at) {
            out.push_str("pnpm");
            consumed += at + "pnpm".len();
            continue;
        }
        out.push_str(program);
        consumed += at + "pnpm".len();
        // A respelled verb only follows the name as a command would: one
        // space, then the verb itself.
        let Some(tail) = rest.strip_prefix(' ') else {
            continue;
        };
        let verb = tail
            .split(|c: char| !c.is_ascii_lowercase() && c != '-')
            .next()
            .unwrap_or_default();
        if let Some((_, respelled)) = SUGGESTION_RENAMES.iter().find(|(named, _)| *named == verb) {
            out.push(' ');
            out.push_str(respelled);
            rest = &tail[verb.len()..];
            consumed += " ".len() + verb.len();
        }
    }
    out.push_str(rest);
    out
}

/// The process-level setup the engine does not do for itself.
///
/// The other engine gets all of this while building the session it runs
/// in; this one builds no session, so each piece was simply absent rather
/// than deliberately skipped.
///
/// The order is load-bearing. The configuration snapshot comes first
/// because the augmentation reads it: without it `runtime_config` finds
/// no snapshot and answers with the built-in defaults, so a project's own
/// runtime settings would never reach a lifecycle script — and nothing
/// would report it, because the augmentation still looks applied.
fn session_prologue(cwd: &Path) -> Result<()> {
    crate::cli::initialize_config_snapshot_at(cwd, false, false)?;
    // macOS leaves the soft descriptor limit at 256, which a large
    // concurrent install exhausts with `Too many open files`.
    nub_core::resource_limits::raise_nofile_limit();
    apply_lifecycle_augmentation(cwd)
}

/// Put nub's runtime augmentation on THIS process's environment, so every
/// lifecycle script the engine spawns inherits it.
///
/// The other engine takes the same overlay through its own context, which
/// this one never reads — but it needs no seam of its own, because the
/// engine APPENDS its resolution shim to an inherited `NODE_OPTIONS`
/// rather than replacing it. Measured with a dependency whose postinstall
/// writes the variable out: a value set here arrives first and the
/// engine's shim follows it.
///
/// Without this a dependency's build script compiles against whatever
/// Node happens to be ambient rather than the project's pinned one, which
/// is the ABI bug the other engine's session prologue exists to close.
///
/// Silent when augmentation cannot be computed — no nub binary to point
/// at, no runtime config — which leaves the engine's own behaviour
/// exactly as it was.
fn apply_lifecycle_augmentation(cwd: &Path) -> Result<()> {
    let discovered = nub_core::node::discovery::discover_node(&super::lifecycle_node_anchor(cwd));
    let Ok(nub_binary) = nub_core::node::spawn::current_nub_binary() else {
        return Ok(());
    };
    let node = discovered.unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());
    let mut runtime = crate::project_config::runtime_config()?;
    let runtime_node_options = crate::cli::lifecycle_node_options(&mut runtime, &node)?;
    let runtime_json = crate::cli::runtime_config_json(&runtime)?;
    let pnp_ctx = nub_core::pnp::detect(cwd);
    // Lifecycle scripts are never compat: PM verbs run augmented, and there
    // is no `--node` lifecycle path.
    let Some(mut aug) = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version.clone(),
        false,
        pnp_ctx.as_ref().map(|c| c.pnp_cjs.as_path()),
        &runtime_node_options,
    ) else {
        return Ok(());
    };
    // npm/pnpm parity: `npm_config_node_options` seeds a script's
    // NODE_OPTIONS only when the ambient environment carries none itself.
    if std::env::var_os("NODE_OPTIONS").is_none()
        && let Ok(configured) = std::env::var("NPM_CONFIG_NODE_OPTIONS")
            .or_else(|_| std::env::var("npm_config_node_options"))
        && !configured.is_empty()
    {
        match &mut aug.node_options {
            Some(options) => {
                options.push(' ');
                options.push_str(&configured);
            }
            None => aug.node_options = Some(configured),
        }
    }
    let (overlay, path_prepends) =
        super::augmentation_to_lifecycle_overlay(&aug, node.path.as_str(), Some(&runtime_json));
    for (key, value) in overlay {
        unsafe { std::env::set_var(key, value) };
    }
    if !path_prepends.is_empty() {
        let mut entries = path_prepends;
        if let Some(existing) = std::env::var_os("PATH") {
            entries.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(entries) {
            unsafe { std::env::set_var("PATH", joined) };
        }
    }
    Ok(())
}

/// The directory this command line makes the project's.
///
/// `--dir` (and `-C`, and `--prefix`) names a project other than the one
/// the process sits in, and the engine applies it without moving the
/// process — it carries the directory as data and resolves every path
/// against the process directory. Everything nub reads for itself before
/// handing the command over is anchored here for the same reason: the
/// project's identity, its `nub.jsonc`, its install settings, and the
/// runtime a lifecycle script is augmented with are all the NAMED
/// project's, and reading them where the process happens to sit answers
/// with a different project's — silently, since a wrong answer here still
/// looks like a working install.
///
/// The engine's own grammar says which token carries the directory, so
/// the two cannot disagree about what the command line named.
fn host_base_dir(argv: &[std::ffi::OsString]) -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    // Joining is what the engine does: an absolute answer replaces the
    // process directory, a relative one extends it.
    Ok(pnpm_cli::working_dir(argv).map_or(cwd.clone(), |dir| cwd.join(dir)))
}

/// Run the engine on `argv` and return its exit status.
///
/// `argv` is the whole command line, program name first, as the host
/// resolved it — not the process argv. nub reads a few flags of its own
/// before the verb and acts on them itself, and the engine's grammar has
/// no spelling for those, so what it runs on is what nub left.
pub(crate) fn run(argv: Vec<std::ffi::OsString>) -> Result<i32> {
    let cwd = host_base_dir(&argv)?;
    let embedder = profile(selection().unwrap_or(Selection::Auto), &cwd)?;
    session_prologue(&cwd)?;
    // The engine's own entry point installs this before it can print. It
    // drops each cause the level above already states in full, so a host
    // that leaves miette at its default renders chains the engine collapses
    // — a divergence invisible on a one-level diagnostic and plain on a
    // deep one.
    pnpm_diagnostics::install_report_handler();
    // Asked BEFORE the run, because a successful install answers it: it
    // writes nub's own lockfile, and the project then looks migrated.
    let command = pnpm_cli::command_name(&argv);
    let pending = pending_migration(embedder, command.as_deref(), &cwd);
    // Asked before the run for the same reason, and it is the stronger case:
    // the install is about to write nub's lockfile, after which no project
    // still looks virgin.
    let stamp = project_is_virgin(embedder, command.as_deref(), &cwd);
    match pnpm_cli::run(argv, embedder) {
        Ok(()) => {
            if let Some(foreign) = pending {
                eprintln!("{}", super::migrate::migration_hint(&foreign));
            }
            if stamp {
                super::install_family::stamp_virgin_dev_engines(&cwd);
            }
            Ok(0)
        }
        Err(report) => {
            // The engine skips its own render for a command that has
            // already printed its report, and answers for which those are,
            // so nothing here has to track the list across a pin move.
            if !pnpm_cli::is_reported_error(&report) {
                // The `Error: ` prefix is the engine's own, not decoration:
                // without it a pnpm-incumbent project's stderr differs from
                // real pnpm's on every failure.
                let rendered = format!("Error: {report:?}");
                // A pnpm-incumbent project must see pnpm's own output verbatim.
                if embedder.program_name == Embedder::PNPM.program_name {
                    eprintln!("{rendered}");
                } else {
                    eprintln!("{}", rebrand(&rendered, embedder));
                }
            }
            Ok(1)
        }
    }
}

/// The commands that resolve the project and write its lockfile. A project
/// still holding another package manager's lockfile has just had it ignored,
/// so this is where saying so belongs — not on a command that only reads.
const RESOLVING_COMMANDS: [&str; 6] = ["install", "add", "remove", "update", "ci", "dedupe"];

/// The commands that may leave nub's mark on a project's manifest.
///
/// Narrower than [`RESOLVING_COMMANDS`] by two, and both exclusions are the
/// point. `ci` refuses without a lockfile it already reads, so it can never
/// meet a virgin project; `dedupe` rewrites an existing lockfile rather than
/// claiming a project. `import` is not here either — it converts a FOREIGN
/// lockfile, so its project is non-virgin by construction.
const STAMPING_COMMANDS: [&str; 4] = ["install", "add", "remove", "update"];

/// Whether nub is the FIRST package manager to touch this project, asked
/// before the command that would stop it being true.
///
/// nub's canonical lockfile is deliberately unbranded, so — unlike every
/// other package manager, whose lockfile name is itself the project's
/// signal — nub leaves nothing downstream tools can read. The
/// `devEngines.packageManager` range is that signal, and it is only nub's to
/// write on a project no one else has claimed.
///
/// Any incumbent signal answers `false`: a lockfile of nub's own, a foreign
/// one, or a pnpm-named file anywhere up the walk. The declaration fields are
/// deliberately NOT consulted here — the writer never overwrites an existing
/// `devEngines.packageManager`, so a hand-written foreign one survives on its
/// own merits rather than by this predicate having to know about it.
fn project_is_virgin(embedder: Embedder, command: Option<&str>, cwd: &Path) -> bool {
    if embedder.program_name == Embedder::PNPM.program_name
        || !command.is_some_and(|name| STAMPING_COMMANDS.contains(&name))
    {
        return false;
    }
    let root = super::host_settings::workspace_root(cwd);
    if super::nub_lockfile_present(&root) || super::migrate::pending_migration(&root).is_some() {
        return false;
    }
    root.ancestors()
        .all(|dir| !super::dir_has_pnpm_named_file(dir))
}

/// The foreign lockfile this command is about to ignore, if there is one and
/// saying so is this program's business.
///
/// Only under nub's own identity: a pnpm project must see what pnpm prints,
/// and pnpm says nothing here. The condition clears itself once the install
/// has run, which is the whole reason it is asked first.
fn pending_migration(
    embedder: Embedder,
    command: Option<&str>,
    cwd: &Path,
) -> Option<std::path::PathBuf> {
    if embedder.program_name == Embedder::PNPM.program_name
        || !command.is_some_and(|name| RESOLVING_COMMANDS.contains(&name))
    {
        return None;
    }
    let project = nub_core::workspace::detect::detect_project(cwd)?;
    super::migrate::pending_migration(&project.workspace_root.unwrap_or(project.root))
}

#[cfg(test)]
mod tests {
    use super::{host_compat_rules, rewrite_suggestions};

    /// The name is substituted wherever it is the PROGRAM's, in prose as
    /// much as in a quoted command. A nub user has no other package manager
    /// to attach it to, and the engine is the one running, so a sentence
    /// about what it requires is a sentence about nub.
    #[test]
    fn the_program_name_is_substituted_wherever_it_is_the_program() {
        for (engine, nub) in [
            (
                r#"Run "pnpm approve-builds" to pick."#,
                r#"Run "nub approve-builds" to pick."#,
            ),
            (
                "run `pnpm clean --lockfile` and `pnpm install`.",
                "run `nub clean --lockfile` and `nub install`.",
            ),
            // The four shapes the quoted-only rule left behind, which are
            // what made this 71 sites rather than a handful.
            (
                "pnpm requires one wheel per package",
                "nub requires one wheel per package",
            ),
            (
                "Package name is required (e.g., pnpm access get status @scope/pkg)",
                "Package name is required (e.g., nub access get status @scope/pkg)",
            ),
            (
                "'pnpm dlx' requires a command to run",
                "'nub dlx' requires a command to run",
            ),
            (
                "Run pnpm dedupe to apply the changes above.",
                "Run nub dedupe to apply the changes above.",
            ),
            // The sentence-final form: a dot ends it, so the name is still
            // the program's.
            (
                "not yet implemented in pnpm.",
                "not yet implemented in nub.",
            ),
        ] {
            assert_eq!(rewrite_suggestions(engine, "nub"), nub, "input: {engine}");
        }
    }

    /// A file, a path segment and a host keep their names whoever is
    /// running: they are real things on disk and on the network, and
    /// rewriting one produces a path that does not exist. This is the
    /// assertion that makes the substitution a word-boundary walk rather
    /// than a replace.
    #[test]
    fn a_name_that_is_not_the_program_is_left_alone() {
        for untouched in [
            "Resolve the merge conflict in pnpm-lock.yaml, then run pnpm import again.",
            "The pnpm-workspace.yaml is not read here",
            "ignoring .pnpmfile.cjs and .pnpmrc",
            "linked from node_modules/.pnpm/is-odd@1.0.0",
            "the store at ~/Library/pnpm/store/v11 is shared",
            "see https://pnpm.io/errors for the list",
        ] {
            let rewritten = rewrite_suggestions(untouched, "nub");
            for token in [
                "pnpm-lock.yaml",
                "pnpm-workspace.yaml",
                ".pnpmfile.cjs",
                ".pnpmrc",
                "node_modules/.pnpm/",
                "Library/pnpm/store",
                "pnpm.io",
            ] {
                assert!(
                    !untouched.contains(token) || rewritten.contains(token),
                    "{token} must survive verbatim: {rewritten}"
                );
            }
        }
        // ...and the one case that mixes both, which is the whole point: the
        // FILE keeps its name while the COMMAND beside it takes nub's. A
        // merge conflict is in that file under that name, so renaming it
        // would send the reader to a path that does not exist. (That a
        // nub-identity project is told about a `pnpm-lock.yaml` at all is a
        // separate defect, already recorded: the filenames inside these
        // diagnostics want `lockfile_basename` threaded through on the fork.)
        assert_eq!(
            rewrite_suggestions(
                "Resolve the conflict in pnpm-lock.yaml, then run pnpm import.",
                "nub"
            ),
            "Resolve the conflict in pnpm-lock.yaml, then run nub import."
        );
    }

    /// A verb nub spells differently is respelled, not merely renamed:
    /// `nub self-update` does not exist and `nub update` updates
    /// dependencies, so the program name alone would be wrong advice.
    #[test]
    fn a_verb_nub_spells_differently_is_respelled() {
        assert_eq!(
            rewrite_suggestions(r#"run "pnpm self-update latest" to downgrade."#, "nub"),
            r#"run "nub upgrade latest" to downgrade."#
        );
    }

    /// The conversion drops a rule the engine cannot parse rather than failing
    /// an install, so a shape the engine stops accepting after a pin move would
    /// otherwise shrink nub's database with no error anywhere.
    #[test]
    fn every_bundled_compatibility_rule_reaches_the_engine() {
        let bundled = super::super::compat_db::bundled_package_extensions();
        assert!(
            !bundled.is_empty(),
            "the bundled database parsed to nothing"
        );
        let dropped: Vec<&String> = bundled
            .iter()
            .filter(|(_, body)| {
                serde_json::from_value::<pnpm_config::PackageExtension>((*body).clone()).is_err()
            })
            .map(|(selector, _)| selector)
            .collect();
        assert!(
            dropped.is_empty(),
            "rules the engine cannot read: {dropped:?}"
        );
        assert_eq!(host_compat_rules().len(), bundled.len());
    }
}
