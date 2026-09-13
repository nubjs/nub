use super::backing::error;
use super::filesystem::{EXPORT_IOCTL, Projection};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
#[cfg(test)]
use std::time::Duration;

pub(crate) fn mount_id(file: &File) -> io::Result<u64> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(error(libc::EOPNOTSUPP));
    }
    Ok(stat.stx_mnt_id)
}

pub(crate) struct NativeOpenRequest {
    pub path: CString,
    pub directory: Option<File>,
    pub flags: u64,
    pub mode: u64,
    pub resolve: Option<u64>,
    pub umask: u32,
}

#[derive(Default)]
struct Counters {
    tid: AtomicU32,
    opened: AtomicU64,
    exported: AtomicU64,
}

struct Job {
    request: NativeOpenRequest,
    result: mpsc::SyncSender<io::Result<File>>,
    ready: File,
    cancelled: Arc<AtomicBool>,
}

pub(crate) struct NativePending {
    result: mpsc::Receiver<io::Result<File>>,
    ready: File,
    cancelled: Arc<AtomicBool>,
}

impl NativePending {
    pub(crate) fn readiness_fd(&self) -> RawFd {
        self.ready.as_raw_fd()
    }

    pub(crate) fn finish(&self) -> io::Result<File> {
        self.result.try_recv().map_err(|_| error(libc::EIO))?
    }

    #[cfg(test)]
    pub(crate) fn finish_blocking(&self) -> io::Result<File> {
        self.result.recv().map_err(|_| error(libc::EIO))?
    }
}

impl Drop for NativePending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct NativeOpenClient {
    sender: Arc<Mutex<Option<mpsc::SyncSender<Job>>>>,
    mount: u64,
    root_inode: u64,
    counters: Arc<Counters>,
}

impl NativeOpenClient {
    pub(crate) fn accepts_root(&self, file: &File) -> io::Result<()> {
        self.accepts_directory(file)?;
        if file.metadata()?.ino() != self.root_inode {
            return Err(error(libc::EACCES));
        }
        Ok(())
    }

    pub(crate) fn accepts_directory(&self, file: &File) -> io::Result<()> {
        if !file.metadata()?.is_dir() {
            return Err(error(libc::ENOTDIR));
        }
        if mount_id(file)? != self.mount {
            return Err(error(libc::EACCES));
        }
        Ok(())
    }

    pub(crate) fn submit(&self, request: NativeOpenRequest) -> io::Result<NativePending> {
        if let Some(dir) = &request.directory {
            self.accepts_directory(dir)?;
        }
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let ready = unsafe { File::from_raw_fd(fd) };
        let (send, result) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let job = Job {
            request,
            result: send,
            ready: ready.try_clone()?,
            cancelled: Arc::clone(&cancelled),
        };
        self.sender
            .lock()
            .map_err(|_| error(libc::EIO))?
            .as_ref()
            .ok_or_else(|| error(libc::ECANCELED))?
            .try_send(job)
            .map_err(|_| error(libc::EAGAIN))?;
        Ok(NativePending {
            result,
            ready,
            cancelled,
        })
    }

    pub(crate) fn stats(&self) -> (u64, u64) {
        (
            self.counters.opened.load(Ordering::Relaxed),
            self.counters.exported.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn resolver_tid(&self) -> u32 {
        self.counters.tid.load(Ordering::Acquire)
    }
}

pub(crate) struct NativeOpenService {
    client: NativeOpenClient,
    worker: Option<JoinHandle<()>>,
    #[cfg(test)]
    gate: Arc<Mutex<Option<OpenGate>>>,
}

#[cfg(test)]
struct OpenGate {
    path: CString,
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct NativeOpenGate {
    entered: mpsc::Receiver<()>,
    release: Option<mpsc::SyncSender<()>>,
}

#[cfg(test)]
impl NativeOpenGate {
    pub(crate) fn wait_until_active(&self) -> io::Result<()> {
        self.entered
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| error(libc::EIO))
    }

    pub(crate) fn release(mut self) -> io::Result<()> {
        self.release
            .take()
            .expect("gate release called once")
            .send(())
            .map_err(|_| error(libc::EIO))
    }
}

#[cfg(test)]
impl Drop for NativeOpenGate {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl NativeOpenService {
    pub(super) fn start(projection: Projection, mount: &Path) -> io::Result<Self> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(mount)?;
        let mount = mount_id(&root)?;
        let root_inode = root.metadata()?.ino();
        let (send, jobs) = mpsc::sync_channel::<Job>(1);
        let counters = Arc::new(Counters::default());
        let worker_counters = Arc::clone(&counters);
        #[cfg(test)]
        let gate = Arc::new(Mutex::new(None));
        #[cfg(test)]
        let worker_gate = Arc::clone(&gate);
        let (ready, startup) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("projection-open".into())
            .spawn(move || {
                let setup = (|| {
                    // Only this trusted thread changes root/cwd/umask. The FUSE
                    // server must retain its own host root and trusted procfs.
                    if unsafe { libc::unshare(libc::CLONE_FS) } < 0
                        || unsafe { libc::fchdir(root.as_raw_fd()) } < 0
                        || unsafe { libc::chroot(c".".as_ptr()) } < 0
                        || unsafe { libc::chdir(c"/".as_ptr()) } < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    unsafe { super::super::linux_landlock::drop_all_capabilities() }?;
                    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
                    projection.register_export(tid)?;
                    worker_counters.tid.store(tid, Ordering::Release);
                    Ok(())
                })();
                if setup.is_err() {
                    let _ = ready.send(setup);
                    return;
                }
                struct Registration(Projection);
                impl Drop for Registration {
                    fn drop(&mut self) {
                        self.0.unregister_export();
                    }
                }
                let _registration = Registration(projection.clone());
                if ready.send(Ok(())).is_err() {
                    return;
                }
                for job in jobs {
                    #[cfg(test)]
                    wait_for_gate(&worker_gate, job.request.path.as_c_str());
                    let result = if job.cancelled.load(Ordering::Acquire) {
                        Err(error(libc::ECANCELED))
                    } else {
                        open_projected(&projection, &job.request, &worker_counters)
                    };
                    if !job.cancelled.load(Ordering::Acquire) {
                        let _ = job.result.send(result);
                        let one = 1u64;
                        unsafe {
                            libc::write(job.ready.as_raw_fd(), (&one as *const u64).cast(), 8)
                        };
                    }
                }
            })?;
        match startup.recv() {
            Ok(Ok(())) => Ok(Self {
                client: NativeOpenClient {
                    sender: Arc::new(Mutex::new(Some(send))),
                    mount,
                    root_inode,
                    counters,
                },
                worker: Some(worker),
                #[cfg(test)]
                gate,
            }),
            Ok(Err(error)) => {
                drop(send);
                let _ = worker.join();
                Err(error)
            }
            Err(error) => {
                drop(send);
                let _ = worker.join();
                Err(io::Error::other(error))
            }
        }
    }

    pub(crate) fn client(&self) -> NativeOpenClient {
        self.client.clone()
    }

    #[cfg(test)]
    pub(crate) fn block_next_path(&self, path: &std::ffi::CStr) -> io::Result<NativeOpenGate> {
        let (entered, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let mut gate = self.gate.lock().map_err(|_| error(libc::EIO))?;
        assert!(
            gate.is_none(),
            "only one native open test gate may be armed"
        );
        *gate = Some(OpenGate {
            path: path.to_owned(),
            entered,
            release: release_rx,
        });
        Ok(NativeOpenGate {
            entered: entered_rx,
            release: Some(release),
        })
    }

    pub(crate) fn shutdown(mut self) -> io::Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> io::Result<()> {
        self.client
            .sender
            .lock()
            .map_err(|_| error(libc::EIO))?
            .take();
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| error(libc::EIO))?;
        }
        Ok(())
    }
}

#[cfg(test)]
fn wait_for_gate(gate: &Mutex<Option<OpenGate>>, path: &std::ffi::CStr) {
    let gate = gate.lock().ok().and_then(|mut gate| match gate.as_ref() {
        Some(armed) if armed.path.as_c_str() == path => gate.take(),
        _ => None,
    });
    if let Some(gate) = gate {
        let _ = gate.entered.send(());
        let _ = gate.release.recv();
    }
}

impl Drop for NativeOpenService {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn open_projected(
    projection: &Projection,
    request: &NativeOpenRequest,
    counters: &Counters,
) -> io::Result<File> {
    let dirfd = request
        .directory
        .as_ref()
        .map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
    unsafe { libc::umask(request.umask) };
    let flags = request.flags | libc::O_CLOEXEC as u64;
    let fd = if let Some(resolve) = request.resolve {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let how = OpenHow {
            flags,
            mode: request.mode,
            resolve,
        };
        unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dirfd,
                request.path.as_ptr(),
                &how,
                size_of::<OpenHow>(),
            ) as i32
        }
    } else {
        unsafe {
            libc::openat(
                dirfd,
                request.path.as_ptr(),
                flags as i32,
                request.mode as libc::mode_t,
            )
        }
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    counters.opened.fetch_add(1, Ordering::Relaxed);
    if flags & libc::O_PATH as u64 != 0 || file.metadata()?.is_dir() {
        return Ok(file);
    }
    if !file.metadata()?.is_file() {
        return Err(error(libc::EACCES));
    }
    projection.arm_export()?;
    // No provider lock is held during this synchronous FUSE callback. Only
    // the kernel-supplied resolver TID and exact open fh can fill the slot.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), EXPORT_IOCTL, 0) };
    let error = io::Error::last_os_error();
    let exported = projection.take_export();
    if rc < 0 {
        return Err(error);
    }
    let exported = exported?;
    counters.exported.fetch_add(1, Ordering::Relaxed);
    Ok(exported)
}
