use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_STATE_DIR: &str = "node_modules";
const INSTALL_STATE_FILE_NAME: &str = "state.json";
const FRESH_STATE_FILE_NAME: &str = "fresh.json";
const LICENSE_STATE_FILE_NAME: &str = "licenses.json";
const HOISTED_PLACEMENTS_FILE_NAME: &str = "hoisted-placements.json";

/// The install-state directory name, `.<name>-state`. Standalone aube:
/// `.aube-state`.
fn state_dir_name() -> String {
    format!(".{}-state", aube_util::embedder().name)
}

/// Resolve the modules dir and state directory path for `project_dir` in a
/// single settings-context load. `check_needs_install` and `write_state`
/// both need both values, and this is on the hot path for every
/// `aube run` / `exec` / `test` / `start` / `restart`.
///
/// The default `stateDir` falls back to the resolved `modulesDir` so the
/// state directory lives alongside the install tree — otherwise a
/// `modulesDir` override would create a phantom `node_modules/`
/// directory just to hold the state directory.
fn resolve_paths(project_dir: &Path) -> (PathBuf, PathBuf) {
    crate::commands::with_settings_ctx(project_dir, |ctx| {
        let modules_dir = project_dir.join(aube_settings::resolved::modules_dir(ctx));
        let raw_state = aube_settings::resolved::state_dir(ctx);
        let state_parent = if raw_state == DEFAULT_STATE_DIR {
            modules_dir.clone()
        } else {
            crate::commands::expand_setting_path(&raw_state, project_dir)
                .unwrap_or_else(|| modules_dir.clone())
        };
        let state_dir = state_parent.join(state_dir_name());
        (modules_dir, state_dir)
    })
}

fn state_dir(project_dir: &Path) -> PathBuf {
    resolve_paths(project_dir).1
}

fn relative_path_or_original(path: &Path, base: &Path) -> String {
    pathdiff::diff_paths(path, base)
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallState {
    pub lockfile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_snapshot_name: Option<String>,
    /// `(size, mtime)` of the root lockfile at install time, mirroring
    /// `package_json_meta`'s fast path: stat once and skip the BLAKE3
    /// re-hash when the snapshot is unchanged. Root lockfiles are the
    /// largest file the freshness check reads (10+ MB on big
    /// monorepos), so this is the difference between an O(1) stat and
    /// re-reading the whole file on every `aube run` startup. Missing
    /// field (older state) falls through to the hash path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_meta: Option<FileMeta>,
    /// Per-member lockfile fingerprints for `sharedWorkspaceLockfile=false`
    /// workspaces, keyed by the member's importer path (relative to the
    /// workspace root). That layout writes one lockfile per member and
    /// *no* shared root lockfile, so `lockfile_hash` above is empty and
    /// the single-lockfile freshness check would treat every install as
    /// "no lockfile found" and re-run the full pipeline. Recording each
    /// member here lets the warm path verify the per-member lockfiles
    /// instead. Every current member is recorded — a depless member with
    /// no lockfile maps to an empty hash — so an added or removed member
    /// also invalidates the warm path. Empty for the default shared
    /// layout and for non-workspace projects.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub member_lockfile_hashes: BTreeMap<String, String>,
    /// `(size, mtime)` per member lockfile, mirroring
    /// `package_json_meta`'s fast path: stat each member lockfile and
    /// only re-hash when the snapshot moved. Keyed identically to
    /// `member_lockfile_hashes`. Members without a lockfile have no
    /// entry here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub member_lockfile_meta: BTreeMap<String, FileMeta>,
    pub package_json_hashes: BTreeMap<String, String>,
    /// Mirrors `FreshnessState::package_json_meta`. See R1 docstring
    /// there for the freshness-check fast-path semantics.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_json_meta: BTreeMap<String, FileMeta>,
    /// Content fingerprints for copied local directory dependencies, keyed by
    /// their project-relative source path. `None` means the state predates
    /// local-source freshness tracking and must miss the warm path once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_directory_hashes: Option<BTreeMap<String, LocalDirectoryFingerprint>>,
    pub aube_version: String,
    #[serde(default, rename = "prod")]
    pub section_filtered: bool,
    #[serde(default)]
    pub settings_hash: String,
    /// Resolved dependency-build scheduling policy. This is separate
    /// from `settings_hash` and `package_json_hashes`: add/remove
    /// legitimately changes the root manifest, but an unchanged
    /// `allowBuilds` / default-trust posture can still use package
    /// delta scheduling. Missing or changed values fall back to the
    /// full eligible build scan.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dep_build_policy_hash: String,
    /// Resolved minimum-release-age policy. Separate from
    /// `settings_hash` for the same reason as `dep_build_policy_hash`:
    /// `settings_hash` mixes in the raw workspace yaml, so an ordinary
    /// catalog / overrides / packageExtensions edit moves it without
    /// touching the age gate. Only a real age-policy change can
    /// invalidate lockfile picks that were already admitted under it,
    /// and that is the one question release-policy revalidation asks.
    /// Empty on fresh state or an install predating this field. That
    /// reads as "previous policy unknown", which does NOT revalidate —
    /// see [`release_policy_changed_since_last_run`] for why the two
    /// directions carry very different costs. The next install records
    /// the hash, and a genuine change is caught from then on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub release_policy_hash: String,
    /// Per-package content fingerprints from the last install,
    /// keyed by dep_path. Drives delta installs. Next install diffs
    /// these against the new lockfile's hashes and only re-fetches
    /// and re-links the entries that moved. Missing or stale values
    /// cascade to a full install. Purely additive, never
    /// load-bearing. Empty on fresh state or pre-delta aube.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_content_hashes: BTreeMap<String, String>,
    /// LtHash accumulator digest (hex) over every package in the
    /// installed graph. Wide-add multiset hash from
    /// `commands::install::delta::LtHash`. Match on this digest
    /// proves graph equivalence in a 32-byte compare and skips the
    /// O(N) map walk. Missing field cascades to the full diff.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub graph_lthash: String,
    /// Per-package Merkle subtree fingerprints, keyed by dep_path.
    /// Lets the delta path skip packages whose subtree matches the
    /// stored value even when their leaf changed. Peer-dep rewrites
    /// shuffle metadata without moving installed content, that is
    /// the case this catches. Missing field cascades to the
    /// leaf-only diff.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_subtree_hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_json_shape_digests: BTreeMap<String, String>,
    #[serde(default)]
    pub layout: Option<InstallLayoutState>,
    /// Spec keys (`name@version`) of registry deps whose build
    /// scripts were skipped on the last install because they are not
    /// on the `allowBuilds` allowlist. Persisted so the warm-path
    /// short-circuit can re-emit the same warning the full pipeline
    /// emits — without it, repeat installs go silent and users
    /// forget pending approvals. Empty on installs where the warning
    /// did not fire (no registry deps with lifecycle scripts, or
    /// `--ignore-scripts` / `strictDepBuilds=true` / `virtualStoreOnly`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreviewed_builds: Vec<String>,
    /// Spec keys of dependency builds the last install *wanted* to run
    /// and could not, for a reason that run invented rather than one
    /// the config decided: the `defaultTrust` floor's advisory-vetting
    /// gate not covering the install, or an allowed package's
    /// materialized directory not being on disk when the lifecycle
    /// phase ran.
    ///
    /// Distinct from `unreviewed_builds`, which is a *stable* denial —
    /// re-running the install could never change it, so sealing on it
    /// is correct. These can differ between two otherwise-identical
    /// runs, so a tree carrying them is incomplete rather than
    /// finished, and `check_needs_install` must not report it up to
    /// date (nubjs/nub#764).
    ///
    /// `None` means the state predates the field. Treated as "unknown,
    /// so re-check" when the state also records unreviewed builds; a
    /// single install turns it into `Some`, so the migration costs one
    /// full install per project and never repeats. Deliberately NOT
    /// `skip_serializing_if` empty — an empty vec must serialize, or
    /// every clean install would read back as legacy-unknown.
    #[serde(default)]
    pub deferred_dep_builds: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FreshnessState {
    lockfile_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lockfile_snapshot_name: Option<String>,
    /// See [`InstallState::lockfile_meta`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lockfile_meta: Option<FileMeta>,
    /// See [`InstallState::member_lockfile_hashes`]. Mirrored into the
    /// freshness sidecar so `check_needs_install` can verify per-member
    /// lockfiles without loading the full state file.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    member_lockfile_hashes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    member_lockfile_meta: BTreeMap<String, FileMeta>,
    package_json_hashes: BTreeMap<String, String>,
    /// Mtime + size per `package.json` keyed identically to
    /// `package_json_hashes`. Lets `package_jsons_stale` skip the
    /// BLAKE3 hash on the fast path: stat once, compare both fields,
    /// only re-hash when mtime or size changed. On a typical
    /// monorepo with 30 direct deps that's 30 BLAKE3 hashes per
    /// `aube run` startup collapsed to 30 stat calls.
    /// Missing field defaults to empty → falls through to the
    /// existing hash path, so older state files stay valid.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    package_json_meta: BTreeMap<String, FileMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_directory_hashes: Option<BTreeMap<String, LocalDirectoryFingerprint>>,
    #[serde(default, rename = "prod")]
    section_filtered: bool,
    #[serde(default)]
    settings_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    dep_build_policy_hash: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    package_json_shape_digests: BTreeMap<String, String>,
    #[serde(default)]
    layout: Option<InstallLayoutState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    unreviewed_builds: Vec<String>,
    /// See [`InstallState::deferred_dep_builds`]. Mirrored into the
    /// freshness sidecar because the warm-path check reads only this
    /// struct.
    #[serde(default)]
    deferred_dep_builds: Option<Vec<String>>,
}

/// `(size, mtime)` snapshot used by `R1` mtime fast path. mtime is
/// stored as (secs, nanos) since UNIX epoch so the comparison
/// preserves the resolution the underlying filesystem reports.
///
/// Linux ext4/btrfs/XFS and macOS APFS report nanosecond mtimes;
/// Windows NTFS reports 100-nanosecond ticks. Truncating to whole
/// seconds would let an in-place edit within the same second as the
/// previous install slip past the freshness check (very plausible in
/// CI where edits + installs happen within milliseconds). FAT32 and
/// other coarse-resolution filesystems still get correct behavior:
/// a same-second overwrite there has nanos == 0 on both samples, so
/// the fast path matches and we skip — but FAT32 does not promise
/// mtime granularity below 2 seconds anyway, so callers running on
/// it should not rely on the fast path. The size comparison still
/// catches any change that grows or shrinks the file.
///
/// # Accepted limitation
///
/// A rewrite that keeps the byte length identical *and* restores the
/// recorded mtime reports fresh without re-hashing. This is a
/// deliberate trade, accepted for every consumer of this type
/// (`lockfile_meta`, `package_json_meta`, `member_lockfile_meta`):
/// closing it would mean hashing the file on every check, which is the
/// cost the fast path exists to remove. Producing that collision takes
/// deliberate mtime restoration — ordinary editors, formatters, VCS
/// checkouts and package managers all move mtime forward, and a tool
/// that restores it defeats make, ninja and git's stat cache the same
/// way. Everything short of that collision is caught: content that
/// changes length fails the size compare, and content rewritten at any
/// other timestamp fails the mtime compare.
///
/// Callers must keep the failure direction one-way — capture the
/// snapshot *before* hashing, never after, so a write racing the
/// capture yields "re-hash unnecessarily" rather than "declare stale
/// content fresh".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub mtime_secs: i64,
    #[serde(default)]
    pub mtime_nanos: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalDirectoryFingerprint {
    pub content_hash: String,
    pub metadata_hash: String,
}

impl FileMeta {
    pub fn capture(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let dur = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        let (secs, nanos) = match dur {
            Some(d) => (d.as_secs() as i64, d.subsec_nanos()),
            None => (0, 0),
        };
        Some(Self {
            size: meta.len(),
            mtime_secs: secs,
            mtime_nanos: nanos,
        })
    }
}

impl From<&InstallState> for FreshnessState {
    fn from(state: &InstallState) -> Self {
        Self {
            lockfile_hash: state.lockfile_hash.clone(),
            lockfile_snapshot_name: state.lockfile_snapshot_name.clone(),
            lockfile_meta: state.lockfile_meta.clone(),
            member_lockfile_hashes: state.member_lockfile_hashes.clone(),
            member_lockfile_meta: state.member_lockfile_meta.clone(),
            package_json_hashes: state.package_json_hashes.clone(),
            package_json_meta: state.package_json_meta.clone(),
            local_directory_hashes: state.local_directory_hashes.clone(),
            section_filtered: state.section_filtered,
            settings_hash: state.settings_hash.clone(),
            dep_build_policy_hash: state.dep_build_policy_hash.clone(),
            package_json_shape_digests: state.package_json_shape_digests.clone(),
            layout: state.layout.clone(),
            unreviewed_builds: state.unreviewed_builds.clone(),
            deferred_dep_builds: state.deferred_dep_builds.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallLayoutState {
    pub linker: InstallLayoutMode,
    /// Tree-shaping settings captured from the successful install. Commands
    /// that inspect the materialized tree must not reconstruct it from current
    /// settings because `.npmrc` may have changed since installation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub modules_dir_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hoisting_limits: Option<InstallHoistingLimits>,
    /// Filename limit used when materializing the virtual store. This must be
    /// read from install state because an environment or CLI override may no
    /// longer be present when a later command inspects the installed tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_store_dir_max_length: Option<usize>,
    pub direct_entries: BTreeMap<String, Vec<String>>,
    pub packages: BTreeMap<String, InstalledPackageState>,
    /// Expected targets for dependency links nested inside shared global
    /// virtual-store entries. Keys are project-relative paths reached through
    /// `node_modules/.aube/<dep_path>`; values are the exact link targets.
    /// `None` identifies state written before this topology check existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gvs_nested_links: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct InstallLicenseState {
    fingerprint: String,
    pub licenses: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub linked_package_dirs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallLayoutMode {
    Isolated,
    Hoisted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallHoistingLimits {
    None,
    Workspaces,
    Dependencies,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackageState {
    pub name: String,
    pub version: String,
    pub package_json_path: String,
    #[serde(default)]
    pub package_json_hash: String,
    /// `link:` dependency — materialized as a bare symlink to an
    /// arbitrary on-disk directory (often a sibling's build output that
    /// may not exist yet). The symlink's own presence is verified via
    /// `direct_entries`; the target's `package.json` is deliberately not
    /// hashed here, matching pnpm, which treats a present (even dangling)
    /// link symlink as installed and never re-resolves on a link target
    /// change.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub link: bool,
}

/// Check if install is needed. Returns None if up-to-date, or Some(reason) if stale.
pub fn check_needs_install(project_dir: &Path) -> Option<String> {
    check_needs_install_inner(project_dir, None)
}

/// Variant of [`check_needs_install`] that also checks `settings_hash`
/// with the caller's `cli_flags` bag. Use from `install::run`'s warm
/// path short circuit so `--node-linker=hoisted` and friends also feed
/// the hash. `ensure_installed` (from `aube run`) uses the plain
/// [`check_needs_install`] on purpose, see the note there.
pub fn check_needs_install_with_flags(
    project_dir: &Path,
    cli_flags: &[(String, String)],
) -> Option<String> {
    check_needs_install_inner(project_dir, Some(cli_flags))
}

/// Whether the resolved install settings differ from the last successful
/// install. This is a narrow post-warm-miss probe for embedders that must
/// distinguish policy/config drift from unrelated work (manifest edits,
/// missing modules, lockfile changes). Missing legacy state returns `false`.
pub(crate) fn install_settings_changed_since_last_run(
    project_dir: &Path,
    cli_flags: &[(String, String)],
) -> bool {
    let (_, state_path) = resolve_paths(project_dir);
    let Some(state) = read_or_migrate_fresh_state(&state_path) else {
        return false;
    };
    hash_settings(project_dir, cli_flags) != state.settings_hash
}

fn check_needs_install_inner(
    project_dir: &Path,
    cli_flags: Option<&[(String, String)]>,
) -> Option<String> {
    // Surface the warm-path verdict on the diagnostic pipeline. A miss
    // re-runs the full resolve/fetch/delta/link pipeline (the visible
    // "re-link even though nothing changed" symptom), so when someone
    // reports an install that won't settle, `AUBE_LOG=debug aube
    // install` now names the exact freshness input that drifted instead
    // of leaving them to guess. Trace-level on a hit keeps the default
    // output clean.
    let reason = check_needs_install_compute(project_dir, cli_flags);
    match &reason {
        Some(reason) => tracing::debug!(
            project_dir = %project_dir.display(),
            "install warm path miss: {reason}"
        ),
        None => tracing::trace!(
            project_dir = %project_dir.display(),
            "install warm path hit: nothing to do"
        ),
    }
    reason
}

fn check_needs_install_compute(
    project_dir: &Path,
    cli_flags: Option<&[(String, String)]>,
) -> Option<String> {
    let _diag =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "check_needs_install");
    let (modules_dir, state_path) = resolve_paths(project_dir);

    // No state directory = never installed (or `rm -rf <modulesDir>` wiped it).
    let _diag_read =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "read_state_file");
    let mut state = match read_or_migrate_fresh_state(&state_path) {
        Some(s) => s,
        None => return Some("install state not found".into()),
    };
    drop(_diag_read);

    // In the default config the state file lives inside `modulesDir` so
    // `rm -rf <modules>` wipes it. But `stateDir` can point elsewhere,
    // in which case the state survives a manual modules-dir nuke and
    // the hashes below would falsely report "up to date". Guard against
    // that explicitly — zero-dep projects still get a modules directory
    // (with `.bin/`) from install, so the directory check covers them.
    if !modules_dir.exists() {
        let name = modules_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("node_modules");
        return Some(format!("{name} is missing"));
    }

    // Check lockfile hash. Honor `gitBranchLockfile` so a branch-specific
    // lockfile is the freshness anchor when present, but fall back to the
    // base lockfile names so a freshly-enabled branch doesn't loop on
    // "no lockfile found" — see `active_lockfile` for the full resolution
    // order.
    let _diag_lock = aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "lockfile_hash");
    let (lockfile_name, lockfile_path) = active_lockfile(project_dir);
    let mut lockfile_missing = false;
    let mut refreshed_lockfile_meta = false;
    if let Some(path) = lockfile_path {
        // This branch also absorbs a `sharedWorkspaceLockfile` flip from
        // false to true. The previous false-layout install left a
        // non-empty `member_lockfile_hashes` and an empty `lockfile_hash`
        // (no shared root lockfile then), but a shared root lockfile now
        // exists, so we land here. Its hash can't match the empty recorded
        // one, so we report a change and the full reinstall rewrites the
        // state into the shared shape.
        //
        // `(size, mtime)` fast path first, mirroring `package_json_meta`:
        // a matching snapshot skips re-reading + BLAKE3-hashing the
        // lockfile — the single largest file this check touches on big
        // monorepos. A miss (older state, mtime-only touch, or a real
        // edit) falls through to the hash.
        let current_meta = FileMeta::capture(&path);
        let meta_matches = match (&current_meta, &state.lockfile_meta) {
            (Some(current), Some(stored)) => current == stored,
            _ => false,
        };
        if !meta_matches {
            let current_hash = hash_file(&path);
            if current_hash != state.lockfile_hash {
                return Some(format!("{lockfile_name} has changed"));
            }
            // Hash matched but the snapshot drifted (touch(1), older
            // state without the field). Refresh it so the next check
            // takes the stat-only path.
            if let Some(current) = current_meta
                && state.lockfile_meta.as_ref() != Some(&current)
            {
                state.lockfile_meta = Some(current);
                refreshed_lockfile_meta = true;
            }
        }
    } else if state.member_lockfile_hashes.is_empty() {
        lockfile_missing = true;
    }
    // Under `sharedWorkspaceLockfile=false` the members own the lockfiles,
    // so verify them whenever any were recorded — independent of the root
    // lockfile. A workspace root that is *itself* a package carries its own
    // per-project lockfile, so `lockfile_path` is `Some` and the branch
    // above only checked the root; a member lockfile can still drift under
    // it. Checking here too keeps member add/remove/edit busting the warm
    // path instead of silently reporting "up to date". When no shared root
    // lockfile exists, this is also the only member check (the `else if`
    // above just avoids a spurious "no lockfile found").
    if !state.member_lockfile_hashes.is_empty()
        && let Some(reason) = member_lockfiles_stale(project_dir, &state)
    {
        return Some(reason);
    }
    drop(_diag_lock);

    let _diag_pjs =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "package_jsons_stale");
    if let Some(reason) = package_jsons_stale(project_dir, &state) {
        return Some(reason);
    }
    drop(_diag_pjs);

    // `package_jsons_stale` only revisits members recorded at the last
    // install, so a member added *since* has no recorded manifest hash and
    // is invisible to it. Under `sharedWorkspaceLockfile=false` the
    // `member_lockfiles_stale` check above already busts on a new member,
    // but the default shared layout records no per-member lockfile state,
    // so nothing there re-enumerates the workspace — a newly-added member
    // silently reports "up to date" (nubjs/nub#441). Fill that gap by
    // re-enumerating current members whenever no per-member lockfile state
    // was recorded (shared layout, and a per-project layout that had no
    // members last install); the non-empty case is already covered above.
    if state.member_lockfile_hashes.is_empty()
        && let Some(reason) = new_workspace_member(project_dir, &state)
    {
        return Some(reason);
    }

    if state.section_filtered {
        return Some(
            "previous install omitted dependency sections; auto-installing full graph".into(),
        );
    }

    // Every other input here describes what the tree was built FROM.
    // None of them says whether the last install's dependency lifecycle
    // phase finished, so a tree with an allowed-but-unrun build looked
    // identical to a complete one and no later install could heal it —
    // it reported "Already up to date" and exited 0 forever
    // (nubjs/nub#764). The install records what it could not build; a
    // miss here re-runs the pipeline, which is what gets those builds
    // another attempt.
    //
    // Only builds deferred for a RUN-SCOPED reason land in the list.
    // A package the config stably denies stays out, so the ordinary
    // unreviewed-builds steady state still takes the warm path.
    if let Some(reason) = deferred_dep_builds_stale(&state) {
        return Some(reason);
    }

    if state.dep_build_policy_hash.is_empty() {
        return Some("dependency build policy state is missing".into());
    }

    let _diag_layout =
        aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "verify_install_layout");
    if let Some(reason) = verify_install_layout(project_dir, state.layout.as_ref()) {
        return Some(reason);
    }
    drop(_diag_layout);

    if let Some(cli_flags) = cli_flags {
        let _diag_settings =
            aube_util::diag::Span::new(aube_util::diag::Category::Frozen, "settings_hash");
        let current_settings_hash = hash_settings(project_dir, cli_flags);
        if current_settings_hash != state.settings_hash {
            return Some("install settings or the active Node version have changed".into());
        }
    }

    // No settings_hash check when cli_flags is None. That path feeds
    // ensure_installed (aube run / exec / test). Those commands do not
    // care about install-shape settings changing because the tree is
    // still the tree built by the last install. Skipping this check
    // also avoids the asymmetry bug where `aube install
    // --node-linker=hoisted` writes a hash with cli_flags set, then
    // bare `aube run` reads without the flag, mismatches, and triggers
    // a spurious auto-install.
    if lockfile_missing {
        return Some("no lockfile found".into());
    }

    let Some(local_directory_hashes) = state.local_directory_hashes.as_mut() else {
        return Some("local dependency fingerprints not recorded".to_string());
    };
    let mut refreshed_metadata = false;
    for (rel, stored) in local_directory_hashes {
        let path = project_dir.join(rel);
        let current_metadata = match aube_store::directory_metadata_fingerprint(&path) {
            Ok(current) if current == stored.metadata_hash => continue,
            Ok(current) => current,
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %err,
                    "local dependency metadata fingerprint failed"
                );
                return Some(format!("local dependency {rel} is unreadable"));
            }
        };
        match aube_store::directory_content_fingerprint(&path) {
            Ok(current_hash) if current_hash == stored.content_hash => {
                stored.metadata_hash = current_metadata;
                refreshed_metadata = true;
            }
            Ok(_) => return Some(format!("local dependency {rel} has changed")),
            Err(err) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %err,
                    "local dependency content fingerprint failed"
                );
                return Some(format!("local dependency {rel} is unreadable"));
            }
        }
    }
    if (refreshed_metadata || refreshed_lockfile_meta)
        && let Err(err) = write_fresh_state(&state_path, &state)
    {
        tracing::debug!(
            path = %fresh_state_file(&state_path).display(),
            error = %err,
            "refresh local dependency metadata state failed"
        );
    }
    None
}

fn package_jsons_stale(project_dir: &Path, state: &FreshnessState) -> Option<String> {
    for (rel, stored_hash) in &state.package_json_hashes {
        let path = if rel == "." {
            project_dir.join("package.json")
        } else {
            project_dir.join(rel)
        };
        if !path.exists() {
            return Some(format!("{rel} is missing"));
        }
        // Fast path: if a `(size, mtime)` snapshot was recorded last
        // install AND it still matches, the file is byte-identical
        // (mtime + size pair is sufficient evidence that nothing was
        // overwritten in place). Skip the BLAKE3 hash entirely. Falls
        // through on schema upgrades where `package_json_meta` is
        // empty.
        if let Some(stored_meta) = state.package_json_meta.get(rel)
            && let Some(current_meta) = FileMeta::capture(&path)
            && current_meta == *stored_meta
        {
            continue;
        }
        if hash_file(&path) == *stored_hash {
            continue;
        }
        let stale_reason = || {
            if rel == "." {
                "package.json has changed".into()
            } else {
                format!("{rel} has changed")
            }
        };
        let Some(stored_shape) = state.package_json_shape_digests.get(rel) else {
            return Some(stale_reason());
        };
        let Ok(content) = std::fs::read(&path) else {
            return Some(stale_reason());
        };
        let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&content);
        let Ok(parsed) = parsed else {
            return Some(stale_reason());
        };
        let current_shape = hex::encode(aube_util::hash::manifest_install_shape_digest(&parsed));
        if current_shape != *stored_shape {
            return Some(stale_reason());
        }
    }
    None
}

/// Fingerprint every workspace member's lockfile for the
/// `sharedWorkspaceLockfile=false` layout. Returns `(hashes, meta)`
/// keyed by the member's importer path relative to `project_dir`.
///
/// Only meaningful when `sharedWorkspaceLockfile` is off; returns empty
/// maps for the default shared layout and for non-workspace projects so
/// the warm path's `member_lockfile_hashes.is_empty()` gate stays
/// inert there. *Every* current member is recorded — a member that has
/// no lockfile yet (e.g. a depless package) maps to an empty hash —
/// so the freshness check can also notice a member being added or
/// removed, not just edited.
fn collect_member_lockfile_state(
    project_dir: &Path,
) -> (BTreeMap<String, String>, BTreeMap<String, FileMeta>) {
    let mut hashes = BTreeMap::new();
    let mut metas = BTreeMap::new();
    let shared = crate::commands::with_settings_ctx(project_dir, |ctx| {
        aube_settings::resolved::shared_workspace_lockfile(ctx)
    });
    if shared {
        return (hashes, metas);
    }
    let Ok(members) = aube_workspace::find_workspace_packages(project_dir) else {
        return (hashes, metas);
    };
    for member_dir in members {
        let key = relative_path_or_original(&member_dir, project_dir);
        match active_lockfile(&member_dir).1 {
            Some(path) => {
                hashes.insert(key.clone(), hash_file(&path));
                if let Some(meta) = FileMeta::capture(&path) {
                    metas.insert(key, meta);
                }
            }
            None => {
                hashes.insert(key, String::new());
            }
        }
    }
    (hashes, metas)
}

/// Freshness check for the per-member lockfiles recorded under
/// `sharedWorkspaceLockfile=false`. Re-enumerates the current workspace
/// members so an added or removed member invalidates the warm path,
/// and compares each member's lockfile with the same mtime-then-hash
/// fast path [`package_jsons_stale`] uses. Returns `Some(reason)` on
/// the first drift, `None` when every member lockfile matches what the
/// last install recorded.
fn member_lockfiles_stale(project_dir: &Path, state: &FreshnessState) -> Option<String> {
    let members = aube_workspace::find_workspace_packages(project_dir).unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    for member_dir in &members {
        let key = relative_path_or_original(member_dir, project_dir);
        let Some(stored_hash) = state.member_lockfile_hashes.get(&key) else {
            return Some(format!("{key} is a new workspace member"));
        };
        seen.insert(key.clone());
        let Some(path) = active_lockfile(member_dir).1 else {
            // An empty stored hash means "member had no lockfile last
            // install" — still none now is consistent. A non-empty hash
            // means the member's lockfile vanished, which is drift.
            if stored_hash.is_empty() {
                continue;
            }
            return Some(format!("{key} lockfile is missing"));
        };
        if let Some(stored_meta) = state.member_lockfile_meta.get(&key)
            && let Some(current_meta) = FileMeta::capture(&path)
            && current_meta == *stored_meta
        {
            continue;
        }
        if hash_file(&path) != *stored_hash {
            return Some(format!("{key} lockfile has changed"));
        }
    }
    for key in state.member_lockfile_hashes.keys() {
        if !seen.contains(key) {
            return Some(format!("{key} was removed from the workspace"));
        }
    }
    None
}

/// Detect a workspace member added since the last install. Complements
/// [`package_jsons_stale`], which only revisits members it *recorded* (so it
/// catches an edited or deleted member but never a newly-added one). Each
/// current member is keyed exactly the way [`collect_package_json_hashes_from_manifests`]
/// records it — the root-as-member (`packages: ['.']`) under `.`, every other
/// member under `<rel>/package.json` — and a member whose key is absent from
/// `package_json_hashes` is new. Returns `Some(reason)` on the first new
/// member, `None` when every current member was already recorded.
/// Non-workspace projects enumerate to nothing and no-op.
/// Whether the last install left a dependency build owed. See
/// [`InstallState::deferred_dep_builds`] for what qualifies.
///
/// The `None` arm is the one-time migration for state written before
/// the field existed. Such a state cannot say what it deferred, and a
/// tree sealed by the pre-fix behavior is exactly the one to heal, so
/// it re-checks once unconditionally.
///
/// Deliberately NOT narrowed to "only when unreviewed builds were
/// recorded". The seal this migration exists for includes a
/// policy-ALLOWED package whose materialized directory was missing when
/// the lifecycle phase ran: allowed means it was never unreviewed, so
/// that state carries an empty unreviewed list and the narrow form
/// would leave exactly the trees it was written to rescue sealed
/// forever. The cost is one full install per project on upgrade, and
/// that install writes the field, so it never repeats.
fn deferred_dep_builds_stale(state: &FreshnessState) -> Option<String> {
    match state.deferred_dep_builds.as_deref() {
        Some([]) => None,
        Some(deferred) => Some(format!(
            "a dependency build did not run on the last install ({}); retrying",
            preview_list(deferred)
        )),
        None => Some(
            "install state predates dependency-build completion tracking; re-checking builds"
                .into(),
        ),
    }
}

/// Comma-joined preview of a spec-key list, capped so a napi-rs-style
/// tree of per-platform packages cannot splat into one unreadable line.
fn preview_list(items: &[String]) -> String {
    const MAX_INLINE: usize = 3;
    if items.len() <= MAX_INLINE {
        return items.join(", ");
    }
    format!(
        "{}, and {} more",
        items[..MAX_INLINE].join(", "),
        items.len() - MAX_INLINE
    )
}

fn new_workspace_member(project_dir: &Path, state: &FreshnessState) -> Option<String> {
    let members = aube_workspace::find_workspace_packages(project_dir).unwrap_or_default();
    for member_dir in &members {
        let rel = relative_path_or_original(member_dir, project_dir);
        // A workspace that lists its own root (`packages: ['.']`) yields an
        // empty rel for the root member, recorded under "." — mirror that
        // special-case so the root never reads as a spurious "new member"
        // and churns the warm path on every install.
        let is_root = rel.is_empty() || rel == ".";
        let manifest_key = if is_root {
            ".".to_string()
        } else {
            relative_path_or_original(&member_dir.join("package.json"), project_dir)
        };
        if !state.package_json_hashes.contains_key(&manifest_key) {
            let dir_key = if is_root { ".".to_string() } else { rel };
            return Some(format!("{dir_key} is a new workspace member"));
        }
    }
    None
}

/// Write state file after a successful install. `section_filtered` should be
/// `true` when the install omitted dependency sections, so that
/// `check_needs_install` knows to trigger a full re-install before commands
/// that expect the whole graph. `cli_flags` is the install's `opts.cli_flags`
/// bag — threaded through so the stored `settings_hash` reflects CLI overrides
/// (e.g. `--node-linker=hoisted`) that shaped the tree on disk.
pub struct WriteStateLayout<'a> {
    pub graph: &'a aube_lockfile::LockfileGraph,
    pub node_linker: aube_linker::NodeLinker,
    pub hoisting_limits: aube_linker::HoistingLimits,
    pub modules_dir_name: &'a str,
    pub aube_dir: &'a Path,
    pub virtual_store_dir_max_length: usize,
    pub placements: Option<&'a aube_linker::HoistedPlacements>,
    pub use_global_virtual_store: bool,
}

fn collect_gvs_nested_links(
    project_dir: &Path,
    layout: &WriteStateLayout<'_>,
) -> std::io::Result<Option<BTreeMap<String, String>>> {
    // O(edges) readlink(2) calls — parallelize per package. Each package
    // yields `Some(links)` on success or `None` when its topology is not
    // recordable, which aborts the whole collection exactly like the
    // serial version did.
    let per_package: Option<Vec<Vec<(String, String)>>> = layout
        .graph
        .packages
        .par_iter()
        .map(|(dep_path, pkg)| {
            let globally_shareable = pkg
                .local_source
                .as_ref()
                .is_none_or(aube_lockfile::LocalSource::is_globally_shareable);
            if !globally_shareable {
                return Some(Vec::new());
            }
            let aube_entry =
                layout
                    .aube_dir
                    .join(aube_lockfile::dep_path_filename::dep_path_to_filename(
                        dep_path,
                        layout.virtual_store_dir_max_length,
                    ));
            if std::fs::read_link(aube_entry).is_err() {
                // Compatibility-selected registry packages can be materialized
                // physically in the project even while the rest of the graph uses
                // GVS. Their links are not shared cache topology and the linker
                // handles them separately.
                return Some(Vec::new());
            }
            let package_dir = crate::commands::install::materialized_pkg_dir(
                layout.aube_dir,
                dep_path,
                &pkg.name,
                layout.virtual_store_dir_max_length,
                layout.placements,
            );
            let node_modules_dir =
                crate::commands::install::dep_modules_dir_for(&package_dir, &pkg.name);
            let mut links = Vec::with_capacity(pkg.dependencies.len());
            for dep_name in pkg.dependencies.keys().filter(|name| *name != &pkg.name) {
                let link_path = node_modules_dir.join(dep_name);
                let Ok(target) = std::fs::read_link(&link_path) else {
                    tracing::debug!(
                        path = %link_path.display(),
                        "global virtual store link topology is not recordable"
                    );
                    return None;
                };
                let Some(target) = target.to_str() else {
                    tracing::debug!(
                        path = %link_path.display(),
                        "global virtual store link target is not valid UTF-8"
                    );
                    return None;
                };
                links.push((
                    relative_path_or_original(&link_path, project_dir),
                    target.to_string(),
                ));
            }
            Some(links)
        })
        .collect();
    Ok(per_package.map(|groups| groups.into_iter().flatten().collect()))
}

pub struct WriteStateInput<'a> {
    pub section_filtered: bool,
    pub package_json_hashes: BTreeMap<String, String>,
    pub cli_flags: &'a [(String, String)],
    pub package_content_hashes: BTreeMap<String, String>,
    pub graph_lthash: String,
    pub package_subtree_hashes: BTreeMap<String, String>,
    pub dep_build_policy_hash: String,
    pub layout: WriteStateLayout<'a>,
    pub unreviewed_builds: Vec<String>,
    /// See [`InstallState::deferred_dep_builds`]. Always `Some` on the
    /// write side — an empty vec is the positive statement "nothing was
    /// owed", which is what distinguishes a fresh clean install from
    /// state written before the field existed.
    pub deferred_dep_builds: Vec<String>,
}

pub fn write_state(project_dir: &Path, input: WriteStateInput<'_>) -> Result<(), std::io::Error> {
    let WriteStateInput {
        section_filtered,
        package_json_hashes,
        cli_flags,
        package_content_hashes,
        graph_lthash,
        package_subtree_hashes,
        dep_build_policy_hash,
        layout,
        unreviewed_builds,
        deferred_dep_builds,
    } = input;

    let state_path = state_dir(project_dir);
    remove_legacy_state_file(&state_path)?;
    // Captured *before* the hash: an edit landing between the two makes
    // the stored meta stale relative to the hashed content, so the next
    // freshness check misses the stat-only fast path and falls through
    // to the hash — slow but correct. The reverse order would let a
    // fresh-meta/stale-hash pair declare an edited lockfile up to date.
    let lockfile_meta = active_lockfile(project_dir)
        .1
        .and_then(|path| FileMeta::capture(&path));
    let (lockfile_hash, lockfile_snapshot_name) =
        snapshot_active_lockfile(project_dir, &state_path)?;
    let settings_hash = hash_settings(project_dir, cli_flags);
    let release_policy_hash = hash_release_policy(project_dir, cli_flags);
    let install_layout = InstallLayoutState::from_graph(project_dir, &layout)?;

    let package_json_shape_digests: BTreeMap<String, String> = package_json_hashes
        .keys()
        .filter_map(|rel| {
            let path = if rel == "." {
                project_dir.join("package.json")
            } else {
                project_dir.join(rel)
            };
            let bytes = std::fs::read(&path).ok()?;
            let parsed: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            Some((
                rel.clone(),
                hex::encode(aube_util::hash::manifest_install_shape_digest(&parsed)),
            ))
        })
        .collect();

    // Capture (size, mtime) per manifest so the next freshness check
    // can skip the BLAKE3 hash on the warm path. See R1 docstring on
    // FreshnessState.package_json_meta.
    let package_json_meta: BTreeMap<String, FileMeta> = package_json_hashes
        .keys()
        .filter_map(|rel| {
            let path = if rel == "." {
                project_dir.join("package.json")
            } else {
                project_dir.join(rel)
            };
            FileMeta::capture(&path).map(|m| (rel.clone(), m))
        })
        .collect();

    // `sharedWorkspaceLockfile=false` writes one lockfile per member and
    // no shared root lockfile, so `lockfile_hash` is empty above.
    // Fingerprint each member's lockfile so the warm path has something
    // to verify; empty for the default shared layout.
    let (member_lockfile_hashes, member_lockfile_meta) = collect_member_lockfile_state(project_dir);
    let local_directory_hashes = collect_local_directory_hashes(project_dir, layout.graph)?;
    let license_fingerprint = license_state_fingerprint(&graph_lthash, &package_content_hashes);

    let state = InstallState {
        lockfile_hash,
        lockfile_snapshot_name,
        lockfile_meta,
        member_lockfile_hashes,
        member_lockfile_meta,
        package_json_hashes,
        package_json_meta,
        local_directory_hashes: Some(local_directory_hashes),
        aube_version: env!("CARGO_PKG_VERSION").to_string(),
        section_filtered,
        settings_hash,
        dep_build_policy_hash,
        release_policy_hash,
        package_content_hashes,
        graph_lthash,
        package_subtree_hashes,
        package_json_shape_digests,
        layout: Some(install_layout),
        unreviewed_builds,
        deferred_dep_builds: Some(deferred_dep_builds),
    };

    let fresh_state = FreshnessState::from(&state);
    if read_package_licenses(&state_path)
        .as_ref()
        .is_none_or(|licenses| licenses.fingerprint != license_fingerprint)
    {
        let license_state =
            collect_package_license_state(project_dir, &layout, license_fingerprint);
        let license_json = serde_json::to_vec(&license_state)?;
        aube_util::fs_atomic::atomic_write(&license_state_file(&state_path), &license_json)?;
    }
    // Compact, not pretty: the per-package maps make this file O(graph)
    // (and `gvs_nested_links` O(edges)), and it is re-read on every
    // freshness check — indentation roughly doubles the bytes parsed for
    // no consumer. `jq` remains the debugging story.
    let json = serde_json::to_vec(&state)?;
    aube_util::fs_atomic::atomic_write(&install_state_file(&state_path), &json)?;
    write_fresh_state(&state_path, &fresh_state)?;

    Ok(())
}

fn license_state_fingerprint(
    graph_lthash: &str,
    package_content_hashes: &BTreeMap<String, String>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(graph_lthash.as_bytes());
    for (dep_path, content_hash) in package_content_hashes {
        hasher.update(&(dep_path.len() as u64).to_le_bytes());
        hasher.update(dep_path.as_bytes());
        hasher.update(content_hash.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn collect_package_license_state(
    project_dir: &Path,
    layout: &WriteStateLayout<'_>,
    fingerprint: String,
) -> InstallLicenseState {
    let mut licenses = BTreeMap::new();
    let mut linked_package_dirs = BTreeMap::new();
    for (dep_path, pkg) in &layout.graph.packages {
        let package_dir = match pkg.local_source.as_ref() {
            Some(aube_lockfile::LocalSource::Link(path)) => {
                let package_dir = project_dir.join(path);
                linked_package_dirs.insert(
                    dep_path.clone(),
                    relative_path_or_original(&package_dir, project_dir),
                );
                package_dir
            }
            _ => crate::commands::install::materialized_pkg_dir(
                layout.aube_dir,
                dep_path,
                &pkg.name,
                layout.virtual_store_dir_max_length,
                layout.placements,
            ),
        };
        if let Some(license) =
            crate::commands::licenses::read_license(&package_dir).or_else(|| pkg.license.clone())
        {
            licenses.insert(dep_path.clone(), license);
        }
    }
    InstallLicenseState {
        fingerprint,
        licenses,
        linked_package_dirs,
    }
}

fn collect_local_directory_hashes(
    project_dir: &Path,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<BTreeMap<String, LocalDirectoryFingerprint>, std::io::Error> {
    let mut hashes = BTreeMap::new();
    for pkg in graph.packages.values() {
        let rel = match pkg.local_source.as_ref() {
            Some(aube_lockfile::LocalSource::Directory(rel))
            | Some(aube_lockfile::LocalSource::Portal(rel)) => rel,
            _ => continue,
        };
        let key = rel.to_string_lossy().replace('\\', "/");
        if hashes.contains_key(&key) {
            continue;
        }
        let (content_hash, metadata_hash) =
            aube_store::directory_fingerprints(&project_dir.join(rel))
                .map_err(std::io::Error::other)?;
        hashes.insert(
            key,
            LocalDirectoryFingerprint {
                content_hash,
                metadata_hash,
            },
        );
    }
    Ok(hashes)
}

fn snapshot_active_lockfile(
    project_dir: &Path,
    _state_path: &Path,
) -> Result<(String, Option<String>), std::io::Error> {
    let (name, path) = active_lockfile(project_dir);
    let Some(path) = path else {
        return Ok((String::new(), None));
    };
    let Ok(content) = std::fs::read(&path) else {
        return Ok((String::new(), None));
    };
    Ok((hash_bytes(&content), Some(name)))
}

/// Read per-package fingerprints from a project's state directory.
/// Returns `None` on any failure path (file missing, malformed
/// JSON, pre-delta aube). Caller treats that as "no prior
/// fingerprints, full install". Never surfaces an error because
/// delta is additive. A miss just lands on the full-install path.
pub fn read_state_package_content_hashes(project_dir: &Path) -> Option<BTreeMap<String, String>> {
    let state = read_state(&state_dir(project_dir))?;
    if state.package_content_hashes.is_empty() {
        return None;
    }
    Some(state.package_content_hashes)
}

/// All delta-install fields from the last install's state, extracted in
/// a single parse. `finalize` needs every one of them; reading each
/// through its own accessor re-parses the full O(graph) state file (and
/// re-resolves the settings context behind `state_dir`) once per field.
pub struct DeltaStateSnapshot {
    /// See [`InstallState::package_content_hashes`]. Empty map means
    /// "no prior fingerprints" (pre-delta aube or fresh state).
    pub package_content_hashes: BTreeMap<String, String>,
    /// See [`InstallState::graph_lthash`]. `None` when unrecorded.
    pub graph_lthash: Option<String>,
    /// See [`InstallState::package_subtree_hashes`]. Empty when
    /// unrecorded.
    pub package_subtree_hashes: BTreeMap<String, String>,
}

/// Read the delta-install snapshot in one state-file parse. `None` when
/// the state file is missing or malformed — callers treat that as "no
/// prior install, full pipeline", same as the per-field accessors.
pub fn read_state_delta_snapshot(project_dir: &Path) -> Option<DeltaStateSnapshot> {
    let state = read_state(&state_dir(project_dir))?;
    Some(DeltaStateSnapshot {
        package_content_hashes: state.package_content_hashes,
        graph_lthash: (!state.graph_lthash.is_empty()).then_some(state.graph_lthash),
        package_subtree_hashes: state.package_subtree_hashes,
    })
}

/// Read the installed layout snapshot used by the install warm path.
///
/// Missing layout state means the install predates layout tracking and
/// should take the normal path once to refresh derived metadata.
pub fn read_state_layout(project_dir: &Path) -> Option<InstallLayoutState> {
    read_state(&state_dir(project_dir))?.layout
}

/// Persist the exact hoisted tree produced by the linker. This is a separate
/// sidecar because filtered installs deliberately do not replace the main
/// freshness state, while commands such as `rebuild` still need to inspect
/// the tree that is actually on disk.
pub fn write_hoisted_placements(
    project_dir: &Path,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> Result<(), std::io::Error> {
    let state_path = state_dir(project_dir);
    let Some(placements) = placements else {
        if state_path.is_file() {
            return Ok(());
        }
        let path = state_path.join(HOISTED_PLACEMENTS_FILE_NAME);
        return match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        };
    };
    remove_legacy_state_file(&state_path)?;
    let path = state_path.join(HOISTED_PLACEMENTS_FILE_NAME);
    let mut recorded: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (dep_path, package_dir) in placements.iter() {
        recorded
            .entry(dep_path.to_string())
            .or_default()
            .push(relative_path_or_original(package_dir, project_dir));
    }
    let json = serde_json::to_vec(&recorded)?;
    aube_util::fs_atomic::atomic_write(&path, &json)
}

/// Read the exact linker-produced hoisted placement map. `None` means the
/// install predates the sidecar (or it is unreadable), so callers may use the
/// legacy planner-based reconstruction as a compatibility fallback.
pub fn read_hoisted_placements(project_dir: &Path) -> Option<aube_linker::HoistedPlacements> {
    let path = state_dir(project_dir).join(HOISTED_PLACEMENTS_FILE_NAME);
    let recorded: BTreeMap<String, Vec<String>> =
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let by_dep_path = recorded
        .into_iter()
        .map(|(dep_path, paths)| {
            let paths = paths
                .into_iter()
                .map(|path| project_dir.join(path))
                .filter(|path| path.exists())
                .collect();
            (dep_path, paths)
        })
        .collect();
    Some(aube_linker::HoistedPlacements::from_package_dirs(
        by_dep_path,
    ))
}

/// Read licenses captured at install time without adding them to the main
/// freshness state parsed by every warm install.
pub fn read_state_package_licenses(project_dir: &Path) -> InstallLicenseState {
    let state_path = state_dir(project_dir);
    read_package_licenses(&state_path)
        .or_else(|| {
            read_package_licenses(&project_dir.join(DEFAULT_STATE_DIR).join(state_dir_name()))
        })
        .unwrap_or_default()
}

/// Read layout state from the default `node_modules` location.
///
/// This fallback is useful after `modulesDir` changes: resolving the current
/// state path then points at the new, not-yet-installed tree while the state
/// describing the materialized tree remains under `node_modules`.
pub fn read_default_state_layout(project_dir: &Path) -> Option<InstallLayoutState> {
    read_state(&project_dir.join(DEFAULT_STATE_DIR).join(state_dir_name()))?.layout
}

/// Read stored subtree hashes for delta installs that want to
/// prune at the subtree granularity rather than the leaf
/// granularity. Absent field cascades to the leaf diff path.
pub fn read_state_subtree_hashes(project_dir: &Path) -> Option<BTreeMap<String, String>> {
    let state = read_state(&state_dir(project_dir))?;
    if state.package_subtree_hashes.is_empty() {
        return None;
    }
    Some(state.package_subtree_hashes)
}

/// Read the resolved dependency-build policy hash from the last
/// install. Missing field means the install predates lifecycle delta
/// scheduling, so callers fall back to the full eligible build scan.
pub fn read_state_dep_build_policy_hash(project_dir: &Path) -> Option<String> {
    let state = read_state(&state_dir(project_dir))?;
    if state.dep_build_policy_hash.is_empty() {
        return None;
    }
    Some(state.dep_build_policy_hash)
}

/// Whether the resolved minimum-release-age policy differs from the one the
/// last install recorded.
///
/// An unknown previous policy — state missing, or written before
/// `release_policy_hash` existed — answers `false`.
///
/// State the cost plainly, because it is real: on a project's first install
/// that does actual work after upgrading past this field, a RAISED age gate is
/// NOT applied to the versions already in the lockfile, and that same install
/// records the new hash, so the raise is never retried. Picks sitting between
/// the old and the new cutoff are kept, and leave the window by aging out
/// rather than by any check. `--force` re-resolves under the current gate and
/// is how to apply a raise retroactively. An empty hash cannot establish what
/// gate, if any, the existing picks once cleared — a lockfile written by
/// another package manager never saw this gate at all — so no safety argument
/// is available from the picks themselves.
///
/// It is accepted because the alternative is worse and certain. Answering
/// "changed" here fires on the upgrade hop for EVERY project, whatever moved
/// its settings — a comment in `.npmrc`, a catalog entry, a different Node
/// major — and revalidation discards the lockfile and re-picks every range at
/// newest. That is the reported defect this narrowing exists to remove, and it
/// would survive one full install per project. A deferred gate re-check is
/// recoverable with one flag; a whole-graph version bump landed in someone's
/// lockfile is not.
///
/// The two arms differ in reachability, not in answer: `read_state` returning
/// `None` is already unreachable from the only caller, because
/// `install_settings_changed_since_last_run` answers `false` on missing state.
pub(crate) fn release_policy_changed_since_last_run(
    project_dir: &Path,
    cli_flags: &[(String, String)],
) -> bool {
    let Some(state) = read_state(&state_dir(project_dir)) else {
        return false;
    };
    if state.release_policy_hash.is_empty() {
        return false;
    }
    state.release_policy_hash != hash_release_policy(project_dir, cli_flags)
}

/// Read the node-linker layout the last install materialized, if
/// recorded. Authoritative over re-resolving settings because it
/// reflects a `--node-linker=hoisted` CLI override that was never
/// persisted to `.npmrc`. `None` when state is missing or predates
/// layout tracking.
pub fn read_state_layout_linker(project_dir: &Path) -> Option<InstallLayoutMode> {
    Some(read_state(&state_dir(project_dir))?.layout?.linker)
}

/// Name of the sentinel that marks a link phase as IN PROGRESS.
fn link_sentinel(project_dir: &Path) -> PathBuf {
    state_dir(project_dir).join("link-in-progress")
}

/// Mark the tree as mid-link. Called before the linker is allowed to
/// mutate `node_modules`, and cleared only once the install finishes.
///
/// This exists because the install state alone cannot answer "is the tree
/// on disk intact?". State is rewritten at the END of a successful install
/// and removed only on `--force` / a GVS switch, so state written by a
/// SUCCESSFUL install outlives a LATER install that died partway through
/// relinking. Anything keying reuse on state alone would then treat a
/// half-materialized package directory as complete — the same class of bug
/// as nubjs/nub#552, arrived at from the other side.
///
/// Best-effort: a sentinel we fail to write just means the next install
/// declines to reuse and does the full wipe-and-refill, which is exactly
/// today's behavior. Failing the install over it would be worse.
pub fn mark_link_in_progress(project_dir: &Path) {
    let path = link_sentinel(project_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, b"");
}

/// Clear the mid-link sentinel. Called once the install has completed and
/// the tree is known to match the state written alongside it.
pub fn clear_link_in_progress(project_dir: &Path) {
    let _ = std::fs::remove_file(link_sentinel(project_dir));
}

/// Whether the last link phase ran to completion. `false` means either a
/// link is in flight or one died partway, and in both cases nothing on
/// disk may be reused.
pub fn link_completed_cleanly(project_dir: &Path) -> bool {
    !link_sentinel(project_dir).exists()
}

/// Read the unreviewed-builds spec keys recorded by the last
/// install. Powers warm-path warning re-emission so repeat
/// installs keep nudging users about pending build approvals.
/// Returns an empty vec when state is missing or pre-feature.
pub fn read_state_unreviewed_builds(project_dir: &Path) -> Vec<String> {
    read_or_migrate_fresh_state(&state_dir(project_dir))
        .map(|s| s.unreviewed_builds)
        .unwrap_or_default()
}

/// Spec keys the last install recorded as owed a build, or `None` when
/// the state cannot say — no state file, or state predating the field.
/// See [`InstallState::deferred_dep_builds`].
///
/// `None` MUST stay distinct from `Some([])`. Flattening the two turns
/// the one-time migration into a no-op exactly where it matters: legacy
/// state busts freshness, but the lifecycle delta would then still
/// narrow to changed packages, drop the stranded build, and write
/// `Some([])` — recording the tree as clean on the very install meant
/// to heal it.
pub fn read_state_deferred_dep_builds(project_dir: &Path) -> Option<Vec<String>> {
    read_or_migrate_fresh_state(&state_dir(project_dir))?.deferred_dep_builds
}

/// Remove the install state directory. Missing state is not an error.
pub fn remove_state(project_dir: &Path) -> Result<(), std::io::Error> {
    let state_path = state_dir(project_dir);
    let result = if state_path.is_dir() {
        std::fs::remove_dir_all(state_path)
    } else {
        std::fs::remove_file(state_path)
    };
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Pick the lockfile path that an install in `project_dir` will actually
/// read or write through, mirroring `aube_lockfile::lockfile_candidates`.
///
/// Order:
///   1. `aube-lock.<branch>.yaml` (only if `gitBranchLockfile` is on
///      and we resolve a branch — the preferred value).
///   2. `aube-lock.yaml` — the default base file. Critical for the
///      freshly-enabled-branch case: the branch file hasn't been
///      written yet, but the base file exists, and without this step
///      `check_needs_install` would fall through to pnpm lockfiles
///      (or to `None` on aube-lock projects) and loop on
///      every `aube run` / `aube exec`.
///   3. `pnpm-lock.<branch>.yaml` / `pnpm-lock.yaml`.
///
/// Returns the display name (for messages) plus the resolved path, if
/// any exists.
fn active_lockfile(project_dir: &Path) -> (String, Option<PathBuf>) {
    let basename = aube_util::embedder().lockfile_basename;
    let preferred = aube_lockfile::aube_lock_filename(project_dir);
    let preferred_path = project_dir.join(&preferred);
    if preferred_path.exists() {
        return (preferred, Some(preferred_path));
    }
    // Freshly-enabled `gitBranchLockfile`: base file exists, branch
    // file does not. Pick up the base so we don't loop on every run.
    if preferred != basename {
        let base = project_dir.join(basename);
        if base.exists() {
            return (basename.to_string(), Some(base));
        }
    }
    // A legacy canonical name still honored on read during a rename
    // transition (e.g. nub's `lock.yaml` superseded by `package.lock`): an
    // unmigrated project must still recognize its lockfile so the freshness
    // check doesn't loop on "no lockfile found".
    for legacy in aube_util::embedder().lockfile_legacy_basenames {
        let legacy_path = project_dir.join(legacy);
        if legacy_path.exists() {
            return ((*legacy).to_string(), Some(legacy_path));
        }
    }
    // Preserve pnpm-lock.yaml (and its branch variant) as an active
    // lockfile when the project already uses it. Derived from the shared
    // branch resolver (`pnpm_lock_filename`), NOT by string-rewriting the
    // canonical name — the canonical extension may not be `.yaml`.
    let pnpm_preferred = aube_lockfile::pnpm_lock_filename(project_dir);
    if pnpm_preferred != "pnpm-lock.yaml" {
        let pnpm_branch = project_dir.join(&pnpm_preferred);
        if pnpm_branch.exists() {
            return (pnpm_preferred, Some(pnpm_branch));
        }
    }
    let pnpm_base = project_dir.join("pnpm-lock.yaml");
    if pnpm_base.exists() {
        return ("pnpm-lock.yaml".to_string(), Some(pnpm_base));
    }
    // Also track npm/yarn/bun lockfiles written by the format-preserving
    // install path, so `check_needs_install` doesn't loop on "no lockfile
    // found" for projects that use these formats.
    for name in [
        "bun.lock",
        "yarn.lock",
        "npm-shrinkwrap.json",
        "package-lock.json",
    ] {
        let path = project_dir.join(name);
        if path.exists() {
            return (name.to_string(), Some(path));
        }
    }
    (preferred, None)
}

fn read_state(state_path: &Path) -> Option<InstallState> {
    if state_path.is_file() {
        let _ = std::fs::remove_file(state_path);
        return None;
    }
    let content = std::fs::read_to_string(install_state_file(state_path)).ok()?;
    serde_json::from_str(&content).ok()
}

fn install_state_file(state_path: &Path) -> PathBuf {
    state_path.join(INSTALL_STATE_FILE_NAME)
}

/// The no-integrity content-address binding: the sha512 some install
/// computed for a registry tarball URL, recorded alongside the URL it
/// addresses so the content-addressed file is self-describing and a
/// (vanishingly unlikely) blake3 key collision is caught on read.
#[derive(Serialize, Deserialize)]
struct NoIntegrityBinding {
    url: String,
    sha512: String,
}

/// The GLOBAL directory holding no-integrity bindings, one
/// content-addressed file per resolved tarball URL, under the store's `v1/`
/// dir next to the CAS `files/` and `index/`.
///
/// WHY it lives in the store and not in `node_modules`: the binding indexes
/// store bytes, so it shares the store's lifetime. The dominant CI warm
/// pattern restores the store cache but `rm -rf node_modules`; a binding
/// inside `node_modules` was wiped with it, forcing a network re-fetch of
/// the ~hundreds of no-integrity packages the store already held. In the
/// store it survives the wipe, so the warm/offline relink content-addresses
/// them straight from the store. Tracks a `storeDir` override so the binding
/// moves with the store it indexes.
pub fn no_integrity_dir(project_dir: &Path) -> PathBuf {
    crate::commands::store_v1_dir(project_dir).join("no-integrity")
}

/// Every directory a binding may be READ from, [`no_integrity_dir`] first,
/// then the read-only global store's when the default store is unwritable
/// and installs are writing a project-local one. Only directories that
/// exist, so an empty list means nothing is bound anywhere and the caller
/// can skip building a registry client.
pub fn no_integrity_read_dirs(project_dir: &Path) -> Vec<PathBuf> {
    let dirs = crate::commands::store_v1_dirs(project_dir);
    std::iter::once(dirs.primary)
        .chain(dirs.read_fallback)
        .map(|v1| v1.join("no-integrity"))
        .filter(|dir| dir.exists())
        .collect()
}

/// [`read_no_integrity_binding`] across [`no_integrity_read_dirs`]: the
/// first directory holding a binding for `url` answers.
pub fn read_no_integrity_binding_in(dirs: &[PathBuf], url: &str) -> Option<String> {
    dirs.iter()
        .find_map(|dir| read_no_integrity_binding(dir, url))
}

fn no_integrity_binding_file(dir: &Path, url: &str) -> PathBuf {
    dir.join(format!(
        "{}.json",
        hex::encode(blake3::hash(url.as_bytes()).as_bytes())
    ))
}

/// Read the global no-integrity binding for one resolved tarball `url`: the
/// sha512 some install computed for it, or `None` if nothing is bound. Used
/// directly by the streaming-resolve path (which resolves packages one at a
/// time and so can't pre-build a projection map); the batch warm path goes
/// through [`read_no_integrity_index_for`].
pub fn read_no_integrity_binding(dir: &Path, url: &str) -> Option<String> {
    let content = std::fs::read_to_string(no_integrity_binding_file(dir, url)).ok()?;
    let binding: NoIntegrityBinding = serde_json::from_str(&content).ok()?;
    // The URL is hashed into the filename; re-check it to reject a key
    // collision rather than serve a different URL's bytes.
    (binding.url == url).then_some(binding.sha512)
}

/// Project the GLOBAL URL-keyed no-integrity bindings down to the
/// `<registry-name>@<version>` → sha512 map the warm classifier, the linker
/// `load_index` fallback, and `ignored-builds` consume — so those readers
/// keep their existing coordinate lookup and never need a registry client.
///
/// A no-integrity package's binding is keyed by the registry tarball URL it
/// resolves to, derived from THIS project's configured registry (`.npmrc`)
/// via `client.tarball_url` — NOT the lockfile's baked `resolved` field. The
/// user's registry config, never a copied or hostile lockfile, defines the
/// trust domain. The same coordinate resolved from a different registry
/// hashes to a different key, so a project scoped to a locked-down registry
/// can never link bytes another project bound from a different registry
/// (the #212/#220 closure). A globally-shared binding for the SAME URL is
/// intentional: it matches npm's and pnpm's per-URL global caches and is
/// what lets a second project — and a `node_modules`-wiped relink — reuse
/// the bytes without a re-fetch.
pub fn read_no_integrity_index_for<'a, I>(project_dir: &Path, pkgs: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = &'a aube_lockfile::LockedPackage>,
{
    let dirs = no_integrity_read_dirs(project_dir);
    // Nothing bound yet (fresh store) → skip building a registry client.
    if dirs.is_empty() {
        return BTreeMap::new();
    }
    let client = crate::commands::make_client(project_dir);
    pkgs.into_iter()
        .filter(|pkg| pkg.integrity.is_none() && pkg.local_source.is_none())
        .filter_map(|pkg| {
            let url = client.tarball_url(pkg.registry_name(), &pkg.version);
            read_no_integrity_binding_in(&dirs, &url)
                .map(|sri| (format!("{}@{}", pkg.registry_name(), pkg.version), sri))
        })
        .collect()
}

/// Persist `url` → `sha512` bindings into the global store, one
/// content-addressed file per URL. Idempotent: a URL already bound to the
/// same sha512 is skipped; a changed value (a re-publish under the same URL)
/// overwrites atomically. One file per URL means concurrent installs binding
/// different URLs never contend on a shared file.
pub fn write_no_integrity_bindings(
    dir: &Path,
    bindings: &BTreeMap<String, String>,
) -> Result<(), std::io::Error> {
    if bindings.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    for (url, sri) in bindings {
        let path = no_integrity_binding_file(dir, url);
        if read_no_integrity_binding(dir, url).as_deref() == Some(sri.as_str()) {
            continue;
        }
        let json = serde_json::to_string(&NoIntegrityBinding {
            url: url.clone(),
            sha512: sri.clone(),
        })?;
        aube_util::fs_atomic::atomic_write(&path, json.as_bytes())?;
    }
    Ok(())
}

fn license_state_file(state_path: &Path) -> PathBuf {
    state_path.join(LICENSE_STATE_FILE_NAME)
}

fn read_package_licenses(state_path: &Path) -> Option<InstallLicenseState> {
    let content = std::fs::read(license_state_file(state_path)).ok()?;
    serde_json::from_slice(&content).ok()
}

fn fresh_state_file(state_path: &Path) -> PathBuf {
    state_path.join(FRESH_STATE_FILE_NAME)
}

fn read_fresh_state(state_path: &Path) -> Option<FreshnessState> {
    if state_path.is_file() {
        let _ = std::fs::remove_file(state_path);
        return None;
    }
    let content = std::fs::read_to_string(fresh_state_file(state_path)).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_or_migrate_fresh_state(state_path: &Path) -> Option<FreshnessState> {
    if let Some(state) = read_fresh_state(state_path) {
        return Some(state);
    }
    let state = FreshnessState::from(&read_state(state_path)?);
    let _ = write_fresh_state(state_path, &state);
    Some(state)
}

fn write_fresh_state(state_path: &Path, state: &FreshnessState) -> Result<(), std::io::Error> {
    // Compact for the same reason as `state.json` above — this sidecar
    // is parsed on every `aube run`/`exec`/`test` startup.
    let json = serde_json::to_vec(state)?;
    aube_util::fs_atomic::atomic_write(&fresh_state_file(state_path), &json)
}

fn remove_legacy_state_file(state_path: &Path) -> Result<(), std::io::Error> {
    if state_path.is_file() {
        std::fs::remove_file(state_path)?;
    }
    Ok(())
}

impl InstallLayoutState {
    fn from_graph(
        project_dir: &Path,
        layout: &WriteStateLayout<'_>,
    ) -> Result<Self, std::io::Error> {
        let linker = match layout.node_linker {
            aube_linker::NodeLinker::Isolated => InstallLayoutMode::Isolated,
            aube_linker::NodeLinker::Hoisted => InstallLayoutMode::Hoisted,
        };
        let hoisting_limits = matches!(layout.node_linker, aube_linker::NodeLinker::Hoisted)
            .then_some(match layout.hoisting_limits {
                aube_linker::HoistingLimits::None => InstallHoistingLimits::None,
                aube_linker::HoistingLimits::Workspaces => InstallHoistingLimits::Workspaces,
                aube_linker::HoistingLimits::Dependencies => InstallHoistingLimits::Dependencies,
            });
        // Record each importer's direct-dependency symlinks — the root
        // (`.`) *and* every workspace member — relative to `project_dir`.
        // `verify_install_layout` walks these, so tracking members means a
        // deleted or incompletely-linked member `node_modules` busts the
        // warm path. Previously only `.` was tracked, so `rm -rf
        // <member>/node_modules && aube install` short-circuited to
        // "Already up to date" and never relinked the member.
        let mut direct_entries = BTreeMap::new();
        for (importer, deps) in &layout.graph.importers {
            let importer_dir = if importer == "." {
                project_dir.to_path_buf()
            } else {
                aube_util::path::normalize_lexical(&project_dir.join(importer))
            };
            let entries = deps
                .iter()
                .map(|dep| {
                    let importer_entry = importer_dir.join(layout.modules_dir_name).join(&dep.name);
                    // A workspace-wide hoist may satisfy this direct edge from
                    // an ancestor node_modules. Record the first placement Node
                    // can actually see instead of a local slot the linker
                    // intentionally left absent.
                    let entry = layout
                        .placements
                        .filter(|_| matches!(layout.node_linker, aube_linker::NodeLinker::Hoisted))
                        .and_then(|placements| {
                            let package_dirs = placements.all_package_dirs(&dep.dep_path);
                            importer_dir.ancestors().find_map(|ancestor| {
                                let candidate =
                                    ancestor.join(layout.modules_dir_name).join(&dep.name);
                                package_dirs.contains(&candidate).then_some(candidate)
                            })
                        })
                        .unwrap_or(importer_entry);
                    relative_path_or_original(&entry, project_dir)
                })
                .collect();
            direct_entries.insert(importer.clone(), entries);
        }

        let mut packages = BTreeMap::new();
        let direct_dep_paths: std::collections::BTreeSet<String> = layout
            .graph
            .importers
            .get(".")
            .into_iter()
            .flat_map(|deps| deps.iter().map(|dep| dep.dep_path.clone()))
            .collect();
        for (dep_path, pkg) in &layout.graph.packages {
            let is_link = matches!(
                pkg.local_source.as_ref(),
                Some(aube_lockfile::LocalSource::Link(_))
            );
            let package_dir = match pkg.local_source.as_ref() {
                Some(aube_lockfile::LocalSource::Link(path)) => project_dir.join(path),
                _ => crate::commands::install::materialized_pkg_dir(
                    layout.aube_dir,
                    dep_path,
                    &pkg.name,
                    layout.virtual_store_dir_max_length,
                    layout.placements,
                ),
            };
            if !direct_dep_paths.contains(dep_path) {
                continue;
            }
            let package_json_path = package_dir.join("package.json");
            packages.insert(
                dep_path.clone(),
                InstalledPackageState {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    package_json_path: relative_path_or_original(&package_json_path, project_dir),
                    package_json_hash: hash_file_if_exists(&package_json_path).unwrap_or_default(),
                    link: is_link,
                },
            );
        }

        let gvs_nested_links = if layout.use_global_virtual_store
            && matches!(layout.node_linker, aube_linker::NodeLinker::Isolated)
        {
            collect_gvs_nested_links(project_dir, layout)?
        } else {
            None
        };

        Ok(Self {
            linker,
            modules_dir_name: layout.modules_dir_name.to_string(),
            hoisting_limits,
            virtual_store_dir_max_length: Some(layout.virtual_store_dir_max_length),
            direct_entries,
            packages,
            gvs_nested_links,
        })
    }
}

fn verify_install_layout(
    project_dir: &Path,
    layout: Option<&InstallLayoutState>,
) -> Option<String> {
    let layout = layout?;
    for entries in layout.direct_entries.values() {
        for rel in entries {
            let path = project_dir.join(rel);
            // `symlink_metadata` (lstat) checks the entry itself, not the
            // path it resolves to. A `link:` dep points at an arbitrary
            // directory — often a sibling's build output that may not be
            // built yet — so `exists()` (which follows the symlink) would
            // report a perfectly-installed link symlink as "missing" and
            // bust the warm path on every install. pnpm uses the same
            // lstat semantics here.
            if path.symlink_metadata().is_err() {
                return Some(format!("installed entry missing: {rel}"));
            }
        }
    }

    for pkg in layout.packages.values() {
        // `link:` deps are bare symlinks (verified above via
        // `direct_entries`). Their target is an arbitrary on-disk
        // directory whose `package.json` may legitimately be absent (an
        // unbuilt sibling) or churn independently of the lockfile, so
        // hashing it here would re-trigger installs forever. pnpm doesn't
        // track link targets in its up-to-date check either.
        if pkg.link {
            continue;
        }
        let pkg_json_path = project_dir.join(&pkg.package_json_path);
        let current_hash = hash_file_if_exists(&pkg_json_path);
        if let Some(current_hash) = current_hash
            && !pkg.package_json_hash.is_empty()
            && pkg.package_json_hash != empty_blake3_hash()
            && current_hash == pkg.package_json_hash
        {
            continue;
        }
        let manifest = match read_installed_package_manifest(&pkg_json_path) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                return Some(format!(
                    "installed package metadata missing: {}",
                    pkg.package_json_path
                ));
            }
            Err(_) => {
                return Some(format!(
                    "installed package metadata unreadable: {}",
                    pkg.package_json_path
                ));
            }
        };
        if manifest.name != pkg.name || manifest.version != pkg.version {
            return Some(format!(
                "installed package metadata changed: {}",
                pkg.package_json_path
            ));
        }
    }

    if let Some(reason) = stale_gvs_nested_link(project_dir, layout) {
        return Some(reason);
    }

    None
}

pub fn gvs_nested_links_are_current(project_dir: &Path, layout: &InstallLayoutState) -> bool {
    layout.gvs_nested_links.is_some() && stale_gvs_nested_link(project_dir, layout).is_none()
}

fn stale_gvs_nested_link(project_dir: &Path, layout: &InstallLayoutState) -> Option<String> {
    // One entry per (package, dependency) edge in the graph — easily
    // 10^5 on a large monorepo — so scan in parallel. `find_map_first`
    // keeps the reported entry deterministic (first in map order) while
    // letting rayon fan the readlink(2) calls out across threads.
    layout
        .gvs_nested_links
        .as_ref()?
        .par_iter()
        .find_map_first(|(rel, expected)| {
            let path = project_dir.join(rel);
            match std::fs::read_link(&path) {
                Ok(actual) if actual == Path::new(expected) => None,
                Ok(_) => Some(format!("global virtual store link changed: {rel}")),
                Err(_) => Some(format!("global virtual store link missing: {rel}")),
            }
        })
}

#[derive(Deserialize)]
struct InstalledManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

fn read_installed_package_manifest(
    path: &Path,
) -> Result<Option<InstalledManifest>, std::io::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let parsed = serde_json::from_str(&content)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    Ok(Some(parsed))
}

pub fn collect_package_json_hashes_from_manifests(
    project_dir: &Path,
    manifests: &[(String, aube_manifest::PackageJson)],
) -> BTreeMap<String, String> {
    manifests
        .par_iter()
        .filter_map(|(rel, _)| {
            let pkg_json = if rel == "." {
                project_dir.join("package.json")
            } else {
                project_dir.join(rel).join("package.json")
            };
            if !pkg_json.is_file() {
                return None;
            }
            let key = if rel == "." {
                ".".to_string()
            } else {
                relative_path_or_original(&pkg_json, project_dir)
            };
            Some((key, hash_file(&pkg_json)))
        })
        .collect()
}

fn hash_settings(project_dir: &Path, cli_flags: &[(String, String)]) -> String {
    // hash resolved settings not raw file bytes. old byte hash tripped on
    // noop edits like `optimisticRepeatInstall=true` (same as default).
    // resolved values collapse defaults to identical hash. cli flags feed
    // through ctx so `--node-linker=hoisted` also shows up here.
    // workspace yaml bytes still hashed on top, covers map shaped settings
    // like catalog, overrides, packageExtensions, onlyBuiltDependencies
    // where any change means a real re-resolve.
    let files = crate::commands::FileSources::load(project_dir);
    let (ws_config, raw_workspace) =
        aube_manifest::workspace::load_both(project_dir).unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let ctx = files.ctx(&raw_workspace, &env, cli_flags);
    let mut hasher = blake3::Hasher::new();
    // node_linker, hoist family, modules_dir, import method. these shape
    // the tree on disk. flip any of them, linker needs to rebuild.
    let node_linker = aube_settings::resolved::node_linker(&ctx);
    hasher.update(b"node_linker=");
    hasher.update(format!("{node_linker:?}").as_bytes());
    hasher.update(b"\0");
    let hoist = aube_settings::resolved::hoist(&ctx);
    hasher.update(format!("hoist={hoist}\0").as_bytes());
    let shamefully_hoist = aube_settings::resolved::shamefully_hoist(&ctx);
    hasher.update(format!("shamefully_hoist={shamefully_hoist}\0").as_bytes());
    let hoist_pattern = aube_settings::resolved::hoist_pattern(&ctx);
    hasher.update(b"hoist_pattern=");
    for p in &hoist_pattern {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let public_hoist_pattern = aube_settings::resolved::public_hoist_pattern(&ctx);
    hasher.update(b"public_hoist_pattern=");
    for p in &public_hoist_pattern {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let modules_dir = aube_settings::resolved::modules_dir(&ctx);
    hasher.update(format!("modules_dir={modules_dir}\0").as_bytes());
    let package_import_method = aube_settings::resolved::package_import_method(&ctx);
    hasher.update(b"package_import_method=");
    hasher.update(format!("{package_import_method:?}").as_bytes());
    hasher.update(b"\0");
    // enable_global_virtual_store is Option<bool>. Debug format keeps
    // None/Some(true)/Some(false) distinct which matters because Some(false)
    // is user opt out while None is "follow default".
    let enable_gvs = aube_settings::resolved::enable_global_virtual_store(&ctx);
    hasher.update(b"enable_gvs=");
    hasher.update(format!("{enable_gvs:?}").as_bytes());
    hasher.update(b"\0");
    let cache_dir = crate::commands::resolved_cache_dir_with_ctx(project_dir, &ctx);
    hasher.update(b"cache_dir=");
    hasher.update(cache_dir.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    let store_dir = crate::commands::resolved_store_dir_with_ctx(project_dir, &ctx);
    hasher.update(b"store_dir=");
    if let Some(store_dir) = store_dir {
        hasher.update(store_dir.as_os_str().as_encoded_bytes());
    }
    hasher.update(b"\0");
    let lockfile_enabled = aube_settings::resolved::lockfile(&ctx);
    hasher.update(format!("lockfile={lockfile_enabled}\0").as_bytes());
    // Resolution policy is part of warm-install freshness too. A project may
    // keep the same manifest and lockfile while tightening its release-age
    // policy; accepting the old warm tree would skip the policy-aware install
    // path entirely.
    hash_release_age_settings(&mut hasher, &ctx);
    // Catalog pruning runs after resolution, so a false→true environment
    // change must invalidate the warm path even though it does not alter the
    // installed dependency tree itself.
    let catalog_prune = crate::commands::install::resolve_catalog_prune(&ctx);
    hasher.update(format!("catalog_prune={catalog_prune}\0").as_bytes());
    // additional tree shape settings. cover enable_modules_dir flip
    // (pnpm equivalent of --lockfile-only persistent), virtual_store_only,
    // hoist_workspace_packages, dedupe_direct_deps, symlink,
    // disable_global_virtual_store_for_packages. any of these flipping
    // means the tree shape needs rebuild.
    let enable_modules_dir = aube_settings::resolved::enable_modules_dir(&ctx);
    hasher.update(format!("enable_modules_dir={enable_modules_dir}\0").as_bytes());
    let virtual_store_only = aube_settings::resolved::virtual_store_only(&ctx);
    hasher.update(format!("virtual_store_only={virtual_store_only}\0").as_bytes());
    let hoist_workspace_packages = aube_settings::resolved::hoist_workspace_packages(&ctx);
    hasher.update(format!("hoist_workspace_packages={hoist_workspace_packages}\0").as_bytes());
    let hoisting_limits = aube_settings::resolved::hoisting_limits(&ctx);
    hasher.update(b"hoisting_limits=");
    hasher.update(format!("{hoisting_limits:?}").as_bytes());
    hasher.update(b"\0");
    // Hoisted-layout algorithm version. A multi-importer workspace under
    // `nodeLinker=hoisted` now plans ONE shared tree (hoist to the
    // workspace root) instead of a full per-importer closure. The graph
    // hash is otherwise identical across the change, so without this a
    // tree materialized by the old per-importer algorithm would be treated
    // as current and never relinked. Bump on any future hoisted-layout
    // change. Gated on the hoisted linker so isolated installs are
    // unaffected.
    if matches!(node_linker, aube_settings::resolved::NodeLinker::Hoisted) {
        hasher.update(b"hoisted_layout_algo=2\0");
    }
    // Hidden-hoist name-selection version. The tree at
    // `<virtual-store>/node_modules/` now claims each name shallowest-first
    // (pnpm's hoist sort) instead of in dep_path order, so which VERSION of a
    // name an undeclared import resolves to can move. Nothing else in this hash
    // sees that: the graph, the settings and the direct entries are all
    // identical across the change, and `verify_install_layout` only lstats the
    // top-level entries — it never inspects the tree. Without this salt a warm
    // `node_modules` keeps the old tree and a phantom import resolves
    // differently warm vs. fresh from one lockfile. Bump on any future
    // hidden-hoist selection change. Gated ON the isolated linker, which is the
    // only one that builds this tree — the hoisted linker returns from
    // `link_all` before `link_hidden_hoist`. Stated positively so a future
    // third linker has to opt in rather than inherit the salt by default.
    if matches!(node_linker, aube_settings::resolved::NodeLinker::Isolated) {
        hasher.update(b"hidden_hoist_algo=1\0");
    }
    let dedupe_direct_deps = aube_settings::resolved::dedupe_direct_deps(&ctx);
    hasher.update(format!("dedupe_direct_deps={dedupe_direct_deps}\0").as_bytes());
    let symlink = aube_settings::resolved::symlink(&ctx);
    hasher.update(format!("symlink={symlink}\0").as_bytes());
    let disable_gvs_for_packages =
        aube_settings::resolved::disable_global_virtual_store_for_packages(&ctx);
    hasher.update(b"disable_gvs_for_packages=");
    for p in &disable_gvs_for_packages {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    // disk_materialize_packages changes which `.aube/<dep>` entries are real
    // dirs vs shared-store symlinks under GVS. Introducing or editing the list
    // must invalidate the install state so the link phase re-runs and converts
    // a stale symlink to a materialized dir (or back) — else the existence-gated
    // step1 accepts the wrong on-disk shape as cached.
    let disk_materialize_packages = aube_settings::resolved::disk_materialize_packages(&ctx);
    hasher.update(b"disk_materialize_packages=");
    for p in &disk_materialize_packages {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    // The Node engine builds run against. Without it a Node major switch leaves
    // this hash unchanged, the install reports itself current, and a native
    // addon compiled for the previous ABI is never revisited — the delta filter
    // and the side-effects cache downstream never get the chance.
    //
    // Gated on the embedder owning Node provisioning, because only then is the
    // value STABLE ACROSS THIS HASH'S TWO CALL SITES. The freshness check runs
    // before `runtime::ensure` populates `runtime::current()`, the state write
    // runs after; under runtime switching those observe different versions, so
    // folding a resolver-derived value would hash differently at check time and
    // write time and never converge — a permanent warm-path miss. An embedder
    // that provisions Node publishes its resolved version into the engine
    // context before any install code runs, so both sites read one value.
    // Standalone aube keeps its byte-for-byte hash; its own delta gate
    // (`dep_build_policy_hash`) is engine-aware and computed after `ensure`,
    // where the resolver has settled.
    if !aube_util::embedder().runtime_switching {
        let engine = crate::engines::build_node_version()
            .map_or_else(aube_lockfile::graph_hash::platform_name, |v| {
                aube_lockfile::graph_hash::engine_name_default(&v).0
            });
        hasher.update(b"engine=");
        hasher.update(engine.as_bytes());
        hasher.update(b"\0");
    }
    // Embedder-supplied extra fingerprint: an install-shape input the host
    // controls outside aube's resolved settings (nub's phantom-eject flag,
    // which changes which packages materialize but rides no setting). `None`
    // for standalone aube ⇒ the block is skipped ⇒ the hash is byte-for-byte
    // unchanged; a host that sets it invalidates the warm tree when its input
    // flips (an upgrade moving the default, or a user opt-out), forcing a
    // re-link instead of trusting a stale node_modules.
    if let Some(ext) = aube_util::embedder().extra_settings_fingerprint {
        hasher.update(b"embedder_ext=");
        hasher.update(ext().as_bytes());
        hasher.update(b"\0");
    }
    // map shaped workspace settings live in yaml. raw byte hash catches
    // catalog edits, overrides bumps, packageExtensions, allowBuilds list.
    // any of those mean re-resolve is needed, yaml bytes are the source.
    hasher.update(b"workspace_yaml=");
    // Iterate the *same* candidate set, in the *same* order, that aube actually
    // probes/reads (`workspace_yaml_names()`): this tool's branded YAML first,
    // then — when the `read_branded_pnpm_config` posture is set — the shared
    // `pnpm-workspace.yaml` compatibility surface. Standalone aube:
    // `["aube-workspace.yaml", "pnpm-workspace.yaml"]`. Under a non-pnpm
    // incumbent the pnpm entry drops out. Routing through the shared helper
    // keeps the hash order aligned with the read order (no divergent local
    // list to drift) and matches the standalone oracle.
    for name in aube_manifest::workspace::workspace_yaml_names() {
        let path = project_dir.join(name);
        hasher.update(name.as_bytes());
        hasher.update(b"\x1f");
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        hasher.update(b"\x1e");
    }
    hasher.update(b"\0");
    // Raw `.npmrc` bytes. Resolved settings above only cover the
    // install-shape keys we read. A user swapping `registry=` or
    // `//host/:_authToken=` changes what tarballs we would fetch
    // but the resolved-values hash never noticed, so fast path
    // stayed green while the actual source of truth for deps
    // changed. Hashing raw bytes is coarse (comment edits
    // invalidate too) but correct.
    hasher.update(b"npmrc=");
    {
        let mut paths: Vec<PathBuf> = vec![project_dir.join(".npmrc")];
        // User-level `~/.npmrc` also drives `registry=` and `_authToken`
        // (see `aube_registry::config::load_npmrc_entries`). Hash it so
        // a token swap or registry change invalidates the fast-path
        // verdict the same way a project-level edit does.
        if let Some(home) = aube_util::env::home_dir() {
            paths.push(home.join(".npmrc"));
        }
        for path in &paths {
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(b"\x1f");
            if let Ok(bytes) = std::fs::read(path) {
                hasher.update(&bytes);
            }
            hasher.update(b"\x1e");
        }
    }
    hasher.update(b"\0");
    // pnpmfile content. A local `.pnpmfile.{cjs,mjs}` (or a
    // `pnpmfilePath` override) `readPackage` / `afterAllResolved` hook
    // rewrites the resolved tree, so editing it must re-resolve. The
    // workspace-yaml hash above only catches the `pnpmfilePath` *setting*,
    // not the hook file's bytes — without this a changed pnpmfile rode the
    // warm path and the hook (e.g. dependency pins) silently never
    // re-applied, leaving node_modules and the lockfile stale (and pnpm's
    // `readPackage` log never reappeared). Mirrors pnpm folding the
    // pnpmfile into its own up-to-date check.
    hasher.update(b"pnpmfile=");
    if let Some(path) =
        crate::pnpmfile::detect(project_dir, None, ws_config.pnpmfile_path.as_deref())
    {
        hasher.update(path.as_os_str().as_encoded_bytes());
        hasher.update(b"\x1f");
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
    }
    hasher.update(b"\0");
    // OS + arch + libc. Optional deps filter by these. Swap host
    // between runs (committed node_modules across machines, shared
    // CI cache volume, Rosetta switch) and the correct prebuilts
    // change. Old fast path did not notice and skipped the install,
    // node_modules had the wrong variant for the active host.
    hasher.update(b"host=");
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(b"\x1f");
    // Piggyback on resolver's runtime libc probe. OS != linux
    // returns empty string, harmless but stable.
    hasher.update(aube_resolver::platform::host_triple().2.as_bytes());
    hasher.update(b"\0");
    // Command-line platform selection (`--os`/`--cpu`/`--libc`), which decides
    // the same thing the host block above does — which prebuilts are correct —
    // but from argv rather than from the machine. The host bytes are IDENTICAL
    // across a flagged and a bare run, so without this a flagged install on a
    // warm tree short-circuits as up-to-date and fetches nothing, and dropping
    // the flags again leaves the foreign tree in place. Both directions have to
    // re-materialize, which is why the selection is hashed rather than merely
    // recorded. The config-sourced spelling of the same setting is already
    // covered: the `pnpm` manifest key rides `INSTALL_SHAPE_FIELDS`, and
    // `pnpm-workspace.yaml` / `.npmrc` are byte-hashed above — this closes the
    // one path that was invisible.
    //
    // An axis nobody named contributes NOTHING, so an unflagged run in a
    // process that never parsed the flags (`ensure_installed`, `verify_deps`)
    // hashes exactly as it did before this existed.
    hasher.update(b"archsel=");
    let selection = aube_util::engine_context().cli_supported_architectures;
    for (axis, values) in [
        ("os", &selection.os),
        ("cpu", &selection.cpu),
        ("libc", &selection.libc),
    ] {
        let Some(values) = values else { continue };
        hasher.update(axis.as_bytes());
        hasher.update(b"=");
        for v in values {
            hasher.update(v.as_bytes());
            hasher.update(b"\x1f");
        }
        hasher.update(b"\x1e");
    }
    hasher.update(b"\0");
    // Patches dir. patch-commit and patch-remove touch patches in
    // `<project>/patches/` and `.aube-patches.json`. Old fast path
    // did not hash either. User edits a patch file, next install
    // says up-to-date, node_modules still has old patched content.
    hasher.update(b"patches=");
    let patches_sidecar_name = format!(".{}-patches.json", aube_util::embedder().name);
    let patches_sidecar = project_dir.join(&patches_sidecar_name);
    if let Ok(bytes) = std::fs::read(&patches_sidecar) {
        hasher.update(patches_sidecar_name.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(&bytes);
        hasher.update(b"\x1e");
    }
    let patches_dir = project_dir.join("patches");
    if let Ok(entries) = std::fs::read_dir(&patches_dir) {
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        // Sort so hash is deterministic across filesystems that
        // return dir entries in different order (ext4 vs tmpfs vs
        // NTFS).
        paths.sort();
        for p in paths {
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            hasher.update(name.as_bytes());
            hasher.update(b"\x1f");
            if let Ok(bytes) = std::fs::read(&p) {
                hasher.update(&bytes);
            }
            hasher.update(b"\x1e");
        }
    }
    hasher.update(b"\0");
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Fingerprint *only* the resolved minimum-release-age policy.
///
/// `hash_settings` also mixes in the raw workspace-yaml bytes, so it moves on
/// every catalog / overrides / packageExtensions edit. That makes it far too
/// broad to stand in for "did the age gate change?", which is the only drift
/// that can invalidate lockfile picks already admitted under the gate.
/// Resolved values only, so a no-op edit collapses to the same hash.
fn hash_release_policy(project_dir: &Path, cli_flags: &[(String, String)]) -> String {
    let files = crate::commands::FileSources::load(project_dir);
    let (_ws_config, raw_workspace) =
        aube_manifest::workspace::load_both(project_dir).unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let ctx = files.ctx(&raw_workspace, &env, cli_flags);
    let mut hasher = blake3::Hasher::new();
    hash_release_age_settings(&mut hasher, &ctx);
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn hash_release_age_settings(hasher: &mut blake3::Hasher, ctx: &aube_settings::ResolveCtx<'_>) {
    let minimum_release_age = aube_settings::resolved::minimum_release_age(ctx);
    hasher.update(format!("minimum_release_age={minimum_release_age}\0").as_bytes());
    let minimum_release_age_exclude =
        aube_settings::resolved::minimum_release_age_exclude(ctx).unwrap_or_default();
    hasher.update(b"minimum_release_age_exclude=");
    for pattern in &minimum_release_age_exclude {
        hasher.update(pattern.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let minimum_release_age_strict = aube_settings::resolved::minimum_release_age_strict(ctx)
        || aube_settings::resolved::paranoid(ctx);
    hasher.update(format!("minimum_release_age_strict={minimum_release_age_strict}\0").as_bytes());
}

fn hash_file(path: &Path) -> String {
    // BLAKE3 is 3–5× faster than SHA-256 on the state-check hot path.
    // The `"blake3:"` prefix makes old `"sha256:"` state mismatch on
    // first run after upgrade, which correctly triggers a rebuild.
    let content = std::fs::read(path).unwrap_or_default();
    let hash = blake3::hash(&content);
    format!("blake3:{}", hash.to_hex())
}

fn hash_bytes(content: &[u8]) -> String {
    let hash = blake3::hash(content);
    format!("blake3:{}", hash.to_hex())
}

fn hash_file_if_exists(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|content| {
        let hash = blake3::hash(&content);
        format!("blake3:{}", hash.to_hex())
    })
}

fn empty_blake3_hash() -> &'static str {
    "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
}

#[cfg(test)]
mod tests {
    use super::{
        InstallLayoutMode, InstallLayoutState, InstallState, InstalledPackageState,
        WriteStateInput, WriteStateLayout, collect_package_json_hashes_from_manifests,
        deferred_dep_builds_stale, empty_blake3_hash, fresh_state_file,
        gvs_nested_links_are_current, hash_file, hash_release_age_settings, hash_release_policy,
        hash_settings, install_state_file, member_lockfiles_stale, new_workspace_member,
        preview_list, read_hoisted_placements, read_or_migrate_fresh_state, read_state,
        relative_path_or_original, release_policy_changed_since_last_run, remove_state, state_dir,
        verify_install_layout, write_hoisted_placements, write_state,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn relative_path_helper_keeps_original_path_when_diff_fails() {
        let original = Path::new("/tmp/aube-test/package.json");
        let base = Path::new("project/../project");

        assert_eq!(
            relative_path_or_original(original, base),
            original.to_string_lossy()
        );
    }

    #[test]
    fn hoisted_placement_sidecar_preserves_exact_conflicting_version_paths() {
        let project_dir = temp_project_dir("exact-hoisted-placements");
        let root_v2 = project_dir.join("node_modules/foo");
        let nested_v1 = project_dir.join("node_modules/bar/node_modules/foo");
        std::fs::create_dir_all(&root_v2).expect("root placement should write");
        std::fs::create_dir_all(&nested_v1).expect("nested placement should write");
        let placements = aube_linker::HoistedPlacements::from_package_dirs(BTreeMap::from([
            ("foo@1.0.0".to_string(), vec![nested_v1.clone()]),
            ("foo@2.0.0".to_string(), vec![root_v2.clone()]),
        ]));

        write_hoisted_placements(&project_dir, Some(&placements))
            .expect("placement sidecar should write");
        let restored =
            read_hoisted_placements(&project_dir).expect("placement sidecar should read");

        assert_eq!(restored.package_dir("foo@1.0.0"), Some(nested_v1.as_path()));
        assert_eq!(restored.package_dir("foo@2.0.0"), Some(root_v2.as_path()));
        remove_state(&project_dir).expect("state directory should remove");
    }

    #[test]
    fn verify_install_layout_treats_legacy_empty_hash_as_cache_miss() {
        let project_dir = temp_project_dir("legacy-empty-hash");
        let state = InstallState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes: BTreeMap::new(),
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            aube_version: String::new(),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            release_policy_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: Some(InstallLayoutState {
                linker: InstallLayoutMode::Isolated,
                modules_dir_name: String::new(),
                hoisting_limits: None,
                virtual_store_dir_max_length: None,
                direct_entries: BTreeMap::new(),
                packages: BTreeMap::from([(
                    "is-odd@3.0.1".to_string(),
                    InstalledPackageState {
                        name: "is-odd".to_string(),
                        version: "3.0.1".to_string(),
                        package_json_path:
                            "node_modules/.aube/missing/node_modules/is-odd/package.json"
                                .to_string(),
                        package_json_hash: empty_blake3_hash().to_string(),
                        link: false,
                    },
                )]),
                gvs_nested_links: None,
            }),
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(Vec::new()),
        };

        assert_eq!(
            verify_install_layout(&project_dir, state.layout.as_ref()),
            Some(
                "installed package metadata missing: node_modules/.aube/missing/node_modules/is-odd/package.json"
                    .to_string()
            )
        );
    }

    /// A `link:` dep is a bare symlink to an arbitrary directory — often a
    /// sibling's build output that may not be built yet. The symlink can
    /// dangle, but its presence still means "installed": pnpm uses lstat
    /// here and stays warm, and the link target's `package.json` is not
    /// hashed (it may legitimately be absent). Regression for a
    /// readPackage-hook-wired `link:` dep busting the warm path on every
    /// install with "installed entry missing".
    #[cfg(unix)]
    #[test]
    fn verify_install_layout_treats_dangling_link_symlink_as_installed() {
        let project_dir = temp_project_dir("dangling-link");
        let scope_dir = project_dir.join("node_modules/@scope");
        std::fs::create_dir_all(&scope_dir).expect("node_modules dir should write");
        // Target deliberately does not exist (an unbuilt sibling output).
        std::os::unix::fs::symlink("../../../api/dist", scope_dir.join("api"))
            .expect("symlink should create");

        let state = InstallLayoutState {
            linker: InstallLayoutMode::Isolated,
            modules_dir_name: String::new(),
            hoisting_limits: None,
            virtual_store_dir_max_length: None,
            direct_entries: BTreeMap::from([(
                ".".to_string(),
                vec!["node_modules/@scope/api".to_string()],
            )]),
            packages: BTreeMap::from([(
                "@scope/api@link:../api/dist".to_string(),
                InstalledPackageState {
                    name: "@scope/api".to_string(),
                    version: "0.0.0".to_string(),
                    package_json_path: "../api/dist/package.json".to_string(),
                    package_json_hash: String::new(),
                    link: true,
                },
            )]),
            gvs_nested_links: None,
        };

        assert_eq!(verify_install_layout(&project_dir, Some(&state)), None);
    }

    /// If the link symlink itself is gone (not merely its target), the
    /// dep genuinely isn't installed and the warm path must bust.
    #[test]
    fn verify_install_layout_flags_missing_link_symlink() {
        let project_dir = temp_project_dir("missing-link");
        std::fs::create_dir_all(project_dir.join("node_modules/@scope"))
            .expect("node_modules dir should write");

        let state = InstallLayoutState {
            linker: InstallLayoutMode::Isolated,
            modules_dir_name: String::new(),
            hoisting_limits: None,
            virtual_store_dir_max_length: None,
            direct_entries: BTreeMap::from([(
                ".".to_string(),
                vec!["node_modules/@scope/api".to_string()],
            )]),
            packages: BTreeMap::new(),
            gvs_nested_links: None,
        };

        assert_eq!(
            verify_install_layout(&project_dir, Some(&state)),
            Some("installed entry missing: node_modules/@scope/api".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_install_layout_flags_retargeted_gvs_nested_link() {
        let project_dir = temp_project_dir("retargeted-gvs-link");
        let link_path = project_dir.join("node_modules/.aube/parent@1.0.0/node_modules/child");
        std::fs::create_dir_all(link_path.parent().expect("link parent"))
            .expect("link parent should write");
        std::os::unix::fs::symlink("../../child@1.0.0/node_modules/child", &link_path)
            .expect("nested link should write");
        let state = InstallLayoutState {
            linker: InstallLayoutMode::Isolated,
            modules_dir_name: "node_modules".to_string(),
            hoisting_limits: None,
            virtual_store_dir_max_length: Some(120),
            direct_entries: BTreeMap::new(),
            packages: BTreeMap::new(),
            gvs_nested_links: Some(BTreeMap::from([(
                "node_modules/.aube/parent@1.0.0/node_modules/child".to_string(),
                "../../child@1.0.0/node_modules/child".to_string(),
            )])),
        };
        assert!(gvs_nested_links_are_current(&project_dir, &state));

        std::fs::remove_file(&link_path).expect("old link should remove");
        std::os::unix::fs::symlink("../../child@1.0.0-stale/node_modules/child", &link_path)
            .expect("stale link should write");

        assert_eq!(
            verify_install_layout(&project_dir, Some(&state)),
            Some(
                "global virtual store link changed: node_modules/.aube/parent@1.0.0/node_modules/child"
                    .to_string()
            )
        );
        assert!(!gvs_nested_links_are_current(&project_dir, &state));
    }

    #[test]
    fn from_graph_records_direct_entries_for_every_importer() {
        let project_dir = temp_project_dir("layout-all-importers");
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep = |name: &str, dep_path: &str| aube_lockfile::DirectDep {
            name: name.to_string(),
            dep_path: dep_path.to_string(),
            dep_type: aube_lockfile::DepType::Production,
            specifier: None,
        };
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![dep("is-odd", "is-odd@3.0.1")]);
        importers.insert("packages/svc".to_string(), vec![dep("zod", "zod@3.23.8")]);
        let graph = aube_lockfile::LockfileGraph {
            importers,
            ..Default::default()
        };

        let layout = InstallLayoutState::from_graph(
            &project_dir,
            &WriteStateLayout {
                graph: &graph,
                node_linker: aube_linker::NodeLinker::Isolated,
                hoisting_limits: aube_linker::HoistingLimits::None,
                modules_dir_name: "node_modules",
                aube_dir: &aube_dir,
                virtual_store_dir_max_length: 120,
                placements: None,
                use_global_virtual_store: false,
            },
        )
        .expect("layout should build");

        // The root importer's direct symlink sits under the workspace
        // root's node_modules.
        assert_eq!(
            layout.direct_entries.get("."),
            Some(&vec!["node_modules/is-odd".to_string()])
        );
        // Every member's direct symlink is tracked under its own
        // node_modules so a deleted/incomplete member node_modules busts
        // the warm path instead of reporting "Already up to date". This is
        // the regression guard for the member-only `node_modules` not
        // being verified.
        assert_eq!(
            layout.direct_entries.get("packages/svc"),
            Some(&vec!["packages/svc/node_modules/zod".to_string()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn from_graph_records_scoped_gvs_links_from_package_node_modules() {
        let project_dir = temp_project_dir("scoped-gvs-links");
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "@scope/parent@1.0.0";
        let entry_name = aube_lockfile::dep_path_filename::dep_path_to_filename(dep_path, 120);
        let global_entry = project_dir.join("gvs/scoped-parent");
        let global_modules = global_entry.join("node_modules");
        std::fs::create_dir_all(global_modules.join("@scope/parent"))
            .expect("scoped package should write");
        std::os::unix::fs::symlink(
            "../../child@1.0.0/node_modules/child",
            global_modules.join("child"),
        )
        .expect("nested link should write");
        std::fs::create_dir_all(&aube_dir).expect("virtual store should write");
        std::os::unix::fs::symlink(&global_entry, aube_dir.join(&entry_name))
            .expect("project GVS link should write");

        let graph = aube_lockfile::LockfileGraph {
            packages: BTreeMap::from([(
                dep_path.to_string(),
                aube_lockfile::LockedPackage {
                    name: "@scope/parent".to_string(),
                    version: "1.0.0".to_string(),
                    dep_path: dep_path.to_string(),
                    dependencies: BTreeMap::from([("child".to_string(), "1.0.0".to_string())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let layout = InstallLayoutState::from_graph(
            &project_dir,
            &WriteStateLayout {
                graph: &graph,
                node_linker: aube_linker::NodeLinker::Isolated,
                hoisting_limits: aube_linker::HoistingLimits::None,
                modules_dir_name: "node_modules",
                aube_dir: &aube_dir,
                virtual_store_dir_max_length: 120,
                placements: None,
                use_global_virtual_store: true,
            },
        )
        .expect("scoped GVS layout should build");

        let expected_path = format!("node_modules/.aube/{entry_name}/node_modules/child");
        assert_eq!(
            layout
                .gvs_nested_links
                .as_ref()
                .and_then(|links| links.get(&expected_path)),
            Some(&"../../child@1.0.0/node_modules/child".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn from_graph_skips_unreadable_gvs_link_topology() {
        let project_dir = temp_project_dir("unreadable-gvs-links");
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "parent@1.0.0";
        let entry_name = aube_lockfile::dep_path_filename::dep_path_to_filename(dep_path, 120);
        let global_entry = project_dir.join("gvs/parent");
        std::fs::create_dir_all(global_entry.join("node_modules/parent"))
            .expect("package should write");
        std::fs::create_dir_all(&aube_dir).expect("virtual store should write");
        std::os::unix::fs::symlink(&global_entry, aube_dir.join(entry_name))
            .expect("project GVS link should write");

        let graph = aube_lockfile::LockfileGraph {
            packages: BTreeMap::from([(
                dep_path.to_string(),
                aube_lockfile::LockedPackage {
                    name: "parent".to_string(),
                    version: "1.0.0".to_string(),
                    dep_path: dep_path.to_string(),
                    dependencies: BTreeMap::from([("missing".to_string(), "1.0.0".to_string())]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let layout = InstallLayoutState::from_graph(
            &project_dir,
            &WriteStateLayout {
                graph: &graph,
                node_linker: aube_linker::NodeLinker::Isolated,
                hoisting_limits: aube_linker::HoistingLimits::None,
                modules_dir_name: "node_modules",
                aube_dir: &aube_dir,
                virtual_store_dir_max_length: 120,
                placements: None,
                use_global_virtual_store: true,
            },
        )
        .expect("unreadable topology should not fail state recording");

        assert_eq!(layout.gvs_nested_links, None);
    }

    #[test]
    fn from_graph_records_visible_hoisted_entries_for_workspace_importers() {
        let project_dir = temp_project_dir("layout-hoisted-importer");
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep = aube_lockfile::DirectDep {
            name: "is-number".to_string(),
            dep_path: "is-number@7.0.0".to_string(),
            dep_type: aube_lockfile::DepType::Production,
            specifier: None,
        };
        let graph = aube_lockfile::LockfileGraph {
            importers: BTreeMap::from([
                (".".to_string(), Vec::new()),
                ("packages/app".to_string(), vec![dep]),
            ]),
            ..Default::default()
        };
        let placements = aube_linker::HoistedPlacements::from_package_dirs(BTreeMap::from([(
            "is-number@7.0.0".to_string(),
            vec![project_dir.join("node_modules/is-number")],
        )]));

        let layout = InstallLayoutState::from_graph(
            &project_dir,
            &WriteStateLayout {
                graph: &graph,
                node_linker: aube_linker::NodeLinker::Hoisted,
                hoisting_limits: aube_linker::HoistingLimits::None,
                modules_dir_name: "node_modules",
                aube_dir: &aube_dir,
                virtual_store_dir_max_length: 120,
                placements: Some(&placements),
                use_global_virtual_store: false,
            },
        )
        .expect("hoisted layout should build");

        assert_eq!(
            layout.direct_entries.get("packages/app"),
            Some(&vec!["node_modules/is-number".to_string()])
        );
    }

    #[test]
    fn from_graph_tracks_hoisted_member_dep_at_its_shared_root_location() {
        // Regression guard (issue #484 follow-up): under nodeLinker=hoisted a
        // member's direct dep hoists to the shared workspace-root
        // node_modules, so its warm-path entry must point THERE, not at the
        // (now empty) <member>/node_modules/<dep>. Tracking the assumed
        // member path would make verify_install_layout report it missing on
        // every warm install and re-link forever.
        let project_dir = temp_project_dir("layout-hoisted-shared");
        let root_nm = project_dir.join("node_modules");
        std::fs::create_dir_all(root_nm.join("react")).unwrap();
        std::fs::write(
            root_nm.join("react/package.json"),
            "{\"name\":\"react\",\"version\":\"19.2.7\"}",
        )
        .unwrap();
        std::fs::create_dir_all(project_dir.join("packages/app/node_modules")).unwrap();

        let dep = |name: &str, dep_path: &str| aube_lockfile::DirectDep {
            name: name.to_string(),
            dep_path: dep_path.to_string(),
            dep_type: aube_lockfile::DepType::Production,
            specifier: None,
        };
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![]);
        importers.insert(
            "packages/app".to_string(),
            vec![dep("react", "react@19.2.7")],
        );
        let mut packages = BTreeMap::new();
        packages.insert(
            "react@19.2.7".to_string(),
            aube_lockfile::LockedPackage {
                name: "react".to_string(),
                version: "19.2.7".to_string(),
                dep_path: "react@19.2.7".to_string(),
                ..Default::default()
            },
        );
        let graph = aube_lockfile::LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let placements = aube_linker::HoistedPlacements::from_graph(
            &project_dir,
            &graph,
            "node_modules",
            aube_linker::HoistingLimits::None,
        )
        .unwrap();

        let aube_dir = root_nm.join(".aube");
        let layout = InstallLayoutState::from_graph(
            &project_dir,
            &WriteStateLayout {
                graph: &graph,
                node_linker: aube_linker::NodeLinker::Hoisted,
                hoisting_limits: aube_linker::HoistingLimits::None,
                modules_dir_name: "node_modules",
                aube_dir: &aube_dir,
                virtual_store_dir_max_length: 120,
                placements: Some(&placements),
                use_global_virtual_store: false,
            },
        )
        .unwrap();

        // The member's react is tracked at the SHARED ROOT, where it hoisted.
        assert_eq!(
            layout.direct_entries.get("packages/app"),
            Some(&vec!["node_modules/react".to_string()])
        );
        // And the warm-path check passes: the root react dir exists.
        assert!(verify_install_layout(&project_dir, Some(&layout)).is_none());
    }

    #[test]
    fn collect_package_json_hashes_from_manifests_uses_file_paths_for_workspaces() {
        let project_dir = temp_project_dir("manifest-hash-keys");
        let root_pkg = project_dir.join("package.json");
        let ws_pkg = project_dir.join("packages/foo/package.json");
        std::fs::create_dir_all(ws_pkg.parent().expect("workspace dir"))
            .expect("workspace dir should be creatable");
        std::fs::write(&root_pkg, "{\"name\":\"root\"}").expect("root package.json should write");
        std::fs::write(&ws_pkg, "{\"name\":\"foo\"}").expect("workspace package.json should write");

        let manifests = vec![
            (".".to_string(), aube_manifest::PackageJson::default()),
            (
                "packages/foo".to_string(),
                aube_manifest::PackageJson::default(),
            ),
        ];

        let hashes = collect_package_json_hashes_from_manifests(&project_dir, &manifests);

        assert_eq!(hashes.get("."), Some(&hash_file(&root_pkg)));
        assert_eq!(
            hashes.get("packages/foo/package.json"),
            Some(&hash_file(&ws_pkg))
        );
    }

    #[test]
    fn state_json_migrates_fresh_state_without_delta_maps() {
        let project_dir = temp_project_dir("fresh-migration");
        let state_path = project_dir.join(".aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        let state = InstallState {
            lockfile_hash: "blake3:lock".to_string(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes: BTreeMap::from([(".".to_string(), "blake3:pkg".to_string())]),
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: "blake3:settings".to_string(),
            dep_build_policy_hash: "blake3:dep-build-policy".to_string(),
            release_policy_hash: "blake3:release-policy".to_string(),
            package_content_hashes: BTreeMap::from([(
                "is-odd@3.0.1".to_string(),
                "blake3:content".to_string(),
            )]),
            graph_lthash: "abcdef".to_string(),
            package_subtree_hashes: BTreeMap::from([(
                "is-odd@3.0.1".to_string(),
                "blake3:subtree".to_string(),
            )]),
            package_json_shape_digests: BTreeMap::from([(".".to_string(), "shape".to_string())]),
            layout: Some(InstallLayoutState {
                linker: InstallLayoutMode::Isolated,
                modules_dir_name: String::new(),
                hoisting_limits: None,
                virtual_store_dir_max_length: None,
                direct_entries: BTreeMap::new(),
                packages: BTreeMap::new(),
                gvs_nested_links: None,
            }),
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(Vec::new()),
        };
        let json = serde_json::to_string(&state).expect("state should serialize");
        std::fs::write(install_state_file(&state_path), json).expect("state should write");

        let migrated = read_or_migrate_fresh_state(&state_path).expect("fresh state should load");
        assert_eq!(migrated.lockfile_hash, "blake3:lock");
        let fresh_json = std::fs::read_to_string(fresh_state_file(&state_path))
            .expect("fresh state should write");
        assert!(fresh_json.contains("package_json_hashes"));
        assert!(!fresh_json.contains("package_content_hashes"));
        assert!(!fresh_json.contains("package_subtree_hashes"));
    }

    #[test]
    fn legacy_state_file_is_deleted_instead_of_migrated() {
        let project_dir = temp_project_dir("legacy-file-delete");
        let state_path = project_dir.join(".aube-state");
        std::fs::write(&state_path, "{}").expect("legacy state file should write");

        assert!(read_or_migrate_fresh_state(&state_path).is_none());
        assert!(!state_path.exists());
    }

    #[test]
    fn unreviewed_builds_roundtrip_persists_into_fresh_state() {
        use super::read_state_unreviewed_builds;
        let project_dir = temp_project_dir("unreviewed-builds-rt");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        let state = InstallState {
            lockfile_hash: "blake3:lock".to_string(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes: BTreeMap::new(),
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            release_policy_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: None,
            unreviewed_builds: vec![
                "esbuild@0.21.5".to_string(),
                "better-sqlite3@11.5.0".to_string(),
            ],
            deferred_dep_builds: Some(Vec::new()),
        };
        let json = serde_json::to_string(&state).expect("state should serialize");
        std::fs::write(install_state_file(&state_path), json).expect("state should write");
        // First read migrates the fresh sidecar.
        let _ = read_state_unreviewed_builds(&project_dir);
        let unreviewed = read_state_unreviewed_builds(&project_dir);
        assert_eq!(
            unreviewed,
            vec![
                "esbuild@0.21.5".to_string(),
                "better-sqlite3@11.5.0".to_string()
            ]
        );
    }

    /// The retry depends on this list surviving into the freshness
    /// sidecar: `lifecycle_delta_filter` reads it back to force a full
    /// eligible scan, and a field left out of `FreshnessState` would
    /// read as empty there — the delta would then drop the owed package
    /// and the next state write would clear the marker, re-sealing the
    /// tree while every other test still passed.
    #[test]
    fn deferred_dep_builds_roundtrip_reaches_the_delta_filter() {
        use super::read_state_deferred_dep_builds;
        let project_dir = temp_project_dir("deferred-builds-rt");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        let state = InstallState {
            lockfile_hash: "blake3:lock".to_string(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes: BTreeMap::new(),
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            release_policy_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: None,
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(vec!["esbuild@0.24.0".to_string()]),
        };
        let json = serde_json::to_string(&state).expect("state should serialize");
        std::fs::write(install_state_file(&state_path), json).expect("state should write");
        // First read migrates the fresh sidecar; the second reads it back.
        let _ = read_state_deferred_dep_builds(&project_dir);
        assert_eq!(
            read_state_deferred_dep_builds(&project_dir),
            Some(vec!["esbuild@0.24.0".to_string()]),
            "the owed build must survive into the sidecar the delta filter reads"
        );
    }

    /// The delta filter distinguishes three states, and collapsing the
    /// first two makes the migration a no-op: state predating the field
    /// (`None`, must force a full scan) is NOT the same as an install
    /// that positively recorded nothing owed (`Some([])`, may narrow).
    #[test]
    fn legacy_state_reads_as_unknown_not_as_nothing_owed() {
        use super::read_state_deferred_dep_builds;
        let project_dir = temp_project_dir("deferred-builds-legacy");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        let legacy_json = r#"{
            "lockfile_hash": "blake3:lock",
            "package_json_hashes": {},
            "aube_version": "0.0.0"
        }"#;
        std::fs::write(install_state_file(&state_path), legacy_json).expect("state should write");

        let _ = read_state_deferred_dep_builds(&project_dir);
        assert_eq!(
            read_state_deferred_dep_builds(&project_dir),
            None,
            "pre-field state must read as unknown, or the lifecycle delta narrows and re-seals \
             the stranded tree the migration exists to heal"
        );
    }

    #[test]
    fn unreviewed_builds_default_when_field_missing_in_state() {
        use super::read_state_unreviewed_builds;
        let project_dir = temp_project_dir("unreviewed-builds-default");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        // Pre-feature state file with no unreviewed_builds key — the
        // serde default keeps the read path working.
        let legacy_json = r#"{
            "lockfile_hash": "blake3:lock",
            "package_json_hashes": {},
            "aube_version": "0.0.0"
        }"#;
        std::fs::write(install_state_file(&state_path), legacy_json)
            .expect("legacy state should write");
        let unreviewed = read_state_unreviewed_builds(&project_dir);
        assert!(unreviewed.is_empty());
    }

    #[test]
    fn remove_state_deletes_directory_and_legacy_file() {
        let project_dir = temp_project_dir("remove-state");
        let state_path = project_dir.join("node_modules/.aube-state");
        std::fs::create_dir_all(&state_path).expect("state dir should write");
        std::fs::write(install_state_file(&state_path), "{}").expect("state json should write");

        remove_state(&project_dir).expect("state directory should remove");
        assert!(!state_path.exists());

        std::fs::create_dir_all(state_path.parent().expect("state parent"))
            .expect("state parent should write");
        std::fs::write(&state_path, "{}").expect("legacy state file should write");

        remove_state(&project_dir).expect("legacy state file should remove");
        assert!(!state_path.exists());
    }

    fn temp_project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aube-state-tests-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }

    // --- no-integrity URL-keyed global binding ---
    //
    // The binding lives in the GLOBAL store keyed by the registry tarball
    // URL a no-integrity coordinate resolves to. These exercise the two
    // properties the relocation must hold simultaneously: (1) it survives a
    // `node_modules` wipe and is shared across projects on the same registry
    // (the perf win — no re-fetch), and (2) a different registry produces a
    // different key so one project can never read another's binding (the
    // #212/#220 cross-registry closure). All hermetic: `store-dir` pins the
    // store and `registry` pins the URL via `.npmrc`; no network.

    fn no_integrity_project(name: &str, registry: &str, store: &Path) -> PathBuf {
        let dir = temp_project_dir(name);
        std::fs::write(
            dir.join(".npmrc"),
            format!("store-dir={}\nregistry={registry}\n", store.display()),
        )
        .expect(".npmrc should write");
        dir
    }

    fn no_integrity_pkg() -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: "foo".into(),
            version: "1.0.0".into(),
            dep_path: "foo@1.0.0".into(),
            ..Default::default()
        }
    }

    fn bind(project: &Path, sha512: &str) {
        let pkg = no_integrity_pkg();
        let url =
            crate::commands::make_client(project).tarball_url(pkg.registry_name(), &pkg.version);
        super::write_no_integrity_bindings(
            &super::no_integrity_dir(project),
            &BTreeMap::from([(url, sha512.to_string())]),
        )
        .expect("binding should persist");
    }

    fn resolved_sha512(project: &Path) -> Option<String> {
        super::read_no_integrity_index_for(project, std::slice::from_ref(&no_integrity_pkg()))
            .get("foo@1.0.0")
            .cloned()
    }

    #[test]
    fn no_integrity_binding_reads_layer_the_writable_dir_over_the_global_one() {
        let base = std::env::temp_dir().join(format!("aube-ni-layer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let local = base.join("local/no-integrity");
        let global = base.join("global/no-integrity");
        let url = "https://reg.test/dep/-/dep-1.0.0.tgz";
        let mut bindings = std::collections::BTreeMap::new();
        bindings.insert(url.to_string(), "sha512-global".to_string());
        super::write_no_integrity_bindings(&global, &bindings).unwrap();
        // Only the global store knows the URL: the warm read still finds it.
        let dirs = vec![local.clone(), global.clone()];
        assert_eq!(
            super::read_no_integrity_binding_in(&dirs, url).as_deref(),
            Some("sha512-global")
        );
        // A local binding for the same URL shadows the global one.
        bindings.insert(url.to_string(), "sha512-local".to_string());
        super::write_no_integrity_bindings(&local, &bindings).unwrap();
        assert_eq!(
            super::read_no_integrity_binding_in(&dirs, url).as_deref(),
            Some("sha512-local")
        );
        assert_eq!(
            super::read_no_integrity_binding_in(&dirs, "https://reg.test/other"),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn no_integrity_binding_is_global_and_survives_node_modules_wipe() {
        let store = temp_project_dir("ni-wipe-store");
        let proj = no_integrity_project("ni-wipe-proj", "https://reg.test/", &store);
        bind(&proj, "sha512-AAA");

        // The binding sits under the store, not node_modules, so the
        // dominant CI warm pattern (`rm -rf node_modules`) can't touch it.
        let dir = super::no_integrity_dir(&proj);
        let nm = proj.join("node_modules");
        std::fs::create_dir_all(nm.join(".aube/foo@1.0.0")).unwrap();
        std::fs::remove_dir_all(&nm).unwrap();

        assert!(dir.starts_with(&store), "binding must live under the store");
        assert!(
            !dir.starts_with(&nm),
            "binding must be outside node_modules"
        );
        assert_eq!(resolved_sha512(&proj).as_deref(), Some("sha512-AAA"));
    }

    #[test]
    fn same_registry_projects_share_one_binding() {
        let store = temp_project_dir("ni-share-store");
        let a = no_integrity_project("ni-share-a", "https://reg.test/", &store);
        let b = no_integrity_project("ni-share-b", "https://reg.test/", &store);

        bind(&a, "sha512-shared");
        // B never wrote a binding but resolves the same coordinate from the
        // same registry, so it reuses A's from the shared store — no refetch.
        assert_eq!(resolved_sha512(&b).as_deref(), Some("sha512-shared"));
    }

    #[test]
    fn different_registries_do_not_share_binding() {
        let store = temp_project_dir("ni-xreg-store");
        let a = no_integrity_project("ni-xreg-a", "https://reg-a.test/", &store);
        let b = no_integrity_project("ni-xreg-b", "https://reg-b.test/", &store);

        bind(&a, "sha512-from-a");
        // The #212/#220 closure: B resolves the SAME coordinate from a
        // DIFFERENT registry, so its URL key differs and it must NOT read
        // A's binding out of the shared store.
        assert!(
            resolved_sha512(&b).is_none(),
            "a different registry must not read another project's binding"
        );

        // Each project keeps its own independent binding.
        bind(&b, "sha512-from-b");
        assert_eq!(resolved_sha512(&a).as_deref(), Some("sha512-from-a"));
        assert_eq!(resolved_sha512(&b).as_deref(), Some("sha512-from-b"));
    }

    #[test]
    fn stale_in_node_modules_binding_is_ignored() {
        let store = temp_project_dir("ni-migrate-store");
        let proj = no_integrity_project("ni-migrate-proj", "https://reg.test/", &store);

        // A pre-relocation binding left inside node_modules must never be
        // read. Bind an UNRELATED coordinate globally so the read builds a
        // client and actually consults the store — proving the stale file is
        // ignored, not merely short-circuited by an absent store dir.
        super::write_no_integrity_bindings(
            &super::no_integrity_dir(&proj),
            &BTreeMap::from([(
                "https://reg.test/other/-/other-9.9.9.tgz".to_string(),
                "sha512-other".to_string(),
            )]),
        )
        .unwrap();
        let legacy = proj.join("node_modules/.aube-state");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("no-integrity-index.json"),
            r#"{"foo@1.0.0":"sha512-stale"}"#,
        )
        .unwrap();

        assert!(
            resolved_sha512(&proj).is_none(),
            "stale in-node_modules binding must not be read"
        );
    }

    #[test]
    fn shape_digest_keeps_fast_path_on_cosmetic_edit() {
        use std::collections::BTreeMap;
        let dir = temp_project_dir("shape-cosmetic");
        let original = r#"{
  "name": "x",
  "dependencies": { "react": "19.0.0" },
  "scripts": { "test": "vitest" }
}"#;
        let pkg_path = dir.join("package.json");
        std::fs::write(&pkg_path, original).unwrap();

        let orig_bytes = std::fs::read(&pkg_path).unwrap();
        let orig_parsed: serde_json::Value = serde_json::from_slice(&orig_bytes).unwrap();
        let orig_shape = hex::encode(aube_util::hash::manifest_install_shape_digest(&orig_parsed));

        let mut pjh = BTreeMap::new();
        pjh.insert(".".to_string(), hash_file(&pkg_path));
        let mut shapes = BTreeMap::new();
        shapes.insert(".".to_string(), orig_shape);
        let state = InstallState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes: pjh,
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            aube_version: env!("CARGO_PKG_VERSION").to_string(),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            release_policy_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            package_json_shape_digests: shapes,
            layout: None,
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(Vec::new()),
        };
        let reformatted = r#"{
  "name": "x",
  "dependencies": { "react": "19.0.0" },
  "scripts": { "test": "jest" }
}
"#;
        std::fs::write(&pkg_path, reformatted).unwrap();

        let new_bytes = std::fs::read(&pkg_path).unwrap();
        let new_parsed: serde_json::Value = serde_json::from_slice(&new_bytes).unwrap();
        let new_shape = hex::encode(aube_util::hash::manifest_install_shape_digest(&new_parsed));
        assert_eq!(
            new_shape, state.package_json_shape_digests["."],
            "shape digest should ignore scripts + whitespace"
        );
    }

    #[test]
    fn member_lockfiles_stale_detects_edit_add_and_remove() {
        // Config-only `sharedWorkspaceLockfile=false` layout: with no
        // shared root lockfile to anchor on, the warm path verifies each
        // member's own lockfile. Drive the edit / add / remove detection
        // directly — no install or registry needed.
        let dir = temp_project_dir("member-lockfiles-stale");
        std::fs::write(
            dir.join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        let write_member = |name: &str, lock: &str| -> PathBuf {
            let d = dir.join("packages").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("package.json"),
                format!("{{\"name\":\"@ws/{name}\"}}"),
            )
            .unwrap();
            let lockfile = d.join("aube-lock.yaml");
            std::fs::write(&lockfile, lock).unwrap();
            lockfile
        };
        let a_lock = write_member("a", "lockfileVersion: '9.0'\n# a\n");
        let b_lock = write_member("b", "lockfileVersion: '9.0'\n# b\n");

        let mut hashes = BTreeMap::new();
        hashes.insert("packages/a".to_string(), hash_file(&a_lock));
        hashes.insert("packages/b".to_string(), hash_file(&b_lock));
        let state = super::FreshnessState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: hashes,
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes: BTreeMap::new(),
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: None,
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(Vec::new()),
        };

        // Every recorded member matches what is on disk → fresh.
        assert_eq!(member_lockfiles_stale(&dir, &state), None);

        // Editing a member's lockfile busts the warm path.
        std::fs::write(&a_lock, "lockfileVersion: '9.0'\n# a edited\n").unwrap();
        assert_eq!(
            member_lockfiles_stale(&dir, &state),
            Some("packages/a lockfile has changed".to_string())
        );
        std::fs::write(&a_lock, "lockfileVersion: '9.0'\n# a\n").unwrap();
        assert_eq!(member_lockfiles_stale(&dir, &state), None);

        // A brand-new member (absent from the recorded state) busts it.
        let c_dir = dir.join("packages/c");
        write_member("c", "lockfileVersion: '9.0'\n# c\n");
        assert_eq!(
            member_lockfiles_stale(&dir, &state),
            Some("packages/c is a new workspace member".to_string())
        );
        std::fs::remove_dir_all(&c_dir).unwrap();

        // A removed member (recorded but gone) busts it.
        std::fs::remove_dir_all(dir.join("packages/b")).unwrap();
        assert_eq!(
            member_lockfiles_stale(&dir, &state),
            Some("packages/b was removed from the workspace".to_string())
        );
    }

    #[test]
    fn new_workspace_member_busts_warm_path_on_shared_layout() {
        // The default `sharedWorkspaceLockfile=true` layout records no
        // per-member lockfile state, so `member_lockfiles_stale` never runs
        // and a member added after the last install is otherwise invisible
        // to the warm path (nubjs/nub#441). `new_workspace_member` fills the
        // gap by re-enumerating current members against the recorded manifest
        // hashes — keyed by the member's `package.json` path, matching
        // `collect_package_json_hashes_from_manifests`.
        let dir = temp_project_dir("new-workspace-member");
        std::fs::write(dir.join("pnpm-workspace.yaml"), "packages:\n  - 'apps/*'\n").unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"root","private":true}"#,
        )
        .unwrap();
        let write_member = |name: &str| {
            let d = dir.join("apps").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("package.json"), format!("{{\"name\":\"{name}\"}}")).unwrap();
        };
        write_member("server");

        // State from the last install: only the root and `apps/server` are
        // recorded, keyed by their package.json paths. No member lockfile
        // state (shared layout).
        let mut package_json_hashes = BTreeMap::new();
        package_json_hashes.insert(".".to_string(), hash_file(&dir.join("package.json")));
        package_json_hashes.insert(
            "apps/server/package.json".to_string(),
            hash_file(&dir.join("apps/server/package.json")),
        );
        let state = super::FreshnessState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes,
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: None,
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(Vec::new()),
        };

        // Every current member was recorded → fresh.
        assert_eq!(new_workspace_member(&dir, &state), None);

        // Adding `apps/web` after the last install busts the warm path.
        write_member("web");
        assert_eq!(
            new_workspace_member(&dir, &state),
            Some("apps/web is a new workspace member".to_string())
        );
    }

    #[test]
    fn new_workspace_member_root_as_member_does_not_churn() {
        // A workspace that lists its own root (`packages: ['.']`, the pnpm
        // pattern where the root is itself a package members depend on) must
        // not read the root as a spurious "new member": the root is recorded
        // under "." but enumerates with an empty rel, so a naive
        // `<rel>/package.json` key would never match and bust the warm path on
        // every install.
        let dir = temp_project_dir("new-member-root-as-member");
        std::fs::write(
            dir.join("pnpm-workspace.yaml"),
            "packages:\n  - '.'\n  - 'apps/*'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"root","private":true}"#,
        )
        .unwrap();
        let app = dir.join("apps/server");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("package.json"), r#"{"name":"server"}"#).unwrap();

        let mut package_json_hashes = BTreeMap::new();
        package_json_hashes.insert(".".to_string(), hash_file(&dir.join("package.json")));
        package_json_hashes.insert(
            "apps/server/package.json".to_string(),
            hash_file(&app.join("package.json")),
        );
        let state = super::FreshnessState {
            lockfile_hash: String::new(),
            lockfile_snapshot_name: None,
            lockfile_meta: None,
            member_lockfile_hashes: BTreeMap::new(),
            member_lockfile_meta: BTreeMap::new(),
            package_json_hashes,
            package_json_meta: BTreeMap::new(),
            local_directory_hashes: Some(BTreeMap::new()),
            section_filtered: false,
            settings_hash: String::new(),
            dep_build_policy_hash: String::new(),
            package_json_shape_digests: BTreeMap::new(),
            layout: None,
            unreviewed_builds: Vec::new(),
            deferred_dep_builds: Some(Vec::new()),
        };

        // Root + apps/server both recorded → fresh (no churn on the root).
        assert_eq!(new_workspace_member(&dir, &state), None);
    }

    #[test]
    fn settings_hash_busts_warm_path_on_pnpmfile_change() {
        // A `.pnpmfile.{mjs,cjs}` `readPackage` hook rewrites the resolved
        // tree, so adding / editing / removing it must change the
        // freshness verdict — otherwise the hook silently never re-applies
        // and the lockfile + node_modules go stale on the warm path.
        //
        // `hash_settings` reaches the cwd-default arm of `pnpmfile::detect`, so
        // this shares the process-global gate seam with pnpmfile's own tests
        // even though it lives in another module of the same test binary.
        let _lock = crate::pnpmfile::default_gate_lock();
        let dir = temp_project_dir("settings-hash-pnpmfile");
        std::fs::write(dir.join("package.json"), r#"{"name":"x"}"#).unwrap();

        let baseline = hash_settings(&dir, &[]);

        // Adding a pnpmfile must change the hash.
        let pnpmfile = dir.join(".pnpmfile.mjs");
        std::fs::write(&pnpmfile, "export function readPackage(p){return p}\n").unwrap();
        let with_file = hash_settings(&dir, &[]);
        assert_ne!(baseline, with_file, "adding a pnpmfile must bust the hash");

        // Editing the hook body must change the hash again.
        std::fs::write(
            &pnpmfile,
            "export function readPackage(p){p.dependencies={};return p}\n",
        )
        .unwrap();
        let edited = hash_settings(&dir, &[]);
        assert_ne!(with_file, edited, "editing a pnpmfile must bust the hash");

        // Removing it returns to the baseline verdict.
        std::fs::remove_file(&pnpmfile).unwrap();
        assert_eq!(
            baseline,
            hash_settings(&dir, &[]),
            "removing the pnpmfile must restore the baseline hash"
        );
    }

    #[test]
    fn release_age_value_exclusions_and_strictness_change_freshness_hash() {
        fn fingerprint(settings: &[(&str, &str)]) -> String {
            let project: Vec<(String, String)> = settings
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect();
            let empty_yaml = BTreeMap::new();
            let ctx = aube_settings::ResolveCtx {
                managed_aube_config: &[],
                project_aube_config: &[],
                project_npmrc: &[],
                project_config: &project,
                user_aube_config: &[],
                user_npmrc: &[],
                workspace_yaml: &empty_yaml,
                global_config_yaml: &empty_yaml,
                env: &[],
                cli: &[],
                embedder_defaults: &[],
            };
            let mut hasher = blake3::Hasher::new();
            hash_release_age_settings(&mut hasher, &ctx);
            hasher.finalize().to_hex().to_string()
        }

        let zero = fingerprint(&[("minimumReleaseAge", "0")]);
        let weeks_999 = fingerprint(&[("minimumReleaseAge", "10069920")]);
        assert_ne!(zero, weeks_999, "0 minutes -> 999 weeks must invalidate");
        assert_ne!(
            weeks_999,
            fingerprint(&[
                ("minimumReleaseAge", "10069920"),
                ("minimumReleaseAgeExclude", "is-number")
            ]),
            "changing exclusions must invalidate"
        );
        assert_ne!(
            weeks_999,
            fingerprint(&[
                ("minimumReleaseAge", "10069920"),
                ("minimumReleaseAgeStrict", "true")
            ]),
            "changing strictness must invalidate"
        );
    }

    #[test]
    fn catalog_edit_moves_settings_hash_but_not_release_policy_hash() {
        // Release-policy revalidation discards the lockfile and re-resolves the
        // whole graph to newest-in-range, so it has to key off the age gate
        // alone. `settings_hash` also covers the raw workspace yaml — which is
        // where catalogs live — so using it as the trigger re-resolves
        // everything on an ordinary dependency bump.
        let dir = temp_project_dir("release-policy-vs-catalog");
        std::fs::write(dir.join("package.json"), r#"{"name":"x"}"#).unwrap();
        let workspace = dir.join("pnpm-workspace.yaml");
        std::fs::write(
            &workspace,
            "minimumReleaseAge: 4320\ncatalog:\n  left-pad: 1.0.0\n",
        )
        .unwrap();

        let settings_before = hash_settings(&dir, &[]);
        let policy_before = hash_release_policy(&dir, &[]);

        std::fs::write(
            &workspace,
            "minimumReleaseAge: 4320\ncatalog:\n  left-pad: 1.1.0\n",
        )
        .unwrap();
        assert_ne!(
            settings_before,
            hash_settings(&dir, &[]),
            "a catalog bump must still bust the broad settings hash"
        );
        assert_eq!(
            policy_before,
            hash_release_policy(&dir, &[]),
            "a catalog bump must not read as age-policy drift"
        );

        std::fs::write(
            &workspace,
            "minimumReleaseAge: 10080\ncatalog:\n  left-pad: 1.1.0\n",
        )
        .unwrap();
        assert_ne!(
            policy_before,
            hash_release_policy(&dir, &[]),
            "changing the age gate must read as age-policy drift"
        );
    }

    #[test]
    fn recorded_release_policy_hash_round_trips_through_write_state() {
        // The narrowing only works if the hash `write_state` records is the one
        // `release_policy_changed_since_last_run` later compares against. If
        // those two ever stop agreeing, the checker returns `true` forever and
        // revalidation silently reverts to firing on every settings change —
        // which the hash-only test above would not catch.
        let dir = temp_project_dir("release-policy-round-trip");
        std::fs::write(dir.join("package.json"), r#"{"name":"x"}"#).unwrap();
        let workspace = dir.join("pnpm-workspace.yaml");
        std::fs::write(&workspace, "minimumReleaseAge: 4320\n").unwrap();

        // Nothing recorded yet, so there is no evidence the gate moved.
        assert!(
            !release_policy_changed_since_last_run(&dir, &[]),
            "missing state must not revalidate"
        );

        let graph = aube_lockfile::LockfileGraph::default();
        let aube_dir = dir.join("node_modules/.aube");
        std::fs::create_dir_all(&aube_dir).unwrap();
        let write = |dir: &Path| {
            write_state(
                dir,
                WriteStateInput {
                    section_filtered: false,
                    package_json_hashes: BTreeMap::new(),
                    cli_flags: &[],
                    package_content_hashes: BTreeMap::new(),
                    graph_lthash: String::new(),
                    package_subtree_hashes: BTreeMap::new(),
                    dep_build_policy_hash: String::new(),
                    layout: WriteStateLayout {
                        graph: &graph,
                        node_linker: aube_linker::NodeLinker::Isolated,
                        hoisting_limits: aube_linker::HoistingLimits::None,
                        modules_dir_name: "node_modules",
                        aube_dir: &aube_dir,
                        virtual_store_dir_max_length: 120,
                        placements: None,
                        use_global_virtual_store: false,
                    },
                    unreviewed_builds: Vec::new(),
                    deferred_dep_builds: Vec::new(),
                },
            )
            .expect("state should write");
        };
        write(&dir);

        assert!(
            !release_policy_changed_since_last_run(&dir, &[]),
            "an unchanged age gate must not revalidate"
        );

        // The bug this PR fixes: a catalog edit moves `settings_hash` but is not
        // an age-policy change, so it must not force revalidation.
        std::fs::write(
            &workspace,
            "minimumReleaseAge: 4320\ncatalog:\n  left-pad: 1.0.0\n",
        )
        .unwrap();
        assert!(
            !release_policy_changed_since_last_run(&dir, &[]),
            "a catalog edit must not revalidate"
        );

        // A tightened gate still must.
        std::fs::write(
            &workspace,
            "minimumReleaseAge: 10080\ncatalog:\n  left-pad: 1.0.0\n",
        )
        .unwrap();
        assert!(
            release_policy_changed_since_last_run(&dir, &[]),
            "a raised age gate must revalidate"
        );

        // A state file predating the field (empty hash) is unknown, not changed.
        // This is the upgrade hop: every nub before this field wrote state
        // without it, so answering "changed" here would hand each upgrading
        // user one full re-resolve on their next settings edit — the very
        // symptom this PR exists to remove.
        write(&dir);
        let mut state = read_state(&state_dir(&dir)).expect("state should read back");
        assert!(
            !state.release_policy_hash.is_empty(),
            "write_state must record the policy hash"
        );
        state.release_policy_hash = String::new();
        std::fs::write(
            install_state_file(&state_dir(&dir)),
            serde_json::to_string(&state).expect("state should serialize"),
        )
        .unwrap();
        assert!(
            !release_policy_changed_since_last_run(&dir, &[]),
            "state predating release_policy_hash must not revalidate"
        );

        // The DOCUMENTED COST, pinned so it cannot be flipped back without
        // reading why: on that same upgrade-hop install a RAISED gate is not
        // applied either, because an empty hash carries no policy to compare
        // against. `--force` is the retroactive path. If this assertion ever
        // fails, the upgrade hop has started re-resolving every project's whole
        // graph again — the defect this narrowing removes.
        std::fs::write(
            &workspace,
            "minimumReleaseAge: 43200\ncatalog:\n  left-pad: 1.0.0\n",
        )
        .unwrap();
        assert!(
            !release_policy_changed_since_last_run(&dir, &[]),
            "an unknown previous policy must not revalidate even when the gate is raised"
        );

        // The unknown-is-not-changed default must not swallow a real change:
        // once a hash is on record, raising the gate still revalidates.
        write(&dir);
        std::fs::write(
            &workspace,
            "minimumReleaseAge: 20160\ncatalog:\n  left-pad: 1.0.0\n",
        )
        .unwrap();
        assert!(
            release_policy_changed_since_last_run(&dir, &[]),
            "a raised age gate must still revalidate once a hash is recorded"
        );
    }

    /// Builds the last install could not run leave the tree incomplete,
    /// so the warm path must not report it up to date (nubjs/nub#764).
    /// The three arms are the whole contract: nothing owed takes the
    /// warm path, an owed build busts it, and state written before the
    /// field existed busts once so an already-sealed tree can heal.
    #[test]
    fn deferred_dep_builds_decide_the_warm_path() {
        fn state(deferred: Option<Vec<String>>, unreviewed: Vec<String>) -> super::FreshnessState {
            super::FreshnessState {
                lockfile_hash: String::new(),
                lockfile_snapshot_name: None,
                lockfile_meta: None,
                member_lockfile_hashes: BTreeMap::new(),
                member_lockfile_meta: BTreeMap::new(),
                package_json_hashes: BTreeMap::new(),
                package_json_meta: BTreeMap::new(),
                local_directory_hashes: Some(BTreeMap::new()),
                section_filtered: false,
                settings_hash: String::new(),
                dep_build_policy_hash: String::new(),
                package_json_shape_digests: BTreeMap::new(),
                layout: None,
                unreviewed_builds: unreviewed,
                deferred_dep_builds: deferred,
            }
        }

        assert_eq!(
            deferred_dep_builds_stale(&state(Some(Vec::new()), Vec::new())),
            None,
            "a clean install owes no builds and must take the warm path"
        );

        // A package denied by config is denied stably: re-running the
        // install could never change the answer, so sealing on it is
        // correct and the warm path must survive it.
        assert_eq!(
            deferred_dep_builds_stale(&state(
                Some(Vec::new()),
                vec!["some-unapproved-pkg@1.0.0".to_string()]
            )),
            None,
            "an ordinary pending approval must not force a reinstall every time"
        );

        let reason = deferred_dep_builds_stale(&state(
            Some(vec!["esbuild@0.24.0".to_string()]),
            vec!["esbuild@0.24.0".to_string()],
        ))
        .expect("an owed build must bust the warm path");
        assert!(
            reason.contains("esbuild@0.24.0"),
            "the reason must name the package so the miss is diagnosable, got: {reason}"
        );

        assert!(
            deferred_dep_builds_stale(&state(None, vec!["esbuild@0.24.0".to_string()])).is_some(),
            "state predating the field may be hiding a stranded build, so re-check once"
        );
        // The seal this migration exists for includes an ALLOWED package
        // whose directory was missing when the lifecycle phase ran.
        // Allowed means never unreviewed, so that state's unreviewed list
        // is empty — narrowing the migration to a non-empty list would
        // leave exactly those trees sealed forever.
        assert!(
            deferred_dep_builds_stale(&state(None, Vec::new())).is_some(),
            "legacy state must re-check even with no unreviewed builds: an allowed build that \
             was never attempted leaves the unreviewed list empty"
        );
    }

    /// The list is a diagnostic, so a napi-rs-style tree of per-platform
    /// packages must not splat its whole graph into one line.
    #[test]
    fn preview_list_caps_the_inline_names() {
        let one = ["a@1".to_string()];
        assert_eq!(preview_list(&one), "a@1");
        let many: Vec<String> = (0..6).map(|i| format!("p{i}@1")).collect();
        assert_eq!(preview_list(&many), "p0@1, p1@1, p2@1, and 3 more");
    }

    #[test]
    fn settings_hash_busts_warm_path_on_storage_override_change() {
        let dir = temp_project_dir("settings-hash-storage-overrides");
        std::fs::write(dir.join("package.json"), r#"{"name":"x"}"#).unwrap();

        let first = hash_settings(
            &dir,
            &[
                ("cacheDir".to_string(), "/host/cache-a".to_string()),
                ("storeDir".to_string(), "/host/store-a".to_string()),
            ],
        );
        let changed_cache = hash_settings(
            &dir,
            &[
                ("cacheDir".to_string(), "/host/cache-b".to_string()),
                ("storeDir".to_string(), "/host/store-a".to_string()),
            ],
        );
        let changed_store = hash_settings(
            &dir,
            &[
                ("cacheDir".to_string(), "/host/cache-a".to_string()),
                ("storeDir".to_string(), "/host/store-b".to_string()),
            ],
        );

        assert_ne!(first, changed_cache);
        assert_ne!(first, changed_store);
    }
}
