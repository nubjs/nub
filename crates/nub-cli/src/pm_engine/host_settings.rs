//! The settings a nub-incumbent project hands the engine.
//!
//! Under nub's profile the engine reads none of pnpm's own configuration, so
//! what a pnpm project keeps in `pnpm-workspace.yaml` reaches the engine from
//! here instead: one [`WorkspaceSettings`], applied where the yaml would be.
//! Its sources are the ones a nub project has always had, lowest precedence
//! first, in the order the previous engine resolved them:
//!
//! 1. nub's defaults: the global virtual store outside CI, a strict maturity
//!    floor, and the store and cache under nub's own cache directory;
//! 2. `.npmrc`, the user's file before the project's;
//! 3. `nub.jsonc`'s `install` block, the curated keys before `install.settings`;
//! 4. `npm_config_*` variables, then `NUB_CACHE_DIR`.
//!
//! The neutral `package.json` fields of the workspace root join them; nothing
//! else sets those, so they take no part in the order. Registry, credential,
//! proxy and TLS keys stay out on purpose: the engine reads them from `.npmrc`
//! and `npm_config_*` itself, under its own trust rules, and a copy in this
//! layer would outrank those rules.

use crate::project_config::{Hoist, InstallConfig, LinkerConfig};
use anyhow::{Context, Result, anyhow, bail};
use pnpm_config::WorkspaceSettings;
use pnpm_config::naming_cases::to_camel_case;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Everything the merge reads, gathered up front so the merge itself touches
/// neither the filesystem nor the process environment.
struct Sources<'a> {
    /// `.npmrc` files that exist, lowest precedence first, with their text.
    npmrc: Vec<(PathBuf, String)>,
    install: &'a InstallConfig,
    /// `npm_config_*` and `NUB_CACHE_DIR`, in environment order.
    env: Vec<(String, String)>,
    /// The workspace root's `package.json`, and the directory it was read
    /// from — a path a setting names is relative to that directory.
    manifest: Map<String, Value>,
    root: PathBuf,
    /// nub's cache directory, when one can be determined.
    cache_root: Option<PathBuf>,
    ci: bool,
}

/// A `.npmrc` value: `key=value`, or the repeated `key[]=value` list form.
pub(crate) enum Raw {
    Scalar(String),
    List(Vec<String>),
}

/// Read every source once, so the merge itself touches neither the filesystem
/// nor the environment and can be reasoned about as a pure function of what
/// was found.
fn gather<'a>(start_dir: &Path, install: &'a InstallConfig) -> Sources<'a> {
    let root = workspace_root(start_dir);
    let env = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| setting_key_of_var(name).is_some() || name == "NUB_CACHE_DIR")
        .collect();
    Sources {
        npmrc: npmrc_files(&root),
        install,
        env,
        manifest: read_manifest(&root),
        root,
        cache_root: nub_core::node::discovery::cache_dir(),
        ci: std::env::var_os("CI").is_some(),
    }
}

/// The settings for the project containing `start_dir`.
pub(crate) fn resolve(start_dir: &Path, install: &InstallConfig) -> Result<WorkspaceSettings> {
    let sources = gather(start_dir, install);
    refuse_legacy_root_allow_builds(&sources.manifest)?;
    announce_dropped_root_install_fields(&sources.manifest);
    let merged = merge(&sources)?;
    serde_json::from_value(Value::Object(merged))
        .context("nub could not hand its install settings to the package manager")
}

/// What `nub.jsonc` and the environment SUPPLY, spelled as engine settings.
///
/// `nub config get`/`list` report this tier rather than the merged map the
/// install is handed: that map has nub's own defaults folded in and carries
/// merge-internal keys the settings table does not name, so reporting it would
/// list a value nobody set under a name `config set` cannot write. Reading the
/// two sources directly keeps the reporting surface to what a user actually
/// put somewhere.
pub(crate) fn supplied_settings(install: &InstallConfig) -> Map<String, Value> {
    let mut out = curated(install).unwrap_or_default();
    for (key, value) in install.settings.iter().flatten() {
        out.insert(key.clone(), value.clone());
    }
    out
}

/// The `npm_config_*` variables that name a setting, plus nub's own cache knob.
///
/// Its own tier, belonging to the merged view and to no file: `config get
/// --local` asks what the project's own file says, so an environment value there
/// would answer a question nobody asked. In the merged view it ranks highest,
/// and omitting it once made `config get cache-dir` print `undefined` while the
/// install was already acting on the value (nubjs/nub#654).
pub(crate) fn env_settings() -> Map<String, Value> {
    env_settings_sourced()
        .into_iter()
        .map(|(_, setting, raw)| (setting, Value::String(raw)))
        .collect()
}

/// The same tier, each entry still carrying the VARIABLE it came from.
///
/// The install report names that variable in its provenance parenthetical, and
/// a settings-keyed map has already thrown it away — `npm_config_node_linker`
/// and `NPM_CONFIG_NODE_LINKER` collapse to one `nodeLinker` entry there. In
/// environment order, so a later duplicate is the winner both readers see.
pub(crate) fn env_settings_sourced() -> Vec<(String, String, String)> {
    let known = known_keys();
    let mut out = Vec::new();
    for (name, value) in std::env::vars() {
        let Some(key) = setting_key_of_var(&name) else {
            continue;
        };
        // The same two steps [`lift`] takes, and for the same reason: the
        // variable's tail is snake_case (`npm_config_cache_dir` carries
        // `cache_dir`), which names no setting until it is camel-cased, and a
        // tail that still names none after that is somebody else's variable.
        // Skipping either step reported `cache_dir` as a free-form key and
        // left `config get cache-dir` answering `undefined` while the install
        // was already using the value.
        let setting = to_camel_case(key);
        if !known.contains(&setting) {
            continue;
        }
        out.push((name, setting, value));
    }
    if let Ok(dir) = std::env::var("NUB_CACHE_DIR")
        && !dir.is_empty()
    {
        out.push(("NUB_CACHE_DIR".to_owned(), "cacheDir".to_owned(), dir));
    }
    out
}

/// Refuse a manifest that still carries the OLD name for the build allowlist.
///
/// The list moved from a top-level `allowBuilds` map to `allowScripts`. Reading
/// neither would silently drop the old map's approvals AND its explicit `false`
/// denials, and the denials are the half that matters: proceeding would RUN a
/// script the project wrote down that it did not want run. So this refuses
/// rather than warns.
///
/// Only under nub's own identity, which is the only place this is reached. A
/// project whose incumbent is pnpm gets pnpm's behaviour exactly, and pnpm has
/// no opinion about a key at this position. `pnpm.allowBuilds` and a
/// `pnpm-workspace.yaml` block are pnpm's own surface and untouched.
fn refuse_legacy_root_allow_builds(manifest: &Map<String, Value>) -> Result<()> {
    if !matches!(manifest.get("allowBuilds"), Some(Value::Object(_))) {
        return Ok(());
    }
    bail!(
        "nub: package.json sets a top-level `allowBuilds` map — that field was renamed to \
         `allowScripts`, which is also the field npm reads. Rename the key in package.json; \
         the entries are unchanged. (`pnpm.allowBuilds` and a `pnpm-workspace.yaml` \
         `allowBuilds:` block are pnpm's own surface and still read as-is.) \
         [ERR_NUB_ALLOW_BUILDS_RENAMED]"
    )
}

/// Manifest-ROOT install keys nub used to read and no longer does, each with
/// the surface that replaces it. All three were nub's own — no package manager
/// reads a top-level `auditConfig`, `allowUnusedPatches` or
/// `allowNonAppliedPatches` — so nothing else will report them, and a key that
/// looks like it is doing something is worse than one that is plainly gone.
const DROPPED_ROOT_INSTALL_FIELDS: [(&str, &str); 3] = [
    (
        "auditConfig",
        "pass `nub audit --ignore <id>`, which takes advisory numbers, GHSA ids and CVE ids",
    ),
    (
        "allowUnusedPatches",
        "remove the `patchedDependencies` entry that matches no installed package",
    ),
    (
        "allowNonAppliedPatches",
        "remove the `patchedDependencies` entry that matches no installed package",
    ),
];

/// Say so when the manifest sets one of those, and say nothing when it does
/// not. A notice, never a failure: unlike the renamed allowlist above, none of
/// these can make an install do something the project did not ask for.
fn announce_dropped_root_install_fields(manifest: &Map<String, Value>) {
    for (key, remedy) in DROPPED_ROOT_INSTALL_FIELDS {
        if manifest.contains_key(key) {
            eprintln!(
                "nub: package.json sets a top-level `{key}` — no package manager reads that key \
                 there, and nub no longer does either. Instead, {remedy}."
            );
        }
    }
}

fn merge(sources: &Sources) -> Result<Map<String, Value>> {
    let known = known_keys();
    let mut merged = Map::new();

    for (path, text) in &sources.npmrc {
        for (key, raw) in npmrc_entries(text) {
            lift(&mut merged, &known, &key, raw, &path.display().to_string())?;
        }
    }

    merged.extend(curated(sources.install)?);
    let passthrough = sources.install.settings.as_ref();
    for (key, value) in passthrough.into_iter().flatten() {
        if !known.contains(key) {
            bail!("install.settings.{key} is not a setting pnpm-workspace.yaml accepts");
        }
        check(key, value).map_err(|error| anyhow!("install.settings.{key}: {error}"))?;
        merged.insert(key.clone(), value.clone());
    }

    for (name, value) in &sources.env {
        if let Some(key) = setting_key_of_var(name) {
            let source = format!("the {name} environment variable");
            lift(
                &mut merged,
                &known,
                key,
                Raw::Scalar(value.clone()),
                &source,
            )?;
        }
    }
    if let Some((_, dir)) = sources
        .env
        .iter()
        .rfind(|(name, _)| name == "NUB_CACHE_DIR")
        && !dir.is_empty()
    {
        merged.insert("cacheDir".to_owned(), Value::String(dir.clone()));
    }

    for (key, value) in manifest_settings(&sources.manifest, &sources.root) {
        if passthrough.is_some_and(|settings| settings.contains_key(&key)) {
            bail!(
                "install.settings.{key} sets the same thing as {} in package.json; keep one of the two",
                manifest_field(&key)
            );
        }
        merged.insert(key, value);
    }

    // nub's defaults fill only what no source set. The shared store is a
    // symlink layout, so it has nothing to say to a hoisted one.
    let isolated = merged
        .get("nodeLinker")
        .is_none_or(|linker| linker.as_str() == Some("isolated"));
    if !sources.ci
        && isolated
        && !merged.contains_key("enableGlobalVirtualStore")
        && !merged.contains_key("virtualStoreType")
    {
        merged.insert("enableGlobalVirtualStore".to_owned(), Value::Bool(true));
    }
    // The engine already applies a 24-hour maturity cutoff of its own, so the
    // minutes need no default here — but it applies that built-in one
    // NON-strictly, falling back to an immature version whenever no mature one
    // satisfies a range. It tells the two apart by whether `minimumReleaseAge`
    // was explicitly configured, which for a project that has configured
    // nothing it was not. Nub documents a real floor rather than an advisory
    // one, so the strict half is pinned here and the minutes are left to the
    // engine. A project that sets either half keeps what it set, and
    // `minimumReleaseAge: 0` still disables both.
    merged
        .entry("minimumReleaseAgeStrict")
        .or_insert(Value::Bool(true));
    // The other half of the same floor: a version the registry publishes no
    // date for. The engine admits it with a warning, so that a registry which
    // strips `time` cannot lock a user out. Nub documents the opposite —
    // "Blocked", under its own error — and documents the two ways out
    // (`minimumReleaseAgeExclude`, or turning the window off), so a package
    // nothing can date must not pass a gate that exists to date it. Also
    // `or_insert`: a project that wants the engine's answer says so.
    merged
        .entry("minimumReleaseAgeIgnoreMissingTime")
        .or_insert(Value::Bool(false));
    // The engine's update notifier checks the registry for a newer pnpm and
    // tells the user how to install it. nub ships the engine and updates it
    // through `nub upgrade`, so the advice would name a release nub does not
    // take and a command nub does not have.
    merged.entry("updateNotifier").or_insert(Value::Bool(false));
    // A sibling named by a plain semver range is the workspace member, not a
    // package of the same name on the registry. npm, yarn and bun all resolve
    // it that way, and a project that reached nub from any of them would
    // otherwise get a 404 for a package sitting in its own tree. The engine's
    // own default is the other one — it matches a workspace package only for a
    // `workspace:`-prefixed range — which is what a pnpm-incumbent project
    // keeps, because it never reaches this function. Direct dependencies only:
    // matching transitively would change what a DEPENDENCY resolves to.
    merged
        .entry("linkWorkspacePackages")
        .or_insert(Value::Bool(true));
    merged
        .entry("userAgent")
        .or_insert(Value::String(lifecycle_user_agent()));
    if let Some(cache_root) = &sources.cache_root {
        for (key, leaf) in [("storeDir", "store"), ("cacheDir", "pm")] {
            if !merged.contains_key(key) {
                let dir = cache_root.join(leaf).to_string_lossy().into_owned();
                merged.insert(key.to_owned(), Value::String(dir));
            }
        }
    }
    Ok(merged)
}

/// The `npm_config_user_agent` a lifecycle script sees under nub's identity.
///
/// Every build script in the tree reads this to decide which package manager
/// it is running under, so leaving the engine's default there tells a nub
/// project's own `postinstall` that pnpm is installing it. Only the leading
/// `name/version` token is nub's; the rest of the string — the `npm/?` and
/// `node/?` placeholders, the platform and the arch — is taken from the
/// engine's own builder rather than rebuilt here, so the two cannot drift on
/// the parts that are not about the brand.
///
/// A pnpm-incumbent project never reaches this function and keeps pnpm's
/// string verbatim, which is what makes a build script there see exactly what
/// it would under pnpm.
fn lifecycle_user_agent() -> String {
    let engine = pnpm_config::default_user_agent();
    let rest = engine.split_once(' ').map_or("", |(_, rest)| rest);
    format!("nub/{} {rest}", env!("CARGO_PKG_VERSION"))
}

/// `nub.jsonc`'s curated `install` keys, spelled as the engine's settings and
/// lowered the way the previous engine lowered them.
fn curated(install: &InstallConfig) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    match &install.linker {
        None => {}
        Some(LinkerConfig::Pnp) => bail!(
            "nub: `install.linker: \"pnp\"` is reserved and not supported yet [ERR_NUB_CONFIG_UNSUPPORTED]"
        ),
        Some(LinkerConfig::Hoisted) => {
            out.insert("nodeLinker".to_owned(), json!("hoisted"));
        }
        Some(LinkerConfig::Global { .. }) => {
            out.insert("nodeLinker".to_owned(), json!("isolated"));
            out.insert("enableGlobalVirtualStore".to_owned(), json!(true));
        }
        Some(LinkerConfig::Isolated { hoist }) => {
            out.insert("nodeLinker".to_owned(), json!("isolated"));
            out.insert("enableGlobalVirtualStore".to_owned(), json!(false));
            match hoist {
                None => {}
                Some(Hoist::Bool(enabled)) => {
                    out.insert("hoist".to_owned(), json!(enabled));
                    if *enabled {
                        out.insert("hoistPattern".to_owned(), json!(["*"]));
                    }
                }
                Some(Hoist::Patterns(patterns)) => {
                    out.insert("hoist".to_owned(), json!(true));
                    out.insert("hoistPattern".to_owned(), json!(patterns));
                }
            }
        }
    }
    if let Some(patterns) = &install.public_hoist {
        // Naming patterns narrows what is hoisted, so the blanket flag goes off
        // rather than staying at whatever a lower source set.
        out.insert("shamefullyHoist".to_owned(), json!(false));
        out.insert("publicHoistPattern".to_owned(), json!(patterns));
    }
    if let Some(age) = install.minimum_release_age {
        // Rounded up: a sub-minute remainder must not weaken the gate.
        out.insert(
            "minimumReleaseAge".to_owned(),
            json!(age.as_secs().div_ceil(60)),
        );
        out.insert("minimumReleaseAgeStrict".to_owned(), json!(true));
    }
    if let Some(exclude) = &install.minimum_release_age_exclude {
        out.insert("minimumReleaseAgeExclude".to_owned(), json!(exclude));
    }
    Ok(out)
}

/// The neutral `package.json` fields that are settings in pnpm's vocabulary.
fn manifest_settings(manifest: &Map<String, Value>, root: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    let mut overrides = Map::new();
    // Both spellings are honored, and `overrides` is read last so it wins.
    for field in ["resolutions", "overrides"] {
        if let Some(Value::Object(pins)) = manifest.get(field) {
            for (selector, spec) in pins {
                if spec.is_string() && !selector.is_empty() {
                    overrides.insert(selector.clone(), spec.clone());
                }
            }
        }
    }
    if !overrides.is_empty() {
        out.insert("overrides".to_owned(), Value::Object(overrides));
    }
    for field in [
        "packageExtensions",
        "patchedDependencies",
        "allowedDeprecatedVersions",
    ] {
        if let Some(value @ Value::Object(_)) = manifest.get(field) {
            out.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(Value::Object(workspaces)) = manifest.get("workspaces") {
        for field in ["catalog", "catalogs"] {
            if let Some(value @ Value::Object(_)) = workspaces.get(field) {
                out.insert(field.to_owned(), value.clone());
            }
        }
    }
    // What the project has decided may run scripts. nub spells it
    // `allowScripts`, which is the name `nub approve-builds` writes back;
    // the engine reads the same decisions under its own name.
    if let Some(Value::Object(decisions)) = manifest.get(ALLOW_SCRIPTS_FIELD) {
        let keyed = decisions
            .iter()
            .map(|(key, decision)| (engine_build_key(key, root), decision.clone()))
            .collect();
        out.insert("allowBuilds".to_owned(), Value::Object(keyed));
    }
    out
}

/// The protocols whose specifier is a PATH, and so has more than one
/// spelling for one directory.
const PATH_PROTOCOLS: [&str; 3] = ["file:", "link:", "portal:"];

/// The key the engine will look a build decision up under.
///
/// nub documents `allowScripts` as keying on the package name for a registry
/// dependency and on the full specifier for anything else, and a path
/// specifier has many spellings for one directory: `file:./dep`, `file:dep`
/// and `file:./sub/../dep` all name the same place. The engine writes exactly
/// one of them, so a decision spelled any other way matches nothing — and the
/// install then fails on a build the project explicitly approved, which is
/// the opposite of what the field is for.
///
/// The rule is the engine's own: resolve the path against the directory the
/// manifest was read from, collapse `.` and `..` the way Node's `path.resolve`
/// does — lexically, touching no disk, so a symlink is not followed — make it
/// relative again, and write it with forward slashes. Measured against the
/// engine on both shapes: a single project's `file:./dep` is keyed
/// `dep@file:dep`, and a workspace member's is keyed from the LOCKFILE dir
/// rather than the member, which is the same directory this resolves against
/// because a nub project carries its decisions in the root manifest.
///
/// Only the path is rewritten. Stripping to the bare package name would be
/// wrong rather than merely lossy: the engine keys a git, tarball or path
/// artifact on its whole identifier precisely so that a name on its own
/// cannot approve one. An absolute path and an unrecognized protocol are both
/// left exactly as written — the safe direction, since a key that matches
/// nothing withholds a permission where a wrong one would grant it.
fn engine_build_key(key: &str, root: &Path) -> String {
    let Some((name, protocol, path)) = PATH_PROTOCOLS.iter().find_map(|protocol| {
        let marker = format!("@{protocol}");
        let at = key.find(&marker).filter(|at| *at > 0)?;
        Some((&key[..at], *protocol, &key[at + marker.len()..]))
    }) else {
        return key.to_owned();
    };
    if path.is_empty() || Path::new(path).is_absolute() {
        return key.to_owned();
    }
    let resolved = lexically_resolve(root, path);
    let Some(relative) = pathdiff::diff_paths(&resolved, root) else {
        return key.to_owned();
    };
    let forward_slashed = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if forward_slashed.is_empty() {
        return key.to_owned();
    }
    format!("{name}@{protocol}{forward_slashed}")
}

/// `base` joined with `relative`, with `.` dropped and `..` popping a segment,
/// decided from the path text alone. This is what makes the answer agree with
/// the engine on a path whose parent does not exist yet, and what keeps a
/// symlinked directory keyed the way the project wrote it.
fn lexically_resolve(base: &Path, relative: &str) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in base.components().chain(Path::new(relative).components()) {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            // At the root there is nothing to pop, and Node's own resolve
            // stops there rather than escaping.
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(segment) => out.push(segment),
        }
    }
    out
}

/// The `package.json` field naming the dependencies whose install scripts may
/// run. nub's own spelling of what the engine calls `allowBuilds`, and the
/// file `approve-builds` writes under nub's identity — a nub project reads no
/// `pnpm-workspace.yaml`, so a decision recorded there would never be read
/// back.
pub(crate) const ALLOW_SCRIPTS_FIELD: &str = "allowScripts";

/// How a user wrote the `package.json` field behind a setting.
fn manifest_field(setting: &str) -> String {
    match setting {
        "overrides" => "`overrides` or `resolutions`".to_owned(),
        "catalog" | "catalogs" => format!("`workspaces.{setting}`"),
        "allowBuilds" => format!("`{ALLOW_SCRIPTS_FIELD}`"),
        other => format!("`{other}`"),
    }
}

/// Record one `.npmrc` or environment entry, if it names a setting this layer
/// carries. Keys pnpm does not know are npm's own and pass by silently; a
/// known key with a value pnpm would refuse is an error naming its source.
fn lift(
    merged: &mut Map<String, Value>,
    known: &BTreeSet<String>,
    key: &str,
    raw: Raw,
    source: &str,
) -> Result<()> {
    if read_by_engine(key) {
        return Ok(());
    }
    let setting = to_camel_case(key);
    if !known.contains(&setting) {
        return Ok(());
    }
    let value = match raw {
        Raw::Scalar(raw) => typed(&setting, &raw).with_context(|| {
            format!("{source} sets `{key}` to `{raw}`, which is not a value pnpm accepts for it")
        })?,
        Raw::List(items) => {
            let value = json!(items);
            check(&setting, &value).map_err(|error| anyhow!("{source} sets `{key}[]`: {error}"))?;
            value
        }
    };
    merged.insert(setting, value);
    Ok(())
}

/// The setting an `npm_config_*` variable names, in its raw spelling.
fn setting_key_of_var(name: &str) -> Option<&str> {
    const PREFIX: &str = "npm_config_";
    let head = name.get(..PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX)
        .then(|| &name[PREFIX.len()..])
        .filter(|key| !key.is_empty())
}

/// Whether the engine reads `key` from `.npmrc` and the environment itself.
/// `user-agent` is here too: the engine sends its own, and an `npm run`
/// parent exports npm's to every child.
fn read_by_engine(key: &str) -> bool {
    const KEYS: &[&str] = &[
        "registry",
        "_auth",
        "_authToken",
        "_password",
        "username",
        "email",
        "tokenHelper",
        "token-helper",
        "https-proxy",
        "http-proxy",
        "proxy",
        "no-proxy",
        "noproxy",
        "ca",
        "cafile",
        "cert",
        "key",
        "strict-ssl",
        "local-address",
        "npmrc-auth-file",
        "userconfig",
        "user-agent",
    ];
    let kebab = key.replace('_', "-");
    key.starts_with("//")
        || key.ends_with(":registry")
        || KEYS.contains(&key)
        || KEYS.contains(&kebab.as_str())
}

/// Every setting name `pnpm-workspace.yaml` accepts. The struct serializes
/// every field under its own spelling, so this follows the engine across pin
/// moves with nothing to keep in step by hand.
fn known_keys() -> BTreeSet<String> {
    match serde_json::to_value(WorkspaceSettings::default()) {
        Ok(Value::Object(fields)) => fields.into_iter().map(|(key, _)| key).collect(),
        _ => BTreeSet::new(),
    }
}

/// Whether the engine accepts `value` for `setting`. The struct drops unknown
/// keys rather than refusing them, so callers check the name first.
fn check(setting: &str, value: &Value) -> Result<(), serde_json::Error> {
    let mut one = Map::new();
    one.insert(setting.to_owned(), value.clone());
    serde_json::from_value::<WorkspaceSettings>(Value::Object(one)).map(drop)
}

/// A `.npmrc` string as the JSON value its setting takes. `.npmrc` carries no
/// types, so each plausible reading is offered to the engine in turn.
fn typed(setting: &str, raw: &str) -> Option<Value> {
    let boolean = match raw {
        "true" => Some(Value::Bool(true)),
        "false" => Some(Value::Bool(false)),
        _ => None,
    };
    let number = raw.parse::<i64>().ok().map(Value::from);
    let list = json!(
        raw.split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
    );
    [
        boolean,
        number,
        Some(Value::String(raw.to_owned())),
        Some(list),
    ]
    .into_iter()
    .flatten()
    .find(|value| check(setting, value).is_ok())
}

pub(crate) fn npmrc_entries(text: &str) -> Vec<(String, Raw)> {
    let mut entries: Vec<(String, Raw)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';', '[']) {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((key, value)) => (key.trim(), unquote(value.trim())),
            // A bare key is npm's shorthand for `key=true`.
            None => (line, "true"),
        };
        if let Some(base) = key.strip_suffix("[]") {
            match entries.iter_mut().rfind(|(existing, _)| existing == base) {
                Some((_, Raw::List(items))) => items.push(value.to_owned()),
                _ => entries.push((base.to_owned(), Raw::List(vec![value.to_owned()]))),
            }
        } else {
            entries.push((key.to_owned(), Raw::Scalar(value.to_owned())));
        }
    }
    entries
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}

/// The user's `.npmrc`, then the project's, keeping the files that exist.
pub(crate) fn npmrc_files(root: &Path) -> Vec<(PathBuf, String)> {
    let user = ["npm_config_userconfig", "NPM_CONFIG_USERCONFIG"]
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .or_else(|| dirs_next::home_dir().map(|home| home.join(".npmrc")));
    let project = root.join(".npmrc");
    let project = (user.as_ref() != Some(&project)).then_some(project);
    user.into_iter()
        .chain(project)
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            Some((path, text))
        })
        .collect()
}

/// The directory whose `package.json` declares the workspace containing
/// `start_dir`, found the way the engine finds it, or `start_dir` itself.
pub(crate) fn workspace_root(start_dir: &Path) -> PathBuf {
    start_dir
        .ancestors()
        .find(|dir| declares_workspace(&read_manifest(dir)))
        .unwrap_or(start_dir)
        .to_path_buf()
}

fn declares_workspace(manifest: &Map<String, Value>) -> bool {
    let Some(workspaces) = manifest.get("workspaces") else {
        return false;
    };
    workspaces
        .as_array()
        .or_else(|| workspaces.get("packages")?.as_array())
        .is_some_and(|patterns| patterns.iter().any(Value::is_string))
}

fn read_manifest(dir: &Path) -> Map<String, Value> {
    std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok())
        .and_then(|value| match value {
            Value::Object(manifest) => Some(manifest),
            _ => None,
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sources(install: &InstallConfig) -> Sources<'_> {
        Sources {
            npmrc: Vec::new(),
            install,
            env: Vec::new(),
            manifest: Map::new(),
            root: PathBuf::from("/app"),
            cache_root: Some(PathBuf::from("/cache/nub")),
            ci: false,
        }
    }

    fn settings(value: Value) -> Option<Map<String, Value>> {
        value.as_object().cloned()
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// Each rung is set by two sources, and the higher one must win: project
    /// `.npmrc` over the user's, `nub.jsonc` over `.npmrc`, the environment
    /// over `nub.jsonc`, and `NUB_CACHE_DIR` over its npm spelling.
    #[test]
    fn each_source_outranks_the_one_below_it() {
        let install = InstallConfig {
            settings: settings(json!({ "strictPeerDependencies": false, "nodeLinker": "hoisted" })),
            ..Default::default()
        };
        let mut sources = sources(&install);
        sources.npmrc = vec![
            (
                PathBuf::from("/home/.npmrc"),
                "dedupe-peers=true\n".to_owned(),
            ),
            (
                PathBuf::from("/app/.npmrc"),
                "dedupe-peers=false\nstrict-peer-dependencies=true\n".to_owned(),
            ),
        ];
        sources.env = env(&[
            ("npm_config_node_linker", "isolated"),
            ("npm_config_cache_dir", "/from/npm"),
            ("NUB_CACHE_DIR", "/from/nub"),
        ]);

        let merged = merge(&sources).expect("merge");

        assert_eq!(merged["dedupePeers"], json!(false));
        assert_eq!(merged["strictPeerDependencies"], json!(false));
        assert_eq!(merged["nodeLinker"], json!("isolated"));
        assert_eq!(merged["cacheDir"], json!("/from/nub"));
        assert_eq!(merged["storeDir"], json!("/cache/nub/store"));
        let resolved: WorkspaceSettings =
            serde_json::from_value(Value::Object(merged)).expect("the engine accepts the merge");
        assert_eq!(resolved.dedupe_peers, Some(false));
    }

    /// Only the `npm_config_` prefix names a setting. The settings table still
    /// LISTS a brand-prefixed alias beside each of those spellings — `AUBE_*`
    /// from the previous engine, `PNPM_CONFIG_*` from pnpm's own surface — and
    /// neither is nub's to read: one carries an engine brand nub does not own,
    /// the other configures a different tool's install. Structural rather than
    /// gated, so the install report can credit whatever reaches this tier
    /// without re-deciding the question.
    #[test]
    fn a_brand_prefixed_variable_is_not_a_setting_source() {
        let install = InstallConfig::default();
        let mut sources = sources(&install);
        sources.env = env(&[
            ("AUBE_NODE_LINKER", "hoisted"),
            ("PNPM_CONFIG_NODE_LINKER", "hoisted"),
        ]);

        let merged = merge(&sources).expect("merge");

        assert!(
            !merged.contains_key("nodeLinker"),
            "a brand-prefixed variable set nothing, so the layout is still unasked: {merged:?}"
        );
        // The positive control: the same setting IS reachable, so the assertion
        // above is about the prefix rather than about `nodeLinker` being inert.
        sources.env = env(&[("npm_config_node_linker", "hoisted")]);
        assert_eq!(
            merge(&sources).expect("merge")["nodeLinker"],
            json!("hoisted")
        );
    }

    #[test]
    fn curated_keys_lower_as_the_previous_engine_lowered_them() {
        let install = InstallConfig {
            linker: Some(LinkerConfig::Isolated {
                hoist: Some(Hoist::Patterns(vec!["@types/*".to_owned()])),
            }),
            public_hoist: Some(vec!["*eslint*".to_owned()]),
            minimum_release_age: Some(Duration::from_secs(61)),
            minimum_release_age_exclude: Some(vec!["@internal/*".to_owned()]),
            settings: None,
        };

        let merged = merge(&sources(&install)).expect("merge");

        assert_eq!(merged["nodeLinker"], json!("isolated"));
        assert_eq!(merged["enableGlobalVirtualStore"], json!(false));
        assert_eq!(merged["hoist"], json!(true));
        assert_eq!(merged["hoistPattern"], json!(["@types/*"]));
        assert_eq!(merged["shamefullyHoist"], json!(false));
        assert_eq!(merged["publicHoistPattern"], json!(["*eslint*"]));
        assert_eq!(
            merged["minimumReleaseAge"],
            json!(2),
            "61 seconds rounds up to 2 minutes"
        );
        assert_eq!(merged["minimumReleaseAgeStrict"], json!(true));
        assert_eq!(merged["minimumReleaseAgeExclude"], json!(["@internal/*"]));
    }

    /// The shared store is nub's default only where it can apply: off in CI,
    /// off for a hoisted layout, and never over an explicit choice.
    #[test]
    fn the_global_virtual_store_is_a_default_and_nothing_more() {
        let plain = InstallConfig::default();
        assert_eq!(
            merge(&sources(&plain)).unwrap()["enableGlobalVirtualStore"],
            json!(true)
        );

        let mut ci = sources(&plain);
        ci.ci = true;
        assert!(!merge(&ci).unwrap().contains_key("enableGlobalVirtualStore"));

        let hoisted = InstallConfig {
            linker: Some(LinkerConfig::Hoisted),
            ..Default::default()
        };
        assert!(
            !merge(&sources(&hoisted))
                .unwrap()
                .contains_key("enableGlobalVirtualStore")
        );

        let mut opted_out = sources(&plain);
        opted_out.npmrc = vec![(
            PathBuf::from("/app/.npmrc"),
            "enable-global-virtual-store=false\n".to_owned(),
        )];
        assert_eq!(
            merge(&opted_out).unwrap()["enableGlobalVirtualStore"],
            json!(false)
        );
    }

    /// A project that configures nothing still gets the maturity cutoff applied
    /// STRICTLY, which is the half the engine's own built-in default does not
    /// give.
    ///
    /// The engine defaults the cutoff to 24 hours and nub agrees with the
    /// number, so this pins only the strictness. Left alone the engine treats
    /// its own built-in cutoff as advisory — no mature version in range means
    /// an immature one is installed rather than the install stopping — and it
    /// decides that by whether the cutoff was configured explicitly, which here
    /// it was not. The minutes are deliberately NOT asserted: they are the
    /// engine's to choose, and pinning them here would turn a change in its
    /// default into a failure of nub's.
    #[test]
    fn an_unconfigured_project_gets_a_strict_release_age_floor() {
        let plain = InstallConfig::default();
        assert_eq!(
            merge(&sources(&plain)).unwrap()["minimumReleaseAgeStrict"],
            json!(true)
        );
        assert!(
            !merge(&sources(&plain))
                .unwrap()
                .contains_key("minimumReleaseAge"),
            "the cutoff itself stays the engine's own default"
        );

        // The other half: a version the registry publishes no date for is
        // blocked rather than admitted with a warning, which is what nub's
        // own error for that case is for.
        assert_eq!(
            merge(&sources(&plain)).unwrap()["minimumReleaseAgeIgnoreMissingTime"],
            json!(false)
        );

        // Saying so explicitly is what a project does to get the engine's
        // advisory behaviour back, so neither default may outrank it.
        let mut relaxed = sources(&plain);
        relaxed.npmrc = vec![(
            PathBuf::from("/app/.npmrc"),
            "minimum-release-age-strict=false\nminimum-release-age-ignore-missing-time=true\n"
                .to_owned(),
        )];
        let relaxed = merge(&relaxed).unwrap();
        assert_eq!(relaxed["minimumReleaseAgeStrict"], json!(false));
        assert_eq!(relaxed["minimumReleaseAgeIgnoreMissingTime"], json!(true));
    }

    #[test]
    fn neutral_package_json_fields_become_settings() {
        let install = InstallConfig::default();
        let mut sources = sources(&install);
        sources.manifest = settings(json!({
            "resolutions": { "is-odd>is-number": "6.0.0", "semver": "7.0.0" },
            "overrides": { "is-odd>is-number": "7.0.0" },
            "packageExtensions": { "foo@1": { "peerDependencies": { "bar": "*" } } },
            "workspaces": { "packages": ["packages/*"], "catalog": { "react": "19.2.0" } }
        }))
        .unwrap();

        let merged = merge(&sources).expect("merge");

        assert_eq!(
            merged["overrides"],
            json!({ "is-odd>is-number": "7.0.0", "semver": "7.0.0" }),
            "`overrides` wins a pin both fields set"
        );
        assert_eq!(
            merged["packageExtensions"]["foo@1"]["peerDependencies"]["bar"],
            json!("*")
        );
        assert_eq!(merged["catalog"], json!({ "react": "19.2.0" }));
        assert!(
            !merged.contains_key("packages"),
            "membership is the engine's to read"
        );
    }

    #[test]
    fn install_settings_refuses_what_the_engine_would_not_accept() {
        let refuse = |install: InstallConfig, manifest: Value, needle: &str| {
            let mut sources = sources(&install);
            sources.manifest = settings(manifest).unwrap();
            let message = merge(&sources).expect_err(needle).to_string();
            assert!(message.contains(needle), "{needle:?} not in {message:?}");
        };
        let passthrough = |value: Value| InstallConfig {
            settings: settings(value),
            ..Default::default()
        };

        refuse(
            passthrough(json!({ "strictPeerDependency": true })),
            json!({}),
            "is not a setting",
        );
        refuse(
            passthrough(json!({ "strictPeerDependencies": "yes" })),
            json!({}),
            "install.settings.strictPeerDependencies",
        );
        refuse(
            passthrough(json!({ "overrides": { "a": "1.0.0" } })),
            json!({ "resolutions": { "a": "2.0.0" } }),
            "`overrides` or `resolutions` in package.json",
        );
    }

    /// Credentials and registries are the engine's to read, npm's own keys pass
    /// by, the list form collects, and a known key with a bad value names the
    /// file it came from.
    #[test]
    fn npmrc_entries_lift_only_the_settings_this_layer_carries() {
        let install = InstallConfig::default();
        let mut sources = sources(&install);
        sources.npmrc = vec![(
            PathBuf::from("/app/.npmrc"),
            "registry=https://registry.example/\n\
             //registry.example/:_authToken=secret\n\
             https-proxy=http://proxy.example/\n\
             loglevel=warn\n\
             public-hoist-pattern[]=*eslint*\n\
             public-hoist-pattern[]=*prettier*\n"
                .to_owned(),
        )];
        sources.env = env(&[("npm_config_user_agent", "npm/11.0.0 node/v26.0.0")]);

        let merged = merge(&sources).expect("merge");

        for absent in ["registry", "httpsProxy", "loglevel"] {
            assert!(!merged.contains_key(absent), "{absent} must not be lifted");
        }
        // `userAgent` is the one this cannot check by absence, because nub
        // supplies its own. Checking the VALUE is the stronger guard anyway:
        // it proves both that the ambient `npm_config_user_agent` was
        // discarded and that the string a build script ends up reading names
        // nub. Lifting the ambient one would tell every postinstall in the
        // tree that whichever npm happened to invoke nub is installing it.
        assert!(
            merged["userAgent"]
                .as_str()
                .is_some_and(|ua| ua.starts_with("nub/")),
            "the ambient npm_config_user_agent must not be lifted: {}",
            merged["userAgent"]
        );
        assert!(!merged.keys().any(|key| key.contains("authToken")));
        assert_eq!(
            merged["publicHoistPattern"],
            json!(["*eslint*", "*prettier*"])
        );

        sources.npmrc = vec![(
            PathBuf::from("/app/.npmrc"),
            "node-linker=sideways\n".to_owned(),
        )];
        sources.env.clear();
        let message = merge(&sources)
            .expect_err("an invalid value is refused")
            .to_string();
        assert!(
            message.contains("/app/.npmrc") && message.contains("node-linker"),
            "{message}"
        );
    }

    /// A path specifier has many spellings for one directory and the engine
    /// writes exactly one of them, so a decision spelled any other way
    /// approves nothing and fails the install it was meant to permit.
    #[test]
    fn a_path_decision_is_keyed_the_way_the_engine_keys_it() {
        let root = Path::new("/app");
        for (written, expected) in [
            ("dep@file:./dep", "dep@file:dep"),
            ("dep@file:dep", "dep@file:dep"),
            ("dep@file:./sub/../dep", "dep@file:dep"),
            ("dep@file:./a/b", "dep@file:a/b"),
            ("dep@link:./dep", "dep@link:dep"),
            ("dep@portal:./dep", "dep@portal:dep"),
            ("@scope/dep@file:./dep", "@scope/dep@file:dep"),
            ("dep@file:../sibling", "dep@file:../sibling"),
        ] {
            assert_eq!(engine_build_key(written, root), expected, "key {written}");
        }
    }

    /// Everything the rule does not own is left exactly as written. A key
    /// that matches nothing withholds a permission; a key rewritten wrongly
    /// would grant one.
    #[test]
    fn a_decision_the_rule_does_not_own_is_left_alone() {
        let root = Path::new("/app");
        for key in [
            "esbuild",
            "@scope/pkg",
            "dep@1.2.3",
            "dep@github:owner/repo",
            "dep@https://example.test/dep.tgz",
            "dep@file:/absolute/dep",
            "dep@file:",
            "@scope/pkg@file:.",
        ] {
            assert_eq!(engine_build_key(key, root), key, "key {key}");
        }
    }

    /// The collapse is decided from the text, so it answers for a path whose
    /// parents do not exist and never follows a symlink into a different
    /// answer than the one the project wrote.
    #[test]
    fn the_collapse_reads_the_path_and_not_the_disk() {
        assert_eq!(
            lexically_resolve(Path::new("/app"), "./nowhere/../dep"),
            Path::new("/app/dep"),
        );
        assert_eq!(
            lexically_resolve(Path::new("/app"), "../dep"),
            Path::new("/dep")
        );
        assert_eq!(
            lexically_resolve(Path::new("/"), "../dep"),
            Path::new("/dep")
        );
    }
    /// A workspace sibling named by a plain semver range must resolve to the
    /// member rather than to whatever carries that name on the registry. The
    /// engine defaults the other way, so without this a monorepo arriving from
    /// npm, yarn or bun gets a 404 for a package in its own tree.
    #[test]
    fn a_workspace_sibling_is_linked_by_a_plain_range_by_default() {
        let plain = InstallConfig::default();
        assert_eq!(
            merge(&sources(&plain)).unwrap()["linkWorkspacePackages"],
            json!(true),
        );

        // A default and nothing more: the project can still turn it off.
        let mut opted_out = sources(&plain);
        opted_out.npmrc = vec![(
            PathBuf::from("/app/.npmrc"),
            "link-workspace-packages=false\n".to_owned(),
        )];
        assert_eq!(
            merge(&opted_out).unwrap()["linkWorkspacePackages"],
            json!(false),
        );
    }
}
