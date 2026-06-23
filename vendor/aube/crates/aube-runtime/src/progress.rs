//! Progress reporting hook. This crate is a library — it never prints.
//! The CLI wires an implementation backed by `clx::progress`; tests
//! and non-interactive callers use [`NoopProgress`].

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Resolving,
    Downloading,
    Verifying,
    Extracting,
}

pub trait DownloadProgress: Send + Sync {
    /// `version` is `None` during [`InstallPhase::Resolving`] — the
    /// exact version isn't known until resolution finishes.
    fn on_phase(&self, _version: Option<&node_semver::Version>, _phase: InstallPhase) {}
    fn on_download_start(&self, _total_bytes: Option<u64>) {}
    fn on_download_chunk(&self, _bytes: u64) {}
    fn on_done(&self) {}
    /// An external tool (mise) is about to inherit the terminal for
    /// its own progress output — the CLI pauses any live progress
    /// renderer so the two don't interleave.
    fn on_external_tool_start(&self) {}
    fn on_external_tool_end(&self) {}
}

pub struct NoopProgress;

impl DownloadProgress for NoopProgress {}
