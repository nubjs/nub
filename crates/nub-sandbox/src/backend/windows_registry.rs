//! Durable ownership records for reusable Windows AppContainers.
//!
//! This module deliberately owns *intent* and liveness, rather than an ACL snapshot.
//! A record is written before a profile, private directory, or ACE is touched; the
//! Windows launcher records only the ACEs it added.  That lets recovery remove Nub's
//! additions without rolling back another program's intervening DACL edits.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const SCHEMA_VERSION: u32 = 2;
pub(crate) const BACKEND_VERSION: &str = "appcontainer-acl-v4";
pub(crate) const MAX_IDLE_ENTRIES: usize = 64;
pub(crate) const MAX_OWNED_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const MAX_IDLE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PolicyIdentity {
    pub(crate) hash: String,
    canonical: String,
    policy_hash: Option<String>,
    objects: BTreeMap<String, Option<String>>,
}

impl PolicyIdentity {
    /// Hash resolved grants, never caller-provided policy JSON.  In particular, the
    /// constructed environment is excluded: it may contain an injected credential,
    /// and credential injection belongs to a command rather than a shared identity.
    pub(crate) fn new(
        read: impl IntoIterator<Item = PathBuf>,
        nodes: impl IntoIterator<Item = PathBuf>,
        write: impl IntoIterator<Item = PathBuf>,
        managed_profile: Option<PathBuf>,
        allow_internet: bool,
        uses_funnel: bool,
    ) -> io::Result<Self> {
        let canonical_paths = |paths: Vec<PathBuf>| -> io::Result<Vec<String>> {
            paths
                .into_iter()
                .map(|path| canonical_path(&path))
                .collect::<io::Result<BTreeSet<_>>>()
                .map(|set| set.into_iter().collect())
        };
        let read = canonical_paths(read.into_iter().collect())?;
        let nodes = canonical_paths(nodes.into_iter().collect())?;
        let write = canonical_paths(write.into_iter().collect())?;
        let profile = managed_profile
            .map(|path| canonical_path_or_lexical(&path))
            .transpose()?;
        let canonical = serde_json::json!({
            "schema": SCHEMA_VERSION,
            "backend": BACKEND_VERSION,
            "read": read,
            "nodes": nodes,
            "write": write,
            "profile": profile,
            "internet": allow_internet,
            "funnel": uses_funnel,
        })
        .to_string();
        let hash = hex(&Sha256::digest(canonical.as_bytes()));
        Ok(Self {
            hash,
            canonical,
            policy_hash: None,
            objects: BTreeMap::new(),
        })
    }

    pub(crate) fn with_network(
        mut self,
        policy: Option<&crate::policy::NetPolicy>,
    ) -> io::Result<Self> {
        // A helper shares the package identity: policies allowing different hosts
        // must never share that identity and its same-package loopback reachability.
        let network = serde_json::to_string(&policy).map_err(io::Error::other)?;
        self.canonical.push_str(&network);
        self.hash = hex(&Sha256::digest(self.canonical.as_bytes()));
        Ok(self)
    }

    pub(crate) fn profile_name(&self) -> String {
        // Keep below the documented 64-char AppContainer profile-name limit.
        format!("nub_sbx_r_{}", &self.hash[..40])
    }

    pub(crate) fn with_private_tmp(mut self, private: bool) -> Self {
        self.canonical.push_str(if private {
            "\nmanaged-tmp=profile/AC/Temp"
        } else {
            "\nmanaged-tmp=none"
        });
        self.hash = hex(&Sha256::digest(self.canonical.as_bytes()));
        self
    }

    pub(crate) fn with_native_compat(mut self, version: Option<&str>) -> Self {
        if let Some(version) = version {
            self.canonical.push_str("\nnative-compat=");
            self.canonical.push_str(version);
            self.hash = hex(&Sha256::digest(self.canonical.as_bytes()));
        }
        self
    }

    /// Retained leases stay keyed by policy; only a new acquisition resolves a new
    /// resource incarnation. File contents and timestamps do not affect identity.
    pub(crate) fn with_objects(
        mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> io::Result<Self> {
        for path in paths {
            let path = canonical_path_or_lexical(&path)?;
            self.objects
                .insert(path.clone(), object_id(Path::new(&path))?);
        }
        self.policy_hash = Some(self.hash);
        self.canonical.push('\n');
        self.canonical
            .push_str(&serde_json::to_string(&self.objects).map_err(io::Error::other)?);
        self.hash = hex(&Sha256::digest(self.canonical.as_bytes()));
        Ok(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WindowObject {
    pub(crate) session: u32,
    pub(crate) station: String,
    /// None names the station itself; Some names a desktop in that station.
    pub(crate) desktop: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AclMutation {
    pub(crate) path: String,
    pub(crate) kind: AclKind,
    pub(crate) access: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AclKind {
    Subtree,
    Object,
    PrivateProfile,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EntryState {
    Preparing,
    Ready,
    Idle,
    Closing,
    RecoveryNeeded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) identity: String,
    #[serde(default)]
    pub(crate) policy_identity: Option<String>,
    pub(crate) canonical_policy: String,
    pub(crate) profile_name: String,
    pub(crate) state: EntryState,
    pub(crate) created_at: u64,
    pub(crate) last_used_at: u64,
    pub(crate) private_paths: Vec<String>,
    pub(crate) owned_bytes: u64,
    pub(crate) mutations: Vec<AclMutation>,
    pub(crate) leases: BTreeSet<String>,
    pub(crate) recovery_error: Option<String>,
    /// A window-object DACL revoke that reached the native mutation boundary but has not yet
    /// durably removed its object from `window_objects`. This is progress, not an error: ordinary
    /// recovery errors must not erase it before a retry can consume the operation safely.
    #[serde(default)]
    pub(crate) window_object_revoke: Option<WindowObject>,
    pub(crate) window_objects: Vec<WindowObject>,
    pub(crate) object_ids: BTreeMap<String, String>,
}

#[derive(Default, Serialize, Deserialize)]
struct RegistryFile {
    schema: u32,
    entries: BTreeMap<String, Entry>,
}

/// A caller lease backed by a kernel object on Windows.  The event disappears when
/// its owning process dies, unlike a disk refcount or a saved PID.
pub(crate) struct Lease {
    name: String,
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: the event handle is solely owned, immutable while shared, and has no
// thread affinity. Closing occurs only after the last resource Arc is dropped.
#[cfg(windows)]
unsafe impl Send for Lease {}
#[cfg(windows)]
unsafe impl Sync for Lease {}

impl Lease {
    fn create(identity: &str) -> io::Result<Self> {
        let nonce = format!("{:x}", now_nanos());
        let name = format!(
            "Global\\nub-sbx-lease-{}-{}-{nonce}",
            &identity[..16],
            std::process::id()
        );
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::HANDLE;
            unsafe extern "system" {
                fn CreateEventW(
                    attributes: *const std::ffi::c_void,
                    manual_reset: i32,
                    initial_state: i32,
                    name: *const u16,
                ) -> HANDLE;
            }
            let wide = wide(&name);
            // A caller keeps this handle open for its whole acquired-resource lifetime.
            let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, wide.as_ptr()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { name, handle })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { name })
        }
    }

    fn live(name: &str) -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            unsafe extern "system" {
                fn OpenEventW(
                    access: u32,
                    inherit: i32,
                    name: *const u16,
                ) -> windows_sys::Win32::Foundation::HANDLE;
            }
            const SYNCHRONIZE: u32 = 0x0010_0000;
            let wide = wide(name);
            let handle = unsafe { OpenEventW(SYNCHRONIZE, 0, wide.as_ptr()) };
            if handle.is_null() {
                // Only "not found" proves death; access denial or resource pressure
                // must retain the lease rather than evict a live caller's grants.
                return io::Error::last_os_error().raw_os_error() != Some(2);
            }
            unsafe { CloseHandle(handle) };
            true
        }
        #[cfg(not(windows))]
        {
            // Host tests inject the liveness decision through `prune_with`; no host PID
            // heuristic is allowed to stand in for the Windows kernel lease contract.
            let _ = name;
            false
        }
    }

    fn close(&mut self) {
        #[cfg(windows)]
        if !self.handle.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

#[cfg(windows)]
impl Drop for Lease {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) struct Acquired {
    pub(crate) entry: Entry,
    pub(crate) fresh: bool,
    lease: Lease,
    root: PathBuf,
    closed: bool,
    admitted_objects: BTreeMap<String, Option<String>>,
}

impl Acquired {
    #[cfg(all(test, windows))]
    pub(crate) fn has_live_lease(&self) -> bool {
        Lease::live(&self.lease.name)
    }

    pub(crate) fn close(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.lease.close();
        release(&self.root, &self.entry.identity, &self.lease.name)?;
        self.closed = true;
        Ok(())
    }
    pub(crate) fn record_window_object(&mut self, object: WindowObject) -> io::Result<()> {
        let _lock = MutationLock::acquire(&self.root)?;
        let mut file = load(&self.root)?;
        let entry = file
            .entries
            .get_mut(&self.entry.identity)
            .ok_or_else(|| io::Error::other("sandbox registry lost an acquired entry"))?;
        if !entry.window_objects.contains(&object) {
            entry.window_objects.push(object);
            save(&self.root, &file)?;
        }
        self.entry = file.entries[&self.entry.identity].clone();
        Ok(())
    }

    pub(crate) fn record_mutation(&mut self, mutation: AclMutation) -> io::Result<()> {
        let observed_id = object_id(Path::new(&mutation.path))?;
        self.record_mutation_id(mutation, observed_id)
    }

    /// The launcher holds this object open across journaling and its ACL write.
    pub(crate) fn record_mutation_id(
        &mut self,
        mutation: AclMutation,
        observed_id: Option<String>,
    ) -> io::Result<()> {
        let mutation = AclMutation {
            path: canonical_path_or_lexical(Path::new(&mutation.path))?,
            ..mutation
        };
        let _lock = MutationLock::acquire(&self.root)?;
        let mut file = load(&self.root)?;
        let changed = {
            let entry = file
                .entries
                .get_mut(&self.entry.identity)
                .ok_or_else(|| io::Error::other("sandbox registry lost an acquired entry"))?;
            if let Some(expected) = entry.object_ids.get(&mutation.path)
                && observed_id.as_ref() != Some(expected)
            {
                return Err(io::Error::other("sandbox ACL object changed during setup"));
            }
            let mut changed = false;
            if let Some(id) = observed_id
                && let std::collections::btree_map::Entry::Vacant(slot) =
                    entry.object_ids.entry(mutation.path.clone())
            {
                slot.insert(id);
                changed = true;
            }
            if !entry.mutations.contains(&mutation) {
                entry.mutations.push(mutation);
                changed = true;
            }
            changed
        };
        if changed {
            save(&self.root, &file)?;
        }
        self.entry = file.entries[&self.entry.identity].clone();
        Ok(())
    }

    pub(crate) fn validate_admitted_object(&self, path: &Path, id: &str) -> io::Result<()> {
        let path = canonical_path_or_lexical(path)?;
        if self.admitted_objects.get(&path).and_then(Option::as_deref) != Some(id) {
            return Err(io::Error::other(format!(
                "sandbox ACL object {path} changed after resource admission"
            )));
        }
        Ok(())
    }

    pub(crate) fn record_private_path(&mut self, path: &Path) -> io::Result<()> {
        let path = canonical_path_or_lexical(path)?;
        let observed_id = object_id(Path::new(&path))?;
        let _lock = MutationLock::acquire(&self.root)?;
        let mut file = load(&self.root)?;
        let changed = {
            let entry = file
                .entries
                .get_mut(&self.entry.identity)
                .ok_or_else(|| io::Error::other("sandbox registry lost an acquired entry"))?;
            if !entry
                .private_paths
                .iter()
                .any(|parent| Path::new(&path).starts_with(parent))
            {
                if let Some(id) = observed_id {
                    entry.object_ids.insert(path.clone(), id);
                }
                entry
                    .private_paths
                    .retain(|child| !Path::new(child).starts_with(&path));
                entry.private_paths.push(path);
                entry.owned_bytes = owned_bytes(&entry.private_paths)?;
                true
            } else {
                false
            }
        };
        if changed {
            save(&self.root, &file)?;
        }
        self.entry = file.entries[&self.entry.identity].clone();
        Ok(())
    }

    /// The setup caller writes `Preparing` before mutating Windows state, then makes
    /// this transition only after profile and all planned grants are usable.
    pub(crate) fn ready(&mut self) -> io::Result<()> {
        let _lock = MutationLock::acquire(&self.root)?;
        let mut file = load(&self.root)?;
        let entry = file
            .entries
            .get_mut(&self.entry.identity)
            .ok_or_else(|| io::Error::other("sandbox registry lost an acquired entry"))?;
        for (path, expected) in &self.admitted_objects {
            if object_id(Path::new(path))?.as_ref() != expected.as_ref() {
                return Err(io::Error::other("sandbox ACL object changed during setup"));
            }
        }
        for path in entry
            .mutations
            .iter()
            .map(|mutation| &mutation.path)
            .chain(&entry.private_paths)
        {
            let observed = object_id(Path::new(path))?;
            match entry.object_ids.entry(path.clone()) {
                std::collections::btree_map::Entry::Occupied(slot) => {
                    if observed.as_ref() != Some(slot.get()) {
                        return Err(io::Error::other("sandbox ACL object changed during setup"));
                    }
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    if let Some(id) = observed {
                        slot.insert(id);
                    }
                }
            }
        }
        save(&self.root, &file)?;
        drop(_lock);
        self.transition(EntryState::Ready, None)
    }

    fn transition(&mut self, state: EntryState, error: Option<String>) -> io::Result<()> {
        let _lock = MutationLock::acquire(&self.root)?;
        let mut file = load(&self.root)?;
        {
            let entry = file
                .entries
                .get_mut(&self.entry.identity)
                .ok_or_else(|| io::Error::other("sandbox registry lost an acquired entry"))?;
            entry.state = state;
            entry.recovery_error = error;
            entry.last_used_at = now_secs();
        }
        save(&self.root, &file)?;
        self.entry = file.entries[&self.entry.identity].clone();
        Ok(())
    }
}

/// Reject reuse when a recorded object no longer resolves to the identity Nub
/// originally mutated.  The native launcher additionally re-checks the ACE before
/// spawning; a missing/replaced object is recovery work, never a reason to grant a
/// SID onto a newly discovered path.
pub(crate) fn validate_entry(entry: &Entry) -> io::Result<()> {
    for (path, expected) in &entry.object_ids {
        if object_id(Path::new(path))?.as_ref() != Some(expected) {
            return Err(io::Error::other(format!(
                "sandbox resource {} requires recovery: ACL object {path} was replaced or removed",
                entry.profile_name
            )));
        }
    }
    for path in &entry.private_paths {
        validate_private_path(entry, Path::new(path))?;
    }
    for mutation in &entry.mutations {
        validate_object(entry, Path::new(&mutation.path))?;
        let observed = canonical_path(Path::new(&mutation.path)).map_err(|error| {
            io::Error::other(format!(
                "sandbox resource {} requires recovery: recorded ACL object {} is unavailable: {error}",
                entry.profile_name, mutation.path
            ))
        })?;
        if observed != mutation.path {
            return Err(io::Error::other(format!(
                "sandbox resource {} requires recovery: recorded ACL object was replaced",
                entry.profile_name
            )));
        }
    }
    Ok(())
}

/// A path alone is not deletion authority. An interrupted creation without a
/// recorded identity stays recoverable state rather than deleting a replacement.
pub(crate) fn validate_private_path(entry: &Entry, path: &Path) -> io::Result<()> {
    let Some(actual) = object_id(path)? else {
        return Ok(());
    };
    if entry.object_ids.get(&canonical_path_or_lexical(path)?) != Some(&actual) {
        return Err(io::Error::other(format!(
            "sandbox resource {} requires recovery: private path {} has no matching ownership identity",
            entry.profile_name,
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn validate_object(entry: &Entry, path: &Path) -> io::Result<()> {
    let path_text = canonical_path_or_lexical(path)?;
    if let Some(expected) = entry.object_ids.get(&path_text)
        && object_id(path)?
            .as_ref()
            .is_some_and(|actual| actual != expected)
    {
        return Err(io::Error::other(format!(
            "sandbox resource {} requires recovery: ACL object {} was replaced",
            entry.profile_name,
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn object_id(path: &Path) -> io::Result<Option<String>> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        let file = match std::fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                #[cfg(test)]
                eprintln!(
                    "WINDOWS_REGISTRY_ERROR {} object-id {}: {error:?}",
                    std::process::id(),
                    path.display()
                );
                return Err(error);
            }
        };
        object_handle_id(file.as_raw_handle()).map(Some)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(meta) => Ok(Some(format!("{}:{}", meta.dev(), meta.ino()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(windows)]
pub(crate) fn object_handle_id(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<String> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(format!(
        "{}:{}:{}",
        info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
    ))
}

impl Drop for Acquired {
    fn drop(&mut self) {
        // Releasing one caller never tears down a shared identity.  Event liveness is
        // rechecked under the registry lock on the next acquisition/cleanup.
        // Close this caller's kernel lease first: otherwise `prune_with` would see
        // the still-live handle owned by this very Drop frame and retain a phantom
        // active entry forever.
        if let Err(error) = self.close() {
            tracing::warn!(%error, "sandbox registry lease release failed");
        }
    }
}

pub(crate) fn acquire(identity: PolicyIdentity) -> io::Result<Acquired> {
    acquire_at(registry_root()?, identity)
}

fn acquire_at(root: PathBuf, identity: PolicyIdentity) -> io::Result<Acquired> {
    let _lock = MutationLock::acquire(&root)?;
    let mut file = load(&root)?;
    prune_with(&mut file, Lease::live);
    let lease = Lease::create(&identity.hash)?;
    let now = now_secs();
    let (entry, fresh) = match file.entries.get_mut(&identity.hash) {
        Some(entry) => {
            if entry.canonical_policy != identity.canonical {
                return Err(io::Error::other("sandbox registry fingerprint collision"));
            }
            match entry.state {
                EntryState::Ready | EntryState::Idle => {
                    entry.state = EntryState::Ready;
                    entry.last_used_at = now;
                    entry.leases.insert(lease.name.clone());
                    (entry.clone(), false)
                }
                EntryState::Preparing | EntryState::Closing | EntryState::RecoveryNeeded => {
                    return Err(io::Error::other(format!(
                        "sandbox resource {} is awaiting recovery ({:?})",
                        entry.profile_name, entry.state
                    )));
                }
            }
        }
        None => {
            // Admission is bounded even when all existing entries need recovery: never
            // overwrite an ownership record merely to make cache space.
            let idle = file
                .entries
                .values()
                .filter(|e| e.leases.is_empty())
                .count();
            let bytes = file
                .entries
                .values()
                .filter(|entry| entry.leases.is_empty())
                .map(|entry| entry.owned_bytes)
                .sum::<u64>();
            if idle >= MAX_IDLE_ENTRIES || bytes > MAX_OWNED_BYTES {
                return Err(io::Error::other(
                    "sandbox reusable-resource cache is full; cleanup is required before admitting another profile",
                ));
            }
            let entry = Entry {
                identity: identity.hash.clone(),
                policy_identity: identity.policy_hash.clone(),
                profile_name: identity.profile_name(),
                canonical_policy: identity.canonical,
                state: EntryState::Preparing,
                created_at: now,
                last_used_at: now,
                private_paths: Vec::new(),
                owned_bytes: 0,
                mutations: Vec::new(),
                leases: BTreeSet::from([lease.name.clone()]),
                recovery_error: None,
                window_object_revoke: None,
                window_objects: Vec::new(),
                object_ids: identity
                    .objects
                    .iter()
                    .filter_map(|(path, id)| id.as_ref().map(|id| (path.clone(), id.clone())))
                    .collect(),
            };
            file.entries.insert(entry.identity.clone(), entry.clone());
            (entry, true)
        }
    };
    save(&root, &file)?;
    Ok(Acquired {
        entry,
        fresh,
        lease,
        root,
        closed: false,
        admitted_objects: identity.objects,
    })
}

/// Mark idle/dead entries that have crossed the retention bounds as `Closing`.
/// The Windows launcher owns the actual ACE/profile removal because it can verify
/// object identity at the mutation boundary.  A failed removal is left journaled as
/// `RecoveryNeeded`, never silently discarded.
pub(crate) fn begin_recovery(all: bool, reserve_slot: bool) -> io::Result<Vec<Entry>> {
    let root = registry_root().inspect_err(|error| {
        #[cfg(test)]
        eprintln!("WINDOWS_REGISTRY_ERROR recovery-root: {error:?}");
        tracing::warn!(%error, "sandbox recovery registry root failed");
    })?;
    begin_recovery_at(&root, all, reserve_slot)
}

fn begin_recovery_at(root: &Path, all: bool, reserve_slot: bool) -> io::Result<Vec<Entry>> {
    let _lock = MutationLock::acquire(root).inspect_err(|error| {
        #[cfg(test)]
        eprintln!("WINDOWS_REGISTRY_ERROR recovery-lock: {error:?}");
        tracing::warn!(%error, "sandbox recovery journal lock failed");
    })?;
    let mut file = load(root).inspect_err(|error| {
        #[cfg(test)]
        eprintln!("WINDOWS_REGISTRY_ERROR recovery-load: {error:?}");
        tracing::warn!(%error, "sandbox recovery journal read failed");
    })?;
    prune_with(&mut file, Lease::live);
    let now = now_secs();
    for entry in file
        .entries
        .values_mut()
        .filter(|entry| entry.leases.is_empty())
    {
        entry.owned_bytes = owned_bytes(&entry.private_paths).inspect_err(|error| {
            #[cfg(test)]
            eprintln!(
                "WINDOWS_REGISTRY_ERROR recovery-size {:?}: {error:?}",
                entry.private_paths
            );
            tracing::warn!(%error, "sandbox recovery owned-data measurement failed");
        })?;
    }
    let selected = select_recovery(&mut file, all, reserve_slot, now);
    save(root, &file).inspect_err(|error| {
        #[cfg(test)]
        eprintln!("WINDOWS_REGISTRY_ERROR recovery-save: {error:?}");
        tracing::warn!(%error, "sandbox recovery journal write failed");
    })?;
    Ok(selected)
}

fn select_recovery(file: &mut RegistryFile, all: bool, reserve_slot: bool, now: u64) -> Vec<Entry> {
    let mut selected = Vec::new();
    let mut idle: Vec<(String, u64, u64)> = file
        .entries
        .iter()
        .filter(|(_, entry)| entry.leases.is_empty())
        .map(|(identity, entry)| (identity.clone(), entry.last_used_at, entry.owned_bytes))
        .collect();
    idle.sort_by_key(|(_, last_used, _)| *last_used);
    // Admission reserves one slot for a miss; close enforces the actual idle cap.
    // Active entries are never candidates, even under byte or age pressure.
    let mut over_count = idle
        .len()
        .saturating_sub(MAX_IDLE_ENTRIES.saturating_sub(usize::from(reserve_slot)));
    let mut bytes = idle.iter().map(|(_, _, bytes)| *bytes).sum::<u64>();
    let mut pressure = BTreeSet::new();
    for (identity, _, owned) in &idle {
        if over_count == 0 && bytes <= MAX_OWNED_BYTES {
            break;
        }
        pressure.insert(identity.clone());
        over_count = over_count.saturating_sub(1);
        bytes = bytes.saturating_sub(*owned);
    }
    for entry in file.entries.values_mut() {
        let expired = now.saturating_sub(entry.last_used_at) >= MAX_IDLE_AGE.as_secs();
        let recover = matches!(
            entry.state,
            EntryState::Preparing | EntryState::Closing | EntryState::RecoveryNeeded
        );
        if entry.leases.is_empty()
            && (all || expired || recover || pressure.contains(&entry.identity))
        {
            entry.state = EntryState::Closing;
            selected.push(entry.clone());
        }
    }
    selected
}

pub(crate) fn finish_recovery(entry: &Entry, result: io::Result<()>) -> io::Result<()> {
    let root = registry_root()?;
    let _lock = MutationLock::acquire(&root)?;
    let mut file = load(&root)?;
    let Some(current) = file.entries.get_mut(&entry.identity) else {
        return Ok(());
    };
    if !current.leases.is_empty() {
        return Err(io::Error::other("refusing to clean a live sandbox lease"));
    }
    match result {
        Ok(()) => {
            file.entries.remove(&entry.identity);
        }
        Err(error) => {
            current.state = EntryState::RecoveryNeeded;
            current.recovery_error = Some(error.to_string());
        }
    }
    save(&root, &file)
}

/// Durably record that cleanup is about to revoke one window-object grant. If the owner dies
/// after the native DACL write but before the journal update, retry can distinguish that completed
/// removal from a fresh, name-only lookup with no ownership witness.
pub(crate) fn begin_window_object_revoke(entry: &Entry, object: &WindowObject) -> io::Result<bool> {
    let root = registry_root()?;
    let _lock = MutationLock::acquire(&root)?;
    let mut file = load(&root)?;
    let current = file
        .entries
        .get_mut(&entry.identity)
        .ok_or_else(|| io::Error::other("sandbox registry lost a recovering entry"))?;
    let retrying = current.window_object_revoke.as_ref() == Some(object);
    if let Some(in_progress) = &current.window_object_revoke
        && in_progress != object
    {
        return Err(io::Error::other(format!(
            "sandbox window-object cleanup has unfinished revoke progress for {in_progress:?}"
        )));
    }
    current.window_object_revoke = Some(object.clone());
    save(&root, &file)?;
    Ok(retrying)
}

/// Remove a durably completed window-object revoke from the journal before the next object is
/// attempted. A later failure therefore cannot make retry replay a known-completed mutation.
pub(crate) fn finish_window_object_revoke(entry: &Entry, object: &WindowObject) -> io::Result<()> {
    let root = registry_root()?;
    let _lock = MutationLock::acquire(&root)?;
    let mut file = load(&root)?;
    let current = file
        .entries
        .get_mut(&entry.identity)
        .ok_or_else(|| io::Error::other("sandbox registry lost a recovering entry"))?;
    current.window_objects.retain(|recorded| recorded != object);
    if current.window_object_revoke.as_ref() == Some(object) {
        current.window_object_revoke = None;
    }
    #[cfg(test)]
    if std::env::var("__NUB_WINDOWS_CLEANUP_FAULT").as_deref()
        == Ok("cleanup-window-object-journal-save")
    {
        return Err(io::Error::other(
            "injected window-object cleanup journal save failure",
        ));
    }
    save(&root, &file)
}

fn release(root: &Path, identity: &str, lease: &str) -> io::Result<()> {
    let _lock = MutationLock::acquire(root)?;
    let mut file = load(root)?;
    if file.entries.contains_key(identity) {
        let entry = file.entries.get_mut(identity).expect("checked");
        entry.leases.remove(lease);
        prune_with(&mut file, Lease::live);
        if let Some(entry) = file.entries.get_mut(identity)
            && entry.leases.is_empty()
            && matches!(entry.state, EntryState::Ready | EntryState::Idle)
        {
            entry.state = EntryState::Idle;
            entry.last_used_at = now_secs();
        }
        save(root, &file)?;
    }
    Ok(())
}

fn prune_with(file: &mut RegistryFile, live: impl Fn(&str) -> bool) {
    for entry in file.entries.values_mut() {
        entry.leases.retain(|lease| live(lease));
        if entry.leases.is_empty() && matches!(entry.state, EntryState::Ready) {
            entry.state = EntryState::Idle;
        }
    }
}

#[cfg(windows)]
pub(crate) fn native_assets_path(profile: &str) -> io::Result<PathBuf> {
    // Only generated profile names reach here, never caller-authored paths.
    if !profile.starts_with("nub_sbx_r_")
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid sandbox profile name",
        ));
    }
    Ok(registry_root()?.join(format!("native-{profile}")))
}

fn registry_root() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        // A protected leaf below writable home is insufficient: a confined caller
        // could rename an ancestor and replace the path. ProgramData's OS-owned
        // parent is outside home/tool grants; the user creates only its own leaf.
        let parent = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::other("ProgramData is required for the Windows sandbox registry")
            })?;
        let parent = std::fs::canonicalize(parent)?;
        let (name, sid) = current_user_sid()?;
        let root = parent.join(format!("nub-sandbox-{name}"));
        let created = match std::fs::create_dir(&root) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        protect_registry_root(&root, sid.as_ptr().cast_mut().cast(), created)?;
        Ok(root)
    }
    #[cfg(not(windows))]
    {
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::other("LOCALAPPDATA is required for the Windows sandbox registry")
            })?;
        Ok(root.join("nub").join("sandbox-registry"))
    }
}

#[cfg(windows)]
fn current_user_sid() -> io::Result<(String, Vec<u32>)> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut bytes);
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    };
    let error = io::Error::last_os_error();
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(error);
    }
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let len = unsafe { GetLengthSid(sid) } as usize;
    let mut owned = vec![0u32; len.div_ceil(4)];
    unsafe {
        std::ptr::copy_nonoverlapping(sid.cast::<u8>(), owned.as_mut_ptr().cast(), len);
    }
    let mut text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe {
        LocalFree(text.cast());
    }
    Ok((name, owned))
}

#[cfg(windows)]
fn protect_registry_root(
    root: &Path,
    user: windows_sys::Win32::Security::PSID,
    created: bool,
) -> io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, SE_FILE_OBJECT, SetEntriesInAclW,
        SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
        GetSecurityDescriptorControl, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    };
    if std::fs::symlink_metadata(root)?.file_attributes() & 0x400 != 0 {
        return Err(io::Error::other(
            "sandbox registry root must not be a reparse point",
        ));
    }
    let path = wide(&root.to_string_lossy());
    let mut owner = std::ptr::null_mut();
    let mut existing_acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    let result = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut existing_acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let owned = unsafe { EqualSid(owner, user) } != 0;
    // Reapplying an inheritable DACL walks the live journal children. Other
    // acquisitions reach this before their resource lock, so those walks can
    // race atomic journal replacement. Validate an unchanged root without writes.
    let mut control = 0;
    let mut revision = 0;
    let mut ace = std::ptr::null_mut();
    let private = owned
        && unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } != 0
        && control & SE_DACL_PROTECTED != 0
        && !existing_acl.is_null()
        && unsafe { (*existing_acl).AceCount } == 1
        && unsafe { GetAce(existing_acl, 0, &mut ace) } != 0
        && unsafe {
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            ace.Header.AceType == 0
                && ace.Header.AceFlags == 3
                && ace.Mask == 0x001f_01ff
                && EqualSid(std::ptr::addr_of!(ace.SidStart).cast_mut().cast(), user) != 0
        };
    unsafe {
        LocalFree(descriptor);
    }
    if private {
        return Ok(false);
    }
    if !owned && !created {
        return Err(io::Error::other(
            "sandbox registry root belongs to another principal",
        ));
    }
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: 0x001f_01ff,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: 3,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.cast(),
        },
    };
    let mut acl = std::ptr::null_mut();
    let result = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let result = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION
                | if created {
                    OWNER_SECURITY_INFORMATION
                } else {
                    0
                },
            if created { user } else { std::ptr::null_mut() },
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    unsafe {
        LocalFree(acl.cast());
    }
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(true)
}

pub(crate) fn reject_registry_grant(path: &Path) -> io::Result<()> {
    let registry = registry_root()?;
    let path = PathBuf::from(canonical_path_or_lexical(path)?);
    let registry = PathBuf::from(normalize(&registry));
    if registry.starts_with(&path) || path.starts_with(&registry) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sandbox grants cannot include the host ownership registry",
        ));
    }
    Ok(())
}

fn load(root: &Path) -> io::Result<RegistryFile> {
    std::fs::create_dir_all(root)?;
    let path = root.join("registry.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let file: RegistryFile = serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::other(format!(
                    "invalid sandbox registry {}: {error}",
                    path.display()
                ))
            })?;
            if file.schema != SCHEMA_VERSION {
                return Err(io::Error::other(format!(
                    "unsupported sandbox registry schema {}",
                    file.schema
                )));
            }
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RegistryFile {
            schema: SCHEMA_VERSION,
            ..Default::default()
        }),
        Err(error) => Err(error),
    }
}

fn save(root: &Path, file: &RegistryFile) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    // Every writer holds the journal lock. Reuse one staging slot so a crash
    // before replacement cannot accumulate untracked files across invocations.
    let tmp = root.join("registry.tmp");
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    use std::io::Write as _;
    let mut journal = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    journal.write_all(&bytes)?;
    journal.sync_all()?;
    drop(journal);
    let destination = root.join("registry.json");
    #[cfg(windows)]
    {
        // `std::fs::rename` does not replace an existing destination on Windows.
        // Never emulate replacement with remove+rename: a power loss in that gap
        // would erase the very journal needed to recover persistent ACL mutations.
        unsafe extern "system" {
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
        let from = wide(&tmp.to_string_lossy());
        let to = wide(&destination.to_string_lossy());
        if unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            #[cfg(test)]
            eprintln!(
                "WINDOWS_REGISTRY_ERROR replace-journal {}: {error:?}",
                destination.display()
            );
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, destination)
    }
}

fn canonical_path(path: &Path) -> io::Result<String> {
    Ok(normalize(std::fs::canonicalize(path)?.as_path()))
}

fn canonical_path_or_lexical(path: &Path) -> io::Result<String> {
    match std::fs::canonicalize(path) {
        Ok(path) => Ok(normalize(&path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => Ok(normalize(
                    &PathBuf::from(canonical_path_or_lexical(parent)?).join(name),
                )),
                _ => Ok(normalize(path)),
            }
        }
        Err(error) => {
            #[cfg(test)]
            eprintln!(
                "WINDOWS_REGISTRY_ERROR {} canonical-path {}: {error:?}",
                std::process::id(),
                path.display()
            );
            Err(error)
        }
    }
}

fn normalize(path: &Path) -> String {
    #[cfg(not(windows))]
    return path.to_string_lossy().into_owned();
    #[cfg(windows)]
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_start_matches("\\\\?\\")
        .to_ascii_lowercase()
}

fn owned_bytes(paths: &[String]) -> io::Result<u64> {
    fn size(path: &Path) -> io::Result<u64> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        // Reparse/symlink targets are caller data, never owned cache bytes.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Ok(0);
            }
        }
        if metadata.is_symlink() {
            return Ok(0);
        }
        if !metadata.is_dir() {
            return Ok(metadata.len());
        }
        let mut bytes = 0u64;
        for entry in std::fs::read_dir(path)? {
            bytes = bytes.saturating_add(size(&entry?.path())?);
        }
        Ok(bytes)
    }
    let mut total = 0u64;
    for path in paths {
        total = total.saturating_add(size(Path::new(path))?);
    }
    Ok(total)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// File locks coordinate even independent logon sessions; kernel release on
/// process death makes a stale lock file harmless. Separate locks keep journal
/// updates possible while one resource setup or DACL operation is in progress.
pub(crate) struct OperationLock {
    _file: std::fs::File,
}

impl OperationLock {
    pub(crate) fn acquire(kind: &str) -> io::Result<Self> {
        #[cfg(all(test, windows))]
        if kind == "acl"
            && let Some(root) = std::env::var_os("__NUB_WINDOWS_CLEANUP_ACL_LOCK_ROOT")
        {
            // Isolated crash-test journals still mutate the shared desktop DACL.
            return Self::at(Path::new(&root), kind);
        }
        Self::at(&registry_root()?, kind)
    }

    fn at(root: &Path, kind: &str) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(format!("{kind}.lock")))?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

struct MutationLock {
    _lock: OperationLock,
}
impl MutationLock {
    fn acquire(root: &Path) -> io::Result<Self> {
        Ok(Self {
            _lock: OperationLock::at(root, "journal")?,
        })
    }
}

#[cfg(all(test, windows))]
pub(crate) fn test_registry_root() -> io::Result<PathBuf> {
    registry_root()
}

#[cfg(all(test, windows))]
pub(crate) fn test_entry(profile: &str) -> io::Result<Option<Entry>> {
    let root = registry_root()?;
    let _lock = MutationLock::acquire(&root)?;
    Ok(load(&root)?
        .entries
        .into_values()
        .find(|entry| entry.profile_name == profile))
}

#[cfg(all(test, windows))]
pub(crate) fn test_insert_window_object_recovery(
    profile_name: &str,
    object: WindowObject,
) -> io::Result<()> {
    let root = registry_root()?;
    let _lock = MutationLock::acquire(&root)?;
    let mut file = load(&root)?;
    let now = now_secs();
    file.entries.insert(
        profile_name.to_string(),
        Entry {
            identity: profile_name.to_string(),
            policy_identity: None,
            canonical_policy: "test window-object recovery".to_string(),
            profile_name: profile_name.to_string(),
            state: EntryState::Closing,
            created_at: now,
            last_used_at: now,
            private_paths: Vec::new(),
            owned_bytes: 0,
            mutations: Vec::new(),
            leases: BTreeSet::new(),
            recovery_error: None,
            window_object_revoke: None,
            window_objects: vec![object],
            object_ids: BTreeMap::new(),
        },
    );
    save(&root, &file)
}

#[cfg(all(test, windows))]
pub(crate) fn test_remove_entry(profile_name: &str) -> io::Result<()> {
    let root = registry_root()?;
    let _lock = MutationLock::acquire(&root)?;
    let mut file = load(&root)?;
    file.entries.remove(profile_name);
    save(&root, &file)
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_adapter_version_participates_in_policy_identity() {
        let identity = super::PolicyIdentity::new([], [], [], None, false, false).unwrap();
        assert_eq!(identity, identity.clone().with_native_compat(None));
        let first = identity.clone().with_native_compat(Some("adapter-one"));
        assert_eq!(
            first,
            identity.clone().with_native_compat(Some("adapter-one"))
        );
        assert_ne!(
            first,
            identity.clone().with_native_compat(Some("adapter-two"))
        );
        assert_ne!(first, identity);
    }
    use super::*;

    #[test]
    fn journal_replacement_recovers_an_interrupted_staging_write() {
        let root = tempfile::tempdir().unwrap();
        let file = RegistryFile {
            schema: SCHEMA_VERSION,
            ..Default::default()
        };
        for _ in 0..3 {
            std::fs::write(root.path().join("registry.tmp"), b"interrupted").unwrap();
            let _lock = MutationLock::acquire(root.path()).unwrap();
            save(root.path(), &file).unwrap();
            assert!(load(root.path()).unwrap().entries.is_empty());
            assert!(!root.path().join("registry.tmp").exists());
        }
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            2,
            "only journal and lock remain"
        );
    }

    #[cfg(windows)]
    #[test]
    fn protected_registry_validation_does_not_rewrite_unchanged_acls() {
        let root = tempfile::tempdir().unwrap();
        let (_, sid) = current_user_sid().unwrap();
        let user = sid.as_ptr().cast_mut().cast();
        assert!(protect_registry_root(root.path(), user, true).unwrap());
        assert!(!protect_registry_root(root.path(), user, false).unwrap());

        // An unexpected additional principal must still be removed on validation.
        super::super::launch::test_set_profile_ace(
            "nub-test-registry-validation",
            root.path(),
            true,
        )
        .unwrap();
        assert!(protect_registry_root(root.path(), user, false).unwrap());
        assert!(!protect_registry_root(root.path(), user, false).unwrap());
        assert!(
            !super::super::launch::test_profile_has_ace(
                "nub-test-registry-validation",
                root.path(),
            )
            .unwrap()
        );
    }

    fn id(root: &Path) -> PolicyIdentity {
        let read = root.join("read");
        let node = root.join("node");
        let write = root.join("write");
        std::fs::create_dir_all(&read).unwrap();
        std::fs::create_dir_all(&node).unwrap();
        std::fs::create_dir_all(&write).unwrap();
        PolicyIdentity::new([read], [node], [write], None, false, false).unwrap()
    }

    #[test]
    fn identity_is_order_independent_and_changes_with_positive_grants() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let first =
            PolicyIdentity::new([a.clone(), b.clone()], [], [], None, false, false).unwrap();
        let second = PolicyIdentity::new([b, a.clone()], [], [], None, false, false).unwrap();
        let changed = PolicyIdentity::new([a], [], [], None, false, false).unwrap();
        assert_eq!(first.hash, second.hash);
        assert_ne!(first.hash, changed.hash);
        assert!(!first.canonical.contains("TOKEN"));
    }

    #[test]
    fn acquisition_persists_intent_before_ready_and_reuses_only_ready_entries() {
        let dir = tempfile::tempdir().unwrap();
        let identity = id(dir.path());
        let mut first = acquire_at(dir.path().join("registry"), identity.clone()).unwrap();
        assert!(first.fresh);
        assert_eq!(first.entry.state, EntryState::Preparing);
        first.ready().unwrap();
        drop(first);
        let second = acquire_at(dir.path().join("registry"), identity).unwrap();
        assert!(!second.fresh);
        assert_eq!(second.entry.state, EntryState::Ready);
    }

    #[test]
    fn stale_leases_are_pruned_but_live_leases_are_not_evicted() {
        let mut file = RegistryFile {
            schema: SCHEMA_VERSION,
            ..Default::default()
        };
        file.entries.insert(
            "x".to_string(),
            Entry {
                identity: "x".to_string(),
                policy_identity: None,
                canonical_policy: "p".to_string(),
                profile_name: "n".to_string(),
                state: EntryState::Ready,
                created_at: 0,
                last_used_at: 0,
                private_paths: Vec::new(),
                owned_bytes: 0,
                mutations: Vec::new(),
                leases: BTreeSet::from(["live".to_string(), "dead".to_string()]),
                recovery_error: None,
                window_object_revoke: None,
                window_objects: Vec::new(),
                object_ids: BTreeMap::new(),
            },
        );
        prune_with(&mut file, |name| name == "live");
        let entry = &file.entries["x"];
        assert_eq!(entry.leases, BTreeSet::from(["live".to_string()]));
        assert_eq!(entry.state, EntryState::Ready);
        prune_with(&mut file, |_| false);
        assert_eq!(file.entries["x"].state, EntryState::Idle);
    }

    #[test]
    fn network_policies_do_not_share_package_loopback_identity() {
        use crate::policy::{Effect, NetPolicy, NetRule, NetTarget};
        let dir = tempfile::tempdir().unwrap();
        let identity = id(dir.path());
        let network = |host: &str| NetPolicy {
            enforce: true,
            rules: vec![NetRule {
                target: NetTarget::Host(host.to_string()),
                effect: Effect::Allow,
            }],
            ..Default::default()
        };
        assert_ne!(
            identity
                .clone()
                .with_network(Some(&network("one.example")))
                .unwrap()
                .hash,
            identity
                .with_network(Some(&network("two.example")))
                .unwrap()
                .hash
        );
    }

    #[test]
    fn owned_budget_counts_nested_files_not_directory_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/data"), vec![0; 16384]).unwrap();
        assert_eq!(
            owned_bytes(&[dir.path().display().to_string()]).unwrap(),
            16384
        );
    }

    #[test]
    fn replaced_acl_object_refuses_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let mut acquired = acquire_at(dir.path().join("registry"), id(dir.path())).unwrap();
        let path = dir.path().join("read");
        acquired
            .record_mutation(AclMutation {
                path: path.display().to_string(),
                kind: AclKind::Subtree,
                access: 1,
            })
            .unwrap();
        acquired.ready().unwrap();
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(validate_entry(&acquired.entry).is_err());
    }

    #[test]
    fn object_incarnations_share_a_policy_but_not_a_resource() {
        let dir = tempfile::tempdir().unwrap();
        let policy = id(dir.path());
        let path = dir.path().join("read");
        let first = policy.clone().with_objects([path.clone()]).unwrap();
        let same = policy.clone().with_objects([path.clone()]).unwrap();
        assert_eq!(first, same);
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        let replacement = policy.clone().with_objects([path]).unwrap();
        assert_ne!(first.hash, replacement.hash);
        assert_eq!(first.policy_hash, Some(policy.hash));
        assert_eq!(first.policy_hash, replacement.policy_hash);
    }

    #[test]
    fn acquisition_cannot_relabel_an_admitted_object_during_setup() {
        let dir = tempfile::tempdir().unwrap();
        let policy = id(dir.path());
        let path = dir.path().join("read");
        let identity = policy.with_objects([path.clone()]).unwrap();
        let mut resource = acquire_at(dir.path().join("registry"), identity).unwrap();
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            resource
                .record_mutation(AclMutation {
                    path: path.display().to_string(),
                    kind: AclKind::Subtree,
                    access: 1,
                })
                .is_err()
        );
        assert!(resource.ready().is_err());
    }

    #[test]
    fn legacy_registry_entries_remain_readable_without_a_policy_key() {
        let entry = idle_entry(0, 1);
        let mut json = serde_json::to_value(&entry).unwrap();
        json.as_object_mut().unwrap().remove("policy_identity");
        let decoded: Entry = serde_json::from_value(json).unwrap();
        assert!(decoded.policy_identity.is_none());
        assert_eq!(decoded.identity, entry.identity);
        assert_eq!(decoded.object_ids, entry.object_ids);
    }

    #[test]
    fn preparation_records_existing_acl_identity_before_ready() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("registry");
        let mut acquired = acquire_at(root.clone(), id(dir.path())).unwrap();
        let path = dir.path().join("read");
        acquired
            .record_mutation(AclMutation {
                path: path.display().to_string(),
                kind: AclKind::Subtree,
                access: 1,
            })
            .unwrap();
        let recorded = load(&root)
            .unwrap()
            .entries
            .remove(&acquired.entry.identity)
            .unwrap();
        assert!(
            recorded
                .object_ids
                .contains_key(&canonical_path(&path).unwrap())
        );
        std::fs::rename(&path, dir.path().join("original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(validate_object(&recorded, &path).is_err());
        assert!(acquired.ready().is_err());
    }

    #[test]
    fn replaced_private_root_is_not_cleanup_authority() {
        let dir = tempfile::tempdir().unwrap();
        let mut acquired = acquire_at(dir.path().join("registry"), id(dir.path())).unwrap();
        let path = dir.path().join("private");
        std::fs::create_dir(&path).unwrap();
        acquired.record_private_path(&path).unwrap();
        acquired.ready().unwrap();
        validate_private_path(&acquired.entry, &path).unwrap();
        std::fs::rename(&path, dir.path().join("original-private")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("caller-output"), b"keep").unwrap();
        assert!(validate_private_path(&acquired.entry, &path).is_err());
        assert!(validate_entry(&acquired.entry).is_err());
        assert_eq!(std::fs::read(path.join("caller-output")).unwrap(), b"keep");
    }

    #[test]
    fn interrupted_private_creation_does_not_authorize_an_unknown_object() {
        let dir = tempfile::tempdir().unwrap();
        let mut acquired = acquire_at(dir.path().join("registry"), id(dir.path())).unwrap();
        let path = dir.path().join("private");
        acquired.record_private_path(&path).unwrap();
        validate_private_path(&acquired.entry, &path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(validate_private_path(&acquired.entry, &path).is_err());
        acquired.ready().unwrap();
        validate_private_path(&acquired.entry, &path).unwrap();
    }

    #[test]
    fn abandoned_closing_record_is_selected_for_recovery_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("registry");
        let mut acquired = acquire_at(root.clone(), id(dir.path())).unwrap();
        acquired.ready().unwrap();
        drop(acquired);
        let first = begin_recovery_at(&root, true, false).unwrap();
        assert_eq!(first.len(), 1);
        let second = begin_recovery_at(&root, false, false).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].identity, first[0].identity);
    }

    fn idle_entry(index: usize, now: u64) -> Entry {
        Entry {
            identity: index.to_string(),
            policy_identity: None,
            canonical_policy: String::new(),
            profile_name: format!("fixture-{index}"),
            state: EntryState::Idle,
            created_at: now,
            last_used_at: now,
            private_paths: Vec::new(),
            owned_bytes: 0,
            mutations: Vec::new(),
            leases: BTreeSet::new(),
            recovery_error: None,
            window_object_revoke: None,
            window_objects: Vec::new(),
            object_ids: BTreeMap::new(),
        }
    }

    #[test]
    fn close_enforces_idle_count_without_evicting_active_resources() {
        let now = MAX_IDLE_AGE.as_secs() + 1;
        let mut file = RegistryFile {
            schema: SCHEMA_VERSION,
            entries: (0..MAX_IDLE_ENTRIES)
                .map(|index| {
                    let entry = idle_entry(index, now);
                    (entry.identity.clone(), entry)
                })
                .collect(),
        };
        assert!(select_recovery(&mut file, false, false, now).is_empty());
        let extra = idle_entry(MAX_IDLE_ENTRIES, now);
        file.entries.insert(extra.identity.clone(), extra);
        let mut live = idle_entry(MAX_IDLE_ENTRIES + 1, 0);
        live.state = EntryState::Ready;
        live.owned_bytes = MAX_OWNED_BYTES + 1;
        live.leases.insert("live".into());
        file.entries.insert(live.identity.clone(), live.clone());
        let selected = select_recovery(&mut file, false, false, now);
        assert_eq!(selected.len(), 1);
        assert_ne!(selected[0].identity, live.identity);
        file.entries.remove(&selected[0].identity);
        assert!(select_recovery(&mut file, false, false, now).is_empty());
        assert_eq!(select_recovery(&mut file, false, true, now).len(), 1);
        assert_eq!(file.entries[&live.identity].state, EntryState::Ready);
    }

    #[test]
    fn close_enforces_byte_age_and_failed_cleanup_bounds() {
        let now = MAX_IDLE_AGE.as_secs() + 5;
        let mut large = idle_entry(0, now - 1);
        large.owned_bytes = MAX_OWNED_BYTES;
        let mut recent = idle_entry(1, now);
        recent.owned_bytes = 1;
        let expired = idle_entry(2, now - MAX_IDLE_AGE.as_secs());
        let mut failed = idle_entry(3, now);
        failed.state = EntryState::RecoveryNeeded;
        failed.recovery_error = Some("retained failure".into());
        let mut file = RegistryFile {
            schema: SCHEMA_VERSION,
            entries: [large, recent, expired, failed]
                .into_iter()
                .map(|entry| (entry.identity.clone(), entry))
                .collect(),
        };
        let selected = select_recovery(&mut file, false, false, now);
        let ids: BTreeSet<_> = selected
            .iter()
            .map(|entry| entry.identity.as_str())
            .collect();
        assert_eq!(ids, BTreeSet::from(["0", "2", "3"]));
        assert_eq!(file.entries["1"].state, EntryState::Idle);
        assert_eq!(
            file.entries["3"].recovery_error.as_deref(),
            Some("retained failure")
        );
    }

    #[test]
    fn last_close_starts_idle_age_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("registry");
        let mut acquired = acquire_at(root.clone(), id(dir.path())).unwrap();
        acquired.ready().unwrap();
        let mut file = load(&root).unwrap();
        file.entries
            .get_mut(&acquired.entry.identity)
            .unwrap()
            .last_used_at = 0;
        save(&root, &file).unwrap();
        let before = now_secs();
        acquired.close().unwrap();
        let file = load(&root).unwrap();
        let entry = &file.entries[&acquired.entry.identity];
        assert_eq!(entry.state, EntryState::Idle);
        assert!(entry.last_used_at >= before);
        let closed_at = entry.last_used_at;
        acquired.close().unwrap();
        assert_eq!(
            load(&root).unwrap().entries[&acquired.entry.identity].last_used_at,
            closed_at
        );
    }

    #[test]
    fn managed_tmp_marker_changes_identity_without_a_generated_path() {
        let dir = tempfile::tempdir().unwrap();
        let identity = id(dir.path());
        let private = identity.clone().with_private_tmp(true);
        let shared = identity.with_private_tmp(false);
        assert_ne!(private.hash, shared.hash);
        assert!(private.canonical.ends_with("managed-tmp=profile/AC/Temp"));
    }
}
