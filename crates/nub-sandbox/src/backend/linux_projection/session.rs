//! Private session composition. Policy admission does not select this route.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::backing::Backing;
use super::filesystem::ProjectionIdentity;
use super::namespace::{NamespacePair, ProjectedMount, ProjectionMountPaths};
use super::native_open::{NativeOpenClient, NativeOpenService, mount_id};
use super::{Projection, rules};
use crate::backend::linux_supervisor::ProjectedLaunch;
use crate::backend::unix_tmp::PrivateTemp;
use crate::policy::FsRuleSet;

type CleanupResult = Arc<Mutex<Option<Result<(), String>>>>;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

// Every private acquisition reserves storage before mounting. A failed cleanup
// retains the owner, not merely its path, and closes further admission until
// an explicit retry succeeds. This is not the production persistent registry.
enum Slot {
    Free,
    Reserved,
    Retrying,
    Failed(Box<SessionState>),
}

static OWNERS: Mutex<[Slot; 8]> = Mutex::new([const { Slot::Free }; 8]);

#[derive(Clone, Copy, PartialEq)]
pub(super) enum AcquireFault {
    None,
    AfterMount,
    AfterServerReady,
}

pub(crate) struct ProjectedSession {
    state: Option<Box<SessionState>>,
    slot: Option<usize>,
}

struct SessionState {
    mount: ProjectedMount,
    opener: Option<NativeOpenService>,
    server: Option<JoinHandle<io::Result<()>>>,
    staging: Option<PrivateTemp>,
    cleanup: CleanupResult,
    worker_error: Option<String>,
}

impl ProjectedSession {
    pub(crate) fn acquire(policy: &FsRuleSet, source: &Path) -> io::Result<Self> {
        Self::acquire_inner(
            policy,
            source,
            AcquireFault::None,
            Arc::new(Mutex::new(None)),
        )
    }

    pub(super) fn acquire_with_fault(
        policy: &FsRuleSet,
        source: &Path,
        fault: AcquireFault,
    ) -> (io::Result<Self>, CleanupResult) {
        let cleanup = Arc::new(Mutex::new(None));
        (
            Self::acquire_inner(policy, source, fault, cleanup.clone()),
            cleanup,
        )
    }

    fn acquire_inner(
        policy: &FsRuleSet,
        source: &Path,
        fault: AcquireFault,
        cleanup: CleanupResult,
    ) -> io::Result<Self> {
        let rules = rules::Rules::compile(policy)?;
        let slot = {
            let mut owners = OWNERS.lock().unwrap_or_else(|error| error.into_inner());
            if owners
                .iter()
                .any(|slot| matches!(slot, Slot::Failed(_) | Slot::Retrying))
            {
                return Err(io::Error::other("projected session cleanup requires retry"));
            }
            let slot = owners
                .iter()
                .position(|slot| matches!(slot, Slot::Free))
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EMFILE))?;
            owners[slot] = Slot::Reserved;
            slot
        };
        let mut session = Self {
            state: None,
            slot: Some(slot),
        };
        let host_uid = unsafe { libc::geteuid() };
        let host_gid = unsafe { libc::getegid() };
        let staging = PrivateTemp::new()?;
        let rw = staging.path().join("rw");
        let read = staging.path().join("read");
        let view = staging.path().join("view");
        for path in [&rw, &read, &view] {
            std::fs::create_dir(path)?;
        }
        let paths = ProjectionMountPaths::new(source, &rw, &read, &view)?;
        session.state = Some(Box::new(SessionState {
            mount: NamespacePair::mount_projected(paths)?,
            opener: None,
            server: None,
            staging: Some(staging),
            cleanup,
            worker_error: None,
        }));
        if fault == AcquireFault::AfterMount {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let state = session.state.as_mut().expect("acquired mount owner");
        let (rw, read) = state.mount.take_backing_roots()?;
        let projection = Projection::new_with_identity(
            rules,
            Backing::with_read_view(rw, read)?,
            ProjectionIdentity::Parent { host_uid, host_gid },
        )?;
        let connection = state.mount.take_connection()?;
        let serve = projection.clone();
        let (started, ready) = mpsc::sync_channel(1);
        state.server = Some(
            std::thread::Builder::new()
                .name("projection-fuse".into())
                .spawn(move || {
                    // Only this worker's fs_struct/umask changes; the already
                    // multithreaded embedding process retains its own state.
                    if unsafe { libc::unshare(libc::CLONE_FS) } < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    unsafe {
                        libc::umask(0);
                    }
                    let fuse = fuser::Session::from_fd(
                        serve,
                        connection,
                        // Kernel owner admission was installed at mount. Protocol
                        // UID zero differs from the host server's effective UID.
                        fuser::SessionACL::All,
                        fuser::Config::default(),
                    )?;
                    if started.send(()).is_err() {
                        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                    }
                    fuse.spawn()?.join()
                })?,
        );
        ready
            .recv_timeout(CLEANUP_TIMEOUT)
            .map_err(io::Error::other)?;
        if fault == AcquireFault::AfterServerReady {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let root = state.mount.root()?;
        state.opener = Some(NativeOpenService::start_with_root(
            projection,
            root.try_clone()?,
            mount_id(root)?,
            root.metadata()?.ino(),
            Some(state.mount.namespaces()),
        )?);
        Ok(session)
    }

    fn state(&self) -> io::Result<&SessionState> {
        self.state
            .as_deref()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    pub(crate) fn launch(&self) -> io::Result<ProjectedLaunch> {
        let state = self.state()?;
        Ok(ProjectedLaunch {
            root: state.mount.root()?.try_clone()?,
            namespaces: Some(state.mount.namespaces().clone()),
            opener: self.client()?,
        })
    }

    pub(crate) fn client(&self) -> io::Result<NativeOpenClient> {
        self.state()?
            .opener
            .as_ref()
            .map(NativeOpenService::client)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    pub(crate) fn cleanup_observer(&self) -> CleanupResult {
        self.state.as_ref().expect("live session").cleanup.clone()
    }

    pub(crate) fn staging_path(&self) -> &Path {
        self.state
            .as_ref()
            .expect("live session")
            .staging
            .as_ref()
            .expect("live session staging")
            .path()
    }

    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        if let Some(state) = self.state.as_mut() {
            cleanup_attempt(state)?;
        }
        let worker_error = self.state.take().and_then(|state| state.worker_error);
        if let Some(slot) = self.slot.take() {
            OWNERS.lock().unwrap_or_else(|error| error.into_inner())[slot] = Slot::Free;
        }
        worker_error.map_or(Ok(()), |error| Err(io::Error::other(error)))
    }

    pub(crate) fn retry_failed_cleanup() -> io::Result<()> {
        for index in 0..8 {
            let state = {
                let mut owners = OWNERS.lock().unwrap_or_else(|error| error.into_inner());
                if !matches!(owners[index], Slot::Failed(_)) {
                    continue;
                }
                match std::mem::replace(&mut owners[index], Slot::Retrying) {
                    Slot::Failed(state) => state,
                    _ => unreachable!(),
                }
            };
            let mut owner = Self {
                state: Some(state),
                slot: Some(index),
            };
            if let Err(error) = owner.shutdown() {
                owner.retain_failure();
                return Err(error);
            }
        }
        Ok(())
    }

    fn retain_failure(&mut self) {
        if let (Some(slot), Some(state)) = (self.slot.take(), self.state.take()) {
            OWNERS.lock().unwrap_or_else(|error| error.into_inner())[slot] = Slot::Failed(state);
        }
    }
}

impl Drop for ProjectedSession {
    fn drop(&mut self) {
        let _ = self.shutdown();
        self.retain_failure();
    }
}

fn cleanup_attempt(state: &mut SessionState) -> io::Result<()> {
    // A destructor must retain handles even when a cleanup callback panics.
    // No registry lock is held while joining or entering namespaces.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.cleanup()))
        .unwrap_or_else(|_| Err(io::Error::other("projected cleanup panicked")));
    *state
        .cleanup
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(match &result {
        Err(error) => Err(error.to_string()),
        Ok(()) => state.worker_error.clone().map_or(Ok(()), Err),
    });
    result
}

impl SessionState {
    fn cleanup(&mut self) -> io::Result<()> {
        if let Some(opener) = self.opener.as_mut() {
            let mut result = opener.try_shutdown();
            // Allow the idle worker to consume its closed queue before using
            // the abort fallback needed by an actually stalled open.
            let grace = Instant::now() + Duration::from_millis(100);
            while result.is_none() && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(1));
                result = opener.try_shutdown();
            }
            if result.is_none() {
                // A raw open can be waiting on a FUSE reply. Abort that exact
                // connection before waiting, retaining both owners on failure.
                self.mount.abort_view()?;
                let deadline = Instant::now() + CLEANUP_TIMEOUT;
                while result.is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                    result = opener.try_shutdown();
                }
            }
            match result {
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "native opener teardown",
                    ));
                }
                Some(Err(error)) => self.worker_error = Some(error.to_string()),
                Some(Ok(())) => (),
            }
            self.opener.take();
        }
        if self.mount.unmount_view().is_err() {
            self.mount.abort_view()?;
        }
        if let Some(server) = self.server.as_ref() {
            let deadline = Instant::now() + CLEANUP_TIMEOUT;
            while !server.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !server.is_finished() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "FUSE server teardown",
                ));
            }
            match self.server.take().expect("finished server").join() {
                Ok(Ok(())) => (),
                Ok(Err(error)) if error.raw_os_error() == Some(libc::ECONNABORTED) => (),
                Ok(Err(error)) => self.worker_error = Some(error.to_string()),
                Err(_) => self.worker_error = Some("FUSE server panicked".into()),
            }
        }
        self.mount.unmount_view()?;
        self.mount.release_backing_namespace()?;
        if let Some(staging) = self.staging.as_mut() {
            staging.close()?;
        }
        self.staging.take();
        Ok(())
    }
}
