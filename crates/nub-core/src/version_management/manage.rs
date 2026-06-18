//! The `nub node` version-management command group — `install` / `ls` /
//! `uninstall` / `pin`. Spec: `wiki/commands/node-versions.md`.
//!
//! Every operation is a thin wrapper over machinery that already ships: the
//! resolver (`node_index::resolve_spec` / `resolve_range`), the cache layout
//! (`discovery::node_store_dir`), the downloader (`provision_node`), and the
//! pin-source chain (`discovery::resolve_pin_chain`). No new runtime capability —
//! this is the *explicit* surface over the implicit auto-provision path.
//!
//! Each op takes the store dir / cwd as a parameter (mirroring
//! `discovery::nub_store_node_in`) so the behaviors unit-test against temp dirs
//! without mutating the process environment. The thin `*_default` wrappers bind
//! the real `~/.cache/nub/node` store + the process cwd for the CLI.
//!
//! Output discipline: install *progress* lands on STDERR (stdout stays clean),
//! per the uv-style silent-install convention the engine already follows.
//! Queryable *results* — `ls` rows and the `pin` path — go to STDOUT, so
//! `nub node ls | …` and `$(nub node pin …)` pipe cleanly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::node_index;
use super::{HostTarget, provision_node, resolve_mirror_base};
use crate::node::discovery;
use crate::node::version::{NodeVersion, VersionPin};
use crate::pm::shim;

/// True when `<dir>` holds an installed Node (`bin/node` unix, `node.exe`
/// windows) — the cache-hit / install-complete signal, matching the store
/// layout `provision_node` writes.
fn version_dir_has_node(dir: &Path) -> bool {
    dir.join("bin").join("node").is_file() || dir.join("node.exe").is_file()
}

/// The concrete versions currently in `store` (`<store>/<version>/`), each dir
/// name parsed as a version and confirmed to carry a Node binary. Newest first.
fn cached_versions(store: &Path) -> Vec<NodeVersion> {
    let mut versions: Vec<NodeVersion> = std::fs::read_dir(store)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let version = entry.file_name().to_str()?.parse::<NodeVersion>().ok()?;
            version_dir_has_node(&entry.path()).then_some(version)
        })
        .collect();
    versions.sort_by(|a, b| b.cmp(a)); // newest first
    versions
}

/// The version the `cwd` currently resolves to, if any — used to mark `ls` and
/// to guard `uninstall`. `None` when discovery can't resolve (no Node anywhere),
/// which is not an error for these read/remove ops.
///
/// This is the production resolver, hardwired to [`discovery::discover_node`].
/// The `ls` / `uninstall` *cores* take the resolver as a parameter so the
/// active-mark and active-guard paths are testable hermetically (a fake
/// resolver), without mutating the process env to redirect `discover_node`'s
/// real store — in-process `XDG_CACHE_HOME` mutation is an `unsafe` data race
/// here (see `cli.rs`'s note), so we inject instead.
fn resolved_version(cwd: &Path) -> Option<NodeVersion> {
    discovery::discover_node(cwd).ok().map(|n| n.version)
}

// ── install ──────────────────────────────────────────────────────────────

/// What `install` did for one spec — surfaced so the CLI can print a tailored
/// line and tests can assert without scraping stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Already present in nub's cache — no-op.
    AlreadyCached(NodeVersion),
    /// Present on PATH (system / nvm / fnm) but not in nub's cache — reported and
    /// skipped, no download (locked decision #2; `--force` deferred).
    AlreadyOnPath(NodeVersion),
    /// Downloaded + extracted into the cache.
    Installed(NodeVersion),
}

impl InstallOutcome {
    pub fn version(&self) -> &NodeVersion {
        match self {
            Self::AlreadyCached(v) | Self::AlreadyOnPath(v) | Self::Installed(v) => v,
        }
    }
}

/// Resolve a spec (`22`, `lts`, `22.13.0`, `latest`, …) to a concrete published
/// version against the dist index. An exact `X.Y.Z` is still routed through the
/// index so a typo'd nonexistent version fails fast rather than 404ing mid-download.
fn resolve_to_concrete(spec: &str, store: &Path, host: &HostTarget) -> Result<NodeVersion> {
    let mirror = resolve_mirror_base(host);
    let index = node_index::load_index(store, &mirror)
        .with_context(|| "fetching the Node release index")?;
    node_index::resolve_spec(spec, &index)
        .ok_or_else(|| anyhow::anyhow!("no published Node version matches \"{spec}\""))
}

/// Install one spec into `store`. Decision #2: a version already on PATH but not
/// in nub's cache is reported + skipped (no download). Progress lands on STDERR
/// via `provision_node`.
pub fn install_one(spec: &str, store: &Path, cwd: &Path) -> Result<InstallOutcome> {
    // Fast offline path: an exact `X.Y.Z` that's already cached is a no-op — don't
    // touch the network just to confirm a version we already hold.
    if let Ok(exact) = spec.trim().parse::<NodeVersion>() {
        if version_dir_has_node(&store.join(exact.to_string())) {
            return Ok(InstallOutcome::AlreadyCached(exact));
        }
    }

    let host = HostTarget::detect()
        .ok_or_else(|| anyhow::anyhow!("this host is not a platform nodejs.org publishes"))?;

    let concrete = resolve_to_concrete(spec, store, &host)?;
    install_concrete(concrete, &host, store, cwd)
}

/// The install body once a CONCRETE version is known: cache no-op → PATH skip →
/// download. Shared by [`install_one`] (spec → concrete via `resolve_spec`) and
/// [`install_from_pin`]'s range leg (range → concrete via `resolve_range`).
fn install_concrete(
    concrete: NodeVersion,
    host: &HostTarget,
    store: &Path,
    cwd: &Path,
) -> Result<InstallOutcome> {
    // Already in nub's cache → no-op.
    if version_dir_has_node(&store.join(concrete.to_string())) {
        return Ok(InstallOutcome::AlreadyCached(concrete));
    }

    // Already available on PATH (system / nvm) at the exact version → skip + report.
    if let Ok(node) = discovery::discover_node(cwd) {
        if node.version == concrete {
            return Ok(InstallOutcome::AlreadyOnPath(concrete));
        }
    }
    // Also catch the case where the resolver wouldn't pick it (no pin) but some
    // PATH node happens to be exactly this version — a direct shell probe.
    if let Some(v) = path_node_version() {
        if v == concrete {
            return Ok(InstallOutcome::AlreadyOnPath(concrete));
        }
    }

    provision_node(&concrete, host, store_root_of(store), None)
        .with_context(|| format!("installing Node {concrete}"))?;
    Ok(InstallOutcome::Installed(concrete))
}

/// `provision_node` takes the cache *root* (it appends `node/` itself), whereas
/// the command group threads the `node/` store dir directly. Recover the root.
fn store_root_of(store: &Path) -> &Path {
    store.parent().unwrap_or(store)
}

/// The version of whatever `node` is on PATH, if any (skipping nub's own shim
/// dirs is `which_node`'s job; here a plain probe is enough for the PATH-reuse
/// skip check). `None` if no node is found or it doesn't report a version.
fn path_node_version() -> Option<NodeVersion> {
    let out = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Bare `nub node install`: resolve the project pin through the FULL chain —
/// `devEngines.runtime` (#1) → `.node-version` (#2) → `.nvmrc` (#3) →
/// `engines.node` (#4), the same `resolve_pin_chain` the run path uses, so the
/// version installed is the version a run would resolve (never the pin-file
/// version when `devEngines.runtime` outranks it). Errors clearly when there's
/// no pin to install; chain warnings (onFail:warn, unusable specs) land on
/// stderr here since this entry point resolves the chain itself.
pub fn install_from_pin(store: &Path, cwd: &Path) -> Result<InstallOutcome> {
    let chain = discovery::resolve_pin_chain(cwd)?;
    for warning in &chain.warnings {
        eprintln!("{warning}");
    }
    let Some((raw, pin, source)) = chain.pin else {
        bail!(
            "nub node install: no version given and no Node pin (devEngines.runtime, \
             .node-version, .nvmrc, or engines.node) found in this project"
        );
    };
    match &pin {
        // A range pin (devEngines.runtime / engines.node) resolves to the newest
        // published version satisfying it — resolve_spec only knows the nvm grammar.
        VersionPin::Range(alternatives) => {
            let host = HostTarget::detect().ok_or_else(|| {
                anyhow::anyhow!("this host is not a platform nodejs.org publishes")
            })?;
            let mirror = resolve_mirror_base(&host);
            let index = node_index::load_index(store, &mirror)
                .with_context(|| "fetching the Node release index")?;
            let concrete = node_index::resolve_range(alternatives, &index).ok_or_else(|| {
                anyhow::anyhow!("no published Node version satisfies \"{raw}\" (from {source})")
            })?;
            install_concrete(concrete, &host, store, cwd)
        }
        _ => install_one(&raw, store, cwd),
    }
}

// ── ls ───────────────────────────────────────────────────────────────────

/// One `ls` row: a cached version + whether the cwd currently resolves to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsEntry {
    pub version: NodeVersion,
    pub active: bool,
}

/// List the versions in nub's cache, newest first, marking the cwd-resolved one
/// — but ONLY when that version is itself cached (the `→` means "this cached one
/// is what would run here", not "something on PATH would run"). Cache-only:
/// never scans nvm / system.
pub fn ls(store: &Path, cwd: &Path) -> Vec<LsEntry> {
    ls_with(store, resolved_version(cwd))
}

/// `ls` core, parameterized over the resolved version so the active-marking
/// contract is testable without touching `discover_node`'s real store.
fn ls_with(store: &Path, active: Option<NodeVersion>) -> Vec<LsEntry> {
    cached_versions(store)
        .into_iter()
        .map(|version| {
            let active = active.as_ref() == Some(&version);
            LsEntry { version, active }
        })
        .collect()
}

// ── uninstall ────────────────────────────────────────────────────────────

/// Remove `version` from nub's cache. `version` is the literal spec the user
/// typed; it must name a concrete cached version (`X.Y.Z`). Errors when the
/// version isn't cached, when the cwd currently resolves to it (removing the
/// live version would break the current project's runs), or when it's the
/// user-global default (removing it would dangle the default + `~/.nub/current`).
pub fn uninstall(version_spec: &str, store: &Path, cwd: &Path) -> Result<NodeVersion> {
    let default = discovery::default_file().and_then(|f| read_default_record(&f));
    uninstall_with(version_spec, store, resolved_version(cwd), default)
}

/// `uninstall` core, parameterized over the resolved + default versions so the
/// active- and default-guards are testable without touching the real store.
fn uninstall_with(
    version_spec: &str,
    store: &Path,
    active: Option<NodeVersion>,
    default: Option<NodeVersion>,
) -> Result<NodeVersion> {
    let version: NodeVersion = version_spec.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "nub node uninstall takes a concrete version (e.g. 22.13.0), got \"{version_spec}\""
        )
    })?;

    let dir = store.join(version.to_string());
    if !version_dir_has_node(&dir) {
        bail!("Node {version} is not in nub's cache (nothing to uninstall)");
    }

    if active.as_ref() == Some(&version) {
        bail!(
            "Node {version} is the version this directory currently resolves to — \
             change the pin (or cd elsewhere) before uninstalling it"
        );
    }

    if default.as_ref() == Some(&version) {
        bail!(
            "Node {version} is your default (nub node default) — \
             set another default or clear it (nub node default --unset) before uninstalling it"
        );
    }

    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    Ok(version)
}

// ── pin ──────────────────────────────────────────────────────────────────

/// Where a `pin` landed: the absolute file written + the literal spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinResult {
    pub path: PathBuf,
    pub spec: String,
}

/// Resolve the directory `pin` writes into:
///   1. workspace root (a `package.json#workspaces` / `pnpm-workspace.yaml` root
///      above the cwd) — Node version is repo-wide;
///   2. else the nearest `package.json` dir (the project boundary);
///   3. else the cwd (loose scripts).
fn pin_target_dir(cwd: &Path) -> PathBuf {
    if let Some(project) = crate::workspace::detect::detect_project(cwd) {
        if let Some(ws_root) = project.workspace_root {
            return ws_root;
        }
        return project.root;
    }
    cwd.to_path_buf()
}

/// Decision #1: edit the pin file that already exists. If the target dir has a
/// `.nvmrc` but NO `.node-version`, update the `.nvmrc` in place (don't drop a
/// `.node-version` that would silently shadow it). Otherwise write/update
/// `.node-version` (the tool-agnostic standard).
fn pin_target_file(dir: &Path) -> PathBuf {
    let node_version = dir.join(".node-version");
    let nvmrc = dir.join(".nvmrc");
    if !node_version.exists() && nvmrc.exists() {
        nvmrc
    } else {
        node_version
    }
}

/// Write `spec` as the project's pin. Decision #3: blind/offline — no network
/// validation; only obviously-malformed input is rejected. Returns the absolute
/// path written (the CLI prints it).
pub fn pin(spec: &str, cwd: &Path) -> Result<PinResult> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("nub node pin requires a version (e.g. 22, lts, 22.13.0)");
    }
    // Reject only obvious garbage — a pin legitimately holds aliases (`lts`, `22`),
    // so anything that parses as a VersionPin is accepted without a network check.
    if spec.parse::<VersionPin>().is_err() {
        bail!("nub node pin: \"{spec}\" is not a valid Node version or alias");
    }

    let dir = pin_target_dir(cwd);
    let file = pin_target_file(&dir);
    std::fs::write(&file, format!("{spec}\n"))
        .with_context(|| format!("writing {}", file.display()))?;

    let path = std::fs::canonicalize(&file).unwrap_or(file);
    Ok(PinResult {
        path,
        spec: spec.to_string(),
    })
}

// ── default ────────────────────────────────────────────────────────────────

/// Outcome of putting the default on PATH (step 2 of `nub node default`). The
/// record (step 1) is already written regardless of this; a `Failed` here is a
/// warning, not a command failure — re-running just retries this step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exposure {
    /// `~/.nub/current` was wired onto PATH; carries the profile outcome (file to
    /// source, already present, or a manual line).
    Path(shim::ProfileOutcome),
    /// PATH editing isn't automated on Windows yet (its own PR) — `dir` is the
    /// version's bin dir for the user to add by hand.
    Unsupported(PathBuf),
    /// The symlink/profile write failed; `0` is the reason. Non-fatal.
    Failed(String),
}

/// What [`set_default`] resolved + did, so the CLI can print a tailored line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultResult {
    /// The concrete version now recorded as the user-global default.
    pub version: NodeVersion,
    /// True when the version had to be downloaded (vs. already in nub's cache).
    pub installed: bool,
    /// How (and whether) the default was put on PATH for other programs.
    pub exposure: Exposure,
}

/// Set the user-global default Node (`nub node default <spec>`): resolve `spec`
/// to a concrete version, provision it if absent, record it in `~/.nub/default`
/// (resolution rung #5), and expose its bin dir on PATH via `~/.nub/current`.
///
/// Two design choices: provision into nub's OWN store even if the version is
/// already on PATH (the default needs a copy nub controls — it's what
/// `~/.nub/current` links), and store the CONCRETE version not the alias (so the
/// record and store can't drift; re-run to bump an `lts`/`22`).
///
/// Step 2 (`expose_default`) is non-fatal: the record is written first, so a
/// PATH-wiring failure still leaves a working default and is reported as a
/// warning, not an error.
pub fn set_default(spec: &str, store: &Path) -> Result<DefaultResult> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("nub node default requires a version (e.g. 22, lts, 22.13.0)");
    }
    let (version, installed) = resolve_and_provision(spec, store)?;
    write_default(&version)?;
    let exposure = expose_default(store, &version);
    Ok(DefaultResult {
        version,
        installed,
        exposure,
    })
}

/// Resolve `spec` to a concrete cached version, downloading it when absent.
/// Returns `(version, installed)` — `installed` true only when a download
/// happened. Fast offline path: an exact already-cached version needs no index
/// fetch.
fn resolve_and_provision(spec: &str, store: &Path) -> Result<(NodeVersion, bool)> {
    if let Ok(exact) = spec.parse::<NodeVersion>() {
        if version_dir_has_node(&store.join(exact.to_string())) {
            return Ok((exact, false));
        }
    }
    let host = HostTarget::detect()
        .ok_or_else(|| anyhow::anyhow!("this host is not a platform nodejs.org publishes"))?;
    let concrete = resolve_to_concrete(spec, store, &host)?;
    if version_dir_has_node(&store.join(concrete.to_string())) {
        return Ok((concrete, false));
    }
    provision_node(&concrete, &host, store_root_of(store), None)
        .with_context(|| format!("installing Node {concrete}"))?;
    Ok((concrete, true))
}

/// Clear the user-global default (`nub node default --unset`): remove
/// `~/.nub/default` and tear down the PATH exposure ([`unexpose_default`]).
/// Returns the version that was set, or `None` when none was. Idempotent —
/// clearing an unset default is a no-op, not an error.
pub fn unset_default() -> Result<Option<NodeVersion>> {
    let Some(file) = discovery::default_file() else {
        return Ok(None);
    };
    let prev = read_default_record(&file);
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("removing {}", file.display())),
    }
    unexpose_default()?;
    Ok(prev)
}

/// `~/.nub/current` — the symlink to the default Node's bin dir that carries it
/// (and its `npm`/`npx`/`corepack`) onto PATH. A sibling of `~/.nub/shims` and
/// install.sh's `~/.nub/bin`.
fn current_link() -> Result<PathBuf> {
    dirs_next::home_dir()
        .map(|h| h.join(".nub").join("current"))
        .ok_or_else(|| anyhow::anyhow!("cannot locate the home directory for ~/.nub/current"))
}

/// Put the default on PATH (step 2). Infallible by design — any error becomes
/// `Exposure::Failed` so a wiring failure can't undo the already-written record;
/// the CLI surfaces it as a warning.
#[cfg(unix)]
fn expose_default(store: &Path, version: &NodeVersion) -> Exposure {
    match try_expose_default(store, version) {
        Ok(outcome) => Exposure::Path(outcome),
        Err(e) => Exposure::Failed(format!("{e:#}")),
    }
}

/// Atomically re-point `~/.nub/current` at `<store>/<version>/bin` (temp link +
/// rename, so the swap never leaves `current` missing) and wire it onto PATH via
/// `shim::add_path_block` (which orders shims ahead of it).
#[cfg(unix)]
fn try_expose_default(store: &Path, version: &NodeVersion) -> Result<shim::ProfileOutcome> {
    let current = current_link()?;
    if let Some(parent) = current.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let bin = store.join(version.to_string()).join("bin");
    let tmp = current.with_file_name(format!("current.nub-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&bin, &tmp)
        .with_context(|| format!("linking {} -> {}", tmp.display(), bin.display()))?;
    std::fs::rename(&tmp, &current).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    shim::add_path_block()
}

/// Windows: PATH editing isn't automated yet (its own PR) — report the version's
/// bin dir for the user to add by hand. The record is already written, so nub's
/// own resolution + the PM shims still pick the default up.
#[cfg(not(unix))]
fn expose_default(store: &Path, version: &NodeVersion) -> Exposure {
    Exposure::Unsupported(store.join(version.to_string()))
}

/// Remove `~/.nub/current` and recompute the managed PATH block (which drops
/// `current` while keeping `~/.nub/shims` if it's still installed). Unix only.
#[cfg(unix)]
fn unexpose_default() -> Result<()> {
    let current = current_link()?;
    match std::fs::remove_file(&current) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", current.display())),
    }
    shim::remove_path_block()?;
    Ok(())
}

#[cfg(not(unix))]
fn unexpose_default() -> Result<()> {
    Ok(())
}

/// Write the concrete default version to `~/.nub/default`, creating `~/.nub` if
/// missing. Thin real-path wrapper over [`write_default_record`].
fn write_default(version: &NodeVersion) -> Result<()> {
    let file = discovery::default_file()
        .ok_or_else(|| anyhow::anyhow!("could not locate ~/.nub for the default record"))?;
    write_default_record(&file, version)
}

/// Write `<version>\n` to `file` (creating its parent), parameterized over the
/// path so the record format is testable against a temp file. The single
/// newline-terminated version line mirrors the pin files `pin` writes.
fn write_default_record(file: &Path, version: &NodeVersion) -> Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(file, format!("{version}\n"))
        .with_context(|| format!("writing {}", file.display()))
}

/// Read the concrete default version from `file`; `None` when absent, empty, or
/// unparseable. Strips a leading BOM, matching the pin-file readers.
fn read_default_record(file: &Path) -> Option<NodeVersion> {
    let content = std::fs::read_to_string(file).ok()?;
    let content = content.strip_prefix('\u{FEFF}').unwrap_or(&content);
    content.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique temp dir under the system temp root (NOT under $HOME, so the
    /// pin walk-up can't escape into a stray ancestor pin/package.json). Mirrors
    /// discovery.rs's `resolution_tmpdir`.
    fn tmpdir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nub-manage-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plant a fake installed Node at `<store>/<version>/bin/node` so the store
    /// ops see it as cached, without a download. Mirrors discovery.rs's pattern.
    fn plant(store: &Path, version: &str) {
        let bin = store.join(version).join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("node"), "").unwrap();
    }

    #[test]
    fn default_record_round_trips_and_rejects_garbage() {
        // The `~/.nub/default` record is a single concrete-version line: what
        // `write_default` writes is what `unset_default` reads back. Exercised via
        // the path-injected core (`write_default_record`/`read_default_record`) so
        // it never touches the real ~/.nub.
        let dir = tmpdir("default-record");
        let file = dir.join("default");

        assert_eq!(read_default_record(&file), None, "absent file → no default");

        write_default_record(&file, &NodeVersion::new(22, 15, 0)).unwrap();
        assert_eq!(
            read_default_record(&file),
            Some(NodeVersion::new(22, 15, 0)),
            "the written concrete version reads back"
        );

        // A blank or unparseable record falls through to None — a corrupt default
        // must not error the resolver, the same posture as an unparseable pin file.
        std::fs::write(&file, "  \n").unwrap();
        assert_eq!(read_default_record(&file), None, "blank record → no default");
        std::fs::write(&file, "not-a-version\n").unwrap();
        assert_eq!(read_default_record(&file), None, "garbage → no default");
    }

    #[test]
    fn ls_lists_newest_first_and_is_cache_only() {
        let store = tmpdir("ls-store");
        for v in ["20.11.0", "22.15.0", "22.13.0"] {
            plant(&store, v);
        }
        // A bare dir with no binary, and a non-version dir, are both ignored.
        std::fs::create_dir_all(store.join("18.19.0")).unwrap(); // no bin/node
        std::fs::create_dir_all(store.join("not-a-version")).unwrap();

        // cwd with no pin and no resolvable cached version → nothing marked active.
        let cwd = tmpdir("ls-cwd");
        let entries = ls(&store, &cwd);
        let versions: Vec<_> = entries.iter().map(|e| e.version.to_string()).collect();
        assert_eq!(
            versions,
            vec!["22.15.0", "22.13.0", "20.11.0"],
            "newest-first, binary-less + non-version dirs excluded"
        );
        assert!(
            entries.iter().all(|e| !e.active),
            "no version is marked active when nothing resolves to a cached one"
        );
    }

    #[test]
    fn ls_marks_active_only_when_the_resolved_version_is_cached() {
        // The active mark lands on exactly the resolved row when that version is
        // cached, and on nothing when the resolved version isn't in the store.
        // We exercise the real marking logic via `ls_with`, injecting the resolver
        // result directly — `discover_node`'s store can't be redirected without an
        // unsafe in-process XDG_CACHE_HOME mutation (a data race against parallel
        // tests), so the core is parameterized over the resolved version instead.
        let store = tmpdir("active-store");
        plant(&store, "22.13.0");
        plant(&store, "20.11.0");

        // Resolves to a cached version → exactly that row is active.
        let entries = ls_with(&store, Some(NodeVersion::new(22, 13, 0)));
        let active: Vec<_> = entries
            .iter()
            .filter(|e| e.active)
            .map(|e| e.version.to_string())
            .collect();
        assert_eq!(
            active,
            vec!["22.13.0"],
            "only the resolved cached row is active"
        );

        // Resolves to a version that is NOT cached → nothing is marked (cache-only).
        let off_cache = ls_with(&store, Some(NodeVersion::new(18, 19, 0)));
        assert!(
            off_cache.iter().all(|e| !e.active),
            "an off-cache resolved version marks no row (→ means a cached version runs here)"
        );

        // No resolution at all → nothing is marked.
        let none = ls_with(&store, None);
        assert!(none.iter().all(|e| !e.active), "no resolution marks no row");
    }

    #[test]
    fn uninstall_removes_a_cached_version() {
        let store = tmpdir("uninstall-store");
        plant(&store, "22.13.0");
        plant(&store, "20.11.0");
        // cwd has no pin → won't resolve to either of these specific versions in
        // a way that guards them (the guard is exact-version match).
        let cwd = tmpdir("uninstall-cwd");

        let removed = uninstall("22.13.0", &store, &cwd).expect("removes the cached version");
        assert_eq!(removed, NodeVersion::new(22, 13, 0));
        assert!(
            !store.join("22.13.0").exists(),
            "the version dir is gone after uninstall"
        );
        assert!(store.join("20.11.0").exists(), "siblings are untouched");
    }

    #[test]
    fn uninstall_errors_when_not_cached() {
        let store = tmpdir("uninstall-missing-store");
        let cwd = tmpdir("uninstall-missing-cwd");
        let err = uninstall("22.13.0", &store, &cwd).unwrap_err().to_string();
        assert!(
            err.contains("not in nub's cache"),
            "names the missing-from-cache reason: {err}"
        );
    }

    #[test]
    fn uninstall_guards_the_active_version() {
        // The version the cwd currently resolves to cannot be uninstalled. Driven
        // deterministically through `uninstall_with` with the resolved version
        // injected — no dependency on an ambient PATH node, so the guard is
        // exercised on a no-node CI leg too (the old form silently passed there).
        let store = tmpdir("guard-store");
        let active = NodeVersion::new(22, 13, 0);
        plant(&store, &active.to_string());

        let err = uninstall_with(&active.to_string(), &store, Some(active.clone()), None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("currently resolves to"),
            "the active-version guard fires with a clear message: {err}"
        );
        assert!(
            store
                .join(active.to_string())
                .join("bin")
                .join("node")
                .is_file(),
            "the guarded version is NOT removed"
        );
    }

    #[test]
    fn uninstall_guards_the_global_default() {
        // The global default can't be uninstalled even from a dir that doesn't
        // resolve to it (active != default) — otherwise `~/.nub/default` and
        // `~/.nub/current` would dangle. Injected so the guard is hermetic.
        let store = tmpdir("guard-default-store");
        let default = NodeVersion::new(22, 15, 0);
        plant(&store, &default.to_string());

        let err = uninstall_with(&default.to_string(), &store, None, Some(default.clone()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("is your default"),
            "the default guard fires with a clear message: {err}"
        );
        assert!(
            store.join(default.to_string()).join("bin").join("node").is_file(),
            "the default version is NOT removed"
        );
    }

    #[test]
    fn pin_writes_node_version_in_nearest_package_json_dir() {
        // A project (package.json, no workspaces) nested below cwd's actual dir:
        // pin from a subdir writes at the project root, not the subdir.
        let root = tmpdir("pin-proj");
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).unwrap();
        let sub = root.join("src");
        std::fs::create_dir_all(&sub).unwrap();

        let result = pin("22", &sub).expect("pin writes");
        assert_eq!(result.spec, "22");
        assert_eq!(
            result.path.file_name().unwrap(),
            ".node-version",
            "writes the tool-agnostic .node-version by default"
        );
        let written = std::fs::read_to_string(root.join(".node-version")).unwrap();
        assert_eq!(written, "22\n", "writes the literal spec verbatim");
        assert!(
            !sub.join(".node-version").exists(),
            "does not drop an orphan pin in the cwd subdir"
        );
    }

    #[test]
    fn pin_writes_at_workspace_root_not_a_member() {
        // workspaces root above; pin from a member dir lands at the root.
        let root = tmpdir("pin-ws");
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"mono","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let member = root.join("packages").join("api");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(member.join("package.json"), r#"{"name":"api"}"#).unwrap();

        let result = pin("lts", &member).expect("pin writes");
        assert_eq!(
            std::fs::canonicalize(root.join(".node-version")).unwrap(),
            result.path,
            "the pin lands at the workspace root, governing all members"
        );
        assert!(
            !member.join(".node-version").exists(),
            "no per-member pin is written"
        );
    }

    #[test]
    fn pin_edits_an_existing_nvmrc_in_place() {
        // Decision #1: when only .nvmrc exists, update it in place rather than
        // dropping a shadowing .node-version.
        let root = tmpdir("pin-nvmrc");
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).unwrap();
        std::fs::write(root.join(".nvmrc"), "18.19.0\n").unwrap();

        let result = pin("22.13.0", &root).expect("pin writes");
        assert_eq!(
            result.path.file_name().unwrap(),
            ".nvmrc",
            "updates the existing .nvmrc rather than shadowing it"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".nvmrc")).unwrap(),
            "22.13.0\n"
        );
        assert!(
            !root.join(".node-version").exists(),
            "no shadowing .node-version is created"
        );
    }

    #[test]
    fn pin_prefers_node_version_when_both_files_exist() {
        // With both present, .node-version wins (precedence #1) — update it, leave
        // .nvmrc alone (don't touch the lower-precedence file).
        let root = tmpdir("pin-both");
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).unwrap();
        std::fs::write(root.join(".node-version"), "18.19.0\n").unwrap();
        std::fs::write(root.join(".nvmrc"), "16.0.0\n").unwrap();

        let result = pin("22", &root).expect("pin writes");
        assert_eq!(result.path.file_name().unwrap(), ".node-version");
        assert_eq!(
            std::fs::read_to_string(root.join(".node-version")).unwrap(),
            "22\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".nvmrc")).unwrap(),
            "16.0.0\n",
            "the lower-precedence .nvmrc is left untouched"
        );
    }

    #[test]
    fn pin_rejects_obvious_garbage_but_accepts_aliases() {
        let root = tmpdir("pin-garbage");
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).unwrap();
        assert!(
            pin("not a version!!", &root).is_err(),
            "garbage is rejected"
        );
        // Aliases + bare majors are valid pins (blind/offline — no network check).
        assert!(pin("lts", &root).is_ok());
        assert!(pin("22", &root).is_ok());
        assert!(pin("lts/iron", &root).is_ok());
    }

    #[test]
    fn install_of_a_cached_version_is_an_offline_no_op() {
        // The exact-spec fast path (install_one's first branch) short-circuits to
        // AlreadyCached *before* HostTarget::detect / any network — so reinstalling
        // an exact version already in the cache is an offline no-op. This is the
        // committed reinstall contract and the highest-value install coverage.
        let store = tmpdir("cached-noop-store");
        plant(&store, "22.13.0");
        let cwd = tmpdir("cached-noop-cwd");

        let outcome = install_one("22.13.0", &store, &cwd).expect("offline cache hit");
        assert_eq!(
            outcome,
            InstallOutcome::AlreadyCached(NodeVersion::new(22, 13, 0)),
            "an exact version already cached is a no-op, resolved without the network"
        );
        // NOTE: the AlreadyOnPath skip (a version on PATH but not cached) needs a
        // real `node --version` matching a planted spec, which isn't hermetic — it's
        // left to the #[ignore]d install_real_node_then_cache_hit integration test.
    }

    #[test]
    fn install_from_pin_errors_when_no_pin_present() {
        // Bare `nub node install` with no pin source up-tree errors before any
        // network — a pure walk-up. tmpdir() lives under the system temp root,
        // not $HOME, so the walk-up can't escape into a stray ancestor pin.
        let store = tmpdir("pinless-store");
        let cwd = tmpdir("pinless-cwd");

        let err = install_from_pin(&store, &cwd).unwrap_err().to_string();
        assert!(
            err.contains("devEngines.runtime") && err.contains(".node-version"),
            "names the pin sources it looked for, for bare `nub node install`: {err}"
        );
    }

    #[test]
    fn install_from_pin_honors_dev_engines_runtime_over_the_pin_file() {
        // Bare `nub node install` must install what a RUN would resolve: the
        // devEngines.runtime pin (#1), not the .node-version (#2) it outranks.
        // Hermetic: the devEngines version is already cached, so the exact-spec
        // fast path answers offline — were the pin file consulted instead, the
        // outcome would name 20.11.0 (also planted, to make the failure loud).
        let store = tmpdir("dev-pin-store");
        plant(&store, "22.13.0");
        plant(&store, "20.11.0");
        let cwd = tmpdir("dev-pin-cwd");
        std::fs::write(
            cwd.join("package.json"),
            r#"{"devEngines":{"runtime":{"name":"node","version":"22.13.0"}}}"#,
        )
        .unwrap();
        std::fs::write(cwd.join(".node-version"), "20.11.0\n").unwrap();

        let outcome = install_from_pin(&store, &cwd).expect("an offline cache hit");
        assert_eq!(
            outcome,
            InstallOutcome::AlreadyCached(NodeVersion::new(22, 13, 0)),
            "bare install must provision the devEngines.runtime version, not the pin file's"
        );
    }

    /// Real-network: bare-spec `install_one` downloads a real Node into a temp
    /// store, then a second call is a cache hit. `#[ignore]` — network + ~25MB.
    ///   cargo test -p nub-core --lib version_management::manage::tests::install_real -- --ignored
    #[test]
    #[ignore = "network: installs a real Node (~25MB) into a temp store"]
    fn install_real_node_then_cache_hit() {
        let store = tmpdir("install-real"); // the `node/` store dir
        let cwd = tmpdir("install-real-cwd");

        let first = install_one("22.13.0", &store, &cwd).expect("install");
        match first {
            InstallOutcome::Installed(v) | InstallOutcome::AlreadyOnPath(v) => {
                assert_eq!(v, NodeVersion::new(22, 13, 0));
            }
            other => panic!("expected an install or PATH-skip, got {other:?}"),
        }

        // If it actually downloaded, the dir is now present and the second call is
        // a cache hit. (If the host already had 22.13.0 on PATH it was skipped —
        // then plant it so the cache-hit assertion is still meaningful.)
        if !store.join("22.13.0").join("bin").join("node").is_file() {
            plant(&store, "22.13.0");
        }
        let second = install_one("22.13.0", &store, &cwd).expect("cache hit");
        assert_eq!(
            second,
            InstallOutcome::AlreadyCached(NodeVersion::new(22, 13, 0)),
            "a second install of a cached version is a no-op cache hit"
        );
        let _ = std::fs::remove_dir_all(&store);
    }
}
