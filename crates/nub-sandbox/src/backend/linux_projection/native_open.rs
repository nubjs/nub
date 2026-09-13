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

/// A one-shot check made by the resolver immediately before it opens a queued request.
pub(crate) type NativeOpenLiveness = Box<dyn FnOnce() -> bool + Send>;

#[derive(Default)]
struct Counters {
    tid: AtomicU32,
    opened: AtomicU64,
    exported: AtomicU64,
}

/// The channel has one slot. Keep an equivalent semaphore token outside the channel so callers
/// that can wait for work may wait alongside their own cancellation descriptor.
struct QueueCapacity {
    available: File,
    #[cfg(test)]
    queued: AtomicU32,
    #[cfg(test)]
    waiting: AtomicU32,
    #[cfg(test)]
    changed: File,
}

impl QueueCapacity {
    fn one() -> io::Result<Self> {
        let fd = unsafe {
            libc::eventfd(
                1,
                libc::EFD_CLOEXEC | libc::EFD_NONBLOCK | libc::EFD_SEMAPHORE,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let available = unsafe { File::from_raw_fd(fd) };
        #[cfg(test)]
        let changed = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        #[cfg(test)]
        if changed < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            available,
            #[cfg(test)]
            queued: AtomicU32::new(0),
            #[cfg(test)]
            waiting: AtomicU32::new(0),
            #[cfg(test)]
            changed: unsafe { File::from_raw_fd(changed) },
        })
    }

    fn try_acquire(self: &Arc<Self>) -> io::Result<Option<QueuePermit>> {
        loop {
            let mut token = 0u64;
            let result = unsafe {
                libc::read(
                    self.available.as_raw_fd(),
                    (&mut token as *mut u64).cast(),
                    std::mem::size_of_val(&token),
                )
            };
            if result == std::mem::size_of_val(&token) as isize {
                debug_assert_eq!(token, 1);
                return Ok(Some(QueuePermit::new(Arc::clone(self))));
            }
            let error = io::Error::last_os_error();
            if result < 0 && error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if result < 0 && error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
    }

    fn readiness_fd(&self) -> RawFd {
        self.available.as_raw_fd()
    }

    fn release(&self) {
        let token = 1u64;
        // A permit is released only after a successful semaphore read, so this cannot fill the
        // one-token counter. The descriptor stays alive through QueuePermit's Arc.
        loop {
            let result = unsafe {
                libc::write(
                    self.available.as_raw_fd(),
                    (&token as *const u64).cast(),
                    std::mem::size_of_val(&token),
                )
            };
            if result == std::mem::size_of_val(&token) as isize {
                return;
            }
            let error = io::Error::last_os_error();
            if result < 0 && error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            unreachable!("native open queue permit could not be returned: {error}");
        }
    }

    #[cfg(test)]
    fn admission_counts(&self) -> (u32, u32) {
        (
            self.queued.load(Ordering::Acquire),
            self.waiting.load(Ordering::Acquire),
        )
    }

    #[cfg(test)]
    fn admission_changed(&self) {
        let one = 1u64;
        let _ = unsafe {
            libc::write(
                self.changed.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of_val(&one),
            )
        };
    }

    #[cfg(test)]
    fn wait_for_admission(&self, queued: u32, waiting: u32) -> io::Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let counts = self.admission_counts();
            if counts.0 >= queued && counts.1 >= waiting {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(error(libc::EIO));
            }
            let mut fd = libc::pollfd {
                fd: self.changed.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if result == 0 {
                return Err(error(libc::EIO));
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut changes = 0u64;
            let _ = unsafe {
                libc::read(
                    self.changed.as_raw_fd(),
                    (&mut changes as *mut u64).cast(),
                    std::mem::size_of_val(&changes),
                )
            };
        }
    }
}

/// Owns a slot from QueueCapacity until the worker receives the matching job.
struct QueuePermit {
    capacity: Arc<QueueCapacity>,
    #[cfg(test)]
    queued: bool,
}

impl QueuePermit {
    fn new(capacity: Arc<QueueCapacity>) -> Self {
        Self {
            capacity,
            #[cfg(test)]
            queued: false,
        }
    }

    fn mark_queued(&mut self) {
        #[cfg(test)]
        {
            self.queued = true;
            self.capacity.queued.fetch_add(1, Ordering::Release);
            self.capacity.admission_changed();
        }
    }
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.queued {
            self.capacity.queued.fetch_sub(1, Ordering::Release);
            self.capacity.admission_changed();
        }
        self.capacity.release();
    }
}

#[cfg(test)]
struct WaitingForPermit<'a>(&'a QueueCapacity);

#[cfg(not(test))]
struct WaitingForPermit;

#[cfg(test)]
impl<'a> WaitingForPermit<'a> {
    fn new(capacity: &'a QueueCapacity) -> Self {
        capacity.waiting.fetch_add(1, Ordering::Release);
        capacity.admission_changed();
        Self(capacity)
    }
}

#[cfg(not(test))]
impl WaitingForPermit {
    fn new(_: &QueueCapacity) -> Self {
        Self
    }
}

#[cfg(test)]
impl Drop for WaitingForPermit<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::Release);
        self.0.admission_changed();
    }
}

struct Job {
    request: NativeOpenRequest,
    result: mpsc::SyncSender<io::Result<File>>,
    ready: File,
    cancelled: Arc<AtomicBool>,
    liveness: NativeOpenLiveness,
    permit: QueuePermit,
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
    capacity: Arc<QueueCapacity>,
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
        self.validate_request(&request)?;
        let permit = self
            .capacity
            .try_acquire()?
            .ok_or_else(|| error(libc::EAGAIN))?;
        self.submit_permitted(request, Box::new(|| true), permit)
    }

    /// Submit after bounded queue admission. `wait` must wait for the returned descriptor and
    /// the caller's cancellation source together, returning false for cancellation.
    pub(crate) fn submit_cancellable<C, W>(
        &self,
        request: NativeOpenRequest,
        liveness: NativeOpenLiveness,
        mut cancelled: C,
        mut wait: W,
    ) -> io::Result<NativePending>
    where
        C: FnMut() -> bool,
        W: FnMut(RawFd) -> io::Result<bool>,
    {
        self.validate_request(&request)?;
        let permit = loop {
            if cancelled() {
                return Err(error(libc::ECANCELED));
            }
            if let Some(permit) = self.capacity.try_acquire()? {
                if cancelled() {
                    return Err(error(libc::ECANCELED));
                }
                break permit;
            }
            let waiting = WaitingForPermit::new(&self.capacity);
            let ready = wait(self.capacity.readiness_fd())?;
            drop(waiting);
            if !ready {
                return Err(error(libc::ECANCELED));
            }
        };
        if cancelled() {
            return Err(error(libc::ECANCELED));
        }
        self.submit_permitted(request, liveness, permit)
    }

    fn validate_request(&self, request: &NativeOpenRequest) -> io::Result<()> {
        if let Some(dir) = &request.directory {
            self.accepts_directory(dir)?;
        }
        Ok(())
    }

    fn submit_permitted(
        &self,
        request: NativeOpenRequest,
        liveness: NativeOpenLiveness,
        permit: QueuePermit,
    ) -> io::Result<NativePending> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let ready = unsafe { File::from_raw_fd(fd) };
        let (send, result) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut permit = permit;
        permit.mark_queued();
        let job = Job {
            request,
            result: send,
            ready: ready.try_clone()?,
            cancelled: Arc::clone(&cancelled),
            liveness,
            permit,
        };
        self.sender
            .lock()
            .map_err(|_| error(libc::EIO))?
            .as_ref()
            .ok_or_else(|| error(libc::ECANCELED))?
            .try_send(job)
            .map_err(|send_error| match send_error {
                mpsc::TrySendError::Full(_) => error(libc::EAGAIN),
                mpsc::TrySendError::Disconnected(_) => error(libc::ECANCELED),
            })?;
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

    #[cfg(test)]
    pub(crate) fn admission_counts(&self) -> (u32, u32) {
        self.capacity.admission_counts()
    }

    #[cfg(test)]
    pub(crate) fn wait_for_admission(&self, queued: u32, waiting: u32) -> io::Result<()> {
        self.capacity.wait_for_admission(queued, waiting)
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
        let capacity = Arc::new(QueueCapacity::one()?);
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
                    let Job {
                        request,
                        result,
                        ready,
                        cancelled,
                        liveness,
                        permit,
                    } = job;
                    // The receiver has removed this job from the one-slot channel. Return its
                    // permit before resolving so another caller may occupy that now-free slot.
                    drop(permit);
                    #[cfg(test)]
                    wait_for_gate(&worker_gate, request.path.as_c_str());
                    // This rejects a notification invalidated while queued; the following
                    // openat cannot be made atomic with the kernel validity query.
                    let opened = if !worker_may_open(&cancelled, liveness) {
                        Err(error(libc::ECANCELED))
                    } else {
                        open_projected(&projection, &request, &worker_counters)
                    };
                    if !cancelled.load(Ordering::Acquire) {
                        let _ = result.send(opened);
                        let one = 1u64;
                        unsafe { libc::write(ready.as_raw_fd(), (&one as *const u64).cast(), 8) };
                    }
                }
            })?;
        match startup.recv() {
            Ok(Ok(())) => Ok(Self {
                client: NativeOpenClient {
                    sender: Arc::new(Mutex::new(Some(send))),
                    capacity,
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

fn worker_may_open(cancelled: &AtomicBool, liveness: NativeOpenLiveness) -> bool {
    !cancelled.load(Ordering::Acquire) && liveness()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> NativeOpenRequest {
        NativeOpenRequest {
            path: CString::new("/app/file").unwrap(),
            directory: None,
            flags: libc::O_RDONLY as u64,
            mode: 0,
            resolve: None,
            umask: 0o022,
        }
    }

    fn stopped_client(capacity: Arc<QueueCapacity>) -> NativeOpenClient {
        NativeOpenClient {
            sender: Arc::new(Mutex::new(None)),
            capacity,
            mount: 0,
            root_inode: 0,
            counters: Arc::new(Counters::default()),
        }
    }

    #[test]
    fn queue_permit_is_singleton_and_raii_returned() {
        let capacity = Arc::new(QueueCapacity::one().unwrap());
        let permit = capacity.try_acquire().unwrap().unwrap();
        assert!(capacity.try_acquire().unwrap().is_none());
        drop(permit);
        assert!(capacity.try_acquire().unwrap().is_some());
    }

    #[test]
    fn cancellation_after_claiming_capacity_returns_the_permit() {
        let capacity = Arc::new(QueueCapacity::one().unwrap());
        let client = stopped_client(Arc::clone(&capacity));
        let mut checks = 0;
        let error = match client.submit_cancellable(
            request(),
            Box::new(|| true),
            || {
                checks += 1;
                checks == 2
            },
            |_| unreachable!("a free permit must not wait"),
        ) {
            Ok(_) => panic!("cancelled admission must not submit a job"),
            Err(error) => error,
        };
        assert_eq!(error.raw_os_error(), Some(libc::ECANCELED));
        assert!(capacity.try_acquire().unwrap().is_some());
    }

    #[test]
    fn stale_queued_job_is_not_eligible_to_open() {
        let cancelled = AtomicBool::new(false);
        assert!(!worker_may_open(&cancelled, Box::new(|| false)));
    }

    #[test]
    fn stopped_sender_returns_the_permit() {
        let capacity = Arc::new(QueueCapacity::one().unwrap());
        let client = stopped_client(Arc::clone(&capacity));
        let error = match client.submit(request()) {
            Ok(_) => panic!("stopped service must not submit a job"),
            Err(error) => error,
        };
        assert_eq!(error.raw_os_error(), Some(libc::ECANCELED));
        assert!(capacity.try_acquire().unwrap().is_some());
    }
}
