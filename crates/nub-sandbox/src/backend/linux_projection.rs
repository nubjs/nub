//! Path-authorized Linux filesystem projection.
//!
//! This provider is not a launch backend. Its caller must supply a private FUSE
//! mount, keep backing descriptors out of command children, and own command-tree
//! teardown before closing the connection. No mount helper or fallback is used.
#![cfg(target_os = "linux")]

mod backing;
mod filesystem;
mod native_open;
mod rules;

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;

use crate::policy::FsRuleSet;

pub(crate) use filesystem::Projection;
pub(crate) use native_open::{NativeOpenClient, NativeOpenRequest, NativeOpenService};

impl Projection {
    /// Acquire a fixed resolved policy and a backing root owned by the provider.
    /// Names in the policy refer to paths relative to this root, prefixed by `/`.
    /// A real host projection uses an O_PATH descriptor for the host root; tests
    /// can supply an isolated tree without changing the authorization algorithm.
    pub(crate) fn acquire(rules: &FsRuleSet, root: File) -> io::Result<Self> {
        Self::new(rules::Rules::compile(rules)?, backing::Backing::new(root)?)
    }

    pub(crate) fn acquire_native(rules: &FsRuleSet, root: File, read: File) -> io::Result<Self> {
        Self::new(
            rules::Rules::compile(rules)?,
            backing::Backing::with_read_view(root, read)?,
        )
    }

    pub(crate) fn native_opener(&self, mount: &Path) -> io::Result<NativeOpenService> {
        NativeOpenService::start(self.clone(), mount)
    }

    /// Serve an already-mounted connection, blocking until its server exits.
    ///
    /// Consumes the sole userspace `/dev/fuse` descriptor. The namespace worker
    /// must not fork command children while this descriptor is inherited, even
    /// with CLOEXEC: a child blocked before exec would keep the connection alive.
    /// `from_fd` performs the blocking INIT handshake, so the mount must already
    /// exist. Mount and unmount ownership stay with the caller, not fuser. The
    /// dedicated worker must set umask to zero before starting its server threads
    /// so each create uses the requesting command's supplied umask exactly.
    pub(crate) fn serve(self, connection: OwnedFd) -> io::Result<()> {
        fuser::Session::from_fd(
            self,
            connection,
            fuser::SessionACL::Owner,
            fuser::Config::default(),
        )?
        .spawn()?
        .join()
    }
}
