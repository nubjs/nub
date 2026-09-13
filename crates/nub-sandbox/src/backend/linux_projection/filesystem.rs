use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, IoctlFlags, KernelConfig, LockOwner, OpenFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyIoctl, ReplyOpen, ReplyWrite, Request,
    TimeOrNow, WriteFlags,
};

use super::backing::{
    Backing, child_path, directory_names, error, read_link, reopen_regular, same_object,
};
use super::rules::Rules;
use crate::policy::FsAccess;

const ROOT: u64 = 1;
const MAX_NODES: usize = 65_536;
const MAX_HANDLES: usize = 16_384;
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_IO: usize = 1024 * 1024;
const TTL: Duration = Duration::ZERO;

// FUSE forwards kernel flags, not libc's user API values. On 64-bit glibc,
// O_LARGEFILE is zero even though the kernel sends its architecture-specific bit.
#[cfg(target_arch = "x86_64")]
const KERNEL_O_LARGEFILE: i32 = 1 << 15;
#[cfg(target_arch = "aarch64")]
const KERNEL_O_LARGEFILE: i32 = 1 << 17;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const KERNEL_O_LARGEFILE: i32 = libc::O_LARGEFILE;
const KERNEL_FMODE_EXEC: i32 = 1 << 5;

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

pub(super) const EXPORT_IOCTL: libc::c_ulong = 0x4e80;

#[derive(Clone)]
pub(crate) struct Projection(Arc<Mutex<State>>);

impl Projection {
    pub(super) fn new(rules: Rules, backing: Backing) -> io::Result<Self> {
        let root = Arc::new(Node {
            path: PathBuf::from("/"),
            pin: backing.pin(Path::new("/"))?,
        });
        Ok(Self(Arc::new(Mutex::new(State {
            backing,
            rules,
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
        let node = self.node(parent)?;
        if !node.pin.metadata()?.is_dir() {
            return Err(error(libc::ENOTDIR));
        }
        self.current(&node)?;
        child_path(&node.path, name)
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
        attributes(ino, &meta, access)
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
        attributes(ino, &meta, self.rules.access(&node.path))
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
        if flags & libc::O_TRUNC != 0 && !writable {
            return Err(error(libc::EACCES));
        }
        match self.rules.access(path) {
            Some(FsAccess::ReadWrite) => Ok(writable),
            Some(FsAccess::Read) if !writable => Ok(false),
            _ => Err(error(libc::EACCES)),
        }
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
        let file = reopen_regular(&pin, flags & !(libc::O_NOFOLLOW | libc::O_TRUNC))?;
        if flags & libc::O_TRUNC != 0 {
            file.set_len(0)?;
        }
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
        let parent = self.current(&self.node(parent)?)?;
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

fn attributes(ino: u64, meta: &Metadata, access: Option<FsAccess>) -> io::Result<FileAttr> {
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
        uid: meta.uid(),
        gid: meta.gid(),
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
        if mode.is_some()
            || uid.is_some()
            || gid.is_some()
            || atime.is_some()
            || mtime.is_some()
            || ctime.is_some()
            || crtime.is_some()
            || chgtime.is_some()
            || bkuptime.is_some()
            || flags.is_some()
        {
            reply.error(fuser::Errno::EOPNOTSUPP);
            return;
        }
        let result = self.state().and_then(|mut state| match size {
            Some(size) => state.truncate(ino.0, fh.map(|h| h.0), size),
            None => state.attr(ino.0, fh.map(|h| h.0)),
        });
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
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
