//! The pnpm 12 engine, embedded in-process.
//!
//! This is the engine a PM command runs on: the project's identity chooses how
//! it is configured, and `NUB_PM_ENGINE` exists only to pin that choice for a
//! differential. nub's PM grammar is pnpm's, so `nub install …` is
//! `pnpm install …` to the engine's parser.
//!
//! The two forced values exist because a differential needs them: `pnpm` runs
//! the engine under pnpm's own rules on a fixture nub would claim, and
//! `pnpm-nub` under nub's (`nub.lock`, `node_modules/.store`, and the settings
//! nub resolves) on one pnpm would. Any other value is ignored.

use super::host_settings;
use super::project_identity::{self, ProjectIdentity};
use anyhow::Result;
use pnpm_config::Embedder;
use std::path::{Path, PathBuf};

/// nub's naming for the files and directories the engine owns.
pub(super) const NUB: Embedder = Embedder {
    program_name: "nub",
    program_version: env!("CARGO_PKG_VERSION"),
    // nub provisions Node and pins its own version; the engine must not act
    // on a packageManager pin or a devEngines.runtime entry on its behalf.
    manage_package_manager_versions: false,
    manage_runtimes: false,
    // nub-incumbent projects declare their members in `package.json`, the
    // neutral spelling every package manager reads. `writes_settings_file`
    // below is what keeps the engine from writing the pnpm file this opts
    // out of reading.
    workspaces_from_package_manifest: true,
    lockfile_basename: "nub.lock",
    // Read-only, and paired with the retirement below: a project last
    // installed before the rename still holds `lock.yaml`, and the engine has
    // to resolve from it or every such project reports no lockfile at all.
    lockfile_legacy_basenames: &[LEGACY_LOCKFILE],
    virtual_store_dirname: super::PROJECT_VIRTUAL_STORE_LEAF,
    // What a diagnostic tells the user to edit when a setting has to change by
    // hand — the `minimumReleaseAgeExclude` prompts above all. The engine's own
    // answer is `pnpm-workspace.yaml`, which a nub project never reads, so the
    // advice would name a file that changes nothing.
    settings_file_display_name: "nub.jsonc",
    // The build allow-list a nub project edits is the `package.json` field
    // `record_allow_scripts` writes and `host_settings` reads back, so a
    // diagnostic telling the user to approve a build has to name that one.
    // The engine's own answer is `allowBuilds`, which nothing here reads.
    allow_builds_display_name: super::host_settings::ALLOW_SCRIPTS_FIELD,
    // Hidden entries nub writes into a modules directory beside the engine's
    // own, so `clean` empties `node_modules` instead of leaving a tree that
    // still looks installed: the engine stamp, and the preload chainer dir
    // `prepare_preload_chain` writes when the project configures a preload.
    hidden_modules_dir_entries: &[".nub-engine", ".nub"],
    // Filled in by `profile` from the running executable: a `const` cannot
    // name a path only this process knows. Without it a git-hosted dependency
    // that pins a package manager is prepared with whatever the machine has.
    pnpm_execpath: None,
    // The shims the lifecycle augmentation puts in front of scripts. Supplied
    // here rather than on this process's `PATH`, because the directory is
    // named per run and the engine hashes `PATH` into its build-cache key.
    script_bin_dir: Some(engine_script_bin_dir),
    // A nub project's configuration is `nub.jsonc`, `package.json` and
    // `.npmrc`, never pnpm's files; `profile` supplies what nub resolved.
    reads_pnpm_config: false,
    // npm's environment is a nub project's too. The TLS keys have no setting
    // `profile` could carry, so without this `npm_config_strict_ssl` and
    // `npm_config_cafile` reached nothing.
    reads_npm_config_env: true,
    // The write-side twin. Several engine paths record a decision by merging
    // it into `pnpm-workspace.yaml` — the `minimumReleaseAgeExclude` entries
    // an approved install persists, resolved catalog entries, the
    // `allowBuilds` lines an install scaffolds, `add --config`'s
    // `configDependencies`. nub reads none of them back, so each one would
    // leave a file nub does not own recording a decision nothing applies.
    // Where the entry is what lets the run proceed the engine now refuses and
    // names `nub.jsonc`; the advisory writes are simply not made.
    writes_settings_file: false,
    workspace_settings: None,
    compat_package_extensions: None,
    allow_builds_writer: Some(record_allow_scripts),
    // A nub project keeps its overrides in `package.json`, which it reads, so
    // `link` records them there instead of refusing.
    overrides_writer: Some(record_overrides),
    // `patchedDependencies` lives in `package.json` too, so `patch-commit` and
    // `patch-remove` record there instead of refusing.
    patched_dependencies_writer: Some(record_patched_dependencies),
    // The Node a run resolves is not known here; `run` fills it in.
    node_execpath: None,
    extract_observer: Some(super::phantom_hooks::extract_observer),
    materialize_policy: Some(super::phantom_hooks::materialize_policy),
    // nub is not finished when a dlx child is: `nubx` records the run-consent
    // ledger after the tool returns, so the engine exiting in the child's place
    // would skip it. A failed child comes back as `ERR_PNPM_DLX_CHILD_FAILED`
    // carrying the code, which stays distinguishable from failing to FETCH the
    // tool — the distinction that ledger is gated on.
    dlx_exits_like_child: false,
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

/// The profile `nubx` and `dlx` fetch a tool under: nub's, carrying nub's
/// defaults and the environment's settings, and none of the project's.
///
/// The engine reads a variable's registry, proxy and TLS keys itself; every
/// other setting one names, such as `npm_config_fetch_retries`, reaches the
/// fetch only through these.
pub(super) fn dlx_profile() -> Result<Embedder> {
    publish_host_settings(host_settings::fetch_settings()?);
    Ok(Embedder {
        workspace_settings: Some(host_workspace_settings),
        ..NUB
    })
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

/// Record the overrides a `link` implies where a nub project reads them back:
/// the neutral `overrides` field of its `package.json`.
///
/// A link is only a link because an override points the dependency at the
/// local directory. The engine would otherwise merge that override into
/// `pnpm-workspace.yaml`, which a nub project reads nothing from — so the next
/// install would resolve the dependency to the registry copy, and the file
/// itself would make the project read as pnpm's. Without a writer the engine
/// refuses to link at all, which is the right default and the wrong answer
/// here, because nub already reads this field.
///
/// An entry REPLACES the selector it names and leaves the rest alone: the user
/// is linking one package at a time.
fn record_overrides(dir: &std::path::Path, entries: &[(&str, &str)]) -> std::io::Result<()> {
    nub_core::pm::resolve::edit_root_manifest(dir, |manifest| {
        // Edited in place where the field already exists, so a link moves
        // nothing else in the file; a malformed value is replaced, since the
        // engine could not have read it either.
        if let Some(serde_json::Value::Object(pins)) =
            manifest.get_mut(host_settings::OVERRIDES_FIELD)
        {
            pin(pins, entries);
            return;
        }
        let mut pins = serde_json::Map::new();
        pin(&mut pins, entries);
        manifest.insert(
            host_settings::OVERRIDES_FIELD.to_owned(),
            serde_json::Value::Object(pins),
        );
    })
    .map_err(std::io::Error::other)?;
    Ok(())
}

/// `package.json`'s key for the patches a nub project applies.
const PATCHED_DEPENDENCIES_FIELD: &str = "patchedDependencies";

/// Record a `patch-commit` or `patch-remove` where a nub project reads it
/// back: the neutral `patchedDependencies` field of its `package.json`.
///
/// The engine still writes and deletes the patch files; this records which
/// selector each one applies to. An edit naming a file replaces the selector's
/// entry, and one naming none drops it, leaving the rest alone.
fn record_patched_dependencies(
    dir: &std::path::Path,
    edits: &[(&str, Option<&str>)],
) -> std::io::Result<()> {
    nub_core::pm::resolve::edit_root_manifest(dir, |manifest| {
        if let Some(serde_json::Value::Object(patched)) =
            manifest.get_mut(PATCHED_DEPENDENCIES_FIELD)
        {
            apply_patch_edits(patched, edits);
            if patched.is_empty() {
                manifest.shift_remove(PATCHED_DEPENDENCIES_FIELD);
            }
            return;
        }
        let mut patched = serde_json::Map::new();
        apply_patch_edits(&mut patched, edits);
        if !patched.is_empty() {
            manifest.insert(
                PATCHED_DEPENDENCIES_FIELD.to_owned(),
                serde_json::Value::Object(patched),
            );
        }
    })
    .map_err(std::io::Error::other)?;
    republish_patched_dependencies(edits);
    Ok(())
}

/// Fold a patch edit into what this process answers with. Both commands
/// install straight after recording it, on a fresh configuration, which would
/// otherwise apply the patches as they stood before the edit.
fn republish_patched_dependencies(edits: &[(&str, Option<&str>)]) {
    let mut slot = HOST_SETTINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut settings = slot.map_or_else(Default::default, Clone::clone);
    let patched = settings
        .patched_dependencies
        .get_or_insert_with(Default::default);
    for (selector, file) in edits {
        match file {
            Some(file) => {
                patched.insert((*selector).to_owned(), (*file).to_owned());
            }
            None => {
                patched.shift_remove(*selector);
            }
        }
    }
    if patched.is_empty() {
        settings.patched_dependencies = None;
    }
    *slot = Some(Box::leak(Box::new(settings)));
}

/// Apply each patch edit to the field: a file records the selector, none drops it.
fn apply_patch_edits(
    patched: &mut serde_json::Map<String, serde_json::Value>,
    edits: &[(&str, Option<&str>)],
) {
    for (selector, file) in edits {
        match file {
            Some(file) => {
                patched.insert(
                    (*selector).to_owned(),
                    serde_json::Value::String((*file).to_owned()),
                );
            }
            None => {
                patched.shift_remove(*selector);
            }
        }
    }
}

/// Apply each override, replacing whatever the field said about that selector.
fn pin(pins: &mut serde_json::Map<String, serde_json::Value>, entries: &[(&str, &str)]) {
    for (selector, specifier) in entries {
        pins.insert(
            (*selector).to_owned(),
            serde_json::Value::String((*specifier).to_owned()),
        );
    }
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

/// The identity this invocation runs the engine under.
///
/// The engine is the only engine, so this no longer chooses one: it only says
/// whether an identity was forced. An unrecognised value is `Auto` rather than
/// an error, because the variable is an internal testing affordance and a
/// typo in it should not change which project identity is detected.
fn selection() -> Selection {
    let Some(asked) = std::env::var_os("NUB_PM_ENGINE") else {
        return Selection::Auto;
    };
    match asked.to_str() {
        Some("pnpm") => Selection::Forced(ProjectIdentity::Pnpm),
        Some("pnpm-nub") => Selection::Forced(ProjectIdentity::Nub),
        _ => Selection::Auto,
    }
}

/// The identity a command line runs under in the project at `cwd`.
fn identity_of(selection: Selection, cwd: &Path) -> ProjectIdentity {
    match selection {
        Selection::Forced(identity) => identity,
        Selection::Auto => project_identity::detect(cwd),
    }
}

/// Whether the engine would run `argv` under pnpm's own profile.
///
/// For the front door, which has to settle a verb both grammars name before
/// either one parses it. Answered by the rule [`profile`] applies, in the
/// directory [`run`] would anchor at.
pub(super) fn runs_as_pnpm(argv: &[std::ffi::OsString]) -> bool {
    host_base_dir(argv).is_ok_and(|cwd| identity_of(selection(), &cwd) == ProjectIdentity::Pnpm)
}

/// The profile this invocation runs the engine under.
///
/// This is also where a configuration that cannot be honoured is refused,
/// because it is the first point at which both the identity and nub's own
/// config file are in hand.
///
/// `clean_install` is `nub ci` under any of its names: its tree is the one a
/// deploy copies, so a nub project's install keeps the store inside it.
fn profile(selection: Selection, cwd: &Path, clean_install: bool) -> Result<Embedder> {
    let identity = identity_of(selection, cwd);
    let loaded = crate::project_config::load_project_config(cwd)?;
    if let Some(loaded) = &loaded
        && let Some(path) = loaded.source.path.as_deref()
    {
        project_identity::check_install_block(identity, path, &loaded.values.install)?;
    }
    Ok(match identity {
        // `pnpm_execpath` even here: pnpm prepares such a dependency with its
        // own executable, and under nub that executable is nub's. Everything
        // else stays pnpm's own profile.
        ProjectIdentity::Pnpm => Embedder {
            pnpm_execpath: nub_execpath(),
            ..Embedder::PNPM
        },
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
            publish_host_settings(host_settings::resolve(cwd, &install, clean_install)?);
            warn_about_a_stray_workspace_yaml(cwd);
            Embedder {
                // Same reason as the pnpm arm above: the running executable is nub's.
                pnpm_execpath: nub_execpath(),
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
///
/// `compat` is false for every install: PM verbs run augmented and there is no
/// `--node` lifecycle path. It is a parameter only because `nubx --node <tool>`
/// runs a TRANSIENT session through the same prologue, and there the flag is
/// the user's.
pub(super) fn session_prologue(cwd: &Path, compat: bool) -> Result<()> {
    crate::cli::initialize_config_snapshot_at(cwd, false, false)?;
    // macOS leaves the soft descriptor limit at 256, which a large
    // concurrent install exhausts with `Too many open files`.
    nub_core::resource_limits::raise_nofile_limit();
    apply_lifecycle_augmentation(cwd, compat)
}

/// The Node the lifecycle augmentation resolved for this run, for the install
/// stamp to name. Set once, before the engine runs.
static LIFECYCLE_NODE_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The same Node's executable, for the engine to hand every script it spawns
/// as `NODE` and `npm_node_execpath`. Left unset, the engine records the first
/// `node` on `PATH` — the shim the augmentation below puts there, in a
/// temporary directory that is gone once the run ends, and not a Node
/// installation node-gyp can take headers from.
static LIFECYCLE_NODE_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The Node this run's scripts run under, once the session prologue resolved one.
pub(super) fn lifecycle_node_execpath() -> Option<&'static Path> {
    LIFECYCLE_NODE_PATH.get().map(PathBuf::as_path)
}

/// The directory of nub's per-run shims, handed to the engine so it can put
/// them on a spawned script's `PATH`.
///
/// It used to go on THIS process's `PATH` instead, which reached the same
/// scripts and also moved every cache key the engine derives from the
/// environment: the build pipeline hashes `PATH`, and the directory carries a
/// pid and a nonce, so the Cargo cache was written and never restored.
static SCRIPT_BIN_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// What [`Embedder::script_bin_dir`] answers with. `None` until the session
/// prologue has created the directory, which the engine allows for.
fn engine_script_bin_dir() -> Option<&'static Path> {
    SCRIPT_BIN_DIR.get().map(PathBuf::as_path)
}

/// nub's own executable, for the engine to prepare a git-hosted dependency
/// that pins a package manager. pnpm answers this with itself when the
/// running program is `pnpm`; under nub the running program is nub.
fn nub_execpath() -> Option<&'static Path> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| nub_core::node::spawn::current_nub_binary().ok())
        .as_deref()
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
fn apply_lifecycle_augmentation(cwd: &Path, compat: bool) -> Result<()> {
    let anchor = super::lifecycle_node_anchor(cwd);
    let discovered = nub_core::node::discovery::discover_node(&anchor);
    // A dependency's script runs from the dependency's own directory, which the
    // global virtual store puts outside the project: name the project whose pins
    // a `node` launched there resolves against.
    unsafe { std::env::set_var(nub_core::node::discovery::LIFECYCLE_PROJECT_ENV, &anchor) };
    let Ok(nub_binary) = nub_core::node::spawn::current_nub_binary() else {
        return Ok(());
    };
    stamp_node_gyp(cwd, &nub_binary);
    let node = discovered.unwrap_or_else(|_| nub_core::node::discovery::ResolvedNode::fallback());
    // The engine keys its build artifacts to whichever Node the lifecycle
    // scripts run under, so the install stamp has to name THIS one. Published
    // here rather than re-discovered at record time, which could name a
    // different Node and stamp a lie.
    let _ = LIFECYCLE_NODE_VERSION.set(node.version.to_string());
    // The fallback names no file, only the bare word `node`.
    if node.path.is_absolute() {
        let _ = LIFECYCLE_NODE_PATH.set(node.path.clone().into_std_path_buf());
    }
    let mut runtime = crate::project_config::runtime_config()?;
    let runtime_node_options = crate::cli::lifecycle_node_options(&mut runtime, &node)?;
    let runtime_json = crate::cli::runtime_config_json(&runtime)?;
    let pnp_ctx = nub_core::pnp::detect(cwd);
    // False for every install — PM verbs run augmented and there is no
    // `--node` lifecycle path. A transient `nubx --node <tool>` is the one
    // caller that passes true, and it has to: the fetched bin's own `node`
    // shebang re-enters nub through the shim this overlay installs, so
    // computing the overlay as augmented would hand the tool augmentation the
    // user asked not to have.
    let Some(mut aug) = nub_core::node::spawn::compute_augmentation_env(
        &nub_binary,
        node.version.clone(),
        compat,
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
    // The engine puts this on each script's own `PATH` through
    // `Embedder::script_bin_dir`. Putting it on this process's `PATH` reached
    // the same scripts, but the directory is named per run, and the engine
    // hashes `PATH` into the build pipeline's Cargo cache key — so the cache
    // was written on every run and restored on none.
    if let Some(shim_dir) = path_prepends.into_iter().next() {
        let _ = SCRIPT_BIN_DIR.set(shim_dir);
    }
    Ok(())
}

/// Point every lifecycle script this session spawns at a runnable node-gyp.
///
/// npm and pnpm both bundle node-gyp with themselves and hand it to a build
/// script through two channels — `npm_config_node_gyp`, and a `node-gyp-bin`
/// directory prepended to `PATH`. This engine supplies neither under nub: it
/// passes `node_gyp_path: None` at every call site, and its `PATH` channel comes
/// from `pnpm_executor::bundled_node_gyp_bin`, which looks for a
/// `dist/node-gyp-bin` beside the running executable — a layout pnpm's npm
/// package has and nub's binary does not. So a dependency with a native addon
/// could not build on a machine without a global node-gyp.
///
/// Both channels are nub's lazy shims, which cost a couple of small file writes
/// and bootstrap the real node-gyp only if something invokes them
/// ([`super::node_gyp`]). Stamped on the process environment, which is what the
/// engine builds a script's environment from — the same seam the augmentation
/// above uses. The `PATH` entry goes on before the augmentation's own prepend,
/// so nub's Node shim still fronts it, and the engine puts every
/// `node_modules/.bin` ahead of the inherited `PATH` regardless.
///
/// An ambient `npm_config_node_gyp` is left alone: a value the user set (or one
/// inherited from an outer nub that already stamped it) is the answer, exactly
/// as on the `nub run` path, where this is stamped before the user's own npm
/// config is applied over it. The `PATH` channel stands down on its own terms —
/// see [`node_gyp::lazy_shim_bin_dir`].
fn stamp_node_gyp(cwd: &Path, nub_binary: &Path) {
    use super::node_gyp;

    // Failure to write the shims degrades to leaving this path exactly where it
    // already was, which is why neither half is fatal.
    if std::env::var_os(node_gyp::CONFIG_ENV).is_none()
        && std::env::var_os(node_gyp::CONFIG_ENV.to_ascii_uppercase()).is_none()
        && let Ok(node_gyp_js) = node_gyp::lazy_js_shim_path()
    {
        unsafe {
            std::env::set_var(node_gyp::CONFIG_ENV, node_gyp_js);
            std::env::set_var(node_gyp::EXE_ENV, nub_binary);
            std::env::set_var(node_gyp::PROJECT_DIR_ENV, cwd);
        }
    }

    let project_bin_dir = super::lifecycle_node_anchor(cwd)
        .join("node_modules")
        .join(".bin");
    if let Ok(Some(shim_dir)) = node_gyp::lazy_shim_bin_dir(&project_bin_dir) {
        let mut entries = vec![shim_dir];
        if let Some(existing) = std::env::var_os("PATH") {
            entries.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(entries) {
            unsafe { std::env::set_var("PATH", joined) };
        }
    }
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
pub(crate) fn run(mut argv: Vec<std::ffi::OsString>) -> Result<i32> {
    let cwd = host_base_dir(&argv)?;
    // The engine's grammar names the command, so `ci`'s aliases arrive as `ci`.
    let command = pnpm_cli::command_name(&argv);
    let clean_install = command.as_deref() == Some("ci");
    let mut embedder = profile(selection(), &cwd, clean_install)?;
    session_prologue(&cwd, false)?;
    // In a pnpm project too: nub resolves the Node either way, and pnpm hands
    // scripts a real Node rather than a shim.
    embedder.node_execpath = lifecycle_node_execpath();
    // The engine's own entry point installs this before it can print. It
    // drops each cause the level above already states in full, so a host
    // that leaves miette at its default renders chains the engine collapses
    // — a divergence invisible on a one-level diagnostic and plain on a
    // deep one.
    pnpm_diagnostics::install_report_handler();
    // A few flags nub reads past pnpm's grammar, in its own projects only: a
    // pnpm-incumbent command line reaches the engine exactly as pnpm reads it.
    if embedder.program_name != Embedder::PNPM.program_name
        && let Some(name) = command.as_deref()
    {
        if matches!(name, "install" | "add" | "update") {
            super::platform_flags::expand_engine_argv(&mut argv).warn();
        }
        if RESOLVING_COMMANDS.contains(&name) {
            argv = super::min_release_age::engine_argv(argv)?;
        }
    }
    // Asked BEFORE the run, because a successful install answers it: it
    // writes nub's own lockfile, and the project then looks migrated.
    let pending = pending_migration(embedder, command.as_deref(), &cwd);
    // Asked before the run for the same reason, and it is the stronger case:
    // the install is about to write nub's lockfile, after which no project
    // still looks virgin.
    let stamp = project_is_virgin(embedder, command.as_deref(), &cwd);
    // Asked here too, while `command` is still in hand: the stamp below records
    // that this tree was materialized, so it rides the resolving verbs.
    let records_install_engine = embedder.program_name != Embedder::PNPM.program_name
        && command
            .as_deref()
            .is_some_and(|name| RESOLVING_COMMANDS.contains(&name));
    // Also asked before, and for a third reason: the answer is a COMPARISON
    // against the lockfile as it stands now.
    let legacy = legacy_lockfile_pending(embedder, command.as_deref(), &cwd);
    report_resolved_layout(embedder, command.as_deref(), &argv, &cwd);
    match pnpm_cli::run(argv, embedder) {
        Ok(()) => {
            if let Some(foreign) = pending {
                eprintln!("{}", super::migrate::migration_hint(&foreign));
            }
            if stamp {
                super::install_family::stamp_virgin_dev_engines(&cwd);
            }
            // Which Node this tree's native addons were built for, so a later
            // run under a different Node major can notice and reinstall
            // (`verify_deps`). Nub identity only: the stamp is nub's own file
            // and a pnpm-incumbent tree must look exactly as pnpm left it.
            if records_install_engine {
                crate::install_engine::record_for(
                    &cwd,
                    0,
                    LIFECYCLE_NODE_VERSION.get().map(String::as_str),
                );
                // Vite realpath-checks every module it serves against
                // `server.fs.allow`, which defaults to the workspace root — and
                // under the global virtual store a dependency's realpath is the
                // machine-global store, outside it. Vite 8.1 reads the store's
                // location out of `.modules.yaml` itself, which the engine now
                // writes; below that it has to be told, so the ejected copy is
                // patched here. Same identity gate as the stamp: pnpm's virtual
                // store is project-local, so a pnpm-incumbent tree never has the
                // problem and must not be touched.
                super::vite_compat::apply(&cwd);
            }
            if let Some(pending) = legacy {
                pending.retire(embedder, &cwd);
            }
            Ok(0)
        }
        Err(report) => {
            report_engine_error(&report, embedder);
            // The same hint the success arm prints, because a FAILURE is where
            // it matters most: `ci` is headless, so in a repo carrying only
            // another package manager's lockfile it cannot do anything but
            // fail, and the migration is the whole remedy. Printing it only on
            // success meant the one verb that can never succeed there was also
            // the one that never said why. After the error, so it reads as the
            // way out rather than as part of the diagnosis; `pending` is
            // already `None` under pnpm identity, whose stderr must stay
            // byte-identical to real pnpm's.
            if let Some(foreign) = pending {
                eprintln!("{}", super::migrate::migration_hint(&foreign));
            }
            Ok(1)
        }
    }
}

/// Render a failing engine report to stderr, the one way every engine call
/// site has to render one.
///
/// Shared rather than inlined because the alternative was measured: the `nubx`
/// dlx path reported through a text rewrite instead, which knew nothing of
/// this engine's codes, so a resolution failure reached the user spelled
/// `ERR_PNPM_*`.
///
/// Three things have to happen together. The engine skips its own render for a
/// command that has already printed its report and answers for which those are,
/// so nothing here tracks that list across a pin move. The `Error: ` prefix is
/// the engine's own, not decoration: without it a pnpm-incumbent project's
/// stderr differs from real pnpm's on every failure. And under any other
/// identity the rendered text is rebranded, because the engine bakes ~800
/// `ERR_PNPM_*` codes into compile-time attributes that no runtime setting can
/// reach.
pub(super) fn report_engine_error(report: &miette::Report, embedder: Embedder) {
    if pnpm_cli::is_reported_error(report) {
        return;
    }
    let rendered = format!("Error: {report:?}");
    if embedder.program_name == Embedder::PNPM.program_name {
        eprintln!("{rendered}");
    } else {
        eprintln!("{}", rebrand(&rendered, embedder));
    }
}

/// An install driven through the engine's front door, for nub's own callers.
///
/// `nub init` scaffolds a project and then installs it, and a Node-major change
/// reinstalls so native addons are rebuilt for it. Neither arrives as a command
/// line, so neither passes the argv routing that hands every other PM verb to
/// the engine — they called the host install path directly, which is what kept
/// them on the other engine after that routing became unconditional. Going
/// through [`run`] gives them exactly what `nub install` gets.
///
/// `--dir` rather than a chdir, because a caller may still need its own cwd
/// afterwards; it is the same way `pm migrate` names the project.
pub(crate) fn engine_install(dir: Option<&std::path::Path>) -> Result<i32> {
    let mut argv = vec![std::ffi::OsString::from("nub")];
    if let Some(dir) = dir {
        argv.push(std::ffi::OsString::from("--dir"));
        argv.push(std::ffi::OsString::from(dir));
    }
    argv.push(std::ffi::OsString::from("install"));
    run(argv)
}

/// Print the install report's resolved-layout header ahead of the engine's
/// progress display, where the vendored engine's own install path prints it.
///
/// A pnpm project gets none of it. pnpm prints no such header, and each row
/// names where a nub project's layout comes from, so under pnpm the block
/// reported sources that install never read.
///
/// Only the second half of that report can follow the engine here. The
/// materialization digest is fed by `record_plan`, which the VENDORED engine's
/// disk-materialize expansion hook fills from the resolved graph; this engine
/// decides materialization per package as each one lands
/// ([`super::phantom_hooks::materialize_policy`]) and builds no plan, so a
/// digest printed after the run would be empty on every install rather than
/// merely quiet on most. It waits for a plan to report.
///
/// The flags are read off the command line rather than a parse: this engine
/// owns the grammar and hands the parse back to nobody. Both readers admit
/// only what the settings table declares, so neither can mistake a host flag
/// for a setting.
fn report_resolved_layout(
    embedder: Embedder,
    command: Option<&str>,
    argv: &[std::ffi::OsString],
    cwd: &Path,
) {
    if embedder.program_name == Embedder::PNPM.program_name
        || !command.is_some_and(|name| RESOLVING_COMMANDS.contains(&name))
    {
        return;
    }
    super::install_report::print_resolved_layout(
        cwd,
        &output_flags(argv),
        &super::install_report::cli_setting_flags(argv),
        command == Some("ci"),
    );
}

/// What this command line says about output verbosity, in nub's own shape.
///
/// nub's pre-verb globals (`nub --silent install`) are already recorded as
/// process defaults that [`OutputFlags`] merges under whatever is set here, so
/// only the post-verb spellings need scanning — the same three the engine
/// itself accepts.
fn output_flags(argv: &[std::ffi::OsString]) -> super::output::OutputFlags {
    use super::output::{LogLevel, OutputFlags, Reporter};
    let mut flags = OutputFlags::default();
    let mut args = argv.iter().filter_map(|arg| arg.to_str()).peekable();
    while let Some(arg) = args.next() {
        let mut value_of = |inline: Option<&str>| match inline {
            Some(value) => Some(value.to_owned()),
            None => args.next().map(str::to_owned),
        };
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg, None),
        };
        match name {
            "--silent" | "-s" => flags.silent = true,
            "--reporter" => {
                flags.reporter = match value_of(inline).as_deref() {
                    Some("silent") => Some(Reporter::Silent),
                    Some("append-only") => Some(Reporter::AppendOnly),
                    Some("default") => Some(Reporter::Default),
                    _ => flags.reporter,
                }
            }
            "--loglevel" => {
                flags.loglevel = match value_of(inline).as_deref() {
                    Some("silent") => Some(LogLevel::Silent),
                    Some("error") => Some(LogLevel::Error),
                    Some("warn") => Some(LogLevel::Warn),
                    Some("info") => Some(LogLevel::Info),
                    Some("debug") => Some(LogLevel::Debug),
                    _ => flags.loglevel,
                }
            }
            _ => {}
        }
    }
    flags
}

/// The commands that resolve the project and write its lockfile. A project
/// still holding another package manager's lockfile has just had it ignored,
/// so this is where saying so belongs — not on a command that only reads.
const RESOLVING_COMMANDS: [&str; 6] = ["install", "add", "remove", "update", "ci", "dedupe"];

/// nub's PRIOR lockfile name, still read during the rename transition and
/// retired the next time a real write lands.
///
/// Spelled here rather than borrowed from the previous engine's alignment
/// module, which carries it only so that engine's writer can honor it: that
/// writer retired the file through a `lockfile_legacy_basenames` profile
/// field, and the pnpm engine has no equivalent, so under the engine the
/// retirement is the host's job and belongs beside the host's other
/// after-the-run work.
const LEGACY_LOCKFILE: &str = "lock.yaml";

/// A legacy lockfile this command might retire, captured before the run.
struct LegacyLockfile {
    path: PathBuf,
    /// nub's current lockfile bytes, or `None` when it does not exist yet.
    wanted_before: Option<Vec<u8>>,
}

impl LegacyLockfile {
    /// Retire the legacy name, but only if the run really rewrote the lockfile.
    ///
    /// The comparison IS the rule, and it is what the three no-write cases
    /// need: a no-op install resolves to the same graph and writes nothing, a
    /// `--frozen-lockfile` op refuses to write by definition, and `ci` never
    /// writes at all — none of them may take the legacy file with them, and
    /// none of them can be told apart from a real install by its name alone.
    fn retire(self, embedder: Embedder, cwd: &Path) {
        let Ok(after) = std::fs::read(cwd.join(embedder.lockfile_basename)) else {
            return;
        };
        if self.wanted_before.is_none_or(|before| before != after) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The legacy lockfile sitting beside nub's own, when this command could
/// retire it.
///
/// Nothing to do under pnpm's own incumbency: `lock.yaml` is nub's prior name
/// and a pnpm project's lockfile is pnpm's, which nub must leave exactly where
/// it found it.
fn legacy_lockfile_pending(
    embedder: Embedder,
    command: Option<&str>,
    cwd: &Path,
) -> Option<LegacyLockfile> {
    if embedder.program_name == Embedder::PNPM.program_name
        || !command.is_some_and(|name| RESOLVING_COMMANDS.contains(&name))
    {
        return None;
    }
    let path = cwd.join(LEGACY_LOCKFILE);
    path.is_file().then(|| LegacyLockfile {
        wanted_before: std::fs::read(cwd.join(embedder.lockfile_basename)).ok(),
        path,
    })
}

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
