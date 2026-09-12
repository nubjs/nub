use miette::miette;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Aliased rather than re-spelled: `aube-store` names the same directory for
/// its own layout docs, and two literals drift.
pub(crate) const PROJECTS_DIR: &str = aube_store::PROJECTS_SUBDIR;
pub(crate) const LOCK_FILE: &str = ".prune.lock";

#[derive(Debug, Serialize, Deserialize)]
struct RegisteredProject {
    project_dir: PathBuf,
    aube_dir: PathBuf,
}

/// A registry record whose project directory and `.aube/` directory both
/// still resolve.
///
/// Reachability marking must count only these. [`plan_prune`] reports the
/// rest as `stale_records` and [`apply_prune`] deletes them, so a record
/// that stopped resolving contributes no marks — it stops counting as a
/// store user rather than disqualifying the sweep.
#[derive(Clone, Debug)]
pub(crate) struct LiveProject {
    pub project_dir: PathBuf,
    pub aube_dir: PathBuf,
}

/// One read of the project registry, classified.
pub(crate) struct RegistrySnapshot {
    pub live: Vec<LiveProject>,
    pub stale_records: Vec<PathBuf>,
}

pub(crate) struct GvsLock(std::fs::File);

impl Drop for GvsLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn open_lock(global_virtual_store: &Path) -> miette::Result<std::fs::File> {
    std::fs::create_dir_all(global_virtual_store).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to create global virtual store {}: {e}",
            global_virtual_store.display()
        )
    })?;
    let path = global_virtual_store.join(LOCK_FILE);
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
                "failed to open global virtual store lock {}: {e}",
                path.display()
            )
        })
}

pub(crate) fn lock_for_install(global_virtual_store: &Path) -> miette::Result<GvsLock> {
    let file = open_lock(global_virtual_store)?;
    file.lock_shared().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to lock global virtual store {} for install: {e}",
            global_virtual_store.display()
        )
    })?;
    Ok(GvsLock(file))
}

pub(crate) fn lock_for_prune(
    global_virtual_store: &Path,
    quiet: bool,
) -> miette::Result<Option<GvsLock>> {
    if !global_virtual_store.exists() {
        return Ok(None);
    }
    let file = open_lock(global_virtual_store)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            if !quiet {
                crate::progress::safe_eprintln(
                    "Waiting for a running global virtual store install to finish before pruning",
                );
            }
            file.lock()
                .map_err(|e| lock_error(global_virtual_store, e))?;
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(lock_error(global_virtual_store, e));
        }
    }
    Ok(Some(GvsLock(file)))
}

fn lock_error(global_virtual_store: &Path, error: std::io::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "failed to lock global virtual store {} for pruning: {error}",
        global_virtual_store.display()
    )
}

pub(crate) fn register_project(
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    std::fs::create_dir_all(global_virtual_store)
        .map_err(|e| registry_error(global_virtual_store, e))?;
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    std::fs::create_dir_all(&projects_dir).map_err(|e| registry_error(&projects_dir, e))?;
    let project_dir = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.into());
    let aube_dir = if aube_dir.is_absolute() {
        aube_dir.to_path_buf()
    } else {
        project_dir.join(aube_dir)
    };
    let record = RegisteredProject {
        project_dir,
        aube_dir,
    };
    let bytes = serde_json::to_vec(&record).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "failed to encode global virtual store project registry entry: {e}"
        )
    })?;
    aube_util::fs_atomic::atomic_write(
        &project_record_path(&projects_dir, &record.project_dir),
        &bytes,
    )
    .map_err(|e| registry_error(&projects_dir, e))
}

pub(crate) fn unregister_if_unreferenced(
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    if project_links_into(aube_dir, global_virtual_store)? {
        return register_project(global_virtual_store, project_dir, aube_dir);
    }
    let project_dir = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.into());
    let path = project_record_path(&global_virtual_store.join(PROJECTS_DIR), &project_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(registry_error(&path, e)),
    }
}

fn project_record_path(projects_dir: &Path, project_dir: &Path) -> PathBuf {
    let key = blake3::hash(project_dir.as_os_str().as_encoded_bytes()).to_hex();
    projects_dir.join(format!("{key}.json"))
}

pub(crate) fn register_fast_path_project(
    _lock: &GvsLock,
    global_virtual_store: &Path,
    project_dir: &Path,
    aube_dir: &Path,
) -> miette::Result<()> {
    if !project_links_into(aube_dir, global_virtual_store)? {
        return Ok(());
    }
    register_project(global_virtual_store, project_dir, aube_dir)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(not(unix))]
    Path(PathBuf),
}

#[derive(Clone, Debug)]
pub(crate) struct CandidateFile {
    pub identity: FileIdentity,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub(crate) struct GvsPrunePlan {
    pub entries: Vec<PathBuf>,
    pub stale_records: Vec<PathBuf>,
    pub files: Vec<CandidateFile>,
    pub vanished_files: Vec<PathBuf>,
    /// The live registry records this plan was computed against. Carried on
    /// the plan so a sweep of another store tier in the same command marks
    /// against one registry snapshot rather than re-reading it and getting a
    /// different answer.
    pub live_projects: Vec<LiveProject>,
}

impl GvsPrunePlan {
    pub fn bytes(&self) -> u64 {
        let mut identities = HashSet::new();
        self.files
            .iter()
            .filter(|file| identities.insert(file.identity.clone()))
            .map(|file| file.bytes)
            .sum()
    }
}

/// Read the project registry once and split it into live records and the
/// stale ones a prune deletes.
///
/// A record that fails to parse is fatal rather than skipped: a registry we
/// cannot fully read is a reachability answer we cannot trust, and skipping
/// the record would silently unprotect whatever only it reached.
pub(crate) fn read_registry(global_virtual_store: &Path) -> miette::Result<RegistrySnapshot> {
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    let mut snapshot = RegistrySnapshot {
        live: Vec::new(),
        stale_records: Vec::new(),
    };
    if !projects_dir.exists() {
        return Ok(snapshot);
    }
    for entry in read_dir(&projects_dir)? {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| prune_error(&path, e))?;
        let project: RegisteredProject =
            serde_json::from_slice(&bytes).map_err(|e| invalid_registry_error(&path, e))?;
        if !project.project_dir.exists() || !project.aube_dir.exists() {
            snapshot.stale_records.push(path);
            continue;
        }
        snapshot.live.push(LiveProject {
            project_dir: project.project_dir,
            aube_dir: project.aube_dir,
        });
    }
    Ok(snapshot)
}

pub(crate) fn plan_prune(global_virtual_store: &Path) -> miette::Result<GvsPrunePlan> {
    if !global_virtual_store.exists() {
        return Ok(GvsPrunePlan::default());
    }
    let mut reachable = HashSet::new();
    let mut plan = GvsPrunePlan::default();
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    if !projects_dir.exists() && !graph_entries(global_virtual_store)?.is_empty() {
        return Err(miette!(
            code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
            "global virtual store project registry {} is missing while entries exist\nhelp: run {} in active projects before pruning again",
            projects_dir.display(),
            aube_util::cmd("install")
        ));
    }

    let registry = read_registry(global_virtual_store)?;
    for project in &registry.live {
        let current = project_entries(global_virtual_store, &project.aube_dir)?;
        reachable.extend(current);
    }
    plan.stale_records = registry.stale_records;
    plan.live_projects = registry.live;

    for entry in read_dir(global_virtual_store)? {
        let name = entry.file_name();
        if !is_graph_entry_name(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|e| prune_error(&entry.path(), e))?;
        if !file_type.is_dir() || reachable.contains(&name) {
            continue;
        }
        let path = entry.path();
        collect_candidate_files(&path, &mut plan.files, &mut plan.vanished_files)?;
        plan.entries.push(path);
    }
    Ok(plan)
}

pub(crate) fn apply_prune(global_virtual_store: &Path, plan: &GvsPrunePlan) -> miette::Result<()> {
    if !global_virtual_store.exists() {
        return Ok(());
    }
    for path in &plan.entries {
        aube_linker::remove_dir_all_with_retry(path).map_err(|e| prune_error(path, e))?;
    }
    for path in &plan.stale_records {
        std::fs::remove_file(path).map_err(|e| prune_error(path, e))?;
    }
    let projects_dir = global_virtual_store.join(PROJECTS_DIR);
    if !projects_dir.exists() {
        std::fs::create_dir_all(&projects_dir).map_err(|e| registry_error(&projects_dir, e))?;
    }
    Ok(())
}

pub(crate) fn collect_candidate_files(
    path: &Path,
    files: &mut Vec<CandidateFile>,
    vanished_files: &mut Vec<PathBuf>,
) -> miette::Result<()> {
    for entry in read_dir(path)? {
        let entry_path = entry.path();
        let metadata = match std::fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                vanished_files.push(entry_path);
                continue;
            }
            Err(error) => return Err(prune_error(&entry_path, error)),
        };
        if metadata.is_dir() {
            collect_candidate_files(&entry_path, files, vanished_files)?;
        } else if metadata.is_file() {
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                FileIdentity::Unix {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                }
            };
            #[cfg(not(unix))]
            let identity = FileIdentity::Path(entry_path.clone());
            files.push(CandidateFile {
                identity,
                bytes: metadata.len(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn prune(global_virtual_store: &Path, dry_run: bool) -> miette::Result<usize> {
    let _lock = lock_for_prune(global_virtual_store, false)?;
    let plan = plan_prune(global_virtual_store)?;
    let removed = plan.entries.len();
    if !dry_run {
        apply_prune(global_virtual_store, &plan)?;
    }
    Ok(removed)
}

fn is_graph_entry_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    !name.starts_with('.') && name != "node_modules"
}

fn graph_entries(global_virtual_store: &Path) -> miette::Result<Vec<OsString>> {
    let mut entries = Vec::new();
    for entry in read_dir(global_virtual_store)? {
        let name = entry.file_name();
        if !is_graph_entry_name(&name) {
            continue;
        }
        if entry
            .file_type()
            .map_err(|e| prune_error(&entry.path(), e))?
            .is_dir()
        {
            entries.push(name);
        }
    }
    Ok(entries)
}

fn project_links_into(aube_dir: &Path, global_virtual_store: &Path) -> miette::Result<bool> {
    // Boolean-only variant of `project_entries`: stop at the first entry
    // that resolves into the GVS instead of readlink+canonicalize-ing the
    // entire `.aube/` directory. This runs on the install fast path
    // (`register_fast_path_project`), where the full walk is O(graph) for
    // an answer that is almost always "yes" at the first entry.
    if !aube_dir.exists() {
        return Ok(false);
    }
    let canonical_gvs =
        std::fs::canonicalize(global_virtual_store).unwrap_or_else(|_| global_virtual_store.into());
    for entry in std::fs::read_dir(aube_dir).map_err(|e| prune_error(aube_dir, e))? {
        let entry = entry.map_err(|e| prune_error(aube_dir, e))?;
        if !is_graph_entry_name(&entry.file_name()) {
            continue;
        }
        let Some(canonical_target) = resolved_link_target(&entry.path(), aube_dir) else {
            continue;
        };
        // Proper-prefix check, matching `project_entries`' strip_prefix +
        // first-component semantics: a target equal to the GVS root
        // itself carries no entry component and doesn't count.
        if canonical_target.starts_with(&canonical_gvs) && canonical_target != canonical_gvs {
            return Ok(true);
        }
    }
    Ok(false)
}

fn project_entries(
    global_virtual_store: &Path,
    aube_dir: &Path,
) -> miette::Result<HashSet<OsString>> {
    let mut entries = HashSet::new();
    if !aube_dir.exists() {
        return Ok(entries);
    }
    let canonical_gvs =
        std::fs::canonicalize(global_virtual_store).unwrap_or_else(|_| global_virtual_store.into());
    for entry in read_dir(aube_dir)? {
        if !is_graph_entry_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let Some(canonical_target) = resolved_link_target(&path, aube_dir) else {
            continue;
        };
        let Ok(relative) = canonical_target.strip_prefix(&canonical_gvs) else {
            continue;
        };
        if let Some(component) = relative.components().next() {
            entries.insert(component.as_os_str().to_os_string());
        }
    }
    Ok(entries)
}

fn resolved_link_target(path: &Path, base: &Path) -> Option<PathBuf> {
    let target = std::fs::read_link(path).ok()?;
    let absolute = if target.is_absolute() {
        target
    } else {
        path.parent().unwrap_or(base).join(target)
    };
    Some(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn read_dir(path: &Path) -> miette::Result<Vec<std::fs::DirEntry>> {
    std::fs::read_dir(path)
        .map_err(|e| prune_error(path, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| prune_error(path, e))
}

fn prune_error(path: &Path, error: std::io::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "failed to prune global virtual store path {}: {error}",
        path.display()
    )
}

fn invalid_registry_error(path: &Path, error: serde_json::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "invalid global virtual store project registry entry {}: {error}",
        path.display()
    )
}

fn registry_error(path: &Path, error: std::io::Error) -> miette::Report {
    miette!(
        code = aube_codes::errors::ERR_AUBE_GVS_PRUNE_FAILED,
        "failed to update global virtual store project registry {}: {error}",
        path.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_registered_and_removes_historical_entries() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(&gvs).expect("global virtual store should be created");
        drop(lock_for_install(&gvs).expect("legacy snapshot should initialize"));
        let project = tmp.path().join("project");
        let aube_dir = project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        let live = gvs.join("live@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&live).expect("live entry should be created");
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        std::os::unix::fs::symlink(&live, aube_dir.join("live@1.0.0"))
            .expect("project link should be created");

        register_project(&gvs, &project, &aube_dir).expect("project should register");
        let orphan = gvs.join("orphan@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&orphan).expect("orphan entry should be created");
        let orphan_project = tmp.path().join("orphan-project");
        let orphan_aube_dir = orphan_project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        std::fs::create_dir_all(&orphan_aube_dir)
            .expect("orphan project virtual store should be created");
        std::os::unix::fs::symlink(&orphan, orphan_aube_dir.join("orphan@1.0.0"))
            .expect("orphan project link should be created");
        register_project(&gvs, &orphan_project, &orphan_aube_dir)
            .expect("orphan project should register");
        std::fs::remove_file(orphan_aube_dir.join("orphan@1.0.0"))
            .expect("orphan project link should be removed");
        assert_eq!(prune(&gvs, true).expect("dry run should succeed"), 1);
        assert!(orphan.exists(), "dry run must not remove candidates");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(live.exists(), "registered entry must survive");
        assert!(!orphan.exists(), "unlinked historical claim must be pruned");
    }

    #[test]
    fn prune_removes_entries_from_deleted_projects() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        let orphan = gvs.join("orphan@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&orphan).expect("new orphan should be created");
        let orphan_project = tmp.path().join("orphan-project");
        let orphan_aube_dir = orphan_project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        std::fs::create_dir_all(&orphan_aube_dir)
            .expect("orphan project virtual store should be created");
        std::os::unix::fs::symlink(&orphan, orphan_aube_dir.join("orphan@1.0.0"))
            .expect("orphan project link should be created");
        register_project(&gvs, &orphan_project, &orphan_aube_dir)
            .expect("orphan project should register");
        std::fs::remove_dir_all(&orphan_project).expect("orphan project should be removed");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(!orphan.exists(), "stale project entry must be pruned");
    }

    #[test]
    fn failed_link_cleanup_removes_only_unreferenced_records() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        let project = tmp.path().join("project");
        let aube_dir = project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        register_project(&gvs, &project, &aube_dir).expect("project should register");

        unregister_if_unreferenced(&gvs, &project, &aube_dir)
            .expect("empty project record should be removed");
        assert_eq!(
            std::fs::read_dir(gvs.join(PROJECTS_DIR))
                .expect("registry should be readable")
                .count(),
            0
        );

        let live = gvs.join("live@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&live).expect("live entry should be created");
        std::os::unix::fs::symlink(&live, aube_dir.join("live@1.0.0"))
            .expect("project link should be created");
        register_project(&gvs, &project, &aube_dir).expect("project should register again");
        unregister_if_unreferenced(&gvs, &project, &aube_dir)
            .expect("referenced project record should be preserved");
        assert_eq!(
            std::fs::read_dir(gvs.join(PROJECTS_DIR))
                .expect("registry should be readable")
                .count(),
            1
        );
    }

    #[test]
    fn first_prune_initializes_an_empty_registry() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(&gvs).expect("global virtual store should be created");

        assert_eq!(prune(&gvs, false).expect("empty prune should succeed"), 0);
        assert!(gvs.join(PROJECTS_DIR).exists());
    }

    #[test]
    fn prune_fails_closed_when_an_initialized_registry_disappears() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        let untracked = gvs.join("untracked@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&untracked).expect("untracked entry should be created");

        let error = prune(&gvs, false).expect_err("missing registry must fail closed");
        assert!(
            error.to_string().contains("project registry"),
            "unexpected error: {error}"
        );
        assert!(untracked.exists(), "failed prune must not delete entries");
    }

    #[test]
    fn prune_removes_untracked_entries_from_interrupted_installs() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        std::fs::create_dir_all(gvs.join(PROJECTS_DIR))
            .expect("project registry should be initialized");
        let interrupted = gvs.join("interrupted@1.0.0-deadbeefdeadbeef");
        std::fs::create_dir_all(&interrupted).expect("untracked entry should be created");

        assert_eq!(prune(&gvs, false).expect("prune should succeed"), 1);
        assert!(!interrupted.exists(), "untracked entry must be pruned");
    }

    #[test]
    fn prune_drops_registry_records_for_deleted_projects() {
        let tmp = tempfile::tempdir().expect("tempdir should be created");
        let gvs = tmp.path().join("virtual-store");
        std::fs::create_dir_all(&gvs).expect("global virtual store should be created");
        drop(lock_for_install(&gvs).expect("install lock should be acquired"));
        let project = tmp.path().join("project");
        let aube_dir = project
            .join("node_modules")
            .join(format!(".{}", aube_util::prog()));
        std::fs::create_dir_all(&aube_dir).expect("project virtual store should be created");
        register_project(&gvs, &project, &aube_dir).expect("project should register");
        std::fs::remove_dir_all(&project).expect("project should be removed");

        prune(&gvs, false).expect("prune should remove stale registry record");
        assert_eq!(
            std::fs::read_dir(gvs.join(PROJECTS_DIR))
                .expect("registry should be readable")
                .count(),
            0
        );
    }
}
