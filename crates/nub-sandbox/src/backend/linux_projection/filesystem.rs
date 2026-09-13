use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, IoctlFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyIoctl, ReplyOpen,
    ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use super::backing::{
    Backing, child_path, directory_names, error, read_link, reopen_regular,
    require_metadata_support, same_object, set_mode, set_owner, set_times,
};
use super::rules::Rules;
use crate::policy::FsAccess;

const ROOT: u64 = 1;
const MAX_NODES: usize = 65_536;
const MAX_HANDLES: usize = 16_384;
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_IO: usize = 1024 * 1024;
const TTL: Duration = Duration::ZERO;
const XATTR_CREATE: i32 = 0x1;
const XATTR_REPLACE: i32 = 0x2;

/// The user-ID domain carried by this projection's FUSE protocol.
///
/// A fixture that serves inside its private user namespace keeps the historical
/// identity-preserving behavior.  A parent-owned server, by contrast, runs in
/// the host namespace while its FUSE connection belongs to a one-ID child user
/// namespace.  Its protocol ID zero is therefore the captured host launcher,
/// not host root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProjectionIdentity {
    /// FUSE protocol IDs already name the backing filesystem's IDs.
    InNamespace,
    /// The connection's namespace maps only protocol ID zero to these backing
    /// IDs.  Every other backing identity is deliberately unrepresentable.
    Parent { host_uid: u32, host_gid: u32 },
}

impl ProjectionIdentity {
    // FUSE_INVALID_UIDGID.  This value is intentionally not a mapped protocol
    // identity: the kernel turns its invalid one-ID-map translation into the
    // user namespace's configured overflow identity for stat-like results.
    const UNMAPPED_ID: u32 = u32::MAX;

    fn outgoing_uid(self, uid: u32) -> u32 {
        match self {
            Self::InNamespace => uid,
            Self::Parent { host_uid, .. } if uid == host_uid => 0,
            Self::Parent { .. } => Self::UNMAPPED_ID,
        }
    }

    fn outgoing_gid(self, gid: u32) -> u32 {
        match self {
            Self::InNamespace => gid,
            Self::Parent { host_gid, .. } if gid == host_gid => 0,
            Self::Parent { .. } => Self::UNMAPPED_ID,
        }
    }

    fn incoming_owner(
        self,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> io::Result<(Option<u32>, Option<u32>)> {
        let map_uid = |uid| match self {
            Self::InNamespace => Ok(uid),
            Self::Parent { host_uid, .. } if uid == 0 => Ok(host_uid),
            Self::Parent { .. } => Err(error(libc::EINVAL)),
        };
        let map_gid = |gid| match self {
            Self::InNamespace => Ok(gid),
            Self::Parent { host_gid, .. } if gid == 0 => Ok(host_gid),
            Self::Parent { .. } => Err(error(libc::EINVAL)),
        };

        // Translate both fields before opening the target so an unmapped
        // counterpart can never leave a partial chown behind.
        Ok((uid.map(map_uid).transpose()?, gid.map(map_gid).transpose()?))
    }
}

// FUSE forwards kernel flags, not libc's user API values. On 64-bit glibc,
// O_LARGEFILE is zero even though the kernel sends its architecture-specific bit.
#[cfg(target_arch = "x86_64")]
const KERNEL_O_LARGEFILE: i32 = 1 << 15;
#[cfg(target_arch = "aarch64")]
const KERNEL_O_LARGEFILE: i32 = 1 << 17;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const KERNEL_O_LARGEFILE: i32 = libc::O_LARGEFILE;
const KERNEL_FMODE_EXEC: i32 = 1 << 5;

fn accepted_rename_flags(flags: u32) -> bool {
    matches!(flags, 0 | libc::RENAME_NOREPLACE | libc::RENAME_EXCHANGE)
}

fn rebase_path(path: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    path.strip_prefix(old).ok().map(|suffix| {
        if suffix.as_os_str().is_empty() {
            new.to_path_buf()
        } else {
            new.join(suffix)
        }
    })
}

fn normalize_open_flags(flags: i32) -> io::Result<i32> {
    // execve's internal marker is not a flag for the backing open syscall.
    // It never permits writing or creating an executable through this request.
    if flags & KERNEL_FMODE_EXEC != 0
        && flags & (libc::O_ACCMODE | libc::O_TRUNC | libc::O_CREAT) != libc::O_RDONLY
    {
        return Err(error(libc::EOPNOTSUPP));
    }
    let large_file = if flags & KERNEL_O_LARGEFILE != 0 {
        libc::O_LARGEFILE
    } else {
        0
    };
    Ok((flags & !(KERNEL_FMODE_EXEC | KERNEL_O_LARGEFILE)) | large_file)
}

struct Node {
    path: PathBuf,
    pin: File,
}

struct Inode {
    node: Arc<Node>,
    lookups: u64,
}

struct Handle {
    inode: u64,
    node: Arc<Node>,
    kind: HandleKind,
    authority: FsAccess,
}

enum HandleKind {
    File { file: File, writable: bool },
    Directory(Vec<(OsString, FileType)>),
}

struct State {
    backing: Backing,
    rules: Rules,
    identity: ProjectionIdentity,
    nodes: HashMap<u64, Inode>,
    paths: BTreeMap<PathBuf, u64>,
    handles: HashMap<u64, Handle>,
    next_inode: u64,
    next_handle: u64,
    export: Option<ExportSlot>,
}

struct ExportSlot {
    tid: u32,
    armed: bool,
    file: Option<File>,
}

/// An xattr path bound to one provider-held inode pin.
///
/// Linux before its newer `*xattrat` syscalls cannot address an O_PATH pin
/// directly with `f*xattr`: those calls return EBADF. Its procfs magic link,
/// however, resolves to the held struct path without re-looking up a mutable
/// backing name, including when the pin names a symlink.
struct XattrTarget {
    _pin: File,
    path: CString,
}

enum XattrReply {
    Size(u32),
    Data(Vec<u8>),
}

pub(super) const EXPORT_IOCTL: libc::c_ulong = 0x4e80;

#[derive(Clone)]
pub(crate) struct Projection(Arc<Mutex<State>>);

impl Projection {
    pub(super) fn new(rules: Rules, backing: Backing) -> io::Result<Self> {
        Self::new_with_identity(rules, backing, ProjectionIdentity::InNamespace)
    }

    /// Create a projection with an explicit FUSE protocol identity domain.
    /// Parent-owned service acquisition must pass `ProjectionIdentity::Parent`;
    /// retaining `new` preserves the old in-namespace fixture behavior.
    pub(super) fn new_with_identity(
        rules: Rules,
        backing: Backing,
        identity: ProjectionIdentity,
    ) -> io::Result<Self> {
        require_metadata_support()?;
        let root = Arc::new(Node {
            path: PathBuf::from("/"),
            pin: backing.pin(Path::new("/"))?,
        });
        Ok(Self(Arc::new(Mutex::new(State {
            backing,
            rules,
            identity,
            nodes: HashMap::from([(
                ROOT,
                Inode {
                    node: root,
                    lookups: 1,
                },
            )]),
            paths: BTreeMap::from([(PathBuf::from("/"), ROOT)]),
            handles: HashMap::new(),
            next_inode: ROOT + 1,
            next_handle: 1,
            export: None,
        }))))
    }

    fn state(&self) -> io::Result<MutexGuard<'_, State>> {
        self.0.lock().map_err(|_| error(libc::EIO))
    }

    pub(super) fn register_export(&self, tid: u32) -> io::Result<()> {
        let mut state = self.state()?;
        if !state.backing.supports_export() || state.export.is_some() {
            return Err(error(libc::EACCES));
        }
        state.export = Some(ExportSlot {
            tid,
            armed: false,
            file: None,
        });
        Ok(())
    }

    pub(super) fn unregister_export(&self) {
        if let Ok(mut state) = self.state() {
            state.export = None;
        }
    }

    pub(super) fn arm_export(&self) -> io::Result<()> {
        let mut state = self.state()?;
        let slot = state.export.as_mut().ok_or_else(|| error(libc::EACCES))?;
        if slot.armed || slot.file.is_some() {
            return Err(error(libc::EBUSY));
        }
        slot.armed = true;
        Ok(())
    }

    pub(super) fn take_export(&self) -> io::Result<File> {
        let mut state = self.state()?;
        let slot = state.export.as_mut().ok_or_else(|| error(libc::EACCES))?;
        slot.armed = false;
        slot.file.take().ok_or_else(|| error(libc::EIO))
    }
}

impl State {
    fn node(&self, ino: u64) -> io::Result<Arc<Node>> {
        self.nodes
            .get(&ino)
            .map(|entry| Arc::clone(&entry.node))
            .ok_or_else(|| error(libc::ESTALE))
    }

    fn child(&self, parent: u64, name: &OsStr) -> io::Result<PathBuf> {
        self.child_parent(parent, name).map(|(path, _)| path)
    }

    /// Return a fresh, held parent descriptor with its policy child path. The
    /// descriptor binds the namespace operation to this parent after validation;
    /// a host may still rename a final component before the later kernel syscall.
    fn child_parent(&self, parent: u64, name: &OsStr) -> io::Result<(PathBuf, File)> {
        let node = self.node(parent)?;
        if !node.pin.metadata()?.is_dir() {
            return Err(error(libc::ENOTDIR));
        }
        let parent = self.current(&node)?;
        Ok((child_path(&node.path, name)?, parent))
    }

    fn current(&self, node: &Node) -> io::Result<File> {
        let pin = self.backing.pin(&node.path)?;
        if !same_object(&node.pin.metadata()?, &pin.metadata()?) {
            return Err(error(libc::ESTALE));
        }
        Ok(pin)
    }

    fn visible(&self, path: &Path, meta: &Metadata) -> bool {
        self.rules.access(path).is_some() || (meta.is_dir() && self.rules.traversable(path))
    }

    fn intern(&mut self, path: PathBuf, pin: File) -> io::Result<u64> {
        if let Some(&ino) = self.paths.get(&path)
            && let Some(entry) = self.nodes.get_mut(&ino)
            && same_object(&entry.node.pin.metadata()?, &pin.metadata()?)
        {
            entry.lookups = entry
                .lookups
                .checked_add(1)
                .ok_or_else(|| error(libc::EOVERFLOW))?;
            return Ok(ino);
        }
        if self.nodes.len() == MAX_NODES {
            return Err(error(libc::EMFILE));
        }
        let ino = self.next_inode;
        self.next_inode = ino.checked_add(1).ok_or_else(|| error(libc::EOVERFLOW))?;
        self.paths.insert(path.clone(), ino);
        self.nodes.insert(
            ino,
            Inode {
                node: Arc::new(Node { path, pin }),
                lookups: 1,
            },
        );
        Ok(ino)
    }

    fn forget(&mut self, ino: u64, count: u64) {
        if ino == ROOT {
            return;
        }
        if let Some(entry) = self.nodes.get_mut(&ino) {
            entry.lookups = entry.lookups.saturating_sub(count);
            if entry.lookups == 0 {
                let path = entry.node.path.clone();
                self.nodes.remove(&ino);
                if self.paths.get(&path) == Some(&ino) {
                    self.paths.remove(&path);
                }
            }
        }
    }

    fn lookup(&mut self, parent: u64, name: &OsStr) -> io::Result<FileAttr> {
        let path = self.child(parent, name)?;
        if !self.rules.traversable(&path) {
            return Err(error(libc::ENOENT));
        }
        let pin = self.backing.pin(&path)?;
        let meta = pin.metadata()?;
        if !self.visible(&path, &meta) {
            return Err(error(libc::ENOENT));
        }
        file_kind(&meta)?;
        let access = self.rules.access(&path);
        let ino = self.intern(path, pin)?;
        attributes(ino, &meta, access, self.identity)
    }

    fn attr(&self, ino: u64, handle: Option<u64>) -> io::Result<FileAttr> {
        let (node, meta) = if let Some(handle) = handle {
            let handle = self.handle(ino, handle)?;
            let meta = match &handle.kind {
                HandleKind::File { file, .. } => file.metadata()?,
                HandleKind::Directory(_) => handle.node.pin.metadata()?,
            };
            (Arc::clone(&handle.node), meta)
        } else {
            let node = self.node(ino)?;
            let meta = self.current(&node)?.metadata()?;
            (node, meta)
        };
        attributes(ino, &meta, self.rules.access(&node.path), self.identity)
    }

    fn insert_handle(&mut self, handle: Handle) -> io::Result<u64> {
        if self.handles.len() == MAX_HANDLES {
            return Err(error(libc::EMFILE));
        }
        let id = self.next_handle;
        self.next_handle = id.checked_add(1).ok_or_else(|| error(libc::EOVERFLOW))?;
        self.handles.insert(id, handle);
        Ok(id)
    }

    fn handle(&self, ino: u64, id: u64) -> io::Result<&Handle> {
        self.handles
            .get(&id)
            .filter(|handle| handle.inode == ino)
            .ok_or_else(|| error(libc::EBADF))
    }

    fn authorize_open(&self, path: &Path, flags: i32) -> io::Result<bool> {
        let accepted = libc::O_ACCMODE
            | libc::O_APPEND
            | libc::O_TRUNC
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_LARGEFILE
            | libc::O_SYNC
            | libc::O_DSYNC
            | libc::O_NOCTTY;
        if flags & !accepted != 0 || flags & libc::O_ACCMODE == libc::O_ACCMODE {
            return Err(error(libc::EOPNOTSUPP));
        }
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        match self.rules.access(path) {
            Some(FsAccess::ReadWrite) => Ok(writable),
            Some(FsAccess::Read) if !writable && flags & libc::O_TRUNC == 0 => Ok(false),
            _ => Err(error(libc::EACCES)),
        }
    }

    fn require_write(&self, path: &Path) -> io::Result<()> {
        if self.rules.access(path) == Some(FsAccess::ReadWrite) {
            Ok(())
        } else {
            Err(error(libc::EACCES))
        }
    }

    fn intern_entry(&mut self, path: PathBuf, pin: File) -> io::Result<FileAttr> {
        let meta = pin.metadata()?;
        file_kind(&meta)?;
        let access = self
            .rules
            .access(&path)
            .ok_or_else(|| error(libc::EACCES))?;
        let ino = self.intern(path, pin)?;
        attributes(ino, &meta, Some(access), self.identity)
    }

    fn open(&mut self, ino: u64, flags: i32) -> io::Result<u64> {
        let flags = normalize_open_flags(flags)?;
        let node = self.node(ino)?;
        let writable = self.authorize_open(&node.path, flags)?;
        let authority = self
            .rules
            .access(&node.path)
            .ok_or_else(|| error(libc::EACCES))?;
        if self.handles.len() == MAX_HANDLES {
            return Err(error(libc::EMFILE));
        }
        let pin = self.backing.pin_authorized(&node.path, authority)?;
        if !same_object(&node.pin.metadata()?, &pin.metadata()?) {
            return Err(error(libc::ESTALE));
        }
        // Reopening through the held descriptor requires following our own proc
        // magic link. No caller-controlled symlink is followed on the host.
        // Linux permits O_RDONLY|O_TRUNC under write authority while retaining
        // a read-only descriptor. The verified pin makes this destructive open
        // safe without a later ftruncate that would require a writable fd.
        let file = reopen_regular(&pin, flags & !libc::O_NOFOLLOW)?;
        self.insert_handle(Handle {
            inode: ino,
            node,
            kind: HandleKind::File { file, writable },
            authority,
        })
    }

    fn create(
        &mut self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
    ) -> io::Result<(FileAttr, u64)> {
        if flags & KERNEL_FMODE_EXEC != 0 {
            return Err(error(libc::EOPNOTSUPP));
        }
        let flags = normalize_open_flags(flags)?;
        let path = self.child(parent, name)?;
        let writable = self.authorize_open(&path, flags & !(libc::O_CREAT | libc::O_EXCL))?;
        if self.rules.access(&path) != Some(FsAccess::ReadWrite) {
            return Err(error(libc::EACCES));
        }
        if self.nodes.len() == MAX_NODES || self.handles.len() == MAX_HANDLES {
            return Err(error(libc::EMFILE));
        }
        // Always exclusive: a racing existing file must go through fresh lookup
        // and open rather than bypass the type/identity checks or get truncated.
        let parent_node = self.node(parent)?;
        let parent = self.current(&parent_node)?;
        let file = self
            .backing
            .create_at(&parent, name, flags, mode & !umask & 0o777)?;
        let pin = file.try_clone()?;
        let ino = self.intern(path, pin)?;
        let node = self.node(ino)?;
        let handle = self.insert_handle(Handle {
            inode: ino,
            node,
            kind: HandleKind::File { file, writable },
            authority: FsAccess::ReadWrite,
        })?;
        Ok((self.attr(ino, Some(handle))?, handle))
    }

    fn mkdir(&mut self, parent: u64, name: &OsStr, mode: u32, umask: u32) -> io::Result<FileAttr> {
        if self.nodes.len() == MAX_NODES {
            return Err(error(libc::EMFILE));
        }
        let (path, parent) = self.child_parent(parent, name)?;
        self.require_write(&path)?;
        self.backing
            .mkdir_at(&parent, name, mode & !umask & 0o777)?;
        let pin = self.backing.pin_child(&parent, name)?;
        self.intern_entry(path, pin)
    }

    fn unlink(&self, parent: u64, name: &OsStr, directory: bool) -> io::Result<()> {
        let (path, parent) = self.child_parent(parent, name)?;
        self.require_write(&path)?;
        self.backing.unlink_at(&parent, name, directory)
    }

    fn symlink(&mut self, parent: u64, name: &OsStr, target: &Path) -> io::Result<FileAttr> {
        if self.nodes.len() == MAX_NODES {
            return Err(error(libc::EMFILE));
        }
        let (path, parent) = self.child_parent(parent, name)?;
        self.require_write(&path)?;
        self.backing.symlink_at(target, &parent, name)?;
        let pin = self.backing.pin_child(&parent, name)?;
        self.intern_entry(path, pin)
    }

    fn rename(
        &mut self,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
    ) -> io::Result<()> {
        if !accepted_rename_flags(flags) {
            return Err(error(libc::EINVAL));
        }
        let (old_path, old_parent) = self.child_parent(parent, name)?;
        let (new_path, new_parent) = self.child_parent(newparent, newname)?;
        self.require_write(&old_path)?;
        self.require_write(&new_path)?;
        let replacements = if self.same_child_object(&old_parent, name, &new_parent, newname) {
            Vec::new()
        } else {
            self.renamed_node_replacements(&old_path, &new_path, flags == libc::RENAME_EXCHANGE)?
        };
        self.backing
            .rename_at(&old_parent, name, &new_parent, newname, flags)?;
        self.install_rebased_nodes(replacements);
        Ok(())
    }

    /// `renameat` is a no-op when two ordinary hardlink names already denote
    /// the same object. Keep their separate path identities in that case: each
    /// spelling has its own policy ceiling and cached FUSE inode.
    fn same_child_object(
        &self,
        old_parent: &File,
        old_name: &OsStr,
        new_parent: &File,
        new_name: &OsStr,
    ) -> bool {
        let Ok(old) = self.backing.pin_child(old_parent, old_name) else {
            return false;
        };
        let Ok(new) = self.backing.pin_child(new_parent, new_name) else {
            return false;
        };
        let Ok(old_meta) = old.metadata() else {
            return false;
        };
        let Ok(new_meta) = new.metadata() else {
            return false;
        };
        same_object(&old_meta, &new_meta)
    }

    /// Rebind cached FUSE inodes to the names produced by a namespace rename.
    ///
    /// The replacement `Arc<Node>` deliberately leaves an already-open handle
    /// on its original node: descriptor operations retain the authority and
    /// object acquired at open time. Fresh operations instead resolve through
    /// the renamed spelling and recheck its policy and backing identity.
    fn renamed_node_replacements(
        &self,
        old_path: &Path,
        new_path: &Path,
        exchange: bool,
    ) -> io::Result<Vec<(u64, Arc<Node>)>> {
        let mut replacements = Vec::new();
        for (&ino, entry) in &self.nodes {
            // Nodes displaced by an earlier replacement have no active path
            // entry. Do not revive them by moving a stale cached inode again.
            if self.paths.get(&entry.node.path) != Some(&ino) {
                continue;
            }
            let path = rebase_path(&entry.node.path, old_path, new_path)
                .or_else(|| exchange.then(|| rebase_path(&entry.node.path, new_path, old_path))?);
            let Some(path) = path.filter(|path| path != &entry.node.path) else {
                continue;
            };
            replacements.push((
                ino,
                Arc::new(Node {
                    path,
                    pin: entry.node.pin.try_clone()?,
                }),
            ));
        }

        Ok(replacements)
    }

    fn install_rebased_nodes(&mut self, replacements: Vec<(u64, Arc<Node>)>) {
        let moved: HashSet<_> = replacements.iter().map(|(ino, _)| *ino).collect();
        self.paths.retain(|_, ino| !moved.contains(ino));
        for (ino, node) in replacements {
            // The replacement was prepared from `self.nodes` while this state
            // lock was held, so no entry can disappear before installation.
            if let Some(entry) = self.nodes.get_mut(&ino) {
                entry.node = Arc::clone(&node);
            }
            self.paths.insert(node.path.clone(), ino);
        }
    }

    fn link(&mut self, ino: u64, newparent: u64, newname: &OsStr) -> io::Result<FileAttr> {
        if self.nodes.len() == MAX_NODES {
            return Err(error(libc::EMFILE));
        }
        let node = self.node(ino)?;
        if node.pin.metadata()?.is_dir() {
            return Err(error(libc::EPERM));
        }
        self.require_write(&node.path)?;
        // A source FUSE inode is not authority after its name was replaced. The
        // normal link syscall must name the current source spelling because an
        // unprivileged process cannot link an arbitrary retained O_PATH fd.
        self.current(&node)?;
        let source_parent_path = node.path.parent().ok_or_else(|| error(libc::EPERM))?;
        let source_name = node.path.file_name().ok_or_else(|| error(libc::EPERM))?;
        let source_parent = self.backing.pin(source_parent_path)?;
        let source = self.backing.pin_child(&source_parent, source_name)?;
        if !same_object(&node.pin.metadata()?, &source.metadata()?) {
            return Err(error(libc::ESTALE));
        }
        let (new_path, new_parent) = self.child_parent(newparent, newname)?;
        self.require_write(&new_path)?;
        self.backing
            .link_at(&source_parent, source_name, &new_parent, newname)?;
        let pin = self.backing.pin_child(&new_parent, newname)?;
        self.intern_entry(new_path, pin)
    }

    fn read(&self, ino: u64, handle: u64, offset: u64, size: usize) -> io::Result<Vec<u8>> {
        if size > MAX_IO || offset > i64::MAX as u64 - size as u64 {
            return Err(error(libc::EINVAL));
        }
        let HandleKind::File { file, .. } = &self.handle(ino, handle)?.kind else {
            return Err(error(libc::EISDIR));
        };
        let mut data = vec![0; size];
        let mut count = 0;
        while count < size {
            match file.read_at(&mut data[count..], offset + count as u64) {
                Ok(0) => break,
                Ok(n) => count += n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            }
        }
        data.truncate(count);
        Ok(data)
    }

    fn write(&self, ino: u64, handle: u64, offset: u64, data: &[u8]) -> io::Result<u32> {
        if data.len() > MAX_IO || offset > i64::MAX as u64 - data.len() as u64 {
            return Err(error(libc::EINVAL));
        }
        let HandleKind::File {
            file,
            writable: true,
        } = &self.handle(ino, handle)?.kind
        else {
            return Err(error(libc::EBADF));
        };
        // Linux pwrite on an O_APPEND fd preserves append atomicity. Otherwise
        // offsets come from the kernel with writeback caching disabled.
        file.write_all_at(data, offset)?;
        Ok(data.len() as u32)
    }

    fn truncate(&mut self, ino: u64, handle: Option<u64>, size: u64) -> io::Result<FileAttr> {
        if size > i64::MAX as u64 {
            return Err(error(libc::EFBIG));
        }
        if let Some(handle) = handle {
            let HandleKind::File {
                file,
                writable: true,
            } = &self.handle(ino, handle)?.kind
            else {
                return Err(error(libc::EBADF));
            };
            file.set_len(size)?;
        } else {
            let node = self.node(ino)?;
            self.authorize_open(&node.path, libc::O_WRONLY)?;
            let file = reopen_regular(&self.current(&node)?, libc::O_WRONLY)?;
            file.set_len(size)?;
        }
        self.attr(ino, handle)
    }

    fn metadata_target(&self, ino: u64, handle: Option<u64>) -> io::Result<File> {
        if let Some(handle) = handle {
            let handle = self.handle(ino, handle)?;
            if handle.authority != FsAccess::ReadWrite {
                return Err(error(libc::EACCES));
            }
            return match &handle.kind {
                // Keep existing-handle operations bound to the object the
                // handle already names, including after a rename or unlink.
                HandleKind::File { file, .. } => file.try_clone(),
                HandleKind::Directory(_) => handle.node.pin.try_clone(),
            };
        }

        let node = self.node(ino)?;
        self.require_write(&node.path)?;
        // This re-resolves the policy spelling and compares it with the inode
        // pin before every fresh metadata operation.
        self.current(&node)
    }

    fn xattr_target(&self, ino: u64, write: bool) -> io::Result<XattrTarget> {
        let node = self.node(ino)?;
        let access = self
            .rules
            .access(&node.path)
            .ok_or_else(|| error(libc::EACCES))?;
        if write && access != FsAccess::ReadWrite {
            return Err(error(libc::EACCES));
        }

        // A FUSE xattr callback has no file handle. Hold the fresh,
        // identity-checked pin through the host operation rather than
        // re-resolving its final component after validation.
        let pin = self.current(&node)?;
        let proc_path = format!("/proc/self/fd/{}", pin.as_raw_fd()).into_bytes();
        Ok(XattrTarget {
            _pin: pin,
            path: CString::new(proc_path).map_err(|_| error(libc::EINVAL))?,
        })
    }

    fn xattr_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| error(libc::EINVAL))
    }

    fn getxattr(&self, ino: u64, name: &OsStr, size: u32) -> io::Result<XattrReply> {
        let target = self.xattr_target(ino, false)?;
        let name = Self::xattr_name(name)?;
        if size == 0 {
            // SAFETY: target and name are owned NUL-terminated byte strings;
            // null with a zero length is the host's size-query form.
            let result = unsafe {
                libc::getxattr(target.path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0)
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            return u32::try_from(result)
                .map(XattrReply::Size)
                .map_err(|_| error(libc::EOVERFLOW));
        }
        let mut value = Vec::new();
        value
            .try_reserve_exact(size as usize)
            .map_err(|_| error(libc::ENOMEM))?;
        value.resize(size as usize, 0);
        // SAFETY: value owns exactly `size` writable bytes, and target/name
        // remain live for the syscall.
        let result = unsafe {
            libc::getxattr(
                target.path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        value.truncate(usize::try_from(result).map_err(|_| error(libc::EOVERFLOW))?);
        Ok(XattrReply::Data(value))
    }

    fn listxattr(&self, ino: u64, size: u32) -> io::Result<XattrReply> {
        let target = self.xattr_target(ino, false)?;
        if size == 0 {
            // SAFETY: target is an owned NUL-terminated path; null with a
            // zero length is the host's size-query form.
            let result = unsafe { libc::listxattr(target.path.as_ptr(), std::ptr::null_mut(), 0) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            return u32::try_from(result)
                .map(XattrReply::Size)
                .map_err(|_| error(libc::EOVERFLOW));
        }
        let mut names = Vec::new();
        names
            .try_reserve_exact(size as usize)
            .map_err(|_| error(libc::ENOMEM))?;
        names.resize(size as usize, 0);
        // SAFETY: names owns exactly `size` writable bytes and target remains
        // live for the syscall.
        let result = unsafe {
            libc::listxattr(target.path.as_ptr(), names.as_mut_ptr().cast(), names.len())
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        names.truncate(usize::try_from(result).map_err(|_| error(libc::EOVERFLOW))?);
        Ok(XattrReply::Data(names))
    }

    fn setxattr(
        &self,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
    ) -> io::Result<()> {
        if position != 0 {
            return Err(error(libc::EOPNOTSUPP));
        }
        if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
            return Err(error(libc::EINVAL));
        }
        let target = self.xattr_target(ino, true)?;
        let name = Self::xattr_name(name)?;
        // SAFETY: target/name are owned NUL-terminated strings and value is a
        // live byte slice for the duration of the syscall.
        if unsafe {
            libc::setxattr(
                target.path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                flags,
            )
        } < 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn removexattr(&self, ino: u64, name: &OsStr) -> io::Result<()> {
        let target = self.xattr_target(ino, true)?;
        let name = Self::xattr_name(name)?;
        // SAFETY: target and name are owned NUL-terminated byte strings.
        if unsafe { libc::removexattr(target.path.as_ptr(), name.as_ptr()) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn setattr(
        &mut self,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        handle: Option<u64>,
    ) -> io::Result<FileAttr> {
        let (uid, gid) = self.identity.incoming_owner(uid, gid)?;
        if mode.is_some() || uid.is_some() || gid.is_some() || atime.is_some() || mtime.is_some() {
            let target = self.metadata_target(ino, handle)?;
            // chown can clear set-id bits, so apply the requested mode after it.
            if uid.is_some() || gid.is_some() {
                set_owner(&target, uid.unwrap_or(u32::MAX), gid.unwrap_or(u32::MAX))?;
            }
            if let Some(mode) = mode {
                set_mode(&target, mode & 0o7777)?;
            }
            if atime.is_some() || mtime.is_some() {
                set_times(&target, &requested_times(atime, mtime)?)?;
            }
        }
        if let Some(size) = size {
            self.truncate(ino, handle, size)?;
        }
        self.attr(ino, handle)
    }

    fn opendir(&mut self, ino: u64) -> io::Result<u64> {
        let node = self.node(ino)?;
        self.current(&node)?;
        let authority = self
            .rules
            .access(&node.path)
            .ok_or_else(|| error(libc::EACCES))?;
        let dir = self
            .backing
            .open(&node.path, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        if !same_object(&dir.metadata()?, &node.pin.metadata()?) {
            return Err(error(libc::ESTALE));
        }
        let mut entries = vec![
            (OsString::from("."), FileType::Directory),
            (OsString::from(".."), FileType::Directory),
        ];
        for name in directory_names(dir, MAX_DIRECTORY_ENTRIES)? {
            let path = child_path(&node.path, &name)?;
            if !self.rules.traversable(&path) {
                continue;
            }
            let Ok(pin) = self.backing.pin(&path) else {
                continue;
            };
            let meta = pin.metadata()?;
            if self.visible(&path, &meta)
                && let Ok(kind) = file_kind(&meta)
            {
                entries.push((name, kind));
            }
        }
        self.insert_handle(Handle {
            inode: ino,
            node,
            kind: HandleKind::Directory(entries),
            authority,
        })
    }

    fn export_handle(&mut self, tid: u32, ino: u64, fh: u64) -> io::Result<()> {
        let slot = self.export.as_ref().ok_or_else(|| error(libc::EACCES))?;
        if slot.tid != tid || !slot.armed || slot.file.is_some() {
            return Err(error(libc::EACCES));
        }
        let handle = self.handle(ino, fh)?;
        let HandleKind::File { file, .. } = &handle.kind else {
            return Err(error(libc::EACCES));
        };
        if handle.authority == FsAccess::Read {
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            use std::os::fd::AsRawFd;
            if unsafe { libc::fstatvfs(file.as_raw_fd(), &mut stat) } < 0 {
                return Err(io::Error::last_os_error());
            }
            if stat.f_flag & libc::ST_RDONLY == 0 {
                return Err(error(libc::EROFS));
            }
        }
        let copy = file.try_clone()?;
        // The exact OPEN handle owns this object even after its name is moved.
        // No current(path) lookup belongs on the retained-handle export path.
        self.export
            .as_mut()
            .ok_or_else(|| error(libc::EACCES))?
            .file = Some(copy);
        Ok(())
    }
}

fn file_kind(meta: &Metadata) -> io::Result<FileType> {
    if meta.is_file() {
        Ok(FileType::RegularFile)
    } else if meta.is_dir() {
        Ok(FileType::Directory)
    } else if meta.file_type().is_symlink() {
        Ok(FileType::Symlink)
    } else {
        Err(error(libc::EACCES))
    }
}

fn timestamp(seconds: i64, nanos: i64) -> SystemTime {
    let value = i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    let delta = Duration::new(
        (value.unsigned_abs() / 1_000_000_000) as u64,
        (value.unsigned_abs() % 1_000_000_000) as u32,
    );
    if value >= 0 {
        UNIX_EPOCH.checked_add(delta)
    } else {
        UNIX_EPOCH.checked_sub(delta)
    }
    .unwrap_or(UNIX_EPOCH)
}

fn requested_time(time: TimeOrNow) -> io::Result<libc::timespec> {
    match time {
        TimeOrNow::Now => Ok(libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_NOW,
        }),
        TimeOrNow::SpecificTime(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(duration) => Ok(libc::timespec {
                tv_sec: i64::try_from(duration.as_secs()).map_err(|_| error(libc::EOVERFLOW))?,
                tv_nsec: i64::from(duration.subsec_nanos()),
            }),
            Err(error_before_epoch) => {
                let duration = error_before_epoch.duration();
                let seconds =
                    i64::try_from(duration.as_secs()).map_err(|_| error(libc::EOVERFLOW))?;
                let nanos = i64::from(duration.subsec_nanos());
                if nanos == 0 {
                    Ok(libc::timespec {
                        tv_sec: -seconds,
                        tv_nsec: 0,
                    })
                } else {
                    Ok(libc::timespec {
                        tv_sec: seconds
                            .checked_neg()
                            .and_then(|n| n.checked_sub(1))
                            .ok_or_else(|| error(libc::EOVERFLOW))?,
                        tv_nsec: 1_000_000_000 - nanos,
                    })
                }
            }
        },
    }
}

fn requested_times(
    atime: Option<TimeOrNow>,
    mtime: Option<TimeOrNow>,
) -> io::Result<[libc::timespec; 2]> {
    let omitted = libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_OMIT,
    };
    Ok([
        atime.map(requested_time).transpose()?.unwrap_or(omitted),
        mtime.map(requested_time).transpose()?.unwrap_or(omitted),
    ])
}

fn attributes(
    ino: u64,
    meta: &Metadata,
    access: Option<FsAccess>,
    identity: ProjectionIdentity,
) -> io::Result<FileAttr> {
    let kind = file_kind(meta)?;
    let mask = match access {
        Some(FsAccess::ReadWrite) => 0o777,
        Some(FsAccess::Read) => 0o555,
        None => 0o111,
    };
    Ok(FileAttr {
        ino: INodeNo(ino),
        size: meta.len(),
        blocks: meta.blocks(),
        atime: timestamp(meta.atime(), meta.atime_nsec()),
        mtime: timestamp(meta.mtime(), meta.mtime_nsec()),
        ctime: timestamp(meta.ctime(), meta.ctime_nsec()),
        crtime: UNIX_EPOCH,
        kind,
        perm: (meta.mode() & mask) as u16,
        nlink: meta.nlink().min(u64::from(u32::MAX)) as u32,
        uid: identity.outgoing_uid(meta.uid()),
        gid: identity.outgoing_gid(meta.gid()),
        rdev: 0,
        blksize: meta.blksize().min(u64::from(u32::MAX)) as u32,
        flags: 0,
    })
}

impl Filesystem for Projection {
    fn ioctl(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: IoctlFlags,
        cmd: u32,
        in_data: &[u8],
        out_size: u32,
        reply: ReplyIoctl,
    ) {
        let result = if u64::from(cmd) != EXPORT_IOCTL
            || !flags.is_empty()
            || !in_data.is_empty()
            || out_size != 0
        {
            Err(error(libc::ENOTTY))
        } else {
            self.state()
                .and_then(|mut state| state.export_handle(req.pid(), ino.0, fh.0))
        };
        match result {
            Ok(()) => reply.ioctl(0, &[]),
            Err(err) => reply.error(err.into()),
        }
    }

    fn init(&mut self, _: &Request, config: &mut KernelConfig) -> io::Result<()> {
        if self.state()?.backing.supports_export() {
            // Server workers inherit this thread's credentials. Namespace-local
            // DAC override must not make backing opens stronger than the caller.
            unsafe { super::super::linux_landlock::drop_all_capabilities() }?;
        }
        config
            .add_capabilities(InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "filesystem projection requires shared mappings with direct I/O",
                )
            })
    }

    fn lookup(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self
            .state()
            .and_then(|mut state| state.lookup(parent.0, name))
        {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err.into()),
        }
    }

    fn forget(&self, _: &Request, ino: INodeNo, nlookup: u64) {
        if let Ok(mut state) = self.state() {
            state.forget(ino.0, nlookup);
        }
    }

    fn getattr(&self, _: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match self
            .state()
            .and_then(|state| state.attr(ino.0, fh.map(|h| h.0)))
        {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err.into()),
        }
    }

    fn readlink(&self, _: &Request, ino: INodeNo, reply: ReplyData) {
        let result = self.state().and_then(|state| {
            let node = state.node(ino.0)?;
            if state.rules.access(&node.path).is_none() {
                return Err(error(libc::EACCES));
            }
            read_link(&state.current(&node)?)
        });
        match result {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err.into()),
        }
    }

    fn open(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self
            .state()
            .and_then(|mut state| state.open(ino.0, flags.0))
        {
            // Path-specific inodes must not cache ordinary reads independently:
            // a write through another hardlink must be visible to this handle.
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::FOPEN_DIRECT_IO),
            Err(err) => reply.error(err.into()),
        }
    }

    fn create(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match self
            .state()
            .and_then(|mut state| state.create(parent.0, name, mode, umask, flags))
        {
            Ok((attr, handle)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(handle),
                FopenFlags::FOPEN_DIRECT_IO,
            ),
            Err(err) => reply.error(err.into()),
        }
    }

    fn mkdir(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        match self
            .state()
            .and_then(|mut state| state.mkdir(parent.0, name, mode, umask))
        {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err.into()),
        }
    }

    fn unlink(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self
            .state()
            .and_then(|state| state.unlink(parent.0, name, false))
        {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn rmdir(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self
            .state()
            .and_then(|state| state.unlink(parent.0, name, true))
        {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn symlink(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        match self
            .state()
            .and_then(|mut state| state.symlink(parent.0, name, target))
        {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err.into()),
        }
    }

    fn rename(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let flags = flags.bits();
        if !accepted_rename_flags(flags) {
            reply.error(fuser::Errno::EINVAL);
            return;
        }
        match self
            .state()
            .and_then(|mut state| state.rename(parent.0, name, newparent.0, newname, flags))
        {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn link(
        &self,
        _: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        match self
            .state()
            .and_then(|mut state| state.link(ino.0, newparent.0, newname))
        {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err.into()),
        }
    }

    fn read(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .state()
            .and_then(|state| state.read(ino.0, fh.0, offset, size as usize))
        {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err.into()),
        }
    }

    fn write(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self
            .state()
            .and_then(|state| state.write(ino.0, fh.0, offset, data))
        {
            Ok(size) => reply.written(size),
            Err(err) => reply.error(err.into()),
        }
    }

    fn setattr(
        &self,
        _: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if ctime.is_some()
            || crtime.is_some()
            || chgtime.is_some()
            || bkuptime.is_some()
            || flags.is_some()
        {
            reply.error(fuser::Errno::EOPNOTSUPP);
            return;
        }
        let result = self.state().and_then(|mut state| {
            state.setattr(ino.0, mode, uid, gid, size, atime, mtime, fh.map(|h| h.0))
        });
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err.into()),
        }
    }

    fn setxattr(
        &self,
        _: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        match self
            .state()
            .and_then(|state| state.setxattr(ino.0, name, value, flags, position))
        {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn getxattr(&self, _: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let result = self
            .state()
            .and_then(|state| state.getxattr(ino.0, name, size));
        match result {
            Ok(XattrReply::Size(size)) => reply.size(size),
            Ok(XattrReply::Data(value)) => reply.data(&value),
            Err(err) => reply.error(err.into()),
        }
    }

    fn listxattr(&self, _: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let result = self.state().and_then(|state| state.listxattr(ino.0, size));
        match result {
            Ok(XattrReply::Size(size)) => reply.size(size),
            Ok(XattrReply::Data(names)) => reply.data(&names),
            Err(err) => reply.error(err.into()),
        }
    }

    fn removexattr(&self, _: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self
            .state()
            .and_then(|state| state.removexattr(ino.0, name))
        {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn flush(&self, _: &Request, ino: INodeNo, fh: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        match self
            .state()
            .and_then(|state| state.handle(ino.0, fh.0).map(|_| ()))
        {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn fsync(&self, _: &Request, ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        let result = self.state().and_then(|state| {
            let HandleKind::File { file, .. } = &state.handle(ino.0, fh.0)?.kind else {
                return Err(error(libc::EBADF));
            };
            if datasync {
                file.sync_data()
            } else {
                file.sync_all()
            }
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn release(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.state().and_then(|mut state| {
            state.handle(ino.0, fh.0)?;
            state.handles.remove(&fh.0);
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn opendir(&self, _: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(fuser::Errno::EACCES);
            return;
        }
        match self.state().and_then(|mut state| state.opendir(ino.0)) {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(err) => reply.error(err.into()),
        }
    }

    fn readdir(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = self.state().and_then(|state| {
            let HandleKind::Directory(entries) = &state.handle(ino.0, fh.0)?.kind else {
                return Err(error(libc::ENOTDIR));
            };
            for (index, (name, kind)) in entries
                .iter()
                .enumerate()
                .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            {
                // Zero requests a subsequent lookup rather than publishing an
                // unreferenced path identity from this directory snapshot.
                if reply.add(INodeNo(0), index as u64 + 1, *kind, name) {
                    break;
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }

    fn releasedir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.release(req, ino, fh, flags, None, false, reply);
    }

    fn access(&self, _: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let result = self.state().and_then(|state| {
            let node = state.node(ino.0)?;
            let meta = state.current(&node)?.metadata()?;
            let access = state.rules.access(&node.path);
            if !state.visible(&node.path, &meta)
                || (mask.bits() & libc::W_OK != 0 && access != Some(FsAccess::ReadWrite))
                || (mask.bits() & libc::R_OK != 0 && access.is_none())
                || (mask.bits() & libc::X_OK != 0 && meta.mode() & 0o111 == 0)
            {
                return Err(error(libc::EACCES));
            }
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err.into()),
        }
    }
}

#[cfg(test)]
mod tests;
