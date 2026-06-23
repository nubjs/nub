//! Typed errors for runtime resolution and installation. Each fatal
//! variant maps onto a stable `ERR_AUBE_*` code (see [`Error::code`])
//! so the CLI layer can wrap them into miette diagnostics with the
//! right exit codes.

use aube_codes::errors;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "the project requires Node.js {requested} but no satisfying version is available{hint}"
    )]
    VersionUnsatisfied { requested: String, hint: String },

    #[error("no Node.js release satisfies {requested}{platform_note}")]
    NoMatchingVersion {
        requested: String,
        platform_note: String,
    },

    #[error("failed to download {url}: {reason}")]
    DownloadFailed { url: String, reason: String },

    #[error(
        "checksum mismatch for {url}: expected sha256 {expected}, got {actual} — the archive was discarded"
    )]
    ChecksumMismatch {
        url: String,
        expected: String,
        actual: String,
    },

    #[error("failed to extract Node.js archive: {reason}")]
    ExtractFailed { reason: String },

    /// `version` carries the full tool spec (`node@22.1.0`,
    /// `aube@1.18.2`).
    #[error("mise failed to install {version}: {reason}")]
    MiseInstallFailed { version: String, reason: String },

    #[error(
        "no Node.js build is published for {platform}; set nodeDownloadMirrors to a mirror that carries one, or install Node via mise or your system package manager"
    )]
    UnsupportedPlatform { platform: String },

    #[error("offline mode is active and {what} is not cached")]
    Offline { what: String },

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// The stable `ERR_AUBE_*` identifier for this error.
    pub fn code(&self) -> &'static str {
        match self {
            Error::VersionUnsatisfied { .. } => errors::ERR_AUBE_RUNTIME_VERSION_UNSATISFIED,
            Error::NoMatchingVersion { .. } => errors::ERR_AUBE_RUNTIME_NO_MATCHING_VERSION,
            Error::DownloadFailed { .. } => errors::ERR_AUBE_RUNTIME_DOWNLOAD_FAILED,
            Error::ChecksumMismatch { .. } => errors::ERR_AUBE_RUNTIME_CHECKSUM_MISMATCH,
            Error::ExtractFailed { .. } => errors::ERR_AUBE_RUNTIME_EXTRACT_FAILED,
            Error::MiseInstallFailed { .. } => errors::ERR_AUBE_RUNTIME_MISE_INSTALL_FAILED,
            Error::UnsupportedPlatform { .. } => errors::ERR_AUBE_RUNTIME_UNSUPPORTED_PLATFORM,
            Error::Offline { .. } => errors::ERR_AUBE_OFFLINE,
            // Generic exit code (no EXIT_TABLE entry) — a lock or
            // rename failure is not a download failure, and the
            // message names the failing path.
            Error::Io { .. } => errors::ERR_AUBE_RUNTIME_IO,
        }
    }

    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}
